# Technical Context: RLoop Technology Stack

## Core Technologies

### Rust Ecosystem
- **Edition**: Rust 2024
- **Core Dependencies**:
  - `mio 1.0`: Cross-platform I/O multiplexing (net, os-ext, os-poll features)
  - `pyo3 0.26`: Python bindings (anyhow, extension-module, generate-import-lib features)
  - `papaya 0.2`: Lock-free concurrent data structures
  - `rustls 0.23`: TLS implementation (std feature)
  - `socket2 0.6`: Low-level socket operations (all features)
  - `anyhow 1.0`: Error handling
  - `log 0.4` + `env_logger 0.11`: Logging

### Build System
- **Cargo**: Rust package manager and build system
- **Maturin 1.8+**: Python extension building
- **PyO3 Build Config**: Python version detection and configuration

### Python Integration
- **Target Versions**: Python 3.12-3.14+ (including free threading support)
- **Package Name**: `rloop` (Python package)
- **Extension Module**: `_rloop` (Rust compiled module)

## Development Environment

### Build Configuration
```toml
[lib]
name = "_rloop"
crate-type = ["cdylib", "rlib"]

[profile.release]
codegen-units = 1
debug = false
incremental = false
lto = "fat"
opt-level = 3
panic = "abort"
strip = true
```

### Development Tools
- **Testing**: pytest 8.3+ with pytest-asyncio and pytest-timeout
- **Linting**: ruff 0.11 with comprehensive rule set
- **Formatting**: ruff format with single quotes
- **Package Management**: uv (package = false for development)

### Python Development Setup
```bash
# Install development dependencies
uv sync --group all

# Run tests
pytest

# Run linting
ruff check
ruff format

# Build package
RUSTFLAGS=-Awarnings maturin develop

### Critical Development Workflow
```bash
# After ANY Rust code changes, must rebuild:
RUSTFLAGS=-Awarnings maturin develop

# Otherwise Python tests won't reflect Rust changes
# This is essential for development workflow
```

## Architecture Details

### Module Structure
```
src/
├── lib.rs                  # Main entry point and PyO3 module definition
├── event_loop.rs           # Core EventLoop implementation
├── handles.rs              # Callback and timer handle system
├── tcp.rs                  # TCP transport and server implementation (RLoop)
├── tokio_tcp.rs            # TCP transport and server implementation (TokioLoop)
├── tokio_event_loop.rs     # TokioLoop event loop implementation
├── tokio_handles.rs        # TokioLoop handle system
├── udp.rs                  # UDP transport implementation
├── ssl.rs                  # TLS/SSL integration
├── server.rs               # Server abstraction
├── io.rs                   # I/O source abstractions
├── time.rs                 # Timer and scheduling utilities
├── utils.rs                # Utility functions
├── py.rs                   # Python-specific utilities
└── log.rs                  # Logging and error handling
```

### Python Package Structure
```
rloop/
├── __init__.py             # Public API and EventLoopPolicy
├── _compat.py              # Compatibility utilities
├── _rloop.pyi              # Type stubs for generated module
├── exc.py                  # Exception definitions
├── futures.py              # Future integration
├── loop.py                 # Loop utilities and helpers
├── server.py               # Server implementations
├── subprocess.py           # Subprocess handling
├── transports.py           # Transport abstractions
└── utils.py                # Utility functions
```

## Key Design Decisions

### Performance Optimizations
- **Lock-free data structures**: papaya HashMap for high concurrency
- **Single-threaded event loop**: Avoids synchronization overhead
- **Zero-copy patterns**: Minimize data copying in I/O operations
- **Adaptive polling**: Dynamic timeout calculation based on workload

### Memory Management
- **Boxed slices**: Pre-allocated I/O buffers (4KB signal buffer, 256KB read buffer)
- **Capacity planning**: Pre-sized collections for expected workloads
- **Explicit cleanup**: Proper resource deallocation patterns

### Error Handling Strategy
- **Rust Result**: Type-safe error handling in core logic
- **anyhow**: Convenient error chaining and context
- **Python bridge**: Convert Rust errors to appropriate Python exceptions
- **Structured logging**: LogExc for detailed error reporting

## Platform Constraints

### Supported Platforms
- **Linux**: Full support with all features
- **macOS**: Full support with all features
- **Windows**: Not supported (Unix-only design)

### Unix-Specific Features
- **Signal handling**: Unix signals via socket pairs
- **File descriptor operations**: Direct Unix file descriptor manipulation
- **POSIX compliance**: Follows Unix I/O semantics

## Dependencies Analysis

### Core Dependencies
- **mio**: Foundation for cross-platform I/O multiplexing
  - Provides event-driven I/O notification
  - Abstracts platform differences (epoll, kqueue, etc.)
  - Handles registration and deregistration of I/O sources

- **pyo3**: Rust-Python bindings
  - Safe Python object manipulation from Rust
  - GIL management and thread safety
  - Exception handling and error conversion

- **papaya**: Lock-free concurrent collections
  - High-performance HashMap without mutex contention
  - Allows concurrent reads from event loop and other threads
  - Essential for call_soon_threadsafe functionality

### Optional Dependencies
- **rustls**: Native Rust TLS implementation
  - Memory-safe TLS without OpenSSL dependency
  - Configurable cipher suites and protocols
  - Integration with Python SSL contexts

## Build and Deployment

### Compilation Requirements
- **Rust 1.70+**: Required for 2024 edition features
- **Python 3.12+**: Minimum supported Python version
- **System libraries**: Standard Unix system libraries

### Build Process
1. **Rust compilation**: Build extension module with Cargo
2. **Python packaging**: Create wheel with Maturin
3. **Type generation**: Generate Python type stubs
4. **Testing**: Run comprehensive test suite

### Distribution
- **PyPI**: Main distribution channel
- **Wheels**: Pre-compiled binaries for supported platforms
- **Source distribution**: For platforms without pre-built wheels

## Development Workflow

### Local Development
```bash
# Clone repository
git clone https://github.com/alanjds/tokioloop
cd tokioloop

# Install development dependencies
uv sync --group all

# Build extension in development mode
maturin develop

# Run tests
pytest tests/

# Run benchmarks
python benchmarks/benchmarks.py
```

### Code Quality
- **Pre-commit hooks**: Automated formatting and linting
- **CI/CD**: GitHub Actions for testing across platforms
- **Documentation**: Comprehensive inline documentation
- **Type hints**: Full type annotation support

## TokioLoop Implementation Status

### Current State (as of 2026-01-12)
- **TokioEventLoop**: Basic infrastructure with task scheduling and signal handling
- **TokioTCPTransport**: Structure exists but I/O operations are placeholders
- **TokioTCPServer**: Structure exists but socket conversion from std to tokio fails
- **Signal Handling**: Working infrastructure via socket-based delivery
- **Task Scheduling**: Immediate and delayed tasks functional

### Critical Issues
- **Socket Conversion**: `std::TcpListener` to `tokio::net::TcpListener` conversion fails
- **TCP Server**: Continuous "Invalid argument (os error 22)" errors
- **TCP Transport**: `start_io_tasks()` is a stub, no real async I/O
- **Event Loop Integration**: Transport I/O not integrated with tokio primitives

### Test Results
- **Import Tests**: ✅ All 6 tests pass
- **Functional Tests**: ❌ TCP tests fail with server accept errors
- **RLoop Comparison**: ✅ RLoop TCP implementation fully functional

### Implementation Approach
- **Stream-based**: Using tokio::net::{TcpStream, TcpListener} directly
- **Transport-managed I/O**: Each transport handles its own async tasks
- **Event Loop Integration**: Through existing task scheduling system
- **Error Handling**: Standard asyncio Python exceptions with Rust debug logging

## Performance Characteristics

### Benchmarks
- **TCP throughput**: Measured against standard asyncio
- **Connection handling**: Concurrent connection scalability
- **Memory usage**: Per-connection memory overhead
- **Latency**: I/O operation response times

### Monitoring
- **Built-in metrics**: Event loop performance counters
- **Logging integration**: Structured logging with env_logger
- **Debug support**: Development-time debugging capabilities

## Current Implementation Challenges

### Technical Debt
- **Socket Conversion**: Mix of std and tokio socket types creates complexity
- **Resource Management**: fd ownership issues between std and tokio
- **Error Handling**: Incomplete error recovery and cleanup
- **Task Spawning**: Current approach may need refinement
- **Socket Exposure**: TokioTCPServer doesn't properly expose Python socket objects

### Architecture Concerns
- **Dual Implementation**: Maintaining both RLoop and TokioLoop increases complexity
- **Memory Management**: Patterns need consistency across implementations
- **Testing Strategy**: Need comprehensive integration tests for both implementations

## Socket Implementation Context (2026-01-28)

### Problem Analysis
The `test_transport_get_extra_info` test fails for TokioLoop because:
1. `server.sockets[0]` raises `IndexError: list index out of range`
2. TokioTCPServer creates Rust socket objects but doesn't expose them as Python objects
3. The test expects `server.sockets` to contain Python socket objects with `getsockname()` method

### RLoop Socket Pattern (Working Reference)
From `src/tcp.rs` analysis:
- Uses `SocketWrapper::from_fd()` to create Python socket objects
- Socket objects stored in transport's `sock` field  
- `get_extra_info()` exposes socket, sockname, peername correctly
- Proper file descriptor management between Rust and Python

### TokioLoop Socket Issues (Current)
From `src/tokio_tcp.rs` analysis:
- Creates `socket2::Socket` objects in `TokioTCPServer::from_fd()`
- Converts to `tokio::net::TcpListener` for async operations
- Stores socket objects in `socks` field but doesn't expose to Python
- Missing conversion from tokio sockets back to Python socket objects

### Memory Safety Requirements
- Python socket objects must own file descriptors separately from Rust sockets
- Prevent use-after-free by ensuring proper lifetime management
- Handle reference counting between Rust and Python sides correctly
- Socket conversion must not duplicate file descriptors unsafely

### Implementation Strategy
1. Create helper function to convert tokio TcpListener to Python socket
2. Store Python socket objects in TokioTCPServer during initialization  
3. Ensure TokioServer._sockets contains actual Python socket objects
4. Maintain memory safety through proper ownership transfer
