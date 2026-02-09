# RLoop

RLoop is an [AsyncIO](https://docs.python.org/3/library/asyncio.html) selector event loop implemented in Rust. The project provides two implementations:

- **RLoop**: mio-based implementation (single-threaded, fully functional)
- **TokioLoop**: tokio-based implementation (multi-threaded, in development)

> **Warning**: RLoop is currently a work in progress and definitely not suited for *production usage*.

> **Note:** RLoop is available on Unix systems only (Linux and macOS).

## Overview

RLoop aims to provide a high-performance alternative to Python's standard library event loop implementation. By implementing the core event loop logic in Rust, it leverages:

- Zero-cost abstractions and memory safety
- Efficient I/O multiplexing (mio or tokio)
- Direct system calls without Python overhead
- Optimized memory management and concurrency patterns

### Two Implementations

#### RLoop (mio-based)
- **Status**: Fully functional
- **Architecture**: Single-threaded event loop based on mio
- **Features**: Complete TCP, UDP, SSL/TLS support
- **Performance**: Production-ready for basic operations
- **Future**: Will be removed once TokioLoop achieves feature parity

#### TokioLoop (tokio-based)
- **Status**: In development
- **Architecture**: Multi-threaded event loop based on tokio
- **Features**: Basic infrastructure functional, TCP implementation in progress
- **Performance**: Targeting significant improvements over RLoop
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

# Your async code here
async def main():
    # ... async code ...

asyncio.run(main())
```

### Using TokioLoop (tokio-based)

```python
import asyncio
from rloop import TokioLoop

loop = TokioLoop()
asyncio.set_event_loop(loop)

# Your async code here
async def main():
    # ... async code ...

asyncio.run(main())
```

## Technology Stack

### Core Technologies
- **Rust 2024 Edition** - Systems programming language for core implementation
- **mio 1.0** - Cross-platform I/O multiplexing (RLoop)
- **tokio 1.0** - Async runtime (TokioLoop)
- **PyO3 0.26** - Python bindings with GIL-less operation
- **papaya 0.2** - Lock-free concurrent data structures
- **rustls 0.23** - Native Rust TLS implementation
- **socket2 0.6** - Low-level socket operations

### Python Integration
- **Target Versions**: Python 3.11+ (free threading support)
- **Package Name**: `rloop` (Python package)
- **Extension Module**: `_rloop` (Rust compiled module)

## Current Status

### RLoop (mio-based)
- **Event Loop**: Fully functional
- **TCP**: All tests passing
- **UDP**: Functional
- **SSL/TLS**: Working
- **Multi-threading**: Single-threaded only

### TokioLoop (tokio-based)
- **Event Loop**: Basic infrastructure functional
- **Multi-threading**: Thread-local tracking, patched asyncio events
- **Task Scheduling**: Immediate and delayed tasks working
- **Signal Handling**: Working via socket-based delivery
- **TCP Server**: Socket conversion fixed (using socket2)
- **TCP Transport**: Incomplete - I/O operations are placeholders
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

## Development

### Building from Source

```bash
# Clone repository
git clone https://github.com/alanjds/tokioloop
cd tokioloop

# Install development dependencies
uv sync --group all

# Build extension in development mode
RUSTFLAGS=-Awarnings maturin develop
```

### Running Tests

```bash
# Run all tests
pytest

# Run specific test module
pytest tests/tcp/

# Run with debug logging
RUST_LOG=debug pytest tests/

# Run linting
ruff check
ruff format
```

### Running Benchmarks

```bash
# Terminal 1: Start the benchmark server
python benchmarks/server.py --loop rloop --addr 127.0.0.1:25000

# Terminal 2: Run the benchmark client
python benchmarks/client.py --addr 127.0.0.1:25000 --duration 10
```

## Architecture

### RLoop Architecture (mio-based)
- **mio-based**: Uses mio for I/O multiplexing
- **Poll-based**: Event-driven polling with adaptive timeouts
- **Single-threaded**: Runs on a single thread only
- **Token-based indexing**: Uses mio Token system for I/O handle lookup

### TokioLoop Architecture (tokio-based)
- **Multi-threaded**: Designed to span coroutines across many threads using tokio's runtime
- **Thread-aware**: Supports Python's free threading model
- **Thread-local loop tracking**: Each thread can have its own TokioLoop instance
- **Runtime-per-thread**: Each TokioLoop initializes its own tokio runtime
- **Async task system**: Uses tokio task system for concurrent operations across threads
- **Signal handling**: Via socket-based delivery mechanism
- **Patched asyncio events**: Overrides `get_running_loop()` and `get_event_loop()` to track threads

## Known Limitations

### Platform Limitations
- Windows not supported (Unix-only design)
- Requires Python 3.11+
- Requires Rust toolchain for building

### TokioLoop Current Limitations
- TCP transport I/O operations are not fully implemented
- Limited error recovery and cleanup
- No comprehensive performance benchmarks yet
- Multi-threading support is functional but needs more testing

## Future Plans

### Short-term
- Fix TCP server socket conversion issues
- Implement full async I/O in transports
- Complete UDP functionality for TokioLoop
- Achieve test parity with RLoop
- Improve multi-threading test coverage

### Long-term
- Remove RLoop (mio-based) implementation
- Add comprehensive performance benchmarks
- Production-ready stability
- Potential Windows support

## License

RLoop is released under the BSD License.

## Resources

- **Repository**: https://github.com/alanjds/tokioloop
- **Documentation**: See inline code documentation
- **Implementation Guide**: `plans/tokio_tcp_verification_guide.md`
- **Memory Bank**: `memory-bank/` directory for project context
- **Progress Report**: `memory-bank/progress.md` for implementation status
