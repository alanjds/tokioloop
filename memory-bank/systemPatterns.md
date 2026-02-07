# System Patterns: RLoop Architecture

## Core Architecture

### Event Loop Design
RLoop implements a single-threaded event loop based on mio for I/O multiplexing. The core pattern follows:

```
Event Loop Step:
1. Check scheduled timers and ready callbacks
2. Poll I/O events with calculated timeout
3. Process I/O events (TCP, UDP, signals, file descriptors)
4. Execute timer callbacks
5. Execute ready callbacks
```

### Key Components

#### Event Loop (`src/event_loop.rs`)
- **Central orchestrator** managing all async operations
- **State management** using atomic operations for thread-safe counters
- **I/O registration** via mio Poll registry with Token-based indexing
- **Callback scheduling** with priority queues for timers and ready queues

#### Handle System (`src/handles.rs`)
- **Callback abstraction** with support for different arities (0, 1, N args)
- **Cancellation support** through atomic flags
- **Context binding** for asyncio compatibility
- **Timer integration** for delayed execution

#### Transport Layer
- **TCP** (`src/tcp.rs`): Connection-oriented transport with TLS support
- **UDP** (`src/udp.rs`): Connectionless transport with optional remote binding
- **SSL** (`src/ssl.rs`): TLS/SSL integration using rustls

### Data Flow Patterns

#### I/O Registration Flow
```
1. Python calls add_reader/add_writer
2. EventLoop creates/update IOHandle with Interest
3. Register/reregister with mio Poll
4. Store callback handle in papaya HashMap
```

#### Event Processing Flow
```
1. mio poll returns events
2. Token-based lookup in IOHandle HashMap
3. Generate appropriate Handle (TCPRead/Write, UDPRead/Write, etc.)
4. Queue handle for execution
5. Execute handle callback
```

#### Timer Scheduling Flow
```
1. call_later called with delay
2. Calculate timestamp relative to epoch
3. Create Timer with handle and timestamp
4. Insert into BinaryHeap (min-heap)
5. During loop step, pop expired timers
```

### Concurrency Patterns

#### Lock-Free Design
- **Papaya HashMap**: Lock-free concurrent hash maps for high performance
- **Atomic counters**: For tracking ready handles without locks
- **Single-threaded execution**: Event loop runs on single thread

#### Lock Usage
- **Mutex**: Protecting mio Poll instance and callback queues
- **RwLock**: For configuration values and exception handlers
- **Token-based indexing**: Avoids lock contention for I/O handle lookup

### Memory Management

#### Transport Storage
- **TCP transports**: HashMap keyed by file descriptor
- **UDP transports**: HashMap keyed by file descriptor
- **Listener streams**: Nested HashMap for tracking connections per listener

#### Cleanup Patterns
- **Explicit close methods** for transport cleanup
- **HashMap cleanup** on connection termination

### Python Integration Patterns

#### PyO3 Usage
- **Python classes**: EventLoop, TCPTransport, UDPTransport, Server
- **GIL-less operation**: `gil_used = false` on PyModule
- **Context preservation**: Copy and maintain asyncio contexts
- **Exception handling**: Bridge Rust errors to Python exceptions

#### Callback Bridge
- **CBHandle**: Wraps Python callbacks with context and cancellation
- **Handle trait**: Unified interface for different callback types
- **Thread-safe scheduling**: Support for call_soon_threadsafe

### Error Handling Patterns

#### Rust to Python Error Bridge
- **LogExc**: Structured error logging with context
- **Exception handlers**: Configurable Python exception handlers
- **Result propagation**: Convert Rust Result to PyResult

#### Resource Cleanup
- **Graceful shutdown**: Handle interrupted I/O operations
- **TLS cleanup**: Proper TLS session termination
- **Signal handling**: Unix signal integration via socket pairs

### Performance Optimizations

#### Poll Optimization
- **Adaptive timeout**: Calculate optimal poll timeout based on scheduled work
- **Idle detection**: Skip polls when work is ready to minimize latency
- **Batch processing**: Process multiple events per iteration

#### Memory Efficiency
- **Boxed slices**: Pre-allocated buffers for I/O operations
- **Capacity planning**: Pre-size collections for expected load
- **Zero-copy patterns**: Minimize data copying where possible

### Integration Points

#### asyncio Compatibility
- **EventLoopPolicy**: Drop-in replacement for asyncio event loop
- **Protocol factories**: Compatible with asyncio protocol patterns
- **Transport interface**: Matches asyncio transport semantics
- **Future support**: Integration with asyncio future ecosystem

#### SSL/TLS Integration
- **rustls backend**: Native Rust TLS implementation
- **Context bridging**: Convert Python SSL contexts to rustls configs
- **Session management**: Handle TLS handshakes and shutdowns

### Critical Implementation Paths

1. **Main loop execution**: `EventLoop::step()` method
2. **I/O event dispatch**: Token-based handle generation
3. **Timer execution**: BinaryHeap-based scheduling
4. **Transport lifecycle**: Creation, operation, cleanup
5. **Python callback execution**: GIL acquisition and callback invocation
