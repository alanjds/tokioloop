"""Multi-connection stress tests for the stream (asyncio.start_server) path.

These exercise tokioloop's in-batch task execution: data_received wakes a parked
TokioStreamReader.readline waiter, whose task __step is run in-batch on the io thread
under the callback-executor lock. The failure modes that a broken in-batch drain would
produce — cross-connection data mixing, reordered/lost/duplicated messages, or corruption
from overlapping callbacks — are all observable as a mismatch between what each client
sends and what it reads back.

Every test runs against all loops via the `loop` fixture, so tokioloop must match the
reference asyncio/uvloop behaviour exactly.
"""

import asyncio

import pytest


async def _echo_lines(reader, writer):
    """Line echo server: read a line, write it straight back."""
    try:
        while True:
            line = await reader.readline()
            if not line:
                break
            writer.write(line)
            await writer.drain()
    finally:
        writer.close()


async def _serve(loop, handler):
    server = await asyncio.start_server(handler, '127.0.0.1', 0)
    host, port = server.sockets[0].getsockname()[:2]
    return server, host, port


async def _ping_pong_client(host, port, cid, n, payload=0):
    """Send n uniquely-tagged lines one at a time, asserting each echoes back exactly."""
    reader, writer = await asyncio.open_connection(host, port)
    try:
        pad = b'x' * payload
        for i in range(n):
            msg = b'%d:%d:' % (cid, i) + pad + b'\n'
            writer.write(msg)
            await writer.drain()
            got = await reader.readline()
            assert got == msg, f'conn {cid} msg {i}: got {got!r} expected {msg!r}'
    finally:
        writer.close()
        try:
            await writer.wait_closed()
        except (ConnectionResetError, BrokenPipeError):
            pass


def test_stream_concurrent_connections_echo(loop):
    """Many connections ping-ponging at once: no cross-talk, no reorder, no loss."""
    n_conns, per_conn = 24, 40

    async def main():
        server, host, port = await _serve(loop, _echo_lines)
        try:
            await asyncio.gather(
                *(_ping_pong_client(host, port, cid, per_conn) for cid in range(n_conns))
            )
        finally:
            server.close()
            await server.wait_closed()

    loop.run_until_complete(main())


def test_stream_concurrent_connections_large(loop):
    """Same, with ~4 KB payloads to exercise multi-chunk lines under concurrency."""
    n_conns, per_conn = 12, 20

    async def main():
        server, host, port = await _serve(loop, _echo_lines)
        try:
            await asyncio.gather(
                *(_ping_pong_client(host, port, cid, per_conn, payload=4096) for cid in range(n_conns))
            )
        finally:
            server.close()
            await server.wait_closed()

    loop.run_until_complete(main())


def test_stream_pipelined_lines_in_order(loop):
    """A single data_received delivering many lines at once must echo them all, in order.

    Stresses the in-batch drain handling several readline wakeups before the GIL is
    released (one write burst -> many buffered lines -> many task steps).
    """
    count = 200

    async def main():
        server, host, port = await _serve(loop, _echo_lines)
        try:
            reader, writer = await asyncio.open_connection(host, port)
            sent = [b'line-%d\n' % i for i in range(count)]
            writer.write(b''.join(sent))  # one burst -> ideally one data_received
            await writer.drain()
            got = [await reader.readline() for _ in range(count)]
            assert got == sent
            writer.close()
            try:
                await writer.wait_closed()
            except (ConnectionResetError, BrokenPipeError):
                pass
        finally:
            server.close()
            await server.wait_closed()

    loop.run_until_complete(main())


def test_stream_interleaved_shared_state(loop):
    """Detects overlapping callbacks via a synchronous (await-free) critical section.

    Each request does ``state['count'] += 1`` with no await in between the read and the
    write-back. Under asyncio's contract callbacks never run concurrently, so that
    read-modify-write is atomic and the final count must equal the number of requests. If
    two callbacks truly overlapped on different threads (the bug the callback-executor lock
    prevents), the += could tear and lose updates, making the final count too low.
    """
    n_conns, per_conn = 16, 25
    state = {'count': 0}

    async def handler(reader, writer):
        try:
            while True:
                line = await reader.readline()
                if not line:
                    break
                state['count'] += 1         # synchronous critical section: no await within
                writer.write(b'%d\n' % state['count'])
                await writer.drain()
        finally:
            writer.close()

    async def client(host, port):
        reader, writer = await asyncio.open_connection(host, port)
        try:
            for _ in range(per_conn):
                writer.write(b'inc\n')
                await writer.drain()
                resp = await reader.readline()
                assert resp.endswith(b'\n')
        finally:
            writer.close()
            try:
                await writer.wait_closed()
            except (ConnectionResetError, BrokenPipeError):
                pass

    async def main():
        server, host, port = await _serve(loop, handler)
        try:
            await asyncio.gather(*(client(host, port) for _ in range(n_conns)))
        finally:
            server.close()
            await server.wait_closed()

    loop.run_until_complete(main())
    # Every request produced exactly one increment, with no lost/torn updates.
    assert state['count'] == n_conns * per_conn
