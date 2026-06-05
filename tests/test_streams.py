"""Correctness tests for rloop.streams.TokioStreamReader.

Each test feeds an identical sequence of chunks to a stock asyncio.StreamReader and to
TokioStreamReader and asserts the consumption API behaves identically — including the
non-readline paths (read/readexactly/readuntil) that a readline-only optimization would
silently corrupt.
"""

import asyncio

import pytest

from rloop.streams import TokioStreamReader


def _feed(reader, chunks, eof=True):
    for c in chunks:
        reader.feed_data(c)
    if eof:
        reader.feed_eof()


async def _both(chunks, op, *, eof=True, limit=2**16):
    ref = asyncio.StreamReader(limit=limit)
    tok = TokioStreamReader(limit=limit)
    _feed(ref, chunks, eof=eof)
    _feed(tok, chunks, eof=eof)
    ref_res = await op(ref)
    tok_res = await op(tok)
    return ref_res, tok_res


async def _assert_same(chunks, op, **kw):
    ref, tok = await _both(chunks, op, **kw)
    assert tok == ref, f'{tok!r} != {ref!r}'


# -- read --------------------------------------------------------------------------------


async def test_read_all_single_chunk():
    await _assert_same([b'hello world'], lambda r: r.read())


async def test_read_all_multi_chunk():
    await _assert_same([b'foo', b'bar', b'baz'], lambda r: r.read())


async def test_read_n_within_first_chunk():
    await _assert_same([b'abcdefgh', b'ijkl'], lambda r: r.read(3))


async def test_read_n_spanning_chunks():
    await _assert_same([b'ab', b'cd', b'ef'], lambda r: r.read(5))


async def test_read_n_larger_than_available():
    await _assert_same([b'ab', b'cd'], lambda r: r.read(100))


async def test_read_zero():
    await _assert_same([b'abc'], lambda r: r.read(0))


async def test_read_empty_eof():
    await _assert_same([], lambda r: r.read())


# -- readexactly -------------------------------------------------------------------------


async def test_readexactly_exact():
    await _assert_same([b'abc', b'def'], lambda r: r.readexactly(6))


async def test_readexactly_spanning():
    await _assert_same([b'ab', b'cd', b'ef'], lambda r: r.readexactly(4))


async def test_readexactly_incomplete_raises_partial():
    async def op(r):
        try:
            await r.readexactly(10)
        except asyncio.IncompleteReadError as e:
            return ('incomplete', e.partial, e.expected)

    await _assert_same([b'abc', b'de'], op)


async def test_readexactly_zero():
    await _assert_same([b'abc'], lambda r: r.readexactly(0))


# -- readline ----------------------------------------------------------------------------


async def test_readline_simple():
    await _assert_same([b'line one\n'], lambda r: r.readline())


async def test_readline_newline_spanning_chunks():
    await _assert_same([b'par', b'tial', b' line\n', b'next\n'], lambda r: r.readline())


async def test_readline_no_newline_eof():
    await _assert_same([b'no newline here'], lambda r: r.readline())


async def test_readline_multiple_lines_one_chunk():
    async def op(r):
        return [await r.readline(), await r.readline(), await r.readline()]

    await _assert_same([b'a\nb\nc\n'], op)


async def test_readline_empty_at_eof():
    await _assert_same([b''], lambda r: r.readline())


# -- readuntil ---------------------------------------------------------------------------


async def test_readuntil_multibyte_separator():
    await _assert_same([b'header\r\nbody'], lambda r: r.readuntil(b'\r\n'))


async def test_readuntil_separator_spanning_chunks():
    await _assert_same([b'abc\r', b'\ndef'], lambda r: r.readuntil(b'\r\n'))


async def test_readuntil_tuple_separator():
    await _assert_same([b'abcXdefY'], lambda r: r.readuntil((b'X', b'Y')))


async def test_readuntil_incomplete_raises():
    async def op(r):
        try:
            await r.readuntil(b'\r\n')
        except asyncio.IncompleteReadError as e:
            return ('incomplete', e.partial)

    await _assert_same([b'abc\r'], op)


async def test_readuntil_limit_overrun():
    async def op(r):
        try:
            return await r.readline()
        except (asyncio.LimitOverrunError, ValueError) as e:
            return (type(e).__name__, str(e))

    # readline (via readuntil) raises ValueError when a line exceeds the limit.
    await _assert_same([b'x' * 50], op, limit=8)


# -- streaming (waiter wakeups) ----------------------------------------------------------


@pytest.mark.parametrize('reader_cls', [asyncio.StreamReader, TokioStreamReader])
async def test_readline_waits_for_late_data(reader_cls):
    reader = reader_cls(limit=2**16)
    loop = asyncio.get_running_loop()
    task = loop.create_task(reader.readline())
    await asyncio.sleep(0)  # let readline park on the waiter
    assert not task.done()
    reader.feed_data(b'delayed ')
    await asyncio.sleep(0)
    assert not task.done()  # no newline yet
    reader.feed_data(b'line\n')
    assert await task == b'delayed line\n'


async def test_at_eof_tracks_buffer():
    reader = TokioStreamReader(limit=2**16)
    reader.feed_data(b'abc')
    reader.feed_eof()
    assert not reader.at_eof()  # data still buffered
    assert await reader.read(3) == b'abc'
    assert reader.at_eof()  # drained + eof
