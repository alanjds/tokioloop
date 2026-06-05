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

**TL;DR:** the planned `io_processing_loop` micro-optimizations (next-step #1 and
the copy reductions in #2/B) were prototyped and **rigorously** benchmarked. Under
drift-controlled measurement **none of them measurably moves PROTO** — it is
already ~92–94 % of asyncio at the Option B baseline. Step 1 (greedy-drain) was
therefore **reverted**; only a perf-neutral allocation cleanup is kept. Profiling
confirms the residual PROTO cost is the unavoidable per-message GIL round-trip,
and that **STREAM (~48–58 %) is the only place left with real headroom** — which
needs the larger StreamReader-bypass work, not loop tweaks.

### A cautionary tale about benchmarking this in CI

This shared CI container is *extremely* noisy: identical binaries varied by
±15 pp between back-to-back runs, and there is large **slow drift** over tens of
minutes. A first pass using a "pinned (server CPUs 0–1, client 2–3) + median of
5×10 s" harness produced what looked like a clean **+8 pp** PROTO win for Step 1
— which **completely evaporated** once measured properly. The drift had simply
favoured the Step 1 binary during the window it was measured in.

The only methodology that gave a trustworthy answer was **interleaved hot-swap
A/B/C**: build every variant's `.so` once, then in each rep copy each variant's
`.so` over the installed one and measure all variants back-to-back, so slow drift
hits every variant equally within a rep. Aggregate the per-rep medians.

### What was measured (interleaved, 8 reps × 8 s, pinned, PROTO)

Variants: **baseline** (this PR head), **tovec** (baseline + read-arm `to_vec()`
removal), **step1** (baseline + fuse/greedy-drain write arm), **stepB**
(step1 + tovec).

| Variant  | 1 KB (% asyncio) | 10 KB (% asyncio) | vs baseline |
|----------|-----------------:|------------------:|:-----------:|
| baseline | 94.0 % | 91.8 % | — |
| tovec    | 92.6 % | 91.8 % | −1.5 % / −0.0 % |
| step1    | 92.2 % | 91.2 % | −2.0 % / −0.7 % |
| stepB    | 95.4 % | 90.5 % | +1.4 % / −1.5 % |

All four land within **±2 % of each other with ~4–6 % per-sample stdev — i.e.
statistically indistinguishable.** A separate 6-rep interleaved run put step1 at
−5 % vs baseline; averaged across both runs step1 is **neutral-to-slightly-
negative**, never positive. (STREAM was only measured non-interleaved and is not
trustworthy here; treat the STREAM gap as "large and unchanged".)

### Decisions

- **Next-step #1 (fuse + greedy-drain write arm): reverted.** No measurable gain,
  and it leans slightly negative — the greedy drain adds lock churn to the common
  single-buffer echo (a `lock` per `pop_front`, plus a separate `lock` for the
  shutdown check) which outweighs the saved `select!` re-entry. It also introduced
  a close-path deadlock during development (the wait loop must break on `closing`,
  not only on having work). Not worth the complexity for zero benefit. *(It may
  still help burst/`writelines` workloads the echo benchmark doesn't exercise; if
  that's ever a target, re-introduce it with a benchmark that actually queues
  multiple buffers.)*
- **Read-arm `to_vec()` removal: kept** as a small, honest cleanup. It removes one
  allocation + copy per message (`read_buf → to_vec() → PyBytes` becomes
  `read_buf → PyBytes`). Perf-neutral in these benchmarks but strictly less work
  with no behaviour change. Committed as `perf(tcp): drop redundant per-message
  Vec copy in io_processing_loop read arm`.
- **Write-path `extract::<Vec<u8>>()` copy: not pursued.** Avoiding it means
  holding `Py<PyBytes>` in `write_buf` and re-acquiring the GIL at write time —
  trading a cheap memcpy for a GIL acquisition, which the profile says is the
  *expensive* operation. Net-negative; left as-is.
- **Correctness throughout:** the full `pytest tests` failure set is byte-for-byte
  identical to the untouched baseline (16 fails / 2 errors — all pre-existing env
  issues: IPv6 `::1` bind, UDP addressing, unix sockets, signals, timer timing).
  TCP data-path tests pass (the one TCP failure is the known IPv6 env case).

### Profiling (next-step #4) — where PROTO's time actually goes

Profiled a live tokioloop PROTO server under 1 KB load with
`py-spy record --native` (release rebuilt with line-table debuginfo to symbolize
Rust frames; the default release profile is stripped). Findings:

- **The asyncio-style main event-loop thread is essentially idle** — parked in
  `run_forever` on a parking_lot condvar. Per-message work runs on tokio workers.
- **Virtually all non-idle samples sit on one stack:** the per-message
  `protocol.call_method1(py, "data_received", (PyBytes::new(..),))` in
  `io_processing_loop` — i.e. the GIL-held `data_received → transport.write`
  round-trip and the native allocation under it.
- The `select!` read/write **branch evaluation never appears** as a cost.
- Disabling glibc malloc trim/mmap (`MALLOC_TRIM_THRESHOLD_=-1`,
  `MALLOC_MMAP_MAX_=0`) produced **no** change — the cost is the GIL round-trip
  itself, not allocator arena thrashing.

This explains the benchmark null result: at ~92–94 % of asyncio the residual
PROTO cost is the *one unavoidable GIL acquisition per message* (asyncio runs
single-threaded with none), and no rearrangement of the Rust loop removes it.

---

## Revised Next Steps

PROTO is effectively done (~92–94 % of asyncio, gated by the per-message GIL
hop). **STREAM is the only remaining opportunity** (~48–58 % of asyncio).

### A. Bypass `asyncio.StreamReader` for the STREAM path *(only real lever)*

Originally #3. The STREAM overhead is the Python-level stream machinery
(`StreamReader.feed_data` → `call_soon` waiter scheduling → `readline()`
continuation), which adds event-loop hops the PROTO path lacks. A tokio-native
stream/reader that feeds the Python `StreamReader` less often (or replaces it) is
the lever. Larger change; prototype behind the existing transport so PROTO is
unaffected, and **measure it with the interleaved hot-swap method below.**

### B. (Done / closed) Cut per-message copies

The read-arm `to_vec()` removal landed (perf-neutral cleanup). The write-path copy
is not worth removing (trades memcpy for a GIL acquire). A non-glibc allocator
(mimalloc/jemalloc) is **not** indicated — the malloc-env test showed the
allocator is not the bottleneck. Consider this line of work exhausted.

### C. (Dropped) Dedicated read/write tasks per transport

Originally #2. **Not recommended** — profiling shows `select!` branch overhead is
not a measurable cost and PROTO is already near parity, so the two-task split
would add shutdown-ordering / `connection_lost`-once / deadlock risk for no gain.

### Notes for whoever picks this up

- **Do not trust single or even median-of-N runs in CI.** Use the interleaved
  hot-swap method: build each variant's `.so` once, then per rep `cp` each over
  `rloop/_rloop.*.so` and measure all variants back-to-back; aggregate per-rep
  medians. Pin server and client to disjoint core sets. Anything below ~3–5 % is
  in the noise here.
- Re-symbolize with `CARGO_PROFILE_RELEASE_DEBUG=line-tables-only
  CARGO_PROFILE_RELEASE_STRIP=false` before `py-spy --native`; the default release
  profile is stripped (`debug = false`).
