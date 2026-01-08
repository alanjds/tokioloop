import asyncio
import atexit
import gc
from asyncio.events import AbstractEventLoop

import pytest
import uvloop

import rloop


EVENT_LOOPS = [
    asyncio.new_event_loop,
    uvloop.new_event_loop,
    rloop.new_event_loop,
    rloop.new_tokio_event_loop,
]


def _namegetter(x):
    klass = type(x)
    name = klass.__qualname__
    module = klass.__module__.partition('.')[0]
    return f'{module}.{name}'


@pytest.fixture(scope='function', params=EVENT_LOOPS, ids=lambda x: _namegetter(x()))
def loop(request):
    return request.param()

def pytest_configure(config):
    """Runs on pytest before collection starts."""
    import asyncio

    # Track created loops
    created_loops = []

    # Store the original new_event_loop function
    original_new_event_loop = asyncio.new_event_loop

    # Override asyncio.new_event_loop to track created loops
    def tracking_new_event_loop() -> AbstractEventLoop:
        loop = original_new_event_loop()
        created_loops.append(loop)
        return loop

    # Monkey patch asyncio.new_event_loop
    asyncio.new_event_loop = tracking_new_event_loop

    # Register a finalizer to clean up loops after pytest finishes
    def cleanup_event_loops():
        for loop in created_loops:
            if not loop.is_closed():
                try:
                    loop.close()
                except Exception:
                    pass  # Ignore errors during cleanup
        created_loops.clear()
        # Ensure all loops are cleaned up
        gc.collect()

    # Will run after pytest finishes
    atexit.register(cleanup_event_loops)
