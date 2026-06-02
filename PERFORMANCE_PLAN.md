# TokioLoop — Performance Improvement Plan

## Context

TokioLoop is a Rust/PyO3 async event loop for Python backed by Tokio. This document specifies
the ordered performance improvements to implement on the `feat/tokioloop-performance` branch.
RLoop is out of scope.

Existing experimental branches are referenced where relevant:
- `feat/tokio-bufwriter` — BufWriter for TCP writes
- `feat/handle-gil` — AsyncFd-based `add_reader`, GIL/free-threading detection
- `feat/tokio-console` — mpsc channel switch, VecDeque loop restructure

---

## Improvement #1 — Fix `add_reader()` / `add_writer()` with `AsyncFd`

**Priority: P0 — Correctness bug (prerequisite for all raw-socket I/O)**

### Problem
`add_reader()` (src/tokio_event_loop.rs ~line 640) schedules the callback immediately without
waiting for fd readability. This causes every raw-socket operation (accept, recv, send) to fail
with `BlockingIOError` or spin unconditionally.

### Fix
1. Add `io_reader_tokens` and `io_writer_tokens` fields: `Arc<papaya::HashMap<usize, tokio_util::sync::CancellationToken>>` 
2. In `add_reader()`:
   - Cancel any existing reader task for the same fd
   - `dup()` the fd to avoid ownership conflicts
   - Spawn a tokio task that loops: `AsyncFd::readable().await` → `guard.clear_ready()` → send fresh `TCBHandle` via `scheduler_tx`
   - Store the `CancellationToken` for later cancellation
3. In `remove_reader()`: look up and cancel the token

### Source
Adapted from `feat/handle-gil` branch — removing the `python_spawn!`/`runtime_mode` dependency
and routing fired callbacks through `scheduler_tx` (not inline execution).

---

## Improvement #2 — Batch callbacks under one GIL acquisition per tick

**Priority: P1 — Highest throughput gain**

### Problem
Each handle runs in its own `Python::attach()` (src/tokio_event_loop.rs ~line 532).
With N callbacks ready per tick, N GIL acquire/release cycles happen sequentially.

### Fix
1. Remove the inner `current_handles_tx/rx` mpsc channel
2. Use `VecDeque<TBoxedHandle>` local to the `_run()` loop
3. Both the `Immediate` scheduler branch AND timer expiry push into this `VecDeque`
4. After each `select!` iteration, drain ALL ready handles under **one** `Python::attach()`:
   ```rust
   Python::attach(|py| {
       while let Some(handle) = current_handles.pop_front() {
           if !handle.cancelled() {
               let _ = handle.run(py, &loop_handlers, &state);
           }
           drop(handle);
       }
   });
   ```

### Source
`feat/tokio-console` does the VecDeque restructure; the single-attach drain is the new step.

---

## Improvement #3 — Switch scheduler channel to `tokio::sync::mpsc`

**Priority: P2 — Enabling improvement, low overhead change**

### Problem
`async_channel::unbounded` (crossbeam-based MPMC) is heavier than tokio-native mpsc for the
single-producer pattern used here.

### Fix
- `scheduler_tx`: `async_channel::Sender` → `tokio::sync::mpsc::UnboundedSender`
- `scheduler_rx`: `async_channel::Receiver` → `Mutex<Option<tokio::sync::mpsc::UnboundedReceiver>>`
- Extract receiver with `.take().unwrap()` at `_run()` start (frozen pyclass workaround)
- `try_send` → `send` (unbounded never blocks)
- Remove `async-channel` from `Cargo.toml` if no other users remain

### Source
`feat/tokio-console` implements this change.

---

## Improvement #4 — Replace timer polling with `tokio::time::sleep_until`

**Priority: P2 — Eliminates CPU waste for timer-heavy workloads**

### Problem
The timer branch polls with a cap of 100μs (src/tokio_event_loop.rs ~line 444). If the next
timer fires in 5 seconds, the loop still wakes every 100μs, burning CPU.

### Fix
Replace the sleep cap logic with an exact deadline:
```rust
let next_us = delayed_tasks.peek().unwrap().when;
let elapsed_us = epoch.elapsed().as_micros();
if next_us > elapsed_us {
    let wait = Duration::from_micros((next_us - elapsed_us) as u64);
    tokio::time::sleep_until(tokio::time::Instant::now() + wait).await;
}
```
When a shorter-deadline timer arrives via `scheduler_rx`, that branch fires first and the loop
re-evaluates — no manual wakeup needed.

---

## Improvement #5 — BufWriter for TCP writes + remove default busy-loop arm

**Priority: P2 — Direct throughput win for write-heavy workloads**

### Problem
Each write does `write_all()` + `flush()` separately (src/tokio_tcp.rs ~line 258-264).
The default `select!` arm sleeps 1ms even when there's nothing to do.

### Fix
1. Wrap writer: `let mut writer = tokio::io::BufWriter::new(writer);`
2. Remove `writer.flush().await` after `write_all()` — BufWriter coalesces up to 8KB
3. Remove the default sleep arm — tokio parks the task naturally when all arms are inactive

### Source
`feat/tokio-bufwriter` implements #1 and #2. The default arm removal is a correction on top.

---

## Improvement #6 — `tokio::sync::Notify` for TCP read-pause backpressure

**Priority: P3 — Eliminates CPU waste when flow control is active**

### Problem
When `pause_reading()` is active, the transport busy-loops with `sleep(10ms)` per connection
(src/tokio_tcp.rs ~line 205).

### Fix
1. Add `resume_notify: Arc<tokio::sync::Notify>` to `TokioTCPTransport`
2. In paused path: `resume_notify.notified().await` (zero-CPU park)
3. In `resume_reading()`: `self.paused.store(false, ...); resume_notify.notify_one()`

---

## Improvement #7 — Larger TCP read buffer (64KB)

**Priority: P4 — Reduces syscall frequency for large transfers**

### Problem
Fixed 8KB `[0u8; 8192]` stack buffer (src/tokio_tcp.rs line 171) requires ~13 `read()` calls
per 100KB message.

### Fix
Change to `[0u8; 65536]` — reduces read syscalls 8x for large messages, one-line change.

---

## Implementation Sequence

```
Step 1:  #1 — fix add_reader/add_writer   → build + test test_sockets.py
Step 2:  #2 + #3 — batch GIL + mpsc       → build + test test_handles.py + tcp/
Step 3:  #4 — sleep_until timers           → build + test test_handles.py
Step 4:  #5 — BufWriter TCP               → build + test tests/tcp/
Step 5:  #6 + #7 — Notify + read buffer   → build + test tests/tcp/
Step 6:  benchmark                         → benchmarks/benchmarks.py
```

---

## Files Modified

| File | Improvements |
|------|-------------|
| `src/tokio_event_loop.rs` | #1, #2, #3, #4 |
| `src/tokio_tcp.rs` | #5, #6, #7 |
| `Cargo.toml` | #3 (remove async-channel if unused) |

---

## Verification

```bash
pytest tests/test_handles.py tests/test_sockets.py tests/tcp/ -v --timeout=15
python benchmarks/benchmarks.py proto   # target: beat asyncio baseline
```
