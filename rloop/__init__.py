from ._compat import _BaseEventLoopPolicy as __BasePolicy
from ._rloop import __version__ as __version__
from .loop import RLoop, TokioLoop


def new_event_loop() -> RLoop:
    return RLoop()


def new_tokio_event_loop() -> TokioLoop:
    return TokioLoop()


class EventLoopPolicy(__BasePolicy):
    def _loop_factory(self) -> RLoop:
        return new_event_loop()


class TokioEventLoopPolicy(__BasePolicy):
    def _loop_factory(self) -> TokioLoop:
        return new_tokio_event_loop()
