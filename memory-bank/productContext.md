# Product Context: RLoop + TokioLoop

## Why This Project Exists

### Problem Statement
Python's standard AsyncIO event loop implementation has performance limitations due to being written in Python. For high-concurrency, I/O-bound applications, event loop can become a bottleneck, limiting scalability of async applications.

### Solution Approach
The project now provides two high-performance alternatives:
- **RLoop**: mio-based implementation for proven stability
- **TokioLoop**: tokio-based implementation leveraging modern async/await patterns

Both implementations core event loop logic in Rust, leveraging:
- Zero-cost abstractions and memory safety
- Efficient I/O multiplexing (mio or tokio)
- Direct system calls without Python overhead
- Optimized memory management and concurrency patterns

### Target Users
- Developers building high-performance async applications
- Systems requiring high concurrency with many connections
- Applications where event loop performance is critical
- Teams comfortable with Rust-based Python extensions
- Users wanting choice between proven mio and modern tokio implementations

### User Experience Goals
- **Drop-in Replacement**: Existing AsyncIO code should work without modifications
- **Performance**: Measurable improvements in throughput and latency
- **Reliability**: Memory safety and error handling equivalent to Rust standards
- **Compatibility**: Support for Python 3.12+ ecosystem
- **Choice**: Coexisting implementations allowing performance comparison

### How It Should Work

#### Integration Pattern (RLoop)
```python
import asyncio
import rloop

# Simple policy switch
asyncio.set_event_loop_policy(rloop.EventLoopPolicy())
loop = asyncio.new_event_loop()
asyncio.set_event_loop(loop)

# Existing async code works unchanged
async def handle_client(reader, writer):
    data = await reader.read(100)
    writer.write(data)
    await writer.drain()
```

#### Integration Pattern (TokioLoop)
```python
import asyncio
from rloop import TokioLoop

# Alternative event loop
loop = TokioLoop()
asyncio.set_event_loop(loop)

# Same async code works unchanged
async def handle_client(reader, writer):
    data = await reader.read(100)
    writer.write(data)
    await writer.drain()
```

#### Expected Behavior
- API compatibility with asyncio.SelectorEventLoop for both implementations
- Same semantics for callbacks, coroutines, and futures
- Identical error handling patterns
- Compatible with existing async libraries (aiohttp, etc.)
- Parallel importability: `from rloop import RLoop, TokioLoop`

### Success Metrics
- **Performance**: Same performance characteristics for both implementations
- **Compatibility**: Pass existing asyncio test suites for both
- **Adoption**: Easy migration path for existing code
- **Stability**: No memory leaks or crashes in production scenarios
- **Choice**: Users can select implementation based on needs

### Market Position
The project now provides two alternatives competing with:
- Standard library asyncio (baseline)
- uvloop (existing Cython-based alternative)
- Custom event loop implementations

Differentiation factors:
- **Dual Implementation**: Choice between mio (RLoop) and tokio (TokioLoop)
- **Simplification**: TokioLoop uses higher-level abstractions
- **Compatibility**: Both maintain full asyncio compatibility
- **Performance**: Both target high-performance scenarios
