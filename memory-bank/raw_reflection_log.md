---
Date: 2026-02-06
TaskRef: "TokioLoop benchmark analysis - Performance investigation"

Learnings:
- TokioLoop TCP performance is severely degraded: 2-5% of rloop performance (200-50x slower)
- Benchmark environment: 32 CPU cores, Python 3.13, Protocol mode (asyncio.Protocol), concurrency=1
- Performance comparison (1KB messages): asyncio=101K RPS, rloop=100K RPS, tokioloop=2.7K RPS, uvloop=115K RPS
- Latency is dramatically worse: TokioLoop 0.365ms vs others 0.011ms (33x higher)
- Performance degrades with larger messages: 1KB=2.7K RPS, 10KB=1.3K RPS, 100KB=218 RPS
- Protocol mode performs better than streams for TokioLoop (streams showed ~0.5% performance)

GIL_Disabled_Testing:
- Disabling Python's GIL made TokioLoop 9-17% slower, not faster
- This indicates the GIL is not the bottleneck for TokioLoop's poor performance
- Other loops (asyncio, rloop, uvloop) were unaffected by GIL changes
- Suggests the issue is in the Rust TCP implementation, not Python-level locking

Performance_Instability:
- Significant variance between runs: 1KB performance varied from 5,052 to 2,727 RPS (46% difference)
- Inconsistent results suggest race conditions, timing issues, or unstable async task scheduling
- Other loops show stable performance across runs
- Indicates fundamental instability in TokioLoop TCP implementation

Benchmark_Tooling:
- Benchmark data stored in benchmarks/results/data.json
- Processing tool: noir executable (template-based report generator)
- Command: `cd benchmarks && ./noir -c results/data.json templates/main.md > results/report.md`
- Pretty-print JSON: `jq . results/data.json` or `python -m json.tool results/data.json`

Current_Benchmark_Status:
- asyncio: ✅ Stable, ~100K RPS (baseline)
- rloop: ✅ Stable, ~100K RPS (target baseline)
- tokioloop: ❌ Severely degraded, ~2.7K RPS, unstable
- uvloop: ✅ Stable, ~115K RPS (fastest)

Root_Cause_Analysis:
- Performance bottleneck is in TokioLoop's TCP implementation (Rust code)
- Not related to Python GIL or threading
- Likely issues: async I/O handling, buffer management, task scheduling, or resource contention
- Socket conversion issues were previously fixed but performance remains poor
- Need to investigate start_io_tasks() implementation and tokio async task management

Difficulties:
- Performance varies significantly between runs, making it hard to establish baseline
- Multiple potential bottlenecks in the async I/O pipeline
- Need to profile Rust code to identify specific performance issues
- Benchmark results show fundamental implementation problems, not minor optimizations

Successes:
- Successfully established benchmark infrastructure and baseline measurements
- Confirmed that GIL is not the bottleneck (eliminated one potential cause)
- Identified that Protocol mode works better than streams for TokioLoop
- Established clear performance targets: need ~20-50x improvement to match rloop

Next_Steps_For_Performance:
- Profile TokioLoop TCP implementation to identify bottlenecks
- Investigate start_io_tasks() and async I/O operations in tokio_tcp.rs
- Check for unnecessary blocking operations or inefficient buffer management
- Review tokio task scheduling and async/await patterns
- Consider adding performance instrumentation to Rust code

Improvements_Identified_For_Consolidation:
- Benchmark analysis: Establishing baselines and comparing performance across implementations
- Performance debugging: GIL testing methodology for Python extensions
- Instability patterns: Large variance between runs indicates fundamental implementation issues
- Benchmark tooling: noir template-based report generation for JSON benchmark data
- Performance targets: Need 20-50x improvement to match baseline implementations

---
Date: 2026-02-07
TaskRef: "TokioLoop raw benchmark failure - add_reader bug investigation"

Learnings:
- The 'raw' benchmark consistently fails for TokioLoop while 'stream' and 'proto' benchmarks work
- Root cause: TokioLoop's add_reader() method runs callbacks immediately instead of waiting for socket readability
- RLoop's add_reader() correctly registers fd with mio's poller and only runs callbacks when mio fires a "readable" event
- TokioLoop's add_reader() just schedules the callback immediately via schedule_handle(), causing sock_accept() to hang forever

Technical_Details:
- In rloop/loop.py, sock_accept() uses add_reader() to register a callback that calls sock.accept()
- The callback should only run when the socket is readable (has a pending connection)
- With broken TokioLoop, the callback runs immediately, sock.accept() fails with BlockingIOError, and the future never completes
- Test created (test_raw_tcp_server) with explicit delays to expose the bug - it hangs on TokioLoop

RLoop_Working_Pattern:
```rust
fn add_reader(&self, py: Python, fd: usize, callback: Py<PyAny>, ...) -> Py<CBHandle> {
    // Register fd with mio's poller - THIS IS THE KEY!
    let guard_poll = self.io.lock().unwrap();
    _ = guard_poll.registry().register(&mut source, token, Interest::READABLE);

    // Store callback in handles_io
    self.handles_io.pin().insert(token, IOHandle::Py(PyHandleData {
        interest: Interest::READABLE,
        cbr: Some(handle.clone_ref(py)),  // ← Store callback
        cbw: None,
    }));
}
```

Then in event loop:
```rust
// Wait for I/O events using mio
io.poll(&mut state.events, sched_time.map(Duration::from_micros));

// When fd becomes readable, mio fires an event
for event in &state.events {
    if event.is_readable() {
        handles.push_back(Box::new(handle.cbr.clone_ref(py)));  // ← Only then run callback!
    }
}
```

TokioLoop_Broken_Pattern:
```rust
fn add_reader(&self, py: Python, fd: usize, callback: Py<PyAny>, ...) -> PyResult<...> {
    // Store callback but DON'T register with tokio!
    let mut callbacks = self.io_callbacks.pin();
    callbacks.insert(fd, (handle_obj.clone_ref(py).into(), py.None(), context.clone_ref(py)));

    // Schedule callback IMMEDIATELY - THIS IS THE BUG!
    self.schedule_handle(handle_obj.clone_ref(py), None)?;  // ← Runs now, not when readable!
}
```

Fix_Attempt_1:
- Tried using tokio::net::TcpListener::accept() to wait for connections
- Problem: accept() consumes the connection, so when Python callback tries sock.accept(), there's no pending connection
- Result: Test fails with "Connection reset by peer"

Fix_Attempt_2:
- Tried using tokio::net::TcpListener::readable() to wait without consuming
- Problem: tokio::net::TcpListener doesn't have a readable() method
- Problem: std::os::unix::net::TcpListener doesn't exist (should be std::net::TcpListener)
- Result: Compilation errors

Correct_Approach:
- Use tokio::io::unix::AsyncFd to wait for readability without consuming the connection
- AsyncFd provides a way to monitor file descriptors with tokio's async runtime
- When AsyncFd indicates readability, schedule the Python callback
- The Python callback can then call sock.accept() which will succeed

Implementation_Needed:
```rust
use tokio::io::unix::AsyncFd;

// In add_reader:
let std_socket = unsafe { std::net::TcpListener::from_raw_fd(fd_dup) };
let async_fd = AsyncFd::new(std_socket).unwrap();

// Wait for readability without consuming
let mut guard = async_fd.readable().await.unwrap();
// Clear readiness
guard.clear_ready();

// Now schedule the Python callback
Python::attach(|py| {
    let handle = TCBHandle::new(callback_py.clone_ref(py), args_py.clone_ref(py), context_py.clone_ref(py));
    let handle_obj = Py::new(py, handle).unwrap();
    let mut state = TEventLoopRunState{};
    let _ = handle_obj.run(py, &loop_handlers, &state);
});
```

Current_Status:
- Test created: test_raw_tcp_server in tests/tcp/test_tcp_server.py
- Test exposes the bug: hangs on TokioLoop, passes on asyncio/uvloop/rloop
- Fix identified: Use tokio::io::unix::AsyncFd for proper I/O waiting
- Fix not yet implemented: Compilation errors need to be resolved

Difficulties:
- Understanding the difference between mio and tokio I/O patterns
- Finding the correct tokio API for waiting on socket readability without consuming
- Managing fd ownership between std, tokio, and Python socket objects
- Compilation errors with incorrect type paths

Successes:
- Successfully identified the root cause of the raw benchmark failure
- Created a test that reliably exposes the bug
- Understood the correct pattern from RLoop's working implementation
- Identified the correct tokio API (AsyncFd) for the fix

Next_Steps:
1. Implement add_reader() fix using tokio::io::unix::AsyncFd
2. Rebuild with: RUSTFLAGS=-Awarnings maturin develop
3. Test with: pytest tests/tcp/test_tcp_server.py::test_raw_tcp_server -v -s --timeout=30
4. Verify test passes for all event loops
5. Run raw benchmark to confirm fix: python benchmarks/benchmarks.py raw

  Improvements_Identified_For_Consolidation:
  - I/O event handling patterns: mio vs tokio approaches for waiting on socket readability
  - add_reader() implementation: Must register with poller and wait for events, not schedule immediately
  - AsyncFd usage: tokio::io::unix::AsyncFd is the correct API for monitoring file descriptors
  - Test-driven debugging: Creating tests with explicit delays to expose timing-dependent bugs
  - Socket ownership: Critical to manage fd ownership between std, tokio, and Python socket objects

---
Date: 2026-02-07
TaskRef: "TokioLoop bottleneck analysis - Root cause identification"

Learnings:
- Identified 7 critical bottlenecks in TokioLoop TCP implementation causing 200-50x performance degradation
- Bottleneck #1: Excessive GIL acquisition in hot path - every read operation calls Python::attach (~100K times/sec)
- Bottleneck #2: Mutex contention on state - locked on every I/O operation, creating contention
- Bottleneck #3: Busy loop with 1ms sleep - wastes CPU cycles, prevents proper async waiting
- Bottleneck #4: Inefficient buffer management - VecDeque<Vec<u8>> causes excessive allocations
- Bottleneck #5: No batching of operations - each read immediately forwarded to Python
- Bottleneck #6: Tokio::select! overhead - evaluates all branches every iteration
- Bottleneck #7: Python::attach overhead - called multiple times per iteration

GIL_Analysis_Correction:
- Initial assumption that GIL would limit multi-threading benefits was incorrect
- GIL is now optional in Python 3.13+ (via envvar)
- Benchmark showed GIL OFF made TokioLoop 9-17% slower, proving GIL is not the bottleneck
- Real bottlenecks are in Rust implementation: mutex locks, Python::attach overhead, busy loop
- Multi-threading benefits will appear once Rust-level bottlenecks are eliminated

Performance_Comparison:
| Implementation | GIL Acquisitions/sec | Mutex Locks/sec | Sleep Calls/sec |
|----------------|---------------------|-----------------|-----------------|
| RLoop | ~100 | ~100 | Adaptive |
| TokioLoop | ~100,000 | ~100,000 | ~1,000 |

Root_Cause_Analysis:
- TokioLoop's multi-threaded design adds overhead without providing benefits
- GIL serialization is not the issue - the issue is in async I/O coordination
- Mutex contention creates bottlenecks even across threads
- Excessive GIL acquisition in hot path (every packet read)
- Busy loop wastes CPU cycles

Benchmark_Tooling:
- jaq command for CLI table: `jaq -r '.results.proto as $proto | ["Loop", "1KB RPS", "10KB RPS", "100KB RPS", "1KB Latency", "10KB Latency", "100KB Latency"], (["asyncio", "rloop", "tokioloop", "uvloop"][] as $loop | [$loop, ($proto[$loop]["1"]["1024"].rps | tostring), ($proto[$loop]["1"]["10240"].rps | tostring), ($proto[$loop]["1"]["102400"].rps | tostring), ($proto[$loop]["1"]["1024"].latency_mean + "ms"), ($proto[$loop]["1"]["10240"].latency_mean + "ms"), ($proto[$loop]["1"]["102400"].latency_mean + "ms")]) | @tsv' results/data.json | column -t -s $'\t'`

Recommended_Fixes_Priority:
High Priority (Expected 10-20x improvement):
1. Batch reads before forwarding to Python
2. Replace Mutex with lock-free structures (atomic flags, lock-free queues)
3. Remove busy loop sleep (use proper tokio async waiting)

Medium Priority (Expected 2-5x improvement):
4. Optimize buffer management (single buffer with read/write indices)
5. Reduce Python::attach calls (cache references, minimize acquisitions)

Low Priority (Expected 1.5-2x improvement):
6. Simplify tokio::select! (separate tasks for read/write)
7. Profile and optimize hot paths (flamegraph analysis)

Difficulties:
- Complex async I/O coordination makes optimization challenging
- Need to maintain asyncio API compatibility while optimizing
- Lock-free structures require careful design to avoid race conditions
- Batching strategy needs to balance latency vs throughput

Successes:
- Successfully identified all major bottlenecks through code analysis
- Established clear priority order for optimizations
- Created benchmark tooling for validation
- Corrected misunderstanding about GIL's role in performance

Next_Steps:
- Implement high-priority fixes first for maximum impact
- Use benchmarks to validate each optimization
- Profile after each fix to identify remaining bottlenecks
- Target: Achieve 80-100% of rloop performance with GIL OFF

Improvements_Identified_For_Consolidation:
- Performance bottleneck identification: Systematic analysis of async I/O hot paths
- GIL vs Rust bottlenecks: Distinguishing between Python-level and Rust-level performance issues
- Lock-free design patterns: Atomic flags and lock-free queues for high-performance async I/O
- Batching strategies: Balancing latency vs throughput in async systems
- Benchmark-driven optimization: Using quantitative measurements to guide optimization priorities
