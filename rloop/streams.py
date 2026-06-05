"""Deque-backed StreamReader for the TokioLoop ``asyncio.start_server`` path.

``asyncio.StreamReader`` copies every incoming chunk into a single ``bytearray``
(``feed_data`` → ``self._buffer.extend(data)``) and then copies again on the way out
(``bytes(self._buffer[:n])``). ``TokioStreamReader`` stores the incoming ``bytes``
chunks as-is in a ``collections.deque`` and only copies when a consumer actually needs
a contiguous result — eliminating the ``feed_data`` copy entirely and allowing a
zero-copy return when a whole chunk is consumed at once (the common one-message-per-recv
case for ``readline``).

It is a drop-in replacement for ``asyncio.StreamReader``: every consumption method
(``read``, ``readexactly``, ``readuntil``, ``readline``) reads from the deque, so none
of them can return stale/placeholder bytes. ``self._buffer`` (the parent bytearray) is
deliberately left empty and unused.

Wiring: ``asyncio.start_server`` builds ``asyncio.streams.StreamReader`` directly inside a
local ``factory()`` and calls ``loop.create_server`` — there is no ``loop.start_server`` to
override. So ``TokioLoop`` monkeypatches ``asyncio.streams.StreamReader`` (see
``rloop/loop.py``), consistent with its existing ``get_running_loop`` / ``get_event_loop``
patches.
"""

from __future__ import annotations

import collections

from asyncio import exceptions as _exc
from asyncio.streams import StreamReader as _AsyncioStreamReader


class TokioStreamReader(_AsyncioStreamReader):
    """``asyncio.StreamReader`` that stores chunks in a deque instead of a bytearray."""

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        # Source of truth for buffered data. ``self._buffer`` (parent bytearray) is
        # kept empty and unused; all accounting goes through ``_chunks`` / ``_len``.
        self._chunks: collections.deque[bytes] = collections.deque()
        self._len: int = 0

    # -- feeding -----------------------------------------------------------------

    def feed_data(self, data: bytes) -> None:
        assert not self._eof, 'feed_data after feed_eof'

        if not data:
            return

        # Zero-copy: keep the chunk object as received instead of extending a bytearray.
        self._chunks.append(data)
        self._len += len(data)
        self._wakeup_waiter()

        if self._transport is not None and not self._paused and self._len > 2 * self._limit:
            try:
                self._transport.pause_reading()
            except NotImplementedError:
                # The transport can't be paused; forget it and buffer everything.
                self._transport = None
            else:
                self._paused = True

    # ``feed_eof`` is inherited unchanged (it only flips ``self._eof`` and wakes waiters).

    def at_eof(self) -> bool:
        return self._eof and not self._len

    def _maybe_resume_transport(self) -> None:
        if self._paused and self._len <= self._limit:
            self._paused = False
            self._transport.resume_reading()

    # -- deque helpers -----------------------------------------------------------

    def _drain_all(self) -> bytes:
        """Consume and return the entire buffer (zero-copy when a single chunk)."""
        chunks = self._chunks
        if not chunks:
            return b''
        out = chunks[0] if len(chunks) == 1 else b''.join(chunks)
        chunks.clear()
        self._len = 0
        return out

    def _consume(self, n: int) -> bytes:
        """Consume and return exactly ``min(n, self._len)`` bytes."""
        if n >= self._len:
            return self._drain_all()
        chunks = self._chunks
        first = chunks[0]
        first_len = len(first)
        if n < first_len:
            chunks[0] = first[n:]
            self._len -= n
            return first[:n]
        if n == first_len:
            chunks.popleft()
            self._len -= n
            return first
        # Spans multiple chunks.
        parts = []
        remaining = n
        while remaining > 0:
            chunk = chunks[0]
            chunk_len = len(chunk)
            if chunk_len <= remaining:
                chunks.popleft()
                parts.append(chunk)
                remaining -= chunk_len
            else:
                parts.append(chunk[:remaining])
                chunks[0] = chunk[remaining:]
                remaining = 0
        self._len -= n
        return b''.join(parts)

    def _coalesce(self) -> bytes:
        """Collapse the deque into a single chunk and return it (for multi-byte scans)."""
        chunks = self._chunks
        if len(chunks) > 1:
            joined = b''.join(chunks)
            chunks.clear()
            chunks.append(joined)
        return chunks[0] if chunks else b''

    def _find_byte(self, byte: bytes, start: int) -> int:
        """Return the absolute index of a single-byte separator at/after ``start``, or -1.

        Single-byte separators can never span a chunk boundary, so this scans chunk by
        chunk without coalescing.
        """
        pos = 0
        for chunk in self._chunks:
            chunk_len = len(chunk)
            if pos + chunk_len <= start:
                pos += chunk_len
                continue
            local = start - pos if start > pos else 0
            idx = chunk.find(byte, local)
            if idx != -1:
                return pos + idx
            pos += chunk_len
        return -1

    # -- consumption API ---------------------------------------------------------

    async def read(self, n=-1):
        if self._exception is not None:
            raise self._exception

        if n == 0:
            return b''

        if n < 0:
            # Read until EOF in limit-sized blocks (avoids unbounded single waiter).
            blocks = []
            while True:
                block = await self.read(self._limit)
                if not block:
                    break
                blocks.append(block)
            return b''.join(blocks)

        if not self._len and not self._eof:
            await self._wait_for_data('read')

        data = self._consume(n)
        self._maybe_resume_transport()
        return data

    async def readexactly(self, n):
        if n < 0:
            raise ValueError('readexactly size can not be less than zero')

        if self._exception is not None:
            raise self._exception

        if n == 0:
            return b''

        while self._len < n:
            if self._eof:
                incomplete = self._drain_all()
                raise _exc.IncompleteReadError(incomplete, n)
            await self._wait_for_data('readexactly')

        data = self._consume(n)
        self._maybe_resume_transport()
        return data

    async def readuntil(self, separator=b'\n'):
        if isinstance(separator, tuple):
            seps = sorted(separator, key=len)
        else:
            seps = [separator]
        if not seps:
            raise ValueError('Separator should contain at least one element')
        min_seplen = len(seps[0])
        max_seplen = len(seps[-1])
        if min_seplen == 0:
            raise ValueError('Separator should be at least one-byte string')

        if self._exception is not None:
            raise self._exception

        offset = 0
        while True:
            match_start = match_end = None
            if max_seplen == 1:
                # Fast path: single-byte separators never span chunks; scan in place.
                isep = self._find_byte(seps[0], offset)
                if isep != -1:
                    match_start, match_end = isep, isep + 1
                buflen = self._len
            else:
                buf = self._coalesce()
                buflen = len(buf)
                if buflen - offset >= min_seplen:
                    for sep in seps:
                        isep = buf.find(sep, offset)
                        if isep != -1:
                            end = isep + len(sep)
                            if match_end is None or end < match_end:
                                match_start, match_end = isep, end

            if match_end is not None:
                break

            if buflen - offset >= min_seplen:
                offset = max(0, buflen + 1 - max_seplen)
                if offset > self._limit:
                    raise _exc.LimitOverrunError(
                        'Separator is not found, and chunk exceed the limit', offset
                    )

            if self._eof:
                chunk = self._drain_all()
                raise _exc.IncompleteReadError(chunk, None)

            await self._wait_for_data('readuntil')

        if match_start > self._limit:
            raise _exc.LimitOverrunError(
                'Separator is found, but chunk is longer than limit', match_start
            )

        chunk = self._consume(match_end)
        self._maybe_resume_transport()
        return chunk

    async def readline(self):
        sep = b'\n'
        seplen = len(sep)
        try:
            line = await self.readuntil(sep)
        except _exc.IncompleteReadError as e:
            return e.partial
        except _exc.LimitOverrunError as e:
            buf = self._coalesce()
            if buf.startswith(sep, e.consumed):
                self._consume(e.consumed + seplen)
            else:
                self._drain_all()
            self._maybe_resume_transport()
            raise ValueError(e.args[0])
        return line
