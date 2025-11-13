import asyncio
import logging
import socket
import ssl

import pytest

import rloop


logging.basicConfig(level=logging.DEBUG)
logger = logging.getLogger(__name__)


class SSLProtocol(asyncio.Protocol):
    def __init__(self, create_future=None):
        self.state = 'INITIAL'
        self.transport = None
        self.data = b''
        if create_future:
            self._done = create_future()

    def _assert_state(self, *expected):
        if self.state not in expected:
            raise AssertionError(f'state: {self.state!r}, expected: {expected!r}')

    def connection_made(self, transport):
        logger.debug(f'{self.__class__.__name__}: connection_made')
        self.transport = transport
        self._assert_state('INITIAL')
        self.state = 'CONNECTED'

    def data_received(self, data):
        logger.debug(f'{self.__class__.__name__}: data_received {len(data)} bytes')
        self._assert_state('CONNECTED')
        self.data += data

    def eof_received(self):
        logger.debug(f'{self.__class__.__name__}: eof_received')
        self._assert_state('CONNECTED')
        self.state = 'EOF'
        self.transport.close()

    def connection_lost(self, exc):
        logger.debug(f'{self.__class__.__name__}: connection_lost')
        self._assert_state('CONNECTED', 'EOF')
        self.transport = None
        self.state = 'CLOSED'
        if hasattr(self, '_done'):
            self._done.set_result(None)


class SSLEchoServerProtocol(SSLProtocol):
    def data_received(self, data):
        super().data_received(data)
        if self.transport:
            self.transport.write(b'echo: ' + data)


class SSLHTTPServerProtocol(SSLProtocol):
    def data_received(self, data):
        logger.debug('received data=%s', data)
        super().data_received(data)
        if self.transport and b'GET' in data:
            # Send a proper HTTP 200 response
            response = (
                b'HTTP/1.1 200 OK\r\n'
                b'Content-Type: text/plain\r\n'
                b'Content-Length: 15\r\n'
                b'Connection: close\r\n'
                b'\r\n'
                b'hello SSL world'
            )
            logger.debug('sending response (len=%s)', len(response))
            self.transport.write(response)
            logger.debug('closing transport')
            self.transport.close()
            logger.debug('closed transport')


class SSLEchoClientProtocol(SSLProtocol):
    def connection_made(self, transport):
        super().connection_made(transport)
        transport.write(b'hello SSL world')

    def data_received(self, data):
        super().data_received(data)
        self.transport.close()


@pytest.fixture
def ssl_context():
    """Create a basic SSL context for testing."""
    ctx = ssl.create_default_context(ssl.Purpose.CLIENT_AUTH)
    return ctx


@pytest.fixture
def server_ssl_context():
    """Create an SSL context for the server."""
    ctx = ssl.create_default_context(ssl.Purpose.SERVER_AUTH)
    # Load test certificates
    import os
    cert_dir = os.path.join(os.path.dirname(__file__), 'certs')
    ctx.load_cert_chain(
        os.path.join(cert_dir, 'cert.pem'),
        os.path.join(cert_dir, 'key.pem')
    )
    return ctx
