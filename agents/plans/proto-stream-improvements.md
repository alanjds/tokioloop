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

**File:** `src/tokio_tcp.rs:239-247`  
**Expected improvement:** proto ~69% → ~75-80% of asyncio

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

### Fix: `TokioStreamReader` — deque-of-bytes instead of bytearray

Add `rloop/streams.py` with a `TokioStreamReader(asyncio.StreamReader)` subclass that stores
incoming chunks in a `collections.deque` of raw `bytes` objects, avoiding the `feed_data`
copy entirely. `readline()` scans the deque using `bytes.find(b'\n')`.

```python
# rloop/streams.py
import asyncio
import collections


class TokioStreamReader(asyncio.StreamReader):
    """asyncio.StreamReader that stores chunks as-is instead of extending a bytearray."""

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self._tsr_chunks: collections.deque = collections.deque()
        self._tsr_len: int = 0

    def feed_data(self, data: bytes) -> None:
        if not data:
            return
        # Append without copying into bytearray
        self._tsr_chunks.append(data)
        self._tsr_len += len(data)
        self._wakeup_waiter()
        # Keep parent's length accounting so at_eof() / flow-control work
        self._buffer.extend(b'\x00' * len(data))
        self._maybe_resume_transport()

    def feed_eof(self) -> None:
        super().feed_eof()
        self._wakeup_waiter()

    def _wakeup_waiter(self) -> None:
        waiter = self._waiter
        if waiter is not None:
            self._waiter = None
            if not waiter.cancelled():
                waiter.set_result(None)

    async def readline(self) -> bytes:
        while True:
            # Scan deque for '\n'
            scanned = 0
            for i, chunk in enumerate(self._tsr_chunks):
                pos = chunk.find(b'\n')
                if pos != -1:
                    return self._tsr_consume_line(i, pos)
                scanned += len(chunk)
            # No '\n' yet — check EOF
            if self._eof:
                return self._tsr_drain_all()
            # Wait for more data
            await self._wait_for_data('readline')

    def _tsr_consume_line(self, chunk_idx: int, pos_in_chunk: int) -> bytes:
        """Extract everything up to and including pos_in_chunk in chunk chunk_idx."""
        parts = []
        for _ in range(chunk_idx):
            c = self._tsr_chunks.popleft()
            parts.append(c)
            self._tsr_len -= len(c)
            del self._buffer[:len(c)]
        head = self._tsr_chunks[0]
        line_part = head[:pos_in_chunk + 1]
        remainder = head[pos_in_chunk + 1:]
        parts.append(line_part)
        self._tsr_len -= pos_in_chunk + 1
        del self._buffer[:pos_in_chunk + 1]
        if remainder:
            self._tsr_chunks[0] = remainder
        else:
            self._tsr_chunks.popleft()
        return b''.join(parts)

    def _tsr_drain_all(self) -> bytes:
        result = b''.join(self._tsr_chunks)
        self._tsr_chunks.clear()
        del self._buffer[:self._tsr_len]
        self._tsr_len = 0
        return result
```

Wire into the event loop by overriding `start_server()` in `rloop/loop.py`:

```python
async def start_server(self, client_connected_cb, host=None, port=None, *,
                       limit=2**16, **kwds):
    from .streams import TokioStreamReader
    def factory():
        reader = TokioStreamReader(limit=limit)
        protocol = asyncio.StreamReaderProtocol(reader, client_connected_cb)
        return protocol
    return await self.create_server(factory, host, port, **kwds)
```

**Files:** `rloop/streams.py` (new), `rloop/loop.py` (add `start_server` override)  
**Expected improvement:** stream ~28% → ~40-50% of asyncio

**Ceiling note:** The return value of `readline()` still requires one join/copy to assemble
the output bytes. Stream will always be one copy behind proto. Closing the gap further would
require a Rust-backed line accumulator that delivers complete lines directly as `PyBytes`.

---

## Implementation Order

1. **Proto fix first** — recompile, run tests, benchmark to confirm improvement.
2. **Stream fix** — implement `TokioStreamReader`, wire into `start_server()`, run stream benchmark.

---

## Verification

```bash
# Build (proto fix requires recompile; stream fix is Python-only)
RUSTFLAGS=-Awarnings maturin develop

# Tests
pytest tests/test_sockets.py tests/tcp/test_tcp_server.py -k TokioLoop -q --timeout=30

# Benchmark all three targets
python benchmarks/benchmarks.py raw proto stream
```

Targets after both fixes:
- proto ≥ 75% across all message sizes
- stream ≥ 40% across all message sizes

---

## Commit Messages

```
perf: eliminate intermediate Vec in io_processing_loop data_received path

Pass the stack read buffer slice directly to PyBytes::new instead of
allocating an intermediate Vec. Eliminates one heap alloc and one memcpy
per TCP read on the proto/create_server path.
```

```
perf: TokioStreamReader — deque-based buffering for start_server path

Subclass asyncio.StreamReader to store incoming data chunks in a deque
instead of extending a bytearray in feed_data(). readline() scans the
deque using bytes.find(b'\\n'), avoiding the extra bytearray alloc+copy.
Override start_server() to inject TokioStreamReader.
```
