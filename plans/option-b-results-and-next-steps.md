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

---

# Detailed Improvement Plan — STREAM (expands revised next-step A)

STREAM is the only path with real headroom (~48–58 % of asyncio here; uvloop
reaches ~85–90 % on the original machine). This section is the concrete,
staged plan for closing it. **uvloop reaching ~90 % with the *same*
`asyncio.StreamReader` is the key fact: the bottleneck is not StreamReader's
Python code, it is how tokioloop delivers reads to it and schedules the
continuation.** Bypassing StreamReader is therefore the *last* resort, not the
first move.

## Root cause: cross-thread delivery + double GIL per echo

Trace one STREAM echo (`data = await reader.readline(); writer.write(data)`):

1. **tokio worker thread** (`io_processing_loop` read arm, `src/tokio_tcp.rs`
   ~line 239): `Python::attach` → `StreamReaderProtocol.data_received` →
   `StreamReader.feed_data` → wakes the `readline()` waiter Future →
   `Future.set_result` → **`loop.call_soon`** (the continuation handle).
2. `call_soon` (`src/tokio_event_loop.rs:850` → `schedule_handle` ~:459):
   `copy_context` (a `PyContext_CopyCurrent` FFI call), `Py::new(TCBHandle)`
   (alloc), then `scheduler_tx.try_send` onto the **unbounded async-channel**.
3. **`_run` thread** (`src/tokio_event_loop.rs:630` select loop): `recv`s the
   handle, `attach_blocking` (**second GIL acquisition**), runs the `readline`
   continuation → coroutine resumes → `writer.write` → `transport.write`
   (enqueue + `write_notify`).
4. tokio worker write arm sends the bytes.

Versus PROTO, which does `data_received → transport.write` in **one** `attach`
on the worker thread with **no** scheduler hop. So STREAM's per-echo tax is:
**one cross-thread channel hop + a second GIL acquisition + a `copy_context` +
a handle allocation.**

Why uvloop avoids it: in asyncio/uvloop, `data_received` runs **on the loop
thread**, so `feed_data → call_soon → continuation` is a same-thread C-level
deque append with a single GIL section. tokioloop runs IO on tokio workers, so
the wakeup must cross threads and re-acquire the GIL. **This is the structural
gap to attack.**

> Before writing code, **profile STREAM to confirm and rank these costs** — all
> PROTO profiling so far does *not* cover the scheduler/`call_soon` path. Run
> `py-spy record --native` against a live tokioloop STREAM server (1 KB), and
> sample **both** the tokio worker threads and the `_run` thread. Attribute time
> to: `copy_context`, handle alloc, `async_channel` send/recv, the two GIL
> acquisitions, and `StreamReader` Python frames. The stage ordering below is the
> expected ranking; let the profile reorder it.

## Staged plan (each stage benchmark-gated; abort if no measurable gain)

### Stage 0 — STREAM profiling + a trustworthy harness *(prerequisite)*

- Add a STREAM mode to the interleaved hot-swap A/B harness (the only
  trustworthy method here — see "Notes" above): build each variant's `.so`
  once, hot-swap `rloop/_rloop.*.so` per rep, measure all variants back-to-back,
  aggregate per-rep medians, server/client pinned to disjoint cores.
- Capture the `py-spy --native` breakdown described above. **Deliverable:** a
  ranked cost table that confirms (or corrects) the root-cause analysis.

### Stage 1 — Make the `call_soon`/scheduling path cheaper *(low risk)*

Targets in `src/tokio_event_loop.rs` `call_soon`/`call_soon_threadsafe`/
`schedule_handle`:

- **Skip `copy_context` on the fast path.** `copy_context` runs
  `PyContext_CopyCurrent` on every `call_soon` (line 851/869). When the caller
  passes no context and the current context is the default, asyncio reuses the
  current context rather than copying. Match that: avoid the copy when it is not
  observably needed (verify against CPython's `Handle` semantics).
- **Cut the per-handle allocation.** Each `call_soon` does `Py::new(TCBHandle)`
  + `into_py_any`. Consider a lighter handle representation or pooling for the
  hot immediate path.
- **Confirm batching actually triggers.** `_run` already greedily drains the
  channel (`try_recv` loop, lines 639/649) and runs a batch under one
  `attach_blocking`. For 1-in-flight echo the batch size is 1, so batching does
  not help concurrency=1 — note this and do not over-invest here.

Exit criterion: a measurable (≥3–5 % interleaved) STREAM gain. If `copy_context`
is not hot in the Stage 0 profile, skip Stage 1 entirely.

### Stage 2 — Remove the cross-thread hop / second GIL *(the real lever, higher risk)*

The structural fix is to run the woken `readline` continuation **in the same GIL
section as the `data_received` that woke it**, instead of bouncing a handle
through `scheduler_tx` to the `_run` thread. Options, cheapest first:

- **2a. Drain-and-run ready callbacks inline on the worker.** After
  `io_processing_loop`'s `data_received` returns (still holding the GIL on the
  worker thread), drain the immediate-handle queue and run the just-scheduled
  continuation(s) inline, rather than sending them to `_run`. Saves the channel
  hop and the second GIL acquisition. **Risk:** asyncio guarantees callbacks run
  on the loop thread in FIFO order; running them on a worker thread can violate
  ordering / reentrancy assumptions and race with `_run` draining the same
  queue. Must be gated to stream transports and carefully serialized (e.g. only
  when `_run` is parked, or via a per-loop "callbacks may run here" handoff).
  Prototype behind a flag; verify with the full `pytest tests` suite, not just
  the echo benchmark.
- **2b. Process stream-transport reads on the loop thread** (uvloop's model):
  deliver `data_received` for stream transports on the `_run` thread so
  `feed_data → call_soon → continuation` is a single-thread, single-GIL section.
  Larger architectural change to how `io_processing_loop` hands data off; keep
  PROTO on its current worker-thread path (it is already near parity).

Exit criterion: STREAM moves toward uvloop's relative standing. If neither 2a nor
2b clears the interleaved-noise floor, stop and record the negative result — do
not ship complexity for noise (the lesson from the Follow-up section above).

### Stage 3 — tokio-native reader bypassing `StreamReader` *(last resort)*

Only if Stage 0 profiling shows `StreamReader`'s own Python frames
(`feed_data`/`_wakeup_waiter`/`readline` buffering) are genuinely hot **after**
Stages 1–2. Provide a Rust object exposing `read`/`readline`/`readexactly` that
the user awaits directly, fed by the tokio read task without asyncio's
`call_soon` waiter machinery. This is a large change with an API surface to keep
compatible; defer unless the profile demands it.

## Guardrails

- **Correctness first:** every stage must keep the full `pytest tests` failure
  set identical to baseline (the known pre-existing env failures only). Stage 2
  especially can break loop-thread/ordering invariants that the echo benchmark
  will not catch — run the whole suite, including the SSL/UDP/streams tests.
- **Measure only with interleaved hot-swap A/B**; treat anything below ~3–5 % as
  noise on this box.
- **Keep PROTO untouched** — it is GIL-gated and effectively done; isolate STREAM
  changes (flag or stream-transport-only path) so a STREAM experiment cannot
  regress PROTO.
- **Define the target up front:** e.g. "close half the STREAM gap to asyncio
  (≈48–58 % → ≥75 %) at 1 KB/10 KB, no PROTO regression, suite green." Stop when
  hit or when a stage shows no measurable movement.

---

# Exploration Results — STREAM Stages 0–2 (2026-06-06, branch `claude/stream-perf-exploration-CWw6E`)

Branched from `96ac303`. Built CPython 3.11.15, release profile, 4-core box.
Measured with a new **interleaved hot-swap A/B harness** (`benchmarks/ab_stream.py`):
server pinned to cores 0–1, client to 2–3, all variants run back-to-back per
(rep, size), reporting the per-variant **median across 5×8 s reps**. Absolute rps
is machine-specific; only the relative `% of asyncio` and the before/after delta
are meaningful.

## Stage 0 — harness + profiling

- `benchmarks/ab_stream.py` added (interleaved hot-swap A/B; `--mode streams|proto`).
- `py-spy --native` (line-tables rebuild) on a live 1 KB STREAM server: `StreamReader`'s
  own Python frames (`feed_data`/`_wakeup_waiter`) are **cold (~3 %)** → Stage 3
  (bypass StreamReader) is correctly last-resort. Visible self-time was allocation
  churn (`mmap64`, `PyObject::new`, `free`) + syscalls; `copy_context`/`async_channel`
  were inlined away under `lto="fat"` and not resolvable as hotspots. Key insight:
  at concurrency=1 the echo is **latency-bound**, so the cross-thread handoff cost
  shows up as *parked threads*, which a CPU sampler barely captures — the 40 pp gap
  is structural handoff latency, confirmed by uvloop (same `StreamReader`, single
  thread) running at ~100–110 %.

## Stage 1 — skip dead `copy_context` in `call_soon` *(applied + measured)*

`call_soon`/`call_soon_threadsafe` built a multi-arg `TCBHandle` whose `run()`
invokes the callback directly and **never enters the stored context** (only the
0/1-arg variants do). The default `copy_context()` was therefore dead work; it is
now skipped when no context is supplied (safe — observably a no-op).

**Result: within noise.** STREAM `% of asyncio`, baseline → stage1:
1 KB 58.6 → 58.8, 10 KB 59.7 → 58.8, 100 KB 60.7 → 60.9 (±1 pp). Confirms
`copy_context` is **not** the differentiator. Kept as a dead-work cleanup, not a
perf lever.

## Stage 2a — inline-drain woken continuations on the IO worker *(the lever)* ✅

Root cause (confirmed): a STREAM echo crosses `scheduler_tx` to the `_run` thread
and acquires the GIL a **second** time to run the `readline` continuation woken by
`feed_data → call_soon`. asyncio/uvloop run read + `data_received` + continuation
in one GIL section on one thread.

Fix: `io_processing_loop` installs a **thread-local inline sink** around its
`data_received` call (`src/tokio_tcp.rs`). While installed, `schedule_handle`
diverts *immediate* handles into the sink instead of the channel
(`src/tokio_event_loop.rs`); the worker then runs them via
`TEventLoop::run_handles_inline` in the **same `Python::attach`**, removing the
cross-thread hop and the second GIL acquire. The sink is taken (disabled) before
running, so nested `call_soon` from the continuations falls back to the channel
(deferred to `_run` next iteration) — preserving FIFO/deferral semantics and
bounding inline work. Gated behind the runtime flag `TOKIOLOOP_INLINE_STREAM=1`
(default off; toggleable without rebuild). Re-entrancy is safe because `TEventLoop`
is `frozen`, and worker-thread continuation execution is already supported by the
existing `get_running_loop` auto-recovery patch in `rloop/loop.py`.

**Result (STREAM `% of asyncio`, median of 5 reps):**

| Size   | baseline | stage2a | Δ | uvloop |
|--------|---------:|--------:|--:|-------:|
| 1 KB   | 56.8 %   | **99.1 %** | **+42.3 pp** | 109.4 % |
| 10 KB  | 57.5 %   | **97.6 %** | **+40.1 pp** | 106.9 % |
| 100 KB | 62.0 %   | **83.8 %** | **+21.8 pp** | 100.3 % |

Far exceeds the ≥75 % target; STREAM reaches near-parity with asyncio at 1–10 KB.

**Guardrails met:**
- **Correctness:** full `pytest tests` with the flag ON has a failure set
  **identical** to the `96ac303` baseline (17 known env failures: IPv6, UDP, unix,
  signals, timer timing). The one transient `test_call_later[uvloop.Loop]` was a
  timing flake (a uvloop test, unaffected by this change; did not reproduce in 3×).
- **PROTO untouched:** PROTO A/B is flat within noise — `data_received` schedules
  no immediate handle there, so the sink stays empty and the inline run is a no-op.

## Recommendation / next steps

- Flip `TOKIOLOOP_INLINE_STREAM` to default-on after an independent validation run
  on a quiet machine + a green full suite; consider promoting it from env flag to a
  stream-transport-gated default.
- Stage 2b (deliver stream reads on the `_run` thread) and Stage 3 (bypass
  `StreamReader`) are **not needed** — 2a already reaches near-parity and the
  profile shows StreamReader is cold.
- Optional follow-up (revised-step B): drop the `read_buf[..n].to_vec()` before
  `PyBytes::new` to remove one alloc+copy per message — independent of 2a.
