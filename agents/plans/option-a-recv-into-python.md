# Option A — "recv-into-Python": Single-Copy EAGAIN Path

## Goal

Eliminate one data copy and one heap allocation on the EAGAIN `sock_recv` path by having
the `_run` event loop perform `recv` directly into Python memory via `PyBytes::new_with`,
instead of first filling a Rust `Vec` and then copying it into a `PyBytes`.

## Branch

`feat/native-sock-option-a` — based on `feat/native-sock-v3`

## Current EAGAIN Path (2 copies, 2 allocs)

```
_sock_recv_native  →  EAGAIN  →  SockRecvMsg::Recv { nbytes, fut }  →  worker channel
worker:  readable().await
         recv(fd, buf, nbytes, MSG_DONTWAIT)   ← ALLOC 1 + COPY 1  (kernel → Vec<u8>)
         scheduler_tx.send(RustCallHandle)
_run:    attach_blocking → PyBytes::new(py, &buf)  ← ALLOC 2 + COPY 2  (Vec → Python heap)
                         → fut.set_result(data)
```

## Target EAGAIN Path (1 copy, 1 alloc)

```
_sock_recv_native  →  EAGAIN  →  SockRecvMsg::Recv { nbytes, fut }  →  worker channel
worker:  readable().await  →  FIONREAD (avail)  →  guard.clear_ready()
         scheduler_tx.send(ScheduledTask::RecvReady { fd, avail, nbytes, fut })
_run:    attach_blocking
         → PyBytes::new_with(py, min(avail, nbytes), |dst| recv(fd, dst, n, 0))
                                                      ← ALLOC 1 + COPY 1  (kernel → Python)
         → fut.set_result(data)
```

The worker no longer allocates or fills a buffer. It only signals readiness.

## Files to Modify

**`src/tokio_event_loop.rs`** — all changes are here.

### Change 1 — Add `ScheduledTask::RecvReady` variant

In the `ScheduledTask` enum (around line 73), add:

```rust
pub(crate) enum ScheduledTask {
    Immediate { handle: TBoxedHandle },
    Delayed { timer: TokioTimer },
    RecvReady { fd: i32, avail: usize, nbytes: usize, fut: Py<PyAny> },
}
```

Update the `Debug` impl to add:
```rust
ScheduledTask::RecvReady { fd, .. } => write!(f, "ScheduledTask::RecvReady {{ fd: {} }}", fd),
```

### Change 2 — Modify `sock_reader_task`

Replace the section that allocates a Vec, calls recv, and sends a `RustCallHandle`:

```rust
// OLD — inside the while loop, after SockRecvMsg::Recv destructure:
let mut buf = vec![0u8; nbytes];
let outcome: Result<Vec<u8>, std::io::Error> = loop {
    let n = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, nbytes, libc::MSG_DONTWAIT) };
    if n >= 0 { buf.truncate(n as usize); break Ok(buf); }
    let e = std::io::Error::last_os_error();
    if e.kind() != std::io::ErrorKind::WouldBlock { break Err(e); }
    match async_fd.readable().await {
        Err(e) => break Err(e),
        Ok(mut guard) => { guard.clear_ready(); }
    }
};
let fatal = outcome.is_err();
let _ = scheduler_tx.try_send(ScheduledTask::Immediate {
    handle: Box::new(RustCallHandle::new(move |py| {
        match outcome {
            Ok(buf) => {
                let data = pyo3::types::PyBytes::new(py, &buf);
                let _ = fut.call_method1(py, "set_result", (data,));
            }
            Err(e) => {
                let _ = fut.call_method1(py, "set_exception", (PyErr::from(e).into_value(py),));
            }
        }
    })),
});
if fatal { break; }
```

Replace with:

```rust
// NEW — wait for readability, query available bytes, signal _run to do the recv
let result: Result<(i32, usize), std::io::Error> = loop {
    // Try immediate recv first (data may already be present)
    let mut avail: libc::c_int = 0;
    let ret = unsafe { libc::ioctl(fd, libc::FIONREAD, &mut avail) };
    if ret == 0 && avail > 0 {
        break Ok((fd, avail as usize));
    }
    // Data not ready — wait for epoll readability event
    match async_fd.readable().await {
        Err(e) => break Err(e),
        Ok(mut guard) => {
            guard.clear_ready();
            // After clearing, loop back to FIONREAD check
        }
    }
};

let fatal = result.is_err();
match result {
    Ok((fd, avail)) => {
        let _ = scheduler_tx.try_send(ScheduledTask::RecvReady { fd, avail, nbytes, fut });
    }
    Err(e) => {
        let _ = scheduler_tx.try_send(ScheduledTask::Immediate {
            handle: Box::new(RustCallHandle::new(move |py| {
                let _ = fut.call_method1(py, "set_exception", (PyErr::from(e).into_value(py),));
            })),
        });
    }
}
if fatal { break; }
```

### Change 3 — Handle `RecvReady` in `_run`'s GIL batch

In the `_run` method (around line 421), the scheduler drains tasks into `current_handles: VecDeque<TBoxedHandle>`.
`RecvReady` cannot go into `current_handles` (it's not a `TBoxedHandle`), so add a parallel queue:

Add alongside `current_handles`:
```rust
let mut pending_recvs: VecDeque<(i32, usize, usize, Py<PyAny>)> = VecDeque::new();
// (fd, avail, nbytes, fut)
```

In the scheduler drain sections (both `Immediate` and `Delayed` match arms), add handling for `RecvReady`:
```rust
ScheduledTask::RecvReady { fd, avail, nbytes, fut } => {
    pending_recvs.push_back((fd, avail, nbytes, fut));
}
```

In the GIL batch section (after `if !current_handles.is_empty()`), add:

```rust
if !pending_recvs.is_empty() {
    attach_blocking(|py| {
        while let Some((fd, avail, nbytes, fut)) = pending_recvs.pop_front() {
            if avail == 0 {
                // EOF
                let data = pyo3::types::PyBytes::new(py, b"");
                let _ = fut.call_method1(py, "set_result", (data,));
                continue;
            }
            let n = avail.min(nbytes);
            let result = pyo3::types::PyBytes::new_with(py, n, |dst: &mut [u8]| {
                let k = unsafe {
                    libc::recv(fd, dst.as_mut_ptr() as *mut libc::c_void, n, 0)
                };
                if k < 0 {
                    return Err(PyErr::from(std::io::Error::last_os_error()));
                }
                // k == n is guaranteed: FIONREAD reported avail >= n bytes
                // and recv(n) reads exactly n bytes atomically on Linux TCP.
                Ok(())
            });
            match result {
                Ok(data) => { let _ = fut.call_method1(py, "set_result", (data,)); }
                Err(e) => { let _ = fut.call_method1(py, "set_exception", (e.into_value(py),)); }
            }
        }
    });
}
```

Alternatively, merge both loops into a single `attach_blocking` call that handles both
`current_handles` and `pending_recvs` — reduces GIL acquire/release from 2 to 1 per tick.

## Edge Cases

| Condition | Handling |
|---|---|
| `avail == 0` | EOF: set_result(b"") |
| `recv` returns error | set_exception(OSError) |
| `recv` returns `k < n` | Should not happen after FIONREAD on Linux TCP; add `debug_assert_eq!(k, n as isize)` and treat remaining bytes as 0 (or raise error) |
| `FIONREAD` returns `-1` | Treat as EOF (connection reset), set_exception |
| Worker `rx` closed (fd closed from Python) | Worker exits loop, removes entry from `sock_readers` — already handled |

## Safety Note

`recv(fd, dst, n, 0)` is called while holding the GIL (inside `attach_blocking`).
After `readable().await` + FIONREAD confirming `avail >= n`, `recv(n)` on Linux TCP
is instantaneous — the kernel copies bytes from the socket buffer without any wait.
The Python main thread is blocked in `runtime.block_on()` for the duration.

`fd` in `_run` is the **original** fd (not `fd_dup`). Reading from either end of a
`dup()` pair reads from the same socket buffer — safe.

## Imports to Add (if not already present)

At the top of `src/tokio_event_loop.rs`, ensure:
```rust
use pyo3::types::PyBytes;
```

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

Target after Option A: **≥86% / ≥75% / ≥55%**

---

## Results (branch `claude/native-sock-option-perf-EyPqK`)

Option A was implemented and an EOF stale-task fix was added (Option B-lite was also
explored but reverted — it was 13 pp slower on 1KB due to per-EAGAIN task spawn overhead).

### Raw benchmark

| Loop | 1KB | 10KB | 100KB |
|------|----:|-----:|------:|
| asyncio | 10,985 | 10,761 | 6,645 |
| tokioloop | 10,913 | 8,714 | 6,350 |
| **ratio** | **99.4%** | **81.0%** | **95.5%** |

### Proto benchmark (`loop.create_server` + `asyncio.Protocol`)

| Loop | 1KB | 10KB | 100KB |
|------|----:|-----:|------:|
| asyncio | 12,667 | 11,932 | 8,417 |
| tokioloop | 8,694 | 8,217 | 4,537 |
| **ratio** | **68.6%** | **68.9%** | **53.9%** |

### Stream benchmark (`asyncio.start_server` + StreamReader.readline)

| Loop | 1KB | 10KB | 100KB |
|------|----:|-----:|------:|
| asyncio | 10,821 | 9,950 | 5,810 |
| tokioloop | 2,961 | 2,959 | 2,222 |
| **ratio** | **27.4%** | **29.7%** | **38.2%** |

Raw is near parity. Proto and stream have identified bottlenecks described in the next plan.

## Next Steps

See **[`agents/plans/proto-stream-improvements.md`](proto-stream-improvements.md)** for the
follow-up optimizations:
- Remove unnecessary `to_vec()` in `io_processing_loop` (`src/tokio_tcp.rs`) → proto +5-10 pp
- `TokioStreamReader` deque-based buffering (`rloop/streams.py`) → stream +15-20 pp
