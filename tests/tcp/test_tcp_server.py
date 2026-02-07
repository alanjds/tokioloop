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
            # Wait a bit for server to be ready
            await asyncio.sleep(0.1)

            # Create a client and connect to a random port
            addr_info = server.sockets[0].getsockname()
            if addr_info:
                host, port = addr_info
                reader, writer = await asyncio.open_connection(host, port)

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
                custom_default = {'custom': 'value'}
                result = proto.transport.get_extra_info('nonexistent_key', custom_default)
                assert result == custom_default

                writer.close()
                await writer.wait_closed()

    loop.run_until_complete(main())


def test_raw_tcp_server(loop):
    """Test raw socket operations using sock_accept/sock_recv/sock_sendall.

    This test mimics the 'raw' benchmark pattern which uses low-level
    socket operations instead of high-level asyncio streams/protocols.
    """
    msg = b'a' * _SIZE  # 1024_000
    state = {'data': b'', 'server_done': False, 'server_ready': False}

    async def echo_server(loop, address):
        """Echo server using raw socket operations."""
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.bind(address)
        sock.listen(5)
        sock.setblocking(False)

        # Signal that server is ready to accept connections
        state['server_ready'] = True

        try:
            await asyncio.sleep(0.1)
            # Accept connection using loop.sock_accept()
            client, addr = await loop.sock_accept(sock)
            logger.info('Connection from %s', addr)

            try:
                client.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            except (OSError, NameError):
                pass

            # Echo data using loop.sock_recv() and loop.sock_sendall()
            with client:
                while True:
                    data = await loop.sock_recv(client, 102400)
                    if not data:
                        break
                    await loop.sock_sendall(client, data)
        finally:
            sock.close()
            state['server_done'] = True

    def client(addr):
        """Client that sends data and receives echo."""
        # Wait for server to be ready
        while not state['server_ready']:
            pass

        import time
        time.sleep(0.5)

        client_sock = socket.socket()
        with client_sock:
            client_sock.connect(addr)
            client_sock.sendall(msg)
            # Shutdown write side to signal EOF to server
            client_sock.shutdown(socket.SHUT_WR)
            while len(state['data']) < _SIZE:
                state['data'] += client_sock.recv(1024 * 16)

    async def main():
        # Create server socket to get a free port
        sock = socket.socket()
        sock.bind(('127.0.0.1', 0))
        addr = sock.getsockname()
        sock.close()

        logger.info('Starting raw echo server on %r', addr)

        # Start the echo server
        server_task = asyncio.create_task(echo_server(loop, addr))

        # Run client in executor (separate thread)
        fut = loop.run_in_executor(None, client, addr)
        await fut

        # Wait for server to finish
        await server_task

        logger.info('Server done: %s', state['server_done'])

    loop.run_until_complete(main())
    assert state['data'] == msg
    assert state['server_done']


# TODO: test buffered proto
