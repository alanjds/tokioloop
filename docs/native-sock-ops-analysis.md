# Native sock_* ops — Analysis

## Idea

`sock_recv`, `sock_sendall`, and `sock_accept` in `asyncio.BaseEventLoop` go through the
Python `add_reader`/`remove_reader` machinery:

1. `add_reader(fd, callback)` — sends a watch request to a persistent Rust watcher task
2. The watcher does `AsyncFd.readable().await` (fd already registered in epoll)
3. When readable, schedules the Python callback via the scheduler channel
4. Python callback runs `recv(fd, nbytes)` and resolves the asyncio Future

The hypothesis was that bypassing the Python callback layer and doing the recv entirely in
Rust would reduce GIL contention and call overhead, improving throughput.

### Implementation (commit `6572499` on `feat/native-sock-ops-analysis`)

Added `_sock_recv_native`, `_sock_sendall_native`, `_sock_accept_native` to `TEventLoop`
(Rust). `TokioLoop` overrides `sock_recv`, `sock_sendall`, `sock_accept` in Python to call
the native variants directly.

Each method has two paths:

**Fast path** — GIL held, no task spawn:
```
recv(fd, MSG_DONTWAIT) → data available → set_result immediately → coroutine resumes without yielding
```

**Slow path** — data not yet available (EAGAIN):
```
dup(fd) → AsyncFd::new(fd_dup) → readable().await → recv(fd, MSG_DONTWAIT) → scheduler_tx.try_send(RustCallHandle)
```

The single-allocation trick: the buffer allocated for the fast-path attempt is moved into
the `async move` block so the slow path reuses it (no second `vec![0u8; nbytes]`).

---

## Results

Raw benchmark (concurrency=1, `sock_recv/sock_sendall` echo server):

| Loop | 1KB | 10KB | 100KB |
|------|----:|-----:|------:|
| asyncio | 10,099 | 9,155 | 6,129 |
| rloop | 11,127 | 10,002 | 5,163 |
| **tokioloop — native** | **3,406** | **3,058** | **2,517** |
| **tokioloop — add_reader (baseline)** | **5,307** | **5,224** | **4,426** |
| uvloop | 12,828 | 10,018 | 6,205 |

**Regression vs add_reader baseline: −36% / −41% / −43%**

---

## Root Cause of Regression

### The fast path doesn't hit in sequential request-reply

In the echo benchmark (concurrency=1):

1. Server sends echo
2. Server immediately calls `sock_recv` again → `recv(MSG_DONTWAIT)` → **EAGAIN**
   (the client hasn't yet received the echo and sent the next message)
3. Every request falls through to the slow path

The fast path only helps when data is already in the kernel buffer at call time — e.g.,
pipelined requests, large messages that arrive in chunks, or concurrent connections.

### Per-request overhead in the slow path

Each slow-path `sock_recv` call does:

| Operation | Estimated cost |
|-----------|----------------|
| `dup(fd)` | ~100 ns |
| `AsyncFd::new()` → `epoll_ctl(EPOLL_CTL_ADD)` | ~2 µs |
| `runtime.spawn(async move {...})` — heap alloc + work-stealing queue | ~5–15 µs |
| `readable().await` — wait for data |  |
| `clear_ready() + drop(async_fd)` → `epoll_ctl(EPOLL_CTL_DEL) + close(fd_dup)` | ~2 µs |
| `scheduler_tx.try_send` + event-loop tick | ~2 µs |

**Total slow-path overhead per request: ~12–22 µs (excluding wait time)**

The add_reader path avoids most of this:
- **No spawn per request** — watcher task is persistent
- **No epoll_ctl per request** — `AsyncFd` stays registered across calls
- Only a channel message triggers a new watch cycle (~100 ns)

The net extra overhead of the native slow path vs add_reader is ~10–20 µs per request,
which at ~3,400 rps compounds to measurable latency inflation across the entire pipeline.

---

## Ideas to Overcome the Regression

### 1. Persistent task per fd (most promising)

Maintain one long-lived tokio task per open fd, analogous to the existing add_reader
watcher. When `sock_recv` is called:
- Send a `(nbytes, fut)` message to the fd's task via a oneshot or channel
- The task loops: `recv(MSG_DONTWAIT)` → if EAGAIN → `AsyncFd.readable().await` → recv → resolve fut
- Task stays alive and `AsyncFd` stays registered across multiple recv calls

Eliminates: spawn per request, epoll_ctl per request.

```
open_connection → spawn one watcher task → reuse for all sock_recv calls on that fd
```

The task would be keyed by fd and stored in a `HashMap<RawFd, JoinHandle<...>>` or similar.
Complexity: need to handle task cleanup when the fd is closed / socket dropped.

### 2. Cache AsyncFd per fd

A lighter variant: store `Arc<AsyncFd<OwnedFd>>` keyed by raw fd in the event loop.
Reuse the same `AsyncFd` across multiple calls; only create/destroy it when the connection
is opened/closed, not per-request.

Eliminates: dup + epoll_ctl per request.
Still spawns a task per request, but that's cheaper without the epoll_ctl overhead.

### 3. Single-threaded async recv (no spawn)

Instead of `runtime.spawn`, implement `sock_recv` as a `Future` that can be polled
directly by the event loop's own async context. The event loop would poll the recv future
on each tick, avoiding thread hops entirely.

Requires restructuring the event loop main loop to be an `async fn` rather than a
synchronous scheduler drain.

### 4. io_uring (Linux 5.1+)

Use `tokio-uring` or raw `io_uring` to submit recv operations and collect completions
without epoll at all. Eliminates all epoll_ctl overhead; recv results arrive via the
completion ring.

Most impactful for large numbers of concurrent connections; overkill for concurrency=1.

### 5. Optimise the fast path for pipelined or concurrent workloads

Even without fixing the slow path, the fast path (`recv(MSG_DONTWAIT)` with GIL held)
provides real benefit in:
- High-concurrency scenarios where many connections have buffered data
- Pipelined clients that send multiple requests before waiting for replies
- After a `readable` notification has fired (data is guaranteed present)

Combining option 1 (persistent task) with the fast path would give: persistent epoll
registration + zero-overhead recv when data is already buffered.
