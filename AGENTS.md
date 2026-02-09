# AGENTS.md

## Project Overview

**TokioLoop** is a high-performance AsyncIO event loop implementation in Rust, built on top of the **tokio** crate. It provides a drop-in replacement for Python's standard library event loop with significant performance improvements and **multi-threading support**.

**Note:** The codebase currently contains references to `RLoop`, a mio-based implementation that served as a solid baseline. RLoop is single-threaded and will be removed from this codebase in the future once TokioLoop achieves stability and feature parity.

## Technology Stack

### Core Technologies
- **Rust 2024 Edition** - Systems programming language for core implementation
- **tokio 1.0** - Async runtime for TokioLoop implementation
- **PyO3 0.26** - Python bindings with `gil_used = false` for GIL-less operation
- **papaya 0.2** - Lock-free concurrent data structures
- **rustls 0.23** - Native Rust TLS implementation
- **socket2 0.6** - Low-level socket operations
- **Maturin 1.8+** - Python extension building

### Python Integration
- **Target Versions**: Python 3.11+ (free threading support)
- **Package Name**: `rloop` (Python package)
- **Extension Module**: `_rloop` (Rust compiled module)

### Development Tools
- **pytest 9.0+** with pytest-asyncio and pytest-timeout
- **ruff 0.11+** for linting and formatting
- **uv** for package management

## Code Organization

### Rust Source Structure (`src/`)

```
src/
├── lib.rs                  # Main entry point, PyO3 module definition
├── event_loop.rs           # RLoop event loop (mio-based, deprecated, single-threaded)
├── handles.rs              # Callback and timer handle system (RLoop)
├── tcp.rs                  # TCP transport/server (RLoop)
├── udp.rs                  # UDP transport implementation
├── ssl.rs                  # TLS/SSL integration
├── server.rs               # Server abstraction
├── io.rs                   # I/O source abstractions
├── time.rs                 # Timer and scheduling utilities
├── utils.rs                # Utility functions
├── py.rs                   # Python-specific utilities
├── log.rs                  # Logging and error handling
├── sock.rs                 # Socket handling
├── tokio_event_loop.rs     # TokioLoop event loop implementation (multi-threaded)
├── tokio_handles.rs        # TokioLoop handle system
├── tokio_tcp.rs            # TokioLoop TCP transport/server
└── tokio_udp.rs            # TokioLoop UDP transport
```

### Python Package Structure (`rloop/`)

```
rloop/
├── __init__.py             # Public API: RLoop, TokioLoop, EventLoopPolicy
├── loop.py                 # Loop utilities, RLoop/TokioLoop classes, multi-threading logic
├── _compat.py              # Compatibility utilities
├── _rloop.pyi              # Type stubs for generated module
├── exc.py                  # Exception definitions
├── futures.py              # Future integration
├── server.py               # Server implementations
├── subprocess.py           # Subprocess handling
├── transports.py           # Transport abstractions
└── utils.py                # Utility functions
```

### Test Structure (`tests/`)

```
tests/
├── conftest.py             # Pytest configuration, loop fixtures
├── test_signals.py         # Signal handling tests
├── test_handles.py         # Handle/scheduling tests
├── test_sockets.py         # Socket operation tests
├── test_tokio_import.py    # TokioLoop import tests
├── tcp/
│   ├── test_tcp_conn.py    # TCP connection tests
│   ├── test_tcp_server.py  # TCP server tests
│   └── test_get_extra_info.py  # Transport extra info tests
├── udp/
│   └── test_udp.py         # UDP tests
└── ssl_/
    └── test_ssl_conn.py    # SSL/TLS tests
```

## Key Implementation Patterns

### Event Loop Design

#### TokioLoop Architecture (Multi-threaded)
- **Multi-threaded**: Designed to span coroutines across many threads using tokio's runtime
- **Thread-aware**: Supports Python's free threading model
- **Thread-local loop tracking**: Each thread can have its own TokioLoop instance
- **Runtime-per-thread**: Each TokioLoop initializes its own tokio runtime
- **Async task system**: Uses tokio task system for concurrent operations across threads
- **Signal handling**: Via socket-based delivery mechanism
- **Patched asyncio events**: Overrides `get_running_loop()` and `get_event_loop()` to track threads

#### TokioLoop Threading Model

The TokioLoop uses thread-local storage to track which loop is running on which thread:

```python
# In rloop/loop.py
_tokioloop_loops = weakref.WeakValueDictionary()  # Track all active loops
_tokioloop_threadlocal = threading.local()  # Track running loop per thread

def _register_tokio_thread(loop: TokioLoop | int, setcurrent=True):
    """Register the TokioLoop on the current thread"""
    # Each TokioLoop can span coroutines within many threads
    # asyncio needs a way to know which TokioLoop the coroutine belongs
    # and that TokioLoop is already running on this thread
```

Key threading features:
- `get_running_loop()` is patched to check thread-local loop
- `get_event_loop()` is patched to find appropriate loop for current thread
- Auto-recovery mechanism attempts to find non-closed loops for threads
- Each loop has a unique `id()` used for tracking across threads

#### RLoop Architecture (Legacy, Single-threaded)
- **mio-based**: Uses mio for I/O multiplexing
- **Poll-based**: Event-driven polling with adaptive timeouts
- **Single-threaded**: Runs on a single thread only
- **Token-based indexing**: Uses mio Token system for I/O handle lookup

### Transport Layer

#### TCP Transport (`src/tokio_tcp.rs`)
- **Stream-based**: Uses `tokio::net::TcpStream` directly
- **Async I/O**: Each transport manages its own async tasks via `start_io_tasks()`
- **Buffer management**: Separate read and write buffers with proper flow control
- **Multi-thread safe**: Supports concurrent operations across threads

#### Server Implementation
- **Listener pattern**: Uses `tokio::net::TcpListener` for accepting connections
- **Protocol factory**: Creates protocol instances for each connection
- **Async acceptance**: Handles connection acceptance asynchronously
- **Thread-aware**: Can spawn tasks across threads

### Python Integration

#### PyO3 Patterns
```rust
// Module initialization with GIL-less operation
#[pymodule(gil_used = false)]
fn _rloop(_py: Python, module: &Bound<PyModule>) -> PyResult<()>
```

#### Multi-threading Support
- **Thread-local storage**: Tracks running loop per thread via `threading.local()`
- **Weak reference tracking**: `WeakValueDictionary` tracks all active loops
- **Patched asyncio events**: Custom `get_running_loop()` and `get_event_loop()`
- **Context preservation**: Copy and maintain asyncio contexts across threads
- **Runtime initialization**: Each TokioLoop initializes its own tokio runtime

#### Callback System
- **CBHandle**: Wraps Python callbacks with context and cancellation
- **Handle trait**: Unified interface for different callback types
- **Thread-safe scheduling**: Support for `call_soon_threadsafe`

### Concurrency Patterns

#### Lock-Free Design
- **Papaya HashMap**: Lock-free concurrent hash maps for high performance
- **Atomic counters**: For tracking state without locks

#### Lock Usage
- **Mutex**: Protecting shared state where necessary
- **RwLock**: For configuration values and exception handlers

### Error Handling

#### Rust to Python Bridge
- **LogExc**: Structured error logging with context
- **Result propagation**: Convert Rust Result to PyResult
- **Exception handlers**: Configurable Python exception handlers

## Development Workflow

### Critical Commands

```bash
# After ANY Rust code changes:
RUSTFLAGS=-Awarnings maturin develop

# Run all tests (IMPORTANT: Use an existing 313t virtualenv)
source /home/alanjds/.virtualenvs/sandb313t/bin/activate && pytest

# Run specific test module
source /home/alanjds/.virtualenvs/sandb313t/bin/activate && pytest tests/tcp/

# Run with debug logging
source /home/alanjds/.virtualenvs/sandb313t/bin/activate && RUST_LOG=debug pytest tests/

# Run linting
ruff check
ruff format
```

### Build Configuration

```toml
[profile.release]
codegen-units = 1
debug = false
incremental = false
lto = "fat"
opt-level = 3
panic = "abort"
strip = true
```

## Testing Strategy

### Test Fixtures (`tests/conftest.py`)

Tests run against multiple event loops:
- `asyncio` (standard library)
- `uvloop` (Cython-based alternative)
- `rloop.RLoop` (mio-based, deprecated, single-threaded)
- `rloop.TokioLoop` (tokio-based, multi-threaded, target)

### Test Patterns

- **Parametrized tests**: Same test runs against all event loops
- **Loop cleanup**: Automatic cleanup of event loops after tests
- **Async tests**: Use `pytest-asyncio` with `auto` mode
- **Thread-safety tests**: Verify multi-threading behavior for TokioLoop

### Running Tests

```bash
# Run all tests
make test

# Run specific event loop tests
pytest -k "TokioLoop"

# Run with verbose output
pytest -v -s

# Run with timeout
pytest --timeout=5
```

## Current Implementation Status

### TokioLoop Status (as of 2026-01-28)  (needs recheck)
- **Event Loop**: ✅ Basic infrastructure functional
- **Multi-threading**: ✅ Thread-local tracking, patched asyncio events
- **Task Scheduling**: ✅ Immediate and delayed tasks working
- **Signal Handling**: ✅ Working via socket-based delivery
- **TCP Server**: ❌ Broken - socket conversion issues
- **TCP Transport**: ❌ Incomplete - I/O operations are placeholders
- **UDP**: ⚠️ Partially implemented

### RLoop Status (Legacy, to be removed)
- **Event Loop**: ✅ Fully functional (single-threaded)
- **TCP**: ✅ All tests passing
- **UDP**: ✅ Functional
- **SSL/TLS**: ✅ Working

## Architecture Decisions

### Performance Optimizations
- **Lock-free data structures**: papaya HashMap for high concurrency
- **Multi-threaded execution**: Leverage tokio's multi-threaded runtime
- **Zero-copy patterns**: Minimize data copying in I/O operations
- **Adaptive polling**: Dynamic timeout calculation based on workload

### Memory Management
- **Boxed slices**: Pre-allocated I/O buffers (4KB signal buffer, 256KB read buffer)
- **Capacity planning**: Pre-size collections for expected workloads
- **Explicit cleanup**: Proper resource deallocation patterns

### Platform Constraints
- **Supported**: Linux and macOS (Unix-only)
- **Not supported**: Windows (Unix-specific design)

## Important Notes for Agents

### When Working with TokioLoop Code

1. **Focus on `src/tokio_*.rs` files** - These are the active implementation files
2. **RLoop files are deprecated** - `src/event_loop.rs`, `src/tcp.rs`, etc. will be removed
3. **TokioLoop is multi-threaded** - Unlike RLoop, it supports concurrent operations across threads
4. **Thread-local loop tracking** - Check `rloop/loop.py` for threading logic
5. **Always rebuild after Rust changes** - `RUSTFLAGS=-Awarnings maturin develop`
6. **Test against multiple loops** - Use parametrized loop fixture in `tests/conftest.py`
7. **Check test results** - Look specifically for `rloop.TokioLoop` in test output

### When Adding New Features

1. Follow existing patterns in `src/tokio_tcp.rs` and `src/tokio_event_loop.rs`
2. Ensure thread-safety for multi-threaded operation
3. Ensure Python compatibility via PyO3 bindings
4. Add tests that run against all event loop types
5. Verify thread-local loop tracking works correctly
6. Update type stubs in `rloop/_rloop.pyi` if adding public API
7. Run full test suite before committing

### When Debugging Issues

1. Enable debug logging: `RUST_LOG=debug`
2. Check logs for TokioLoop-specific messages
3. Compare behavior with working RLoop implementation
4. Use verbose pytest output: `pytest -v -s`
5. Check thread-local loop registration for TokioLoop
6. Verify patched asyncio events are working correctly
7. Look for multi-threading issues if tests fail intermittently

### Code Style

- **Rust**: Follow standard Rust conventions
- **Python**: Use `ruff` for linting and formatting
- **No emojis** in comments unless explicitly requested
- **Minimal comments** - let code be self-documenting

## Module Relationships

```
Python Layer (rloop/*.py)
    ↓ PyO3 Bindings (with GIL-less operation)
Rust Layer (src/*.rs)
    ↓ tokio Runtime (multi-threaded)
System Layer (kernel I/O)
```

### Key Inter-Module Dependencies

- `lib.rs` → All other modules (orchestrator)
- `tokio_event_loop.rs` → `tokio_handles.rs`, `tokio_tcp.rs`, `tokio_udp.rs`
- `rloop/loop.py` → Python asyncio ecosystem (with patched events)
- `tests/conftest.py` → All test modules (provides loop fixtures)

## Common Tasks Reference

### Adding a New Transport Method

1. Implement in `src/tokio_tcp.rs` following existing patterns
2. Ensure thread-safety for multi-threaded operation
3. Add Python binding in appropriate `init_pymodule()` function
4. Add tests in `tests/tcp/` directory
5. Rebuild: `RUSTFLAGS=-Awarnings maturin develop`

### Fixing a Test Failure

1. Identify which event loop is failing
2. Check if it's TokioLoop-specific or all loops
3. For TokioLoop, check thread-local loop tracking
4. Enable debug logging: `RUST_LOG=debug pytest -v`
5. Compare with RLoop behavior if available
6. Fix and rebuild

### Adding New Test Cases

1. Add to appropriate test file in `tests/`
2. Use parametrized `loop` fixture from `conftest.py`
3. Ensure test works with all event loops
4. For TokioLoop, verify thread-safety if applicable
5. Mark test with appropriate markers if needed

### Debugging Multi-threading Issues

1. Check `_tokioloop_loops` and `_tokioloop_threadlocal` in `rloop/loop.py`
2. Verify `get_running_loop()` and `get_event_loop()` patches are working
3. Check that `initialize_runtime(loop_id)` is called correctly
4. Look for thread ID mismatches in logs
5. Ensure `Python::attach()` is called correctly in Rust code
6. Ensure no Python object is dropped while detached from Python. It causes panics. 

## Known Limitations

### TokioLoop Current Limitations
- TCP server socket conversion from `std::TcpListener` to `tokio::net::TcpListener` fails
- TCP transport I/O operations are not fully implemented
- Limited error recovery and cleanup
- Multi-threading support is functional but needs more testing

### Platform Limitations
- Windows not supported (Unix-only design)
- Requires Python 3.11+
- Requires Rust toolchain for building

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

## Resources

- **Repository**: https://github.com/alanjds/tokioloop
- **Documentation**: See inline code documentation
- **Implementation Guide**: `plans/tokio_tcp_verification_guide.md`
- **Memory Bank**: `memory-bank/` directory for project context
- **Progress Report**: `memory-bank/progress.md` for implementation status
