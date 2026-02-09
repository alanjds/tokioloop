# TokioLoop

TokioLoop is an [AsyncIO](https://docs.python.org/3/library/asyncio.html) selector event loop implemented in Rust. The project provides two implementations:

- **RLoop**: mio-based implementation (single-threaded, fully functional)
- **TokioLoop**: tokio-based implementation (multi-threaded, in development)

> **Warning**: RLoop is currently a work in progress and definitely not suited for *production usage*.

> **Note:** RLoop is available on Unix systems only (Linux and macOS).

## Overview

TokioLoop aims to provide a high-performance alternative to Python's standard library event loop implementation. By implementing the core event loop logic in Rust, it leverages:

- Zero-cost abstractions and memory safety
- Efficient I/O multiplexing (mio or tokio)
- Direct system calls without Python overhead
- Optimized memory management and concurrency patterns

### Two Implementations

#### RLoop (mio-based)
- **Architecture**: Single-threaded event loop based on mio
- **Future**: Will be removed once TokioLoop achieves feature parity

#### TokioLoop (tokio-based)
- **Status**: In development
- **Architecture**: Multi-threaded event loop based on tokio
- **Features**: Basic infrastructure functional, TCP implementation in progress
- **Future**: Primary implementation once stable

## Installation

```bash
pip install rloop
```

## Usage

### Using RLoop (mio-based)

```python
import asyncio
import rloop

asyncio.set_event_loop_policy(rloop.EventLoopPolicy())
loop = asyncio.new_event_loop()
asyncio.set_event_loop(loop)
```

### Using TokioLoop (tokio-based)

```python
import asyncio
from rloop import TokioLoop

loop = TokioLoop()
asyncio.set_event_loop(loop)
```

## Technology Stack


## Current Status

### RLoop (mio-based)
- **Event Loop**: Functional
- **TCP**: All tests passing
- **UDP**: Functional
- **SSL/TLS**: Working on TLS 1.2 only.
- **Multi-threading**: Single-threaded only

### TokioLoop (tokio-based)
- **Event Loop**: Basic infrastructure functional
- **Multi-threading**: Thread-local tracking, patched asyncio events
- **Task Scheduling**: Immediate and delayed tasks working
- **Signal Handling**: Working via socket-based delivery
- **TCP**: Working for Asynctio Protocols and Streams
- **UDP**: Partially implemented

## Differences from stdlib

At current time, when compared with the stdlib's event loop, RLoop doesn't support the following features:

- Unix Domain Sockets
- debugging

RLoop also doesn't implement the following methods:

- `loop.sendfile`
- `loop.connect_accepted_socket`
- `loop.sock_recvfrom`
- `loop.sock_recvfrom_into`
- `loop.sock_sendto`
- `loop.sock_sendfile`

### `call_later` with negative delays

While the stdlib's event loop will use the actual delay of callbacks when `call_later` is used with negative numbers, RLoop will treat those as `call_soon`, and thus the effective order will follow the invocation order, not the delay.

## License

RLoop is released under the BSD License.

## Resources

- **Repository**: https://github.com/alanjds/tokioloop
- **Memory Bank**: `memory-bank/` directory for project context
- **Progress Report**: `memory-bank/progress.md` for implementation status
