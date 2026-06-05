# Option B: Results & Next Steps

Branch: `claude/native-sock-option-perf-IpfRF`  
Base: `feat/native-sock-v3`  
Date: 2026-06-04

> **Follow-up (2026-06-05, branch `claude/option-b-plan-review-pns7f`):** the
> next-step #1 was implemented and validated, and the PROTO/STREAM gap was
> profiled with `py-spy --native`. The profiling overturned the premise of the
> original next-step #2. See **[Follow-up Work](#follow-up-work-2026-06-05)** and
> **[Revised Next Steps](#revised-next-steps)** at the bottom — they supersede the
> "Proposed Next Steps" list captured below.

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

## Proposed Next Steps (priority order)  *(original — superseded by [Revised Next Steps](#revised-next-steps))*

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

---

## Follow-up Work (2026-06-05)

Branch `claude/option-b-plan-review-pns7f` (stacked on this PR's head).

### Next-step #1 implemented — fuse + greedily drain the write arm ✅

`io_processing_loop`'s write arm previously (a) `return`ed immediately after
`write_notify.notified()`, forcing a full outer-loop / `select!` re-entry before
the just-queued write ran, and (b) popped only **one** buffer per `select!`
iteration. The arm now loops on the notification so the write happens in the same
poll, and drains the whole `write_buf` greedily so N queued buffers cost one
iteration. The wait loop also breaks on `closing` so `close()`/`abort()` (which
wake the arm without enqueuing work) let the outer loop observe `is_closing` and
exit — without that guard the task deadlocks on close.

**Measurement methodology (important — the absolute numbers below are NOT
comparable to the tables above).** The tables above were taken on the original
PR machine. The numbers below were taken in a noisy shared CI container where
single runs varied by ±15 pp, so they use a **pinned** harness: server pinned to
CPUs 0–1, client to 2–3, **median of 5×10 s** runs, concurrency=1. Only the
*before/after delta on the same box* is meaningful.

**PROTO — tokioloop % of asyncio (pinned median):**

| Size   | Baseline (PR head) | Step 1 | Δ |
|--------|-------------------:|-------:|--:|
| 1 KB   | 86.8 % | **~95 %** (94.0 / 93.7 / 97.3 over 3 runs) | **+8 pp** ✅ |
| 10 KB  | 86.9 % | **~92 %** (91.7 / 91.0 / 92.4) | **+5 pp** ✅ |
| 100 KB | 74.4 % | ~65 % | inconclusive (see note) |

**STREAM — tokioloop % of asyncio (pinned median):**

| Size   | Baseline | Step 1 | Δ |
|--------|---------:|-------:|--:|
| 1 KB   | 53.3 % | 55.2 % | +1.9 pp |
| 10 KB  | 52.8 % | 54.5 % | +1.7 pp |
| 100 KB | 47.5 % | 48.9 % | +1.4 pp |

- The 1 KB / 10 KB PROTO gain is robust and repeatable across three runs — this
  is the headline result.
- **100 KB is within run-to-run noise**, not a regression: the same baseline
  binary measured 61 % (unpinned) to 74 % (pinned) at 100 KB, overlapping
  Step 1's ~65 %. There is no code-level mechanism for Step 1 to slow a single
  100 KB echo (one buffer in flight ⇒ the greedy drain is identical to the old
  single-pop path).
- STREAM improves marginally and consistently.
- **Correctness:** full `pytest tests` failure set is byte-for-byte identical
  with and without the change (16 fails / 2 errors — all pre-existing
  environment issues: IPv6 `::1` bind, UDP addressing, unix sockets, signals,
  timer timing). TCP data-path tests pass (the one TCP failure is the known IPv6
  env case).

Committed as `perf(tcp): fuse + greedily drain the write arm in io_processing_loop`.

### Next-step #4 done — profiling overturns the #2 hypothesis 🔎

Profiled a live tokioloop PROTO server under 1 KB load with
`py-spy record --native` (release build rebuilt with line-table debuginfo to
symbolize Rust frames). Findings:

- **The asyncio-style main event-loop thread is essentially idle** — parked in
  `run_forever` on a parking_lot condvar. The per-message work runs on tokio
  worker threads.
- **Virtually all non-idle samples sit on one stack:** the per-message
  `protocol.call_method1(py, "data_received", (PyBytes::new(..),))` in
  `io_processing_loop` — i.e. the GIL-held `data_received → transport.write`
  round-trip and the native allocation underneath it (`PyBytes::new`, the
  read-side `to_vec()`, and `data.extract::<Vec<u8>>()` inside `write`).
- The `select!` write/read **branch evaluation does not show up** as a cost.
- Disabling glibc malloc trimming/mmap (`MALLOC_TRIM_THRESHOLD_=-1`,
  `MALLOC_MMAP_MAX_=0`) produced **no** improvement, so the cost is the GIL
  round-trip + copies themselves, not glibc arena thrashing.

**Conclusion:** the original next-step **#2 (dedicated read/write tasks to reduce
`select!` branch overhead) targets a bottleneck that the profile does not show**,
and after Step 1 the PROTO path is already ~92–95 % of asyncio for 1–10 KB. It
was therefore *not* implemented — doing so would add real teardown/deadlock risk
(Step 1 itself hit a close-path deadlock during development) for no predicted
gain. STREAM, not PROTO, is where the large remaining gap lives.

---

## Revised Next Steps

Re-prioritized from the profiling evidence above. **STREAM is the real
opportunity** (~48–55 % of asyncio); PROTO is close to parity for small/medium
messages.

### A. Bypass `asyncio.StreamReader` for the STREAM path *(highest value)*

This is the original #3 and remains the only path to closing the STREAM gap. The
STREAM overhead is the Python-level stream machinery (`StreamReader.feed_data` →
`call_soon` waiter scheduling → `readline()` continuation), which adds event-loop
hops the PROTO path does not have. A tokio-native stream/reader that feeds the
Python `StreamReader` less often (or replaces it) is the lever. Larger change;
prototype behind the existing transport so PROTO is unaffected.

### B. Cut per-message copies in the PROTO/STREAM data path *(small, evidence-backed)*

The profile puts the residual PROTO cost in per-message allocation. Concrete,
low-risk reductions:
- **Read arm:** `let data = read_buf[..n].to_vec();` then `PyBytes::new(py, &data)`
  allocates and copies twice. Build `PyBytes::new(py, &read_buf[..n])` directly
  and drop the intermediate `to_vec()` — removes one alloc+copy per message.
- **Write path:** `data.extract::<Vec<u8>>(py)` copies every `transport.write`
  payload into a fresh `Vec`. Investigate borrowing via the buffer protocol /
  retaining the `Py<PyBytes>` to avoid the copy (must still own the bytes across
  the `.await`).
- Consider a non-glibc global allocator (mimalloc/jemalloc) **only if** a future
  profile attributes time to `malloc`/`free` once the copies above are removed —
  the env-var test above suggests the current allocator is not the bottleneck, so
  this is speculative.

### C. (Dropped) Dedicated read/write tasks per transport

The original #2. **Not recommended** — profiling shows `select!` branch overhead
is not a measurable cost, PROTO is already near parity post-Step 1, and the
two-task split adds shutdown-ordering / `connection_lost`-once / deadlock risk.
Revisit only if a future profile actually attributes time to the combined
`select!`.

### Notes for whoever picks this up

- Benchmark on a quiet machine, or reuse the pinned + median methodology (server
  on one core set, client on another, median of ≥5 runs); single unpinned runs in
  CI swing ±15 pp and will mislead.
- Re-symbolize with `CARGO_PROFILE_RELEASE_DEBUG=line-tables-only
  CARGO_PROFILE_RELEASE_STRIP=false` before `py-spy --native`; the default release
  profile is stripped (`debug = false`).
