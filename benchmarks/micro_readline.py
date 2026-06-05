"""Isolated readline throughput: TokioStreamReader vs stock asyncio.StreamReader.

Feeds chunks then consumes them in-process (no socket), so it measures the reader cost
alone — the deque reader avoids the feed_data bytearray copy and returns whole-chunk lines
zero-copy, so its advantage grows with message size.

    python benchmarks/micro_readline.py
"""

import asyncio
import time

from rloop.streams import TokioStreamReader


async def _bench(reader_cls, msg, iters):
    reader = reader_cls(limit=1024 * 1024)
    t0 = time.perf_counter()
    for _ in range(iters):
        reader.feed_data(msg)
        line = await reader.readline()
        assert len(line) == len(msg)
    return time.perf_counter() - t0


async def main():
    for size in (1024, 10240, 102400):
        msg = b'x' * (size - 1) + b'\n'
        iters = max(2000, 2_000_000 // size)
        best = {}
        for name, cls in (('asyncio', asyncio.StreamReader), ('tokio', TokioStreamReader)):
            best[name] = min(await _bench(cls, msg, iters) for _ in range(5))
        a, t = best['asyncio'], best['tokio']
        print(
            f'size={size:>6}  iters={iters:>6}  '
            f'asyncio={a * 1e3:7.1f}ms  tokio={t * 1e3:7.1f}ms  speedup={a / t:5.2f}x'
        )


if __name__ == '__main__':
    asyncio.run(main())
