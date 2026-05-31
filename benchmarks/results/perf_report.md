# TokioLoop Performance Report

Benchmarks run on Python 3.11, 4-core host. Concurrency = 1 client.
Message sizes: 1KB / 10KB / 100KB. Metric: requests/second (higher = better).

---

## Baseline — All TCP/loop optimizations active, add_reader still a stub

**Commit:** `e25aad6` (fix: revert scheduler channel from mpsc to async_channel)

**Active improvements at this point:**
- #2 GIL batching: single `attach_blocking()` per tick for all ready callbacks
- #3 Scheduler channel: `async_channel` (reverted from broken mpsc attempt)
- #4 Timer precision: `tokio::time::sleep` replaces 100μs polling
- #5 BufWriter TCP + write_notify: batched writes, flush-when-empty
- #6 Notify for pause/resume: zero-CPU backpressure
- #7 Read buffer 64KB (was 8KB)

**NOT YET implemented:** #1 — `add_reader()` AsyncFd fix

### RAW benchmark (sock_recv/sock_sendall — uses add_reader internally)

| loop       |     1KB |    10KB |   100KB |
|------------|--------:|--------:|--------:|
| asyncio    |  10,925 |  10,677 |   7,236 |
| rloop      |  12,842 |  11,043 |   6,071 |
| tokioloop  |   4,084 |   4,080 |   3,597 |
| uvloop     |  13,239 |  11,699 |   6,391 |

**tokioloop vs asyncio (raw):** 37% / 38% / 50% — severely degraded because `add_reader()` is a stub that fires immediately without waiting for fd readability (causes tight loop + excessive context switches).

### PROTO benchmark (asyncio protocols/transports — native TokioTCP)

| loop       |     1KB |    10KB |   100KB |
|------------|--------:|--------:|--------:|
| asyncio    |  11,815 |  11,044 |   8,310 |
| rloop      |  13,412 |  12,796 |   8,437 |
| tokioloop  |  12,704 |  11,970 |   6,295 |
| uvloop     |  13,373 |  12,727 |   8,671 |

**tokioloop vs rloop (proto):** 94.7% / 93.5% / 74.6% — very close for small messages; gap at 100KB likely due to `sock_recv`/`sock_sendall` path in client still hitting the stub.

---

## Step 1 — add_reader watcher: Python::attach instead of attach_blocking

**Change:** `attach_blocking` (which calls `tokio::task::block_in_place`) was used in add_reader/add_writer watcher tasks to create a `TCBHandle`. `block_in_place` signals tokio to migrate pending tasks to other workers — useful for long blocking operations, but for a short GIL acquisition (just `clone_ref` + `Py::new`), the work-stealing overhead exceeds the benefit. Changed to `Python::attach` directly.

**Commit:** `(pending)`

### RAW benchmark (sock_recv/sock_sendall)

| loop       |     1KB |    10KB |   100KB |
|------------|--------:|--------:|--------:|
| asyncio    |  11,378 |  10,792 |   6,725 |
| rloop      |  13,207 |  12,099 |   6,714 |
| tokioloop  |   4,402 |   4,374 |   3,928 |
| uvloop     |  13,256 |  12,155 |   7,795 |

**Delta vs baseline:** +7.8% / +7.2% / +9.2% for tokioloop.

**tokioloop vs asyncio (raw):** 38.7% / 40.5% / 58.4% — improvement but fundamental architectural gap remains.

**Root cause of raw gap:** Each `sock_recv` call goes through Python-level `add_reader` (BaseEventLoop compat), which requires spawning a tokio task, GIL for handle creation, channel round-trip, then GIL again to run the Python callback. asyncio uses a direct epoll → callback path with one GIL acquisition. Closing this gap requires native `sock_recv/sock_accept/sock_sendall` implementations in Rust (future work).

### PROTO benchmark (asyncio protocols/transports)

| loop       |     1KB |    10KB |   100KB |
|------------|--------:|--------:|--------:|
| asyncio    |  12,465 |  11,961 |   8,123 |
| rloop      |  13,269 |  12,275 |   8,811 |
| tokioloop  |  12,564 |  10,962 |   5,916 |
| uvloop     |  12,571 |  11,844 |   8,504 |

**tokioloop vs rloop (proto):** 94.7% / 89.3% / 67.1% — excellent for small messages; 100KB gap under investigation.

---

## Step 2 — close()/abort() wake + connection_lost via scheduler

**Changes (commit `89ec07e`):**
1. `close()` and `abort()` now call `write_notify.notify_one()` so `io_processing_loop` exits its `notified().await` park immediately (fixes `test_transport_get_extra_info` hang).
2. `connection_lost` routed through the event loop scheduler (`RustCallHandle`): `io_processing_loop` sends a closure via `scheduler_tx.try_send()` instead of calling `attach_blocking` directly.
3. Stopping check in main select loop: `yield_now()` + scheduler drain + run-with-GIL before `break`. Ensures `connection_lost` is called before `run_until_complete` returns.

**Effect on tests:** `test_tcp_server_recv_send[rloop.TokioLoop]` now passes (`proto.state == 'CLOSED'` instead of `'EOF'`).

**Note:** This is a correctness fix, not a throughput optimization. Numbers should be within noise of step 1.

### RAW benchmark (sock_recv/sock_sendall)

| loop       |     1KB |    10KB |   100KB |
|------------|--------:|--------:|--------:|
| asyncio    |  11,180 |  10,610 |   7,174 |
| rloop      |  12,565 |  12,763 |   6,219 |
| tokioloop  |   5,307 |   5,224 |   4,426 |
| uvloop     |  12,479 |  11,254 |   7,418 |

**Delta vs step 1:** +20.6% / +19.4% / +12.7% for tokioloop.

**tokioloop vs asyncio (raw):** 47.5% / 49.2% / 61.7% — significant improvement vs step 1 (38.7%/40.5%/58.4%).

**Why the improvement?** Removing `attach_blocking` from the connection teardown path reduces GIL contention between `io_processing_loop` and the event loop's main callback batch. Previously, each connection close caused a worker thread to compete for the GIL via `block_in_place`. With `connection_lost` now routed through the scheduler, the GIL is only acquired by the event loop's own `attach_blocking` call.

### PROTO benchmark (asyncio protocols/transports)

| loop       |     1KB |    10KB |   100KB |
|------------|--------:|--------:|--------:|
| asyncio    |   N/A¹  |   N/A¹  |   N/A¹  |
| rloop      |  14,047 |  12,911 |   7,831 |
| tokioloop  |  11,888 |  11,170 |   6,398 |
| uvloop     |  12,395 |  12,596 |   8,666 |

¹ asyncio server failed to bind (port conflict during run — not a regression).

**tokioloop vs rloop (proto):** 84.6% / 86.5% / 81.7% — numbers shifted from step 1 within noise; rloop baseline also shifted. Correctness fix has no meaningful throughput impact.

---

