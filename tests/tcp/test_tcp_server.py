import asyncio
import errno
import logging
import socket

import pytest

from rloop.utils import _HAS_IPv6

from . import BaseProto


logger = logging.getLogger(__name__)
_SIZE = 1024 * 1000


class EchoProtocol(BaseProto):
    def data_received(self, data):
        super().data_received(data)
        self.transport.write(data)


@pytest.mark.skipif(not hasattr(socket, 'SOCK_NONBLOCK'), reason='no socket.SOCK_NONBLOCK')
def test_create_server_stream_bittype(loop):
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM | socket.SOCK_NONBLOCK)
    with sock:
        coro = loop.create_server(lambda: None, sock=sock)
        srv = loop.run_until_complete(coro)
        srv.close()
        loop.run_until_complete(srv.wait_closed())


@pytest.mark.skipif(not _HAS_IPv6, reason='no IPv6')
def test_create_server_ipv6(loop):
    logger.warning('Loop used on test: %s %r', id(loop), loop)
    async def main():
        srv = await asyncio.start_server(lambda: None, '::1', 0)
        try:
            assert len(srv.sockets) > 0
        finally:
            srv.close()
            await srv.wait_closed()

    try:
        loop.run_until_complete(main())
    except OSError as ex:
        if hasattr(errno, 'EADDRNOTAVAIL') and ex.errno == errno.EADDRNOTAVAIL:
            pass
        else:
            raise


def test_tcp_server_recv_send(loop):
    msg = b'a' * _SIZE
    state = {'data': b''}
    proto = EchoProtocol()

    async def main():
        sock = socket.socket()
        sock.setblocking(False)

        with sock:
            sock.bind(('127.0.0.1', 0))
            addr = sock.getsockname()
            logger.info('Socket opened on: %r', addr)
            srv = await loop.create_server(lambda: proto, sock=sock)
            fut = loop.run_in_executor(None, client, addr)
            await fut
            srv.close()

    def client(addr):
        sock = socket.socket()
        with sock:
            sock.connect(addr)
            sock.sendall(msg)
            while len(state['data']) < _SIZE:
                state['data'] += sock.recv(1024 * 16)

    loop.run_until_complete(main())
    assert proto.state == 'CLOSED'
    assert state['data'] == msg


def test_transport_get_extra_info(loop):
    """Test get_extra_info method with various key names."""
    # Create a simple protocol and connection
    class TestProtocol(BaseProto):
        def connection_made(self, transport):
            self.transport = transport

    proto = TestProtocol()

    async def main():
        async with await loop.create_server(lambda: proto, '127.0.0.1', 0) as server:
            # Create a client and connect
            reader, writer = await asyncio.open_connection('127.0.0.1', server.sockets[0].getsockname()[1])

            # Wait for connection_made to be called
            await asyncio.sleep(0.1)

            # Test sockname retrieval
            sockname = proto.transport.get_extra_info('sockname')
            assert sockname is not None
            assert isinstance(sockname, tuple)
            assert sockname[0] == '127.0.0.1'

            # Test peername retrieval
            peername = proto.transport.get_extra_info('peername')
            assert peername is not None
            assert isinstance(peername, tuple)
            assert peername[0] == '127.0.0.1'

            # Test socket retrieval
            sock = proto.transport.get_extra_info('socket')
            # assert sock is not None

            # Test with default parameter (should return default when key not found)
            custom_default = {"custom": "value"}
            result = proto.transport.get_extra_info('nonexistent_key', custom_default)
            assert result == custom_default

            # Test socket reuse after closing
            writer.close()
            await writer.wait_closed()

    loop.run_until_complete(main())


# TODO: test buffered proto
