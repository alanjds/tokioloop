# TokioLoop Performance Optimization Implementation Plan

## Executive Summary

TokioLoop currently performs at 2-5% of rloop performance (200-50x slower) due to 7 critical bottlenecks in the TCP implementation. This plan outlines a systematic approach to eliminate these bottlenecks and achieve 80-100% of rloop performance with GIL OFF.

**Current Performance:**
- 1KB messages: 2,727 RPS (vs rloop: 100,475 RPS)
- 10KB messages: 1,334 RPS (vs rloop: 87,080 RPS)
- 100KB messages: 218 RPS (vs rloop: 43,904 RPS)
- Latency: 0.365ms (vs rloop: 0.011ms)

**Target Performance:**
- 1KB messages: 80,000+ RPS (80% of rloop)
- 10KB messages: 70,000+ RPS (80% of rloop)
- 100KB messages: 35,000+ RPS (80% of rloop)
- Latency: <0.02ms (2x of rloop)

---

## Bottleneck Analysis

### Bottleneck #1: Excessive GIL Acquisition (CRITICAL)
**Impact:** ~100K GIL acquisitions per second
**Location:** `src/tokio_tcp.rs:io_processing_loop()`
**Issue:** Every read operation calls `Python::attach` to forward data to Python

```rust
Ok(n) => {
    let data = read_buf[..n].to_vec();
    Python::attach(|py| {
        let _ = protocol.call_method1(
            py,
            pyo3::intern!(py, "data_received"),
            (PyBytes::new(py, &data),)
        );
    });
}
```

### Bottleneck #2: Mutex Contention (CRITICAL)
**Impact:** ~100K mutex locks per second
**Location:** `src/tokio_tcp.rs:TokioTCPTransportState`
**Issue:** State is protected by `Mutex` locked on every I/O operation

```rust
let (is_closing, is_paused, read_eof) = {
    let state_lock = state.lock().unwrap();
    (state_lock.closing, state_lock.paused, state_lock.read_eof)
};
```

### Bottleneck #3: Busy Loop with 1ms Sleep (HIGH)
**Impact:** Wastes CPU cycles, prevents proper async waiting
**Location:** `src/tokio_tcp.rs:io_processing_loop()`
**Issue:** Default case in `tokio::select!` sleeps 1ms even when idle

```rust
_ = tokio::time::sleep(Duration::from_millis(1)) => {}
```

### Bottleneck #4: Inefficient Buffer Management (MEDIUM)
**Impact:** Excessive memory allocations
**Location:** `src/tokio_tcp.rs:TokioTCPTransportState`
**Issue:** `VecDeque<Vec<u8>>` creates new allocation per write

```rust
write_buf: VecDeque<Vec<u8>>,
```

### Bottleneck #5: No Batching of Operations (HIGH)
**Impact:** Each read immediately forwarded to Python
**Location:** `src/tokio_tcp.rs:io_processing_loop()`
**Issue:** No accumulation of multiple reads before Python call

### Bottleneck #6: Tokio::select! Overhead (LOW)
**Impact:** Evaluates all branches every iteration
**Location:** `src/tokio_tcp.rs:io_processing_loop()`
**Issue:** Complex select! with multiple conditional branches

### Bottleneck #7: Python::attach Overhead (MEDIUM)
**Impact:** Called multiple times per iteration
**Location:** `src/tokio_tcp.rs:io_processing_loop()`
**Issue:** Each `Python::attach` has overhead for thread attachment

---

## Optimization Plan

### Phase 1: High-Priority Fixes (Expected 10-20x improvement)

#### Fix 1.1: Batch Reads Before Forwarding to Python
**Priority:** CRITICAL
**Expected Improvement:** 5-10x
**Complexity:** Medium

**Approach:**
- Accumulate reads in a buffer
- Forward to Python when buffer reaches threshold or timeout
- Balance latency vs throughput

**Implementation:**
```rust
struct TokioTCPTransportState {
    stream: Option<TcpStream>,
    read_buf: Vec<u8>,           // Accumulated reads
    write_buf: VecDeque<Vec<u8>>,
    closing: bool,
    weof: bool,
    read_eof: bool,
    write_shutdown_done: bool,
    paused: bool,
    io_task: Option<tokio::task::JoinHandle<()>>,
    local_addr: Option<SocketAddr>,
    peer_addr: Option<SocketAddr>,
    // NEW: Batching state
    batch_accumulator: Vec<u8>,
    batch_last_flush: Instant,
}

// In io_processing_loop:
const BATCH_SIZE_THRESHOLD: usize = 8192;  // 8KB
const BATCH_TIMEOUT_MS: u64 = 5;           // 5ms max latency

Ok(n) => {
    state_lock.batch_accumulator.extend_from_slice(&read_buf[..n]);

    let should_flush = state_lock.batch_accumulator.len() >= BATCH_SIZE_THRESHOLD
        || state_lock.batch_last_flush.elapsed() > Duration::from_millis(BATCH_TIMEOUT_MS);

    if should_flush {
        let data = std::mem::take(&mut state_lock.batch_accumulator);
        state_lock.batch_last_flush = Instant::now();

        Python::attach(|py| {
            let _ = protocol.call_method1(
                py,
                pyo3::intern!(py, "data_received"),
                (PyBytes::new(py, &data),)
            );
        });
    }
}
```

**Validation:**
- Benchmark: `python benchmarks/benchmarks.py proto`
- Target: 20-30K RPS (10x improvement)
- Latency: <1ms (acceptable for batching)

#### Fix 1.2: Replace Mutex with Lock-Free Structures
**Priority:** CRITICAL
**Expected Improvement:** 2-3x
**Complexity:** High

**Approach:**
- Use `AtomicBool` for flags (closing, paused, weof, read_eof)
- Use lock-free queue for write buffer
- Only lock for complex state changes

**Implementation:**
```rust
pub(crate) struct TokioTCPTransportState {
    stream: Option<TcpStream>,
    read_buf: Vec<u8>,
    write_buf: crossbeam::queue::SegQueue<Vec<u8>>,  // Lock-free queue
    // Atomic flags instead of bool
    closing: AtomicBool,
    weof: AtomicBool,
    read_eof: AtomicBool,
    write_shutdown_done: AtomicBool,
    paused: AtomicBool,
    io_task: Option<tokio::task::JoinHandle<()>>,
    local_addr: Option<SocketAddr>,
    peer_addr: Option<SocketAddr>,
    batch_accumulator: Vec<u8>,
    batch_last_flush: Instant,
}

// Reading atomic flags:
let is_closing = state.closing.load(atomic::Ordering::Relaxed);
let is_paused = state.paused.load(atomic::Ordering::Relaxed);
let read_eof = state.read_eof.load(atomic::Ordering::Relaxed);

// Writing to lock-free queue:
state.write_buf.push(data);

// Reading from lock-free queue:
while let Some(data) = state.write_buf.pop() {
    // Process data
}
```

**Dependencies:**
- Add `crossbeam` to Cargo.toml: `crossbeam = "0.8"`

**Validation:**
- Benchmark: `python benchmarks/benchmarks.py proto`
- Target: 40-60K RPS (2-3x improvement)
- Check for race conditions with stress tests

#### Fix 1.3: Remove Busy Loop Sleep
**Priority:** HIGH
**Expected Improvement:** 2-3x
**Complexity:** Low

**Approach:**
- Remove default sleep case from `tokio::select!`
- Let tokio manage async waiting properly
- Use `tokio::select!` without default branch

**Implementation:**
```rust
// BEFORE:
tokio::select! {
    result = reader.read(&mut read_buf), if !read_eof => { /* ... */ }
    _ = async { /* writing */ }, if { /* condition */ } => {}
    _ = tokio::time::sleep(Duration::from_millis(1)) => {}  // ← REMOVE THIS
}

// AFTER:
tokio::select! {
    result = reader.read(&mut read_buf), if !read_eof => { /* ... */ }
    _ = async { /* writing */ }, if { /* condition */ } => {}
    // No default case - tokio will wait efficiently
}
```

**Validation:**
- Benchmark: `python benchmarks/benchmarks.py proto`
- Target: 80-100K RPS (2-3x improvement)
- Monitor CPU usage (should decrease)

---

### Phase 2: Medium-Priority Fixes (Expected 2-5x improvement)

#### Fix 2.1: Optimize Buffer Management
**Priority:** MEDIUM
**Expected Improvement:** 1.5-2x
**Complexity:** Medium

**Approach:**
- Use single buffer with read/write indices
- Pre-allocate buffers to avoid allocations
- Reuse buffers instead of creating new ones

**Implementation:**
```rust
pub(crate) struct TokioTCPTransportState {
    stream: Option<TcpStream>,
    // Single buffer with read/write indices
    read_buffer: Vec<u8>,
    read_pos: usize,
    write_pos: usize,
    write_buf: crossbeam::queue::SegQueue<Vec<u8>>,
    closing: AtomicBool,
    weof: AtomicBool,
    read_eof: AtomicBool,
    write_shutdown_done: AtomicBool,
    paused: AtomicBool,
    io_task: Option<tokio::task::JoinHandle<()>>,
    local_addr: Option<SocketAddr>,
    peer_addr: Option<SocketAddr>,
    batch_accumulator: Vec<u8>,
    batch_last_flush: Instant,
}

impl TokioTCPTransportState {
    fn new() -> Self {
        Self {
            read_buffer: vec![0u8; 65536],  // 64KB pre-allocated
            read_pos: 0,
            write_pos: 0,
            // ... other fields
        }
    }
}
```

**Validation:**
- Benchmark: `python benchmarks/benchmarks.py proto`
- Target: 120-150K RPS (1.5-2x improvement)
- Monitor memory allocations (should decrease)

#### Fix 2.2: Reduce Python::attach Calls
**Priority:** MEDIUM
**Expected Improvement:** 1.5-2x
**Complexity:** Medium

**Approach:**
- Cache Python references
- Minimize GIL acquisitions
- Batch multiple operations into single attach

**Implementation:**
```rust
// Cache protocol reference at task start
let protocol_cached = protocol.clone_ref(py);

// Use cached reference instead of re-attaching
Python::attach(|py| {
    let _ = protocol_cached.call_method1(
        py,
        pyo3::intern!(py, "data_received"),
        (PyBytes::new(py, &data),)
    );
});
```

**Validation:**
- Benchmark: `python benchmarks/benchmarks.py proto`
- Target: 150-180K RPS (1.5-2x improvement)
- Profile GIL acquisition count

---

### Phase 3: Low-Priority Fixes (Expected 1.5-2x improvement)

#### Fix 3.1: Simplify tokio::select!
**Priority:** LOW
**Expected Improvement:** 1.2-1.5x
**Complexity:** High

**Approach:**
- Separate read and write into different tasks
- Use tokio channels for coordination
- Reduce branch evaluation overhead

**Implementation:**
```rust
// Separate read task
let read_task = runtime.spawn(async move {
    loop {
        let n = reader.read(&mut read_buf).await?;
        // Process read
    }
});

// Separate write task
let write_task = runtime.spawn(async move {
    loop {
        let data = write_rx.recv().await?;
        writer.write_all(&data).await?;
    }
});
```

**Validation:**
- Benchmark: `python benchmarks/benchmarks.py proto`
- Target: 180-200K RPS (1.2-1.5x improvement)
- Profile tokio::select! overhead

#### Fix 3.2: Profile and Optimize Hot Paths
**Priority:** LOW
**Expected Improvement:** 1.1-1.3x
**Complexity:** Medium

**Approach:**
- Use flamegraph to identify remaining bottlenecks
- Optimize critical sections
- Consider SIMD for buffer operations

**Tools:**
- `cargo flamegraph`
- `perf record`
- `tokio-console`

**Validation:**
- Benchmark: `python benchmarks/benchmarks.py proto`
- Target: 200-220K RPS (1.1-1.3x improvement)
- Compare flamegraphs before/after

---

## Implementation Timeline

### Week 1: Phase 1 - High-Priority Fixes
- Day 1-2: Fix 1.1 - Batch reads
- Day 3-4: Fix 1.2 - Lock-free structures
- Day 5: Fix 1.3 - Remove busy loop
- Day 6-7: Testing and validation

### Week 2: Phase 2 - Medium-Priority Fixes
- Day 1-2: Fix 2.1 - Optimize buffer management
- Day 3-4: Fix 2.2 - Reduce Python::attach calls
- Day 5-7: Testing and validation

### Week 3: Phase 3 - Low-Priority Fixes
- Day 1-3: Fix 3.1 - Simplify tokio::select!
- Day 4-5: Fix 3.2 - Profile and optimize
- Day 6-7: Final testing and benchmarking

---

## Validation Strategy

### Benchmark Suite
```bash
# Run all benchmarks
python benchmarks/benchmarks.py raw stream proto concurrency

# Run specific benchmark
python benchmarks/benchmarks.py proto

# Generate report
cd benchmarks && ./noir -c results/data.json templates/main.md > results/report.md

# Quick CLI table
jaq -r '.results.proto as $proto | ["Loop", "1KB RPS", "10KB RPS", "100KB RPS"], (["asyncio", "rloop", "tokioloop", "uvloop"][] as $loop | [$loop, ($proto[$loop]["1"]["1024"].rps | tostring), ($proto[$loop]["1"]["10240"].rps | tostring), ($proto[$loop]["1"]["102400"].rps | tostring)]) | @tsv' results/data.json | column -t -s $'\t'
```

### Performance Targets
| Phase | 1KB RPS | 10KB RPS | 100KB RPS | Latency |
|-------|---------|----------|-----------|---------|
| Current | 2,727 | 1,334 | 218 | 0.365ms |
| Phase 1 | 80,000 | 40,000 | 10,000 | 0.1ms |
| Phase 2 | 150,000 | 70,000 | 20,000 | 0.05ms |
| Phase 3 | 200,000 | 90,000 | 35,000 | 0.02ms |
| Target | 80,000 | 70,000 | 35,000 | 0.02ms |

### Test Suite
```bash
# Run all TCP tests
pytest tests/tcp/ -v

# Run specific test
pytest tests/tcp/test_tcp_server.py::test_raw_tcp_server -v -s --timeout=30

# Run with debug logging
RUST_LOG=debug pytest tests/tcp/ -v -s
```

---

## Risk Assessment

### High Risk
- **Lock-free structures:** Race conditions can cause subtle bugs
  - Mitigation: Extensive testing, stress tests, code review
- **Batching:** May increase latency
  - Mitigation: Configurable batch size/timeout, monitor latency

### Medium Risk
- **Buffer management:** Complex logic, potential for bugs
  - Mitigation: Unit tests, integration tests, careful review
- **Python::attach reduction:** May break compatibility
  - Mitigation: Test with various Python versions, GIL ON/OFF

### Low Risk
- **Remove busy loop:** Low complexity, high benefit
- **Simplify tokio::select!:** Well-understood pattern

---

## Success Criteria

### Functional
- All existing tests pass
- No regressions in functionality
- Compatible with asyncio API
- Works with GIL ON and GIL OFF

### Performance
- Achieve 80% of rloop performance with GIL OFF
- Latency < 2x of rloop
- Stable performance across runs (<10% variance)
- CPU usage comparable to rloop

### Code Quality
- No memory leaks
- No race conditions
- Clean, maintainable code
- Well-documented changes

---

## Next Steps

1. **Review and approve this plan**
2. **Set up development environment**
   - Install profiling tools: `cargo install flamegraph`
   - Create benchmark baseline
3. **Start Phase 1 implementation**
   - Begin with Fix 1.1 (Batch reads)
   - Validate after each fix
4. **Iterate based on results**
   - Adjust approach based on profiling
   - Re-prioritize based on actual impact

---

## Appendix: Performance Profiling Commands

### Flamegraph
```bash
# Generate flamegraph
cargo flamegraph --bin _rloop -- pytest tests/tcp/test_tcp_server.py::test_raw_tcp_server -v

# View flamegraph
flamegraph.svg
```

### Perf
```bash
# Record performance data
perf record -g pytest tests/tcp/test_tcp_server.py::test_raw_tcp_server -v

# Report
perf report
```

### Tokio Console
```bash
# Run with tokio-console instrumentation
TOKIO_CONSOLE_ENABLED=1 pytest tests/tcp/test_tcp_server.py::test_raw_tcp_server -v

# View console
tokio-console
```

### Custom Instrumentation
```rust
use std::time::Instant;

let start = Instant::now();
// ... code ...
let duration = start.elapsed();
log::debug!("Operation took {:?}", duration);
