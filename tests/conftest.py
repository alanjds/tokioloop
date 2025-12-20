import asyncio

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
