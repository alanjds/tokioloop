import asyncio
import logging


logger = logging.getLogger(__name__)


class BaseProto(asyncio.Protocol):
    _done = None

    def __init__(self, create_future=None):
        logger.info('Instantiating %s', self.__class__.__name__)
        self.state = 'INITIAL'
        self.transport = None
        self.data = b''
        if create_future:
            self._done = create_future()

    def __repr__(self) -> str:
        kind = self.__class__.__name__
        return f'{kind}(state={self.state} transport={self.transport})'

    def _assert_state(self, *expected):
        if self.state not in expected:
            raise AssertionError(f'state: {self.state!r}, expected: {expected!r}')

    def connection_made(self, transport):
        logger.info('Connection made to %r', self)
        self.transport = transport
        self._assert_state('INITIAL')
        self.state = 'CONNECTED'

    def data_received(self, data):
        logger.info('Date received by %r', self)
        self._assert_state('CONNECTED')

    def eof_received(self):
        logger.info('EOS received by %r', self)
        self._assert_state('CONNECTED')
        self.state = 'EOF'

    def connection_lost(self, exc):
        logger.info('Connection lost by %r', self)
        self._assert_state('CONNECTED', 'EOF')
        self.transport = None
        self.state = 'CLOSED'
        if self._done:
            self._done.set_result(None)
