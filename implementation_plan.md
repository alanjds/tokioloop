# Implementation Plan

## Current Focus: Fix TokioLoop Raw Benchmark Failure

### Issue Description

The 'raw' benchmark consistently fails for TokioLoop while 'stream' and 'proto' benchmarks work. The test `test_raw_tcp_server` hangs on TokioLoop but passes on asyncio, uvloop, and rloop.RLoop.

### Root Cause

TokioLoop's `add_reader()` method runs callbacks **immediately** instead of waiting for socket readability. This causes `sock_accept()` to hang forever because:

1. `sock_accept()` calls `add_reader()` to register a callback that calls `sock.accept()`
2. The callback should only run when the socket is readable (has a pending connection)
3. With broken TokioLoop, the callback runs immediately
4. `sock.accept()` fails with `BlockingIOError` (no pending connection)
5. The future never completes, causing the test to hang

### Technical Details

**RLoop Working Pattern:**
- Registers fd with mio's poller using `Interest::READABLE`
- Stores callback in `handles_io` map
- In event loop: `io.poll()` waits for I/O events
- When mio fires a "readable" event, only then runs the callback

**TokioLoop Broken Pattern:**
- Stores callback in `io_callbacks` map
- **Immediately schedules** callback via `schedule_handle()`
- Never waits for fd to become readable
- Callback runs immediately, `sock.accept()` fails, future never completes

### Solution

Use `tokio::io::unix::AsyncFd` to properly wait for socket readability without consuming the connection:

```rust
use tokio::io::unix::AsyncFd;

// In add_reader:
let fd_dup = unsafe { libc::dup(fd as i32) };
let std_socket = unsafe { std::net::TcpListener::from_raw_fd(fd_dup) };
let async_fd = AsyncFd::new(std_socket).unwrap();

// Wait for readability without consuming
let mut guard = async_fd.readable().await.unwrap();
guard.clear_ready();

// Now schedule the Python callback
Python::attach(|py| {
    let handle = TCBHandle::new(callback_py.clone_ref(py), args_py.clone_ref(py), context_py.clone_ref(py));
    let handle_obj = Py::new(py, handle).unwrap();
    let mut state = TEventLoopRunState{};
    let _ = handle_obj.run(py, &loop_handlers, &state);
});
```

### Files to be Modified

- `src/tokio_event_loop.rs` - Fix `add_reader()` method to use `AsyncFd`

### Implementation Order

1. Add `use tokio::io::unix::AsyncFd;` to imports
2. Replace `add_reader()` implementation with AsyncFd-based approach
3. Rebuild: `RUSTFLAGS=-Awarnings maturin develop`
4. Test: `pytest tests/tcp/test_tcp_server.py::test_raw_tcp_server -v -s --timeout=30`
5. Verify test passes for all event loops
6. Run raw benchmark: `python benchmarks/benchmarks.py raw`

### Testing

- Test: `test_raw_tcp_server` in `tests/tcp/test_tcp_server.py`
- Verify passes for: asyncio, uvloop, rloop.RLoop, rloop.TokioLoop
- Run raw benchmark to confirm fix

### Success Criteria

- `test_raw_tcp_server` passes for all event loops
- Raw benchmark completes successfully
- No regression in other tests

---

## Previous: Fix `server.sockets` property (COMPLETED)

This was a previous issue that has been resolved. The `_DetachedSocketWrapper` class was created in `rloop/loop.py` with `getsockname()` method, and `Server.sockets` property now returns socket wrappers instead of addresses.
