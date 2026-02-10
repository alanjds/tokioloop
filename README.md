# TokioLoop

TokioLoop is an [AsyncIO](https://docs.python.org/3/library/asyncio.html) selector event loop implemented in Rust. The project provides two implementations:

- **RLoop**: mio-based implementation (single-threaded, functional)
- **TokioLoop**: tokio-based implementation (multi-threaded, in development)

> **Warning**: TokioLoop is currently a work in progress and definitely not suited for *production usage*.<br/>
> **Note:** TokioLoop is available on Unix systems only (Linux and macOS).

### Two Implementations

The current codebase for now have _two_ event loops based on Rust:

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
git clone https://github.com/alanjds/tokioloop.git
cd tokioloop
maturin develop
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

asyncio.set_event_loop_policy(rloop.TokioLoopPolicy())
loop = asyncio.new_event_loop()
asyncio.set_event_loop(loop)
```

## Differences from stdlib

At current time, when compared with the stdlib's event loop, TokioLoop doesn't support the following features:

- Unix Domain Sockets
- SSL
- debugging

TokioLoop also doesn't implement the following methods:

- `loop.sendfile`
- `loop.connect_accepted_socket`
- `loop.sock_recvfrom`
- `loop.sock_recvfrom_into`
- `loop.sock_sendto`
- `loop.sock_sendfile`

### `call_later` with negative delays

While the stdlib's event loop will use the actual delay of callbacks when `call_later` is used with negative numbers, TokioLoop will treat those as `call_soon`, and thus the effective order will follow the invocation order, not the delay.

## License

TokioLoop is released under the BSD License.

## Resources

- **Repository**: https://github.com/alanjds/tokioloop
- **Memory Bank**: `memory-bank/` directory for project context
- **Progress Report**: `memory-bank/progress.md` for implementation status
