# Consolidated Learnings - TokioLoop Project

This file contains distilled, actionable insights from the raw reflection log for long-term reference.

## Development Workflow

### Critical Commands
- **After Rust changes:** `RUSTFLAGS=-Awarnings maturin develop` - Required for Python tests to reflect changes
- **Run tests:** `pytest tests/tcp/ -v` for TCP validation
- **Debug logging:** `RUST_LOG=debug` for detailed Rust logging
- **Specific test:** `pytest tests/tcp/test_tcp_server.py::test_raw_tcp_server -v -s --timeout=30`

### Mixed Language Projects
- Rust + Python extensions require rebuild steps after Rust code changes
- Use `maturin develop` for development builds
- Test-driven development is essential for validating Rust changes

## I/O Event Handling Patterns

### mio vs tokio Approaches

**mio (RLoop - Working):**
```rust
// Register fd with mio's poller
let guard_poll = self.io.lock().unwrap();
guard_poll.registry().register(&mut source, token, Interest::READABLE);

// Store callback in handles_io
self.handles_io.pin().insert(token, IOHandle::Py(PyHandleData {
    interest: Interest::READABLE,
    cbr: Some(handle.clone_ref(py)),
    cbw: None,
}));

// In event loop: Wait for I/O events
io.poll(&mut state.events, sched_time.map(Duration::from_micros));

// When fd becomes readable, mio fires an event
for event in &state.events {
    if event.is_readable() {
        handles.push_back(Box::new(handle.cbr.clone_ref(py)));  // Only then run callback!
    }
}
```

**tokio (TokioLoop - Correct Pattern):**
```rust
use tokio::io::unix::AsyncFd;

// Duplicate fd to avoid ownership issues
let fd_dup = unsafe { libc::dup(fd as i32) };

// Convert to std socket
let std_socket = unsafe { std::net::TcpListener::from_raw_fd(fd_dup) };

// Wrap in AsyncFd for tokio monitoring
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

### add_reader() Implementation Pattern
- **Must register with poller and wait for events, not schedule immediately**
- Store callback for later execution when fd becomes readable
- Only run callback when the poller fires a "readable" event
- **Anti-pattern:** Scheduling callback immediately causes sock_accept() to hang forever

### AsyncFd Usage
- **tokio::io::unix::AsyncFd** is the correct API for monitoring file descriptors with tokio
- Allows waiting for readability without consuming the connection
- Critical for implementing add_reader() correctly in tokio-based event loops

## Socket Conversion Patterns

### Socket2 Conversion Method
- **Preferred method** for converting between std and tokio socket types
- Pipeline: `std socket -> socket2::Socket -> tokio::net::TcpListener`
- Provides reliable fd ownership management between std and tokio
- FD duplication and proper ownership transfer are critical for success

### Socket Ownership
- Critical to manage fd ownership between std, tokio, and Python socket objects
- Use `libc::dup()` to duplicate file descriptors before conversion
- Proper ownership transfer prevents resource leaks and crashes

## Test-Driven Debugging

### Exposing Timing-Dependent Bugs
- Create tests with explicit delays to expose timing-dependent bugs
- Example: `test_raw_tcp_server` uses `await asyncio.sleep(0.1)` and `time.sleep(0.5)` to ensure no pending connection when `sock_accept()` is called
- This forces the event loop to wait for readability, exposing bugs in add_reader() implementation

### Benchmark Analysis
- Establish baselines and compare performance across implementations
- Use noir template-based report generation for JSON benchmark data
- Command: `cd benchmarks && ./noir -c results/data.json templates/main.md > results/report.md`
- Pretty-print JSON: `jq . results/data.json` or `python -m json.tool results/data.json`

## Performance Debugging

### GIL Testing Methodology
- Test with GIL disabled: `python -X gil=0` to identify GIL-related bottlenecks
- If disabling GIL makes code slower, the bottleneck is not in Python-level locking
- This helps isolate whether issues are in Rust implementation vs Python integration

### Instability Patterns
- Large variance between runs (e.g., 46% difference) indicates fundamental implementation issues
- Suggests race conditions, timing issues, or unstable async task scheduling
- Stable implementations show consistent performance across runs

## Current TokioLoop Status

### Working Components
- ✅ Basic event loop infrastructure
- ✅ Task scheduling (immediate and delayed)
- ✅ Signal handling
- ✅ Python-Rust bindings
- ✅ Import tests (6/6 pass)

### Broken Components
- ❌ TCP server functionality (socket conversion issues)
- ❌ TCP client functionality (incomplete)
- ❌ Real I/O operations (missing)
- ❌ add_reader() implementation (runs immediately instead of waiting for readability)
- ❌ Raw benchmark (fails due to add_reader bug)

### Performance Issues
- Severely degraded: 2-5% of rloop performance (200-50x slower)
- Latency: 33x higher than baseline implementations
- Performance degrades with larger messages
- Significant variance between runs indicates instability

## Key Technical Challenges

1. **Integrating tokio async I/O with asyncio compatibility layer**
2. **Proper error handling and resource management**
3. **Event loop lifecycle and task completion**
4. **Performance while maintaining compatibility**
5. **Debugging complex async interactions**

## Success Criteria

- All existing TCP tests pass
- No memory leaks or resource issues
- Proper error handling and cleanup
- Comprehensive debug logging for troubleshooting
- Full asyncio API compatibility
- Performance within 20-50% of baseline implementations
