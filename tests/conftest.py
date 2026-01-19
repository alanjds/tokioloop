import asyncio
import atexit
import gc
from asyncio.events import AbstractEventLoop

import pytest
import uvloop

import rloop


# Store the original new_event_loop function
_original_new_event_loop = asyncio.new_event_loop

# Track created loops
_created_loops = []


# Override asyncio.new_event_loop to track created loops
def _tracking_new_event_loop() -> AbstractEventLoop:
    loop = _original_new_event_loop()
    _created_loops.append(loop)
    return loop


def pytest_configure(config):
    """Runs on pytest before collection starts."""
    import asyncio

    # Monkey patch asyncio.new_event_loop
    asyncio.new_event_loop = _tracking_new_event_loop

    # Register a finalizer to clean up loops after pytest finishes
    def cleanup_event_loops():
        for loop in _created_loops:
            if not loop.is_closed():
                try:
                    loop.close()
                except Exception:
                    pass  # Ignore errors during cleanup
        _created_loops.clear()
        # Ensure all loops are cleaned up
        gc.collect()

    # Will run after pytest finishes
    atexit.register(cleanup_event_loops)


EVENT_LOOPS = [
    _tracking_new_event_loop,
    uvloop.new_event_loop,
    rloop.new_event_loop,
    rloop.new_tokio_event_loop,
]

EVENT_LOOP_NAMES = {
    _tracking_new_event_loop: f'asyncio.{type(_tracking_new_event_loop()).__qualname__}',
    uvloop.new_event_loop: f'uvloop.{type(uvloop.new_event_loop()).__qualname__}',
    rloop.new_event_loop: 'rloop.RLoop',
    rloop.new_tokio_event_loop: 'rloop.TokioLoop',
}


def _namegetter(x):
    return EVENT_LOOP_NAMES[x]


@pytest.fixture(scope='function', params=EVENT_LOOPS, ids=lambda x: _namegetter(x))
def loop(request):
    return request.param()
