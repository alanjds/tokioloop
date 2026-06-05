# Proto & Stream Improvements — Next Steps After Option A

## Branch

`claude/native-sock-option-perf-EyPqK` — based on `feat/native-sock-option-a`

## Baseline (current branch, after Option A + EOF fix)

| Target | Loop | 1KB | 10KB | 100KB |
|--------|------|----:|-----:|------:|
| raw | asyncio | 10,985 | 10,761 | 6,645 |
| raw | tokioloop | 10,913 | 8,714 | 6,350 |
| raw | **ratio** | **99.4%** | **81.0%** | **95.5%** |
| proto | asyncio | 12,667 | 11,932 | 8,417 |
| proto | tokioloop | 8,694 | 8,217 | 4,537 |
| proto | **ratio** | **68.6%** | **68.9%** | **53.9%** |
| stream | asyncio | 10,821 | 9,950 | 5,810 |
| stream | tokioloop | 2,961 | 2,959 | 2,222 |
| stream | **ratio** | **27.4%** | **29.7%** | **38.2%** |

The raw path is close to parity. Proto and stream have clear room to improve.

---

## Proto Bottleneck — Unnecessary Intermediate `Vec`

`src/tokio_tcp.rs`, inside `io_processing_loop()` at the `Ok(n)` read arm:

```rust
// Current — two heap operations:
let data = read_buf[..n].to_vec();   // ALLOC + COPY 1: stack slice → heap Vec
Python::attach(|py| {
    let _ = protocol.call_method1(py, "data_received",
        (PyBytes::new(py, &data),)   // ALLOC + COPY 2: Vec → Python bytes
    );
});
```

`read_buf` is `[0u8; 65536]` on the stack. The intermediate `Vec` was never necessary —
`PyBytes::new` accepts a `&[u8]` slice directly. The fix eliminates one heap allocation and
one data copy per read:

```rust
// Fixed — one heap operation:
Python::attach(|py| {
    let _ = protocol.call_method1(py, "data_received",
        (PyBytes::new(py, &read_buf[..n]),)   // ALLOC + COPY 1: stack → Python bytes
    );
});
```

**File:** `src/tokio_tcp.rs` (`io_processing_loop`, `Ok(n)` read arm)  
**Expected improvement:** proto ~69% → ~75-80% of asyncio  
**Status:** ✅ implemented — `to_vec()` removed; `&read_buf[..n]` passed directly to `PyBytes::new`.

**Validated A/B (Python 3.13.7, same host, concurrency=1, tokioloop/asyncio ratio):**

| size | before (`to_vec`) | after (slice) | Δ |
|------|------:|------:|------:|
| 1 KB | 85.1% | 87.6% | +2.5 pp |
| 10 KB | 88.4% | 87.5% | ~flat (noise) |
| 100 KB | 71.2% | 87.5% | **+16.3 pp** |

The eliminated 64 KB alloc+copy dominates at large payloads, exactly where the gain lands.
(Absolute ratios are host-dependent and differ from the original benchmark hardware; the
before/after delta is the meaningful signal.)

---

## Stream Bottleneck — asyncio.StreamReader Double Buffer

TokioLoop has no override for `asyncio.StreamReader`. When `asyncio.start_server()` is used:

```
io_processing_loop reads chunk [Rust]
  → protocol.data_received(bytes_obj) [Python GIL]
    → StreamReaderProtocol.data_received()
      → reader.feed_data(data)
          self._buffer.extend(data)     ← COPY 1: bytes → bytearray
      → readline() waiter wakes
        bytes(self._buffer[:isep+1])   ← COPY 2: bytearray → output bytes
```

The two extra copies (one into the bytearray buffer, one out to the return value) explain
the ~28% floor vs proto's ~69%.

> **Status:** NOT implemented. The original draft of this section had two blocking
> defects (wrong wiring + a correctness bug). They are documented below so a future
> effort starts from an accurate design. This is a larger, correctness-sensitive task,
> not a quick win.

### Wiring: there is no `loop.start_server` — patch `asyncio.streams.StreamReader`

The original draft proposed overriding `start_server()` on the loop. **That does not work.**
Verified on **Python 3.13.12** (and 3.11), in both GIL and no-GIL builds (free-threading is a
CPython build flag, not a separate stdlib — `asyncio` is identical):

- `asyncio.AbstractEventLoop.start_server` and `asyncio.base_events.BaseEventLoop.start_server`
  **do not exist**. There is no loop-level `start_server` to override.
- `asyncio.start_server` is a free function in `asyncio.streams`. It constructs the reader
  itself and calls `loop.create_server`:

  ```python
  async def start_server(client_connected_cb, host=None, port=None, *, limit=..., **kwds):
      loop = events.get_running_loop()
      def factory():
          reader = StreamReader(limit=limit, loop=loop)            # ← reader built here
          protocol = StreamReaderProtocol(reader, client_connected_cb, loop=loop)
          return protocol
      return await loop.create_server(factory, host, port, **kwds)
  ```

The benchmark (`benchmarks/server.py`) and `tests/tcp/test_tcp_server.py` both call
`asyncio.start_server`, so a loop method would never be consulted. The only way to inject a
custom reader is to **monkeypatch `asyncio.streams.StreamReader`** so the `factory()` above
constructs ours. This mirrors tokioloop's existing monkeypatch pattern for
`asyncio.events.get_running_loop` / `get_event_loop` in `rloop/loop.py` (search
`_patch_asyncio_events_get_running_loop`). The patch must be installed when the TokioLoop
policy is activated and restored on teardown.

### Correctness: a reader replacement must keep ALL consumption methods consistent

The original draft kept a deque of chunks but padded the inherited `self._buffer` bytearray
with `b'\x00' * len(data)` placeholder bytes "for accounting", and only overrode `readline()`.
**This corrupts every other reader.** `asyncio.StreamReader.read()`, `readexactly()`, and
`readuntil()` all return slices of `self._buffer` — they would hand back null bytes. The stream
benchmark only exercises `readline`, so it would *pass the benchmark while silently returning
garbage* for any general `start_server` consumer.

A correct deque-based reader must therefore either:

1. **Override every consumption path** against the deque — `read`, `readexactly`, `readuntil`,
   `readline`, plus `at_eof` / flow-control hooks — and not store real bytes in `self._buffer`
   at all (don't pad it with placeholders); or
2. **Not subclass** `StreamReader`; instead provide an independent reader that reimplements the
   `StreamReader` consumption surface over the deque.

Either way the design owns the full read API, not just `readline`. Reusing asyncio's private
internals (`_buffer`, `_waiter`, `_wait_for_data`, `_maybe_resume_transport`, `_eof`) is also
version-fragile across 3.13/3.14 and GIL vs no-GIL; pin behaviour with tests on each target.

### Expected improvement and ceiling

**Files:** `rloop/streams.py` (new), `rloop/loop.py` (install/remove the `StreamReader`
monkeypatch alongside the existing event patches)  
**Expected improvement:** stream ~28% → ~40-50% of asyncio

**Ceiling note:** `readline()`'s return value still requires one join/copy to assemble the
output bytes. Stream will always be one copy behind proto. Closing the gap further would
require a Rust-backed line accumulator that delivers complete lines directly as `PyBytes`.

---

## Implementation Order

1. **Proto fix** — recompile, run tests, benchmark to confirm improvement. ✅ done.
2. **Stream fix (future)** — install the `asyncio.streams.StreamReader` monkeypatch, implement a
   reader that owns the full consumption API, add tests for `read`/`readexactly`/`readuntil`/
   `readline` on 3.13+ (GIL and no-GIL), then benchmark.

---

## Verification

```bash
# Build (proto fix requires recompile)
RUSTFLAGS=-Awarnings maturin develop

# Tests
pytest tests/test_sockets.py tests/tcp/test_tcp_server.py -k TokioLoop -q --timeout=30

# Benchmark all three targets
python benchmarks/benchmarks.py raw proto stream
```

Targets:
- proto ≥ 75% across all message sizes (this change)
- stream ≥ 40% across all message sizes (future stream work)

---

## Commit Messages

```
perf: eliminate intermediate Vec in io_processing_loop data_received path

Pass the stack read buffer slice directly to PyBytes::new instead of
allocating an intermediate Vec. Eliminates one heap alloc and one memcpy
per TCP read on the proto/create_server path.
```

The stream optimization is intentionally left unimplemented — see the "Stream
Bottleneck" section for the corrected design (monkeypatch `asyncio.streams.StreamReader`,
own the full consumption API). A commit message will be drafted when that work is done.
