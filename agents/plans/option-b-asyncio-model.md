# Option B — "Asyncio Model": fd Polling Inside _run, No Worker Tasks

## Goal

Eliminate the per-fd worker Tokio tasks for `sock_recv` entirely. Instead, `_run` itself
manages a dynamic set of `AsyncFd` readability futures (one per pending `sock_recv` call),
selecting on them alongside the scheduler and timers — exactly like asyncio's epoll model.
When an fd fires, `_run` does `recv` directly while holding the GIL into `PyBytes::new_with`.

This removes **both** channel hops (Python→worker and worker→scheduler) and **both** task
wake-ups that the current architecture requires on the EAGAIN path.

## Branch

`feat/native-sock-option-b` — based on `feat/native-sock-v3`

## Current EAGAIN Path (2 hops, 2 copies)

```
_sock_recv_native  →  EAGAIN
  →  SockRecvMsg::Recv → worker channel  (HOP 1)
     worker:  readable().await  →  recv → Vec  (COPY 1)
  →  RustCallHandle → scheduler channel  (HOP 2)
     _run:  attach_blocking  →  PyBytes::new  (COPY 2)  →  set_result
```

## Target EAGAIN Path (0 hops, 1 copy)

```
_sock_recv_native  →  EAGAIN
  →  insert (AsyncFd(fd_dup), nbytes, fut) into pending_reads map
  →  notify _run via pending_reads_notify.notify_one()

_run select! loop:
  …
  ready_fd = pending_reads_set.next() =>
    attach_blocking  →  PyBytes::new_with + recv(fd, dst, n, 0)  (COPY 1)  →  set_result
```

## Architecture Changes

### New shared state on `TEventLoop`

Add to the `TEventLoop` struct:

```rust
// (fd_dup, AsyncFd guard ready, nbytes, fut) queued by _sock_recv_native on EAGAIN
pending_reads: Arc<Mutex<HashMap<i32, (OwnedFd, usize, Py<PyAny>)>>>,
pending_reads_notify: Arc<tokio::sync::Notify>,
```

`_sock_recv_native` no longer spawns worker tasks. On EAGAIN it:
1. `dup(fd)` → `fd_dup`
2. Inserts `(OwnedFd(fd_dup), nbytes, fut)` into `pending_reads`
3. Calls `pending_reads_notify.notify_one()`

### `_run` loop changes

`_run` maintains a `FuturesUnordered` of per-fd readability pollers:

```rust
use futures::stream::FuturesUnordered;
use tokio::io::unix::AsyncFd;

struct ReadPoller {
    fd_orig: i32,
    async_fd: AsyncFd<OwnedFd>,
    nbytes: usize,
    fut: Py<PyAny>,
}

impl Future for ReadPoller {
    type Output = (i32, usize, Py<PyAny>, Result<(), std::io::Error>);
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.async_fd.poll_read_ready(cx) {
            Poll::Ready(Ok(mut guard)) => {
                guard.clear_ready();
                Poll::Ready((self.fd_orig, self.nbytes, self.fut.clone(), Ok(())))
            }
            Poll::Ready(Err(e)) => Poll::Ready((self.fd_orig, self.nbytes, self.fut.clone(), Err(e))),
            Poll::Pending => Poll::Pending,
        }
    }
}
```

In `_run`'s `tokio::select!` block, add a new arm:

```rust
// Arm: drain newly-registered pending reads into the FuturesUnordered set
_ = pending_reads_notify.notified() => {
    let mut map = pending_reads.lock().unwrap();
    for (fd_dup, (owned, nbytes, fut)) in map.drain() {
        let async_fd = AsyncFd::new(owned).unwrap();
        read_pollers.push(ReadPoller { fd_orig: /* original fd */ , async_fd, nbytes, fut });
    }
}

// Arm: an fd became readable
Some((fd, nbytes, fut, result)) = read_pollers.next(), if !read_pollers.is_empty() => {
    // result is Ok(()) or Err(io::Error)
    ready_recvs.push((fd, nbytes, fut, result));
}
```

After the `select!`, handle `ready_recvs` in the GIL batch:

```rust
if !ready_recvs.is_empty() {
    attach_blocking(|py| {
        for (fd, nbytes, fut, result) in ready_recvs.drain(..) {
            match result {
                Err(e) => { let _ = fut.call_method1(py, "set_exception", (PyErr::from(e).into_value(py),)); }
                Ok(()) => {
                    let mut avail: libc::c_int = 0;
                    unsafe { libc::ioctl(fd, libc::FIONREAD, &mut avail) };
                    if avail == 0 {
                        let _ = fut.call_method1(py, "set_result", (pyo3::types::PyBytes::new(py, b""),));
                    } else {
                        let n = (avail as usize).min(nbytes);
                        let res = pyo3::types::PyBytes::new_with(py, n, |dst| {
                            let k = unsafe { libc::recv(fd, dst.as_mut_ptr() as _, n, 0) };
                            if k < 0 { return Err(PyErr::from(std::io::Error::last_os_error())); }
                            Ok(())
                        });
                        match res {
                            Ok(data) => { let _ = fut.call_method1(py, "set_result", (data,)); }
                            Err(e) => { let _ = fut.call_method1(py, "set_exception", (e.into_value(py),)); }
                        }
                    }
                }
            }
        }
    });
}
```

### Remove `sock_reader_task`, `get_or_spawn_reader`, `sock_readers`

- Remove `sock_readers: Arc<papaya::HashMap<...>>` field from `TEventLoop`
- Remove `sock_reader_task` function
- Remove `get_or_spawn_reader` method
- Remove `SockRecvMsg` enum (no longer needed)
- `_sock_recv_native` EAGAIN path: push to `pending_reads` + notify (replace worker dispatch)

## Fd Ownership

- `_sock_recv_native` holds `fd` (original, Python socket owns it)
- On EAGAIN: `dup(fd)` → `fd_dup` owned by `OwnedFd` → moved into `AsyncFd` in `_run`
- When `AsyncFd` fires and recv is done, `OwnedFd` is dropped → `fd_dup` is closed
- `recv` in `_run` is called with `fd` (original), not `fd_dup`

## Concurrency: Multiple Pending Reads on the Same fd

`asyncio` only allows one pending `sock_recv` per fd at a time (an error is raised otherwise).
TokioLoop should enforce the same: if `pending_reads` already has an entry for `fd`, raise
`BlockingIOError` or insert into a per-fd queue. The simplest safe approach: one entry per fd
(matching asyncio semantics).

## Cargo.toml Changes

Add `futures` dependency (for `FuturesUnordered` + `StreamExt`):

```toml
[dependencies]
futures = "0.3"
```

(Check if already present — may be a transitive dep of tokio.)

Or use `tokio_util::task::JoinMap` / inline `FuturesUnordered` from `futures::stream`.

## Files to Modify

- `src/tokio_event_loop.rs` — primary changes (struct, _run, _sock_recv_native, remove worker infra)
- `Cargo.toml` — add `futures = "0.3"` if not already present

## Verification

```bash
# Build
maturin develop

# Tests
.venv/bin/python -m pytest tests/test_sockets.py tests/tcp/test_tcp_server.py -k TokioLoop -q

# Benchmark (compare against feat/native-sock-v3 baseline)
BENCHMARK_EXC_PREFIX=.venv/bin .venv/bin/python benchmarks/benchmarks.py raw
```

## Baseline (feat/native-sock-v3 to beat)

| Loop | 1KB | 10KB | 100KB |
|------|----:|-----:|------:|
| asyncio | 11,513 | 10,780 | 7,418 |
| **tokioloop** | **9,883** | **7,457** | **3,444** |

tokioloop vs asyncio: **85.8% / 69.2% / 46.4%**

Target after Option B: **≥88% / ≥80% / ≥60%**

## Key Risks

- `FuturesUnordered::next()` returns `None` when empty — guard the select arm with `if !read_pollers.is_empty()`
- `pending_reads_notify.notified()` is edge-triggered; ensure no wakeup is lost when
  `_sock_recv_native` inserts while `_run` is draining (use `notify_one` after each insert,
  and drain ALL entries each time the arm fires, not just one)
- Cancellation: if a Python future is cancelled before the fd fires, the `ReadPoller` must
  be removed from `FuturesUnordered`. Use a cancellation flag or check `fut.is_cancelled()`
  in the ready handler before calling `set_result`
