# New Benchmarks to Expose io_uring Advantages

## Context

The existing benchmarks (`raw`, `stream`, `proto`, `concurrency`) do not stress the
io_uring code path because the client is **strictly synchronous** (send → wait for full
echo → repeat) and concurrency is low (1–3 connections). The server socket almost always
has data ready on the first `recv(MSG_DONTWAIT)`, so the EAGAIN path — which is the only
place io_uring activates — rarely fires.

### Current results (same-machine, io_uring branch vs feat/native-sock-v3)

| Benchmark | tokioloop v3 | tokioloop uring | Δ |
|-----------|-------------:|----------------:|--:|
| raw 1KB c=1 | 15,348 | 15,090 | −1.7% |
| raw 10KB c=1 | 10,633 | 9,976 | −6.2% |
| raw 100KB c=1 | 3,638 | 3,760 | +3.4% |
| raw 1KB c=2 | — | 16,338 | — |
| raw 1KB c=3 | — | 17,046 | — |

io_uring shows no meaningful advantage because:
1. EAGAIN fires rarely (loopback, client blocks until echo received)
2. Low concurrency (≤3) means the io_uring ring handles at most 3 in-flight ops — no batching benefit

io_uring's advantage appears when:
- **Many connections are simultaneously waiting** → one `submit_and_wait(1)` wakes on any completion;
  epoll requires one `epoll_ctl` + `epoll_wait` cycle per connection
- **Clients have think-time (slow/bursty)** → EAGAIN fires on every recv, making the uring
  path the hot path instead of the exception

---

## Benchmark 1: `high_conc` — High concurrency (10 / 50 / 100 clients)

**Goal**: Drive the server with enough concurrent connections that the io_uring thread
accumulates many in-flight `IORING_OP_RECV` entries simultaneously. A single
`submit_and_wait(1)` then services whichever connection's data arrives first, vs.
the epoll worker pattern which has separate `AsyncFd` registrations per fd.

### Changes to `benchmarks/benchmarks.py`

Add constant and new target function (near the `concurrency` function):

```python
HIGH_CONCURRENCIES = [10, 50, 100]

def high_conc():
    """Raw sock_recv/sendall with many concurrent connections."""
    results = {}
    for loop in LOOPS:
        with server(loop):
            results[loop] = benchmark(msgs=[1024], concurrencies=HIGH_CONCURRENCIES)
    return results
```

Register in `all_benchmarks`:
```python
'high_conc': high_conc,
```

No changes to client or server — uses the existing raw server mode.

---

## Benchmark 2: `slow` — Forced EAGAIN via per-message client delay

**Goal**: Add artificial think-time (e.g. 2 ms) between messages so that the server recv
almost always hits EAGAIN, routing every operation through the io_uring path.  At moderate
concurrency (10–50) the ring sees many simultaneous in-flight recvs and can batch them.

### Changes to `benchmarks/client.py`

1. Add argument (in `argparse` setup section):
```python
parser.add_argument(
    '--delay-ms', default=0, type=float,
    dest='delay_ms',
    help='sleep between sends in ms to force EAGAIN on server recv',
)
```

2. In `bench()`, after receiving the full echo response:
```python
if args.delay_ms:
    time.sleep(args.delay_ms / 1000.0)
```

(`args` is already passed into `bench()` via the module-level argument or via `functools.partial`
— match the existing pattern in that file for passing arguments to the worker process.)

### Changes to `benchmarks/benchmarks.py`

1. Add constant:
```python
SLOW_DELAY_MS = 2   # 2 ms think-time forces EAGAIN on every server recv
SLOW_CONCURRENCIES = [10, 50]
```

2. Thread `delay_ms` through the call chain:

`benchmark(msgs, concurrencies, delay_ms=0)` → passes `--delay-ms {delay_ms}` to the
client command in `client(duration, concurrency, msgsize, delay_ms=0)`.

3. New target:
```python
def slow():
    """Raw with artificial think-time to force EAGAIN on every recv."""
    results = {}
    for loop in LOOPS:
        with server(loop):
            results[loop] = benchmark(
                msgs=[1024],
                concurrencies=SLOW_CONCURRENCIES,
                delay_ms=SLOW_DELAY_MS,
            )
    return results
```

Register: `'slow': slow`.

---

## Files to Modify

| File | Change |
|------|--------|
| `benchmarks/benchmarks.py` | Add `HIGH_CONCURRENCIES`, `SLOW_DELAY_MS`, `SLOW_CONCURRENCIES`; add `high_conc()` and `slow()` functions; thread `delay_ms` through `benchmark()` and `client()`; register both targets |
| `benchmarks/client.py` | Add `--delay-ms` argument; add sleep after echo in `bench()` |

---

## Verification

```bash
# High concurrency (no server changes needed)
BENCHMARK_EXC_PREFIX=.venv/bin .venv/bin/python benchmarks/benchmarks.py high_conc

# Slow/forced-EAGAIN
BENCHMARK_EXC_PREFIX=.venv/bin .venv/bin/python benchmarks/benchmarks.py slow

# Both together
BENCHMARK_EXC_PREFIX=.venv/bin .venv/bin/python benchmarks/benchmarks.py high_conc slow
```

Expected: `tokioloop` should improve relative to `asyncio` at high concurrency and with slow
clients. At `high_conc` c=100, the io_uring thread should have ~100 queued `IORING_OP_RECV`
entries; `submit_and_wait(1)` completes as data arrives on any fd, keeping latency low without
per-fd epoll overhead. At `slow` with 2 ms delay, every `sock_recv` hits EAGAIN, making io_uring
the hot path for 100% of operations instead of the rare fallback.
