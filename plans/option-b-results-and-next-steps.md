# Option B: Results & Next Steps

Branch: `claude/native-sock-option-perf-IpfRF`  
Base: `feat/native-sock-v3`  
Date: 2026-06-04

---

## What Was Done

Option B replaces the per-EAGAIN persistent-worker-task approach (from v3) with
poll sets integrated directly into `_run`'s `select!` loop.  Two poll sets were
implemented:

### ReadPollSet (`sock_recv` EAGAIN path)

- Persistent `AsyncFd<BorrowedFd>` per fd — no `dup()` or `epoll_ctl ADD/DEL`
  on each EAGAIN event.
- `BorrowedFd`: non-owning wrapper (`struct BorrowedFd(i32)` + `impl AsRawFd`)
  so the Python socket retains fd ownership.
- Inner retry loop after spurious EPOLLIN + `clear_ready()`: re-polls immediately
  so `Poll::Pending` registers the waker.  Without this fix the task stalled
  permanently after any spurious wakeup.
- `FIONREAD` to size recv buffer exactly, avoiding over-allocation.

### WritePollSet (`sock_sendall` EAGAIN path)

- Replaces `sock_writer_task` / `SockSendMsg` worker tasks.
- `AsyncFd<OwnedFd>` (from `dup()`) to avoid epoll conflict with ReadPollSet's
  `BorrowedFd` on the same original fd.
- Greedy send loop avoids `try_io` (which would prematurely clear_ready on
  partial sends).
- Same inner retry loop as ReadPollSet for EAGAIN correctness.

### Architecture change summary

```
v3 EAGAIN path (recv):
  _sock_recv_native → EAGAIN → worker channel (hop 1)
    worker: readable().await → recv → Vec (copy 1)
    → scheduler channel (hop 2)
      _run: attach_blocking → PyBytes::new (copy 2) → set_result

Option B EAGAIN path (recv):
  _sock_recv_native → EAGAIN → pending_reads map + notify_one
    _run select! arm: drains map into ReadPollSet (no hop, no copy)
    ReadPollSet fires: recv → Vec (1 copy) → batch with handles
      attach_blocking → PyBytes::new + set_result

Option B EAGAIN path (sendall):  identical pattern via WritePollSet
```

---

## Benchmark Results

All benchmarks at concurrency=1, duration=10s.  Base machine: Linux x86-64.

### RAW (`sock_recv` / `sock_sendall` — the primary target)

| Loop      |    1 KB |   10 KB |  100 KB |
|-----------|--------:|--------:|--------:|
| asyncio   | 11,268  | 10,702  |  7,674  |
| rloop     | 12,348  | 11,646  |  6,003  |
| **tokioloop** | **10,658** | **9,476** | **5,394** |
| uvloop    | 13,725  | 11,664  |  7,645  |

**tokioloop % of asyncio:**

| Size   | v3 baseline | Option B (this PR) | Target |
|--------|------------:|-------------------:|-------:|
| 1 KB   | 85.8 %      | **94.6 %**         | ≥ 88 % ✅ |
| 10 KB  | 69.2 %      | **88.5 %**         | ≥ 80 % ✅ |
| 100 KB | 46.4 %      | **70.3 %**         | ≥ 60 % ✅ |

All three size targets are met.

### STREAM (asyncio `start_server` + reader/writer — `TokioTCPTransport`)

| Loop      |   1 KB |  10 KB | 100 KB |
|-----------|-------:|-------:|-------:|
| asyncio   | 10,164 |  9,034 |  5,140 |
| rloop     |  8,877 |  7,672 |  3,983 |
| **tokioloop** | **2,587** | **2,554** | **1,781** |
| uvloop    | 10,340 | 10,044 |  4,720 |

tokioloop: **~28–35 % of asyncio** (unchanged from v3).

### PROTO (Protocol class via `create_server` — `TokioTCPTransport`)

| Loop      |   1 KB |  10 KB | 100 KB |
|-----------|-------:|-------:|-------:|
| asyncio   | 11,907 | 11,566 |  6,763 |
| rloop     | 10,652 | 10,216 |  6,065 |
| **tokioloop** | **8,798** | **8,409** | **4,676** |
| uvloop    | 12,093 | 11,842 |  6,921 |

tokioloop: **~69–70 % of asyncio** (unchanged from v3).

---

## Root-Cause Analysis: STREAM / PROTO Gap

### PROTO (69 %)

`io_processing_loop` calls `Python::attach` (one GIL acquire/release) per read
chunk to invoke `data_received`.  asyncio runs everything in a single Python
thread with no GIL transitions.  For 1 KB echo at ~9 K rps that's ~9 K GIL
transitions/s on top of the baseline work.

**Attempted fix:** route `data_received` through `scheduler_tx` (same path as
`connection_lost`), so `_run`'s `attach_blocking` batches multiple callbacks in
one GIL acquisition.

**Outcome:** PROTO dropped to **29 %** — worse.  Root cause: the scheduler hop
adds a full `select!` iteration between the read and the write.  For synchronous
echo (read → data_received → transport.write → send), the direct
`Python::attach` path completes in 2 `select!` iterations; the scheduler path
requires 3.  The extra hop costs more than the GIL-batch savings at these
message rates.

### STREAM (28–35 %)

Same PROTO overhead, **plus** one additional asyncio-internal event-loop
iteration: `StreamReader.feed_data()` schedules the `readline()` continuation
via `call_soon`, which must pass through `_run`'s scheduler channel before
`transport.write()` is called.  That's 2 round-trips through the scheduler per
echo vs 1 for PROTO.

---

## Proposed Next Steps (priority order)

### 1. Fuse read and write in `io_processing_loop` (PROTO/STREAM improvement)

The write arm currently does two things: detect work and do work.  After
`notified()` wakes it, it `return`s and the outer loop re-enters `select!`
before performing the write.  A future that performs the write without an
extra `select!` iteration would recover at least one hop.

One approach: instead of the `write_notify → notified() → return → next iter`
pattern, keep the write arm's future alive across the notification so it does
the write in the same future poll.

### 2. Dedicated I/O task per transport (PROTO improvement)

The current design runs read and write in the same `select!`.  When a write
is pending, every `select!` iteration evaluates the write branch even when
there is no data to send, adding overhead.  Splitting into separate read and
write tokio tasks would let each poll independently and reduce branch overhead.

### 3. Bypass asyncio StreamReader for stream path (STREAM improvement)

The STREAM gap is largely due to the asyncio Python-level stream machinery
(readline buffering, waiter scheduling, etc.).  A dedicated tokio-native stream
implementation that bypasses `asyncio.StreamReader` would eliminate those extra
Python hops.  This is a larger change but is the only path to matching uvloop's
stream performance.

### 4. Profile 1 KB PROTO gap vs asyncio

At 1 KB with PROTO, the echo is essentially:
  read → `Python::attach(data_received → transport.write)` → write arm sends

This is 1 GIL acquire per round trip, yet tokioloop runs at only 69 % of
asyncio.  The remaining gap likely lies in:
- `block_in_place` overhead (converting a tokio async thread to a blocking one)
- `write_buf` Mutex + `write_notify` Notify overhead vs asyncio's direct write
- The extra `writer.flush().await` call after each write (a second await point)

Profiling with `py-spy top` against a running PROTO server would make the
bottleneck visible.

---

## Files Changed in This PR

| File | Change |
|------|--------|
| `src/tokio_event_loop.rs` | Add `ReadPollSet`, `WritePollSet` structs + Stream impls; replace `sock_reader_task` / `SockRecvMsg` / `sock_writers` / `get_or_spawn_writer` / `sock_writer_task` with poll-set–based `pending_reads` / `pending_writes` maps; wire both sets into `_run`'s select! loop |
| `Cargo.toml` | Add `futures = "0.3"` |
| `results/data.json` | Updated benchmark data |
| `.gitignore` | Add `.claude/` |
