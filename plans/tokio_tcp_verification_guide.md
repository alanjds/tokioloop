# TokioLoop TCP Features Verification Guide

## Overview

This guide explains how to verify that TokioLoop TCP features are working correctly. TokioLoop is the Tokio-based event loop implementation that will eventually replace the mio-based RLoop.

## Quick Verification

### 1. Run TCP Tests

The fastest way to verify TCP functionality is to run the existing test suite:

```bash
# Run all tests (includes TCP tests)
make test

# Or run only TCP tests
pytest -v tests/tcp/
```

**Expected Output:**
- All tests should pass
- Tests run against multiple event loops: asyncio, uvloop, RLoop, and TokioLoop
- Look for `rloop.TokioLoop` in the test output to confirm TokioLoop is being tested

### 2. Run Benchmarks

Benchmarks provide real-world performance validation:

```bash
# Terminal 1: Start the benchmark server
python benchmarks/server.py --loop tokioloop --addr 127.0.0.1:25000

# Terminal 2: Run the benchmark client
python benchmarks/client.py --addr 127.0.0.1:25000 --duration 10
```

**Expected Output:**
- Server should print: `using TokioLoop` and `serving on: ('127.0.0.1', 25000)`
- Client should print statistics showing successful message exchange
- No connection errors or timeouts

---

## Detailed Verification Methods

### Method 1: Automated Testing

#### Test Suite Structure

The TCP tests are located in [`tests/tcp/`](tests/tcp/):

- [`test_tcp_conn.py`](tests/tcp/test_tcp_conn.py) - Tests TCP connection lifecycle
- [`test_tcp_server.py`](tests/tcp/test_tcp_server.py) - Tests TCP server functionality

#### What the Tests Verify

**Connection Tests ([`test_tcp_conn.py`](tests/tcp/test_tcp_conn.py:25)):**
- ✅ Client can connect to server
- ✅ Data can be sent from client to server
- ✅ Server can respond to client
- ✅ Connections close properly
- ✅ Protocol state transitions work correctly

**Server Tests ([`test_tcp_server.py`](tests/tcp/test_tcp_server.py:53)):**
- ✅ Server can accept connections
- ✅ Large data transfers (1MB messages)
- ✅ Echo functionality works
- ✅ Multiple concurrent connections
- ✅ IPv6 support (if available)

#### Running Specific Tests

```bash
# Run only TokioLoop tests
pytest -v tests/tcp/ -k "TokioLoop"

# Run with verbose output to see what's happening
pytest -v tests/tcp/ -s

# Run with logging enabled
RUST_LOG=debug pytest -v tests/tcp/
```

#### Interpreting Test Results

**Success Indicators:**
- All tests pass (green checkmarks)
- No assertion failures
- No unhandled exceptions
- Test completion messages show proper state transitions

**Failure Indicators:**
- Connection timeouts
- Data mismatches
- State not reaching 'CLOSED'
- Protocol methods not being called

---

### Method 2: Manual Testing with Python Scripts

#### Simple Echo Server Test

Create a test script `test_tcp_manual.py`:

```python
import asyncio
import rloop

async def echo_server(reader, writer):
    data = await reader.read(100)
    message = data.decode()
    print(f"Received: {message}")

    writer.write(data)
    await writer.drain()

    print("Closing connection")
    writer.close()
    await writer.wait_closed()

async def main():
    # Use TokioLoop
    loop = rloop.new_tokio_event_loop()
    asyncio.set_event_loop(loop)

    server = await asyncio.start_server(echo_server, '127.0.0.1', 8888)

    addr = server.sockets[0].getsockname()
    print(f'Serving on {addr}')

    async with server:
        await server.serve_forever()

if __name__ == '__main__':
    asyncio.run(main())
```

Run it:
```bash
python test_tcp_manual.py
```

In another terminal, test with netcat:
```bash
echo "Hello TokioLoop" | nc 127.0.0.1 8888
```

**Expected Output:**
- Server prints: `Received: Hello TokioLoop`
- Server prints: `Closing connection`
- Netcat receives: `Hello TokioLoop`

#### Protocol-Based Test

Create `test_protocol.py`:

```python
import asyncio
import rloop

class EchoProtocol(asyncio.Protocol):
    def connection_made(self, transport):
        self.transport = transport
        print("Connection made")

    def data_received(self, data):
        print(f"Data received: {data}")
        self.transport.write(data)

    def connection_lost(self, exc):
        print("Connection lost")

async def main():
    loop = rloop.new_tokio_event_loop()
    asyncio.set_event_loop(loop)

    server = await loop.create_server(EchoProtocol, '127.0.0.1', 8889)

    async with server:
        await asyncio.sleep(30)  # Run for 30 seconds

if __name__ == '__main__':
    asyncio.run(main())
```

---

### Method 3: Benchmark Testing

#### Running Benchmarks

**Start Server:**
```bash
python benchmarks/server.py --loop rloop --addr 127.0.0.1:25000 --print
```

**Run Client:**
```bash
# Basic test
python benchmarks/client.py --addr 127.0.0.1:25000 --duration 10

# High concurrency test
python benchmarks/client.py --addr 127.0.0.1:25000 --duration 10 --concurrency 10

# Large message test
python benchmarks/client.py --addr 127.0.0.1:25000 --duration 10 --msize 10240
```

#### Benchmark Output Interpretation

**Successful Benchmark:**
```
12345 1.0KiB messages in 10.0 seconds
Latency: min 0.123ms; max 5.678ms; mean 0.812ms;
std: 0.234ms (28.8%)
Latency distribution: 50% under 0.712ms; 75% under 0.923ms; 90% under 1.234ms; 95% under 1.567ms; 99% under 2.345ms
Requests/sec: 1234.5
Transfer/sec: 1.2MiB
```

**Key Metrics to Check:**
- **Requests/sec**: Should be > 1000 for 1KB messages
- **Latency**: Mean should be < 1ms for local connections
- **Transfer/sec**: Should match message size × requests/sec
- **No errors**: No connection failures or timeouts

---

### Method 4: Log-Based Verification

#### Enable Debug Logging

TokioLoop uses Rust's `log` crate. Enable debug logging:

```bash
# Set log level
export RUST_LOG=debug

# Run tests with logging
pytest -v tests/tcp/
```

#### Key Log Messages to Look For

**Successful Connection:**
```
[DEBUG] TokioTCPTransport::from_py called
[DEBUG] TokioTCPTransport::from_py: Tokio stream created successfully
[TRACE] TCP connection is_closing. Exiting the loop [fd=X]
[DEBUG] Called connection_lost on TCP [fd=X]
```

**Data Transfer:**
```
[TRACE] TCP wrote N bytes [fd=X]
[TRACE] TCP connection EOF received [fd=X]
```

**Server Accept:**
```
[DEBUG] TokioTCPServer::from_fd called with fd: X
```

#### Common Log Patterns

| Pattern | Meaning |
|---------|---------|
| `TokioTCPTransport::from_py called` | New TCP transport created |
| `Tokio stream created successfully` | Stream conversion successful |
| `TCP wrote N bytes` | Data sent successfully |
| `TCP connection EOF received` | Remote closed connection |
| `Called connection_lost` | Connection cleanup complete |
| `TCP writer shutdown completed` | Graceful shutdown |

---

### Method 5: Code-Level Verification

#### Check Implementation Status

Review [`src/tokio_tcp.rs`](src/tokio_tcp.rs:1) to verify:

**Transport Features:**
- ✅ `TokioTCPTransport` class exists (line 42)
- ✅ `from_py()` method creates transport (line 57)
- ✅ `attach()` method starts I/O (line 111)
- ✅ `io_processing_loop()` handles read/write (line 144)
- ✅ `write()` method buffers data (line 382)
- ✅ `write_eof()` method handles EOF (line 405)
- ✅ `close()` method initiates shutdown (line 445)

**Server Features:**
- ✅ `TokioTCPServer` class exists (line 477)
- ✅ `from_fd()` method creates server (line 492)

#### Verify Test Coverage

Check that tests cover:
- ✅ Connection establishment
- ✅ Data transmission (client → server)
- ✅ Data transmission (server → client)
- ✅ Connection closure
- ✅ Large data transfers
- ✅ Multiple concurrent connections

---

## Troubleshooting

### Common Issues

#### Issue: Tests Fail with "Connection Refused"

**Cause:** Server not starting or port in use

**Solution:**
```bash
# Check if port is in use
lsof -i :25000

# Kill any existing processes
killall python

# Run tests again
pytest -v tests/tcp/
```

#### Issue: Tests Timeout

**Cause:** I/O loop not processing events

**Solution:**
```bash
# Enable debug logging to see what's happening
RUST_LOG=debug pytest -v tests/tcp/ -s

# Check for stuck tasks or deadlocks
```

#### Issue: Data Mismatches

**Cause:** Buffer corruption or race condition

**Solution:**
```bash
# Run tests with sanitizers (if available)
cargo test --release

# Check for memory issues
valgrind python -m pytest tests/tcp/
```

#### Issue: "TokioLoop not found"

**Cause:** TokioLoop not built or imported

**Solution:**
```bash
# Rebuild the project
make build-dev

# Verify import works
python -c "import rloop; print(rloop.new_tokio_event_loop)"
```

---

## Verification Checklist

Use this checklist to verify TCP functionality:

### Basic Functionality
- [ ] All TCP tests pass (`pytest tests/tcp/`)
- [ ] Tests run against TokioLoop (check test IDs)
- [ ] No test failures or errors
- [ ] Connections establish successfully
- [ ] Data transfers in both directions
- [ ] Connections close cleanly

### Performance
- [ ] Benchmarks run without errors
- [ ] Requests/sec > 1000 for 1KB messages
- [ ] Latency < 1ms mean for local connections
- [ ] No connection timeouts
- [ ] No memory leaks (check with `top` or `htop`)

### Logging
- [ ] Debug logs show expected flow
- [ ] No error or warning logs
- [ ] Connection lifecycle logs appear
- [ ] Data transfer logs appear

### Code Quality
- [ ] Implementation follows asyncio protocol
- [ ] All required methods implemented
- [ ] Error handling is proper
- [ ] Resource cleanup is correct

---

## Advanced Verification

### Stress Testing

Run stress tests to verify stability:

```bash
# High concurrency
python benchmarks/client.py --addr 127.0.0.1:25000 --duration 60 --concurrency 50

# Large messages
python benchmarks/client.py --addr 127.0.0.1:25000 --duration 30 --msize 102400

# Long duration
python benchmarks/client.py --addr 127.0.0.1:25000 --duration 300
```

### Memory Leak Testing

Monitor memory usage during extended runs:

```bash
# Start server
python benchmarks/server.py --loop rloop --addr 127.0.0.1:25000 &
SERVER_PID=$!

# Monitor memory
watch -n 1 "ps -p $SERVER_PID -o pid,vsz,rss,cmd"

# Run client in loop
for i in {1..100}; do
    python benchmarks/client.py --addr 127.0.0.1:25000 --duration 5
done

# Check for memory growth
```

### Comparison Testing

Compare TokioLoop against other event loops:

```bash
# Test asyncio
python benchmarks/server.py --loop asyncio --addr 127.0.0.1:25001 &
python benchmarks/client.py --addr 127.0.0.1:25001 --duration 10

# Test uvloop
python benchmarks/server.py --loop uvloop --addr 127.0.0.1:25002 &
python benchmarks/client.py --addr 127.0.0.1:25002 --duration 10

# Test RLoop
python benchmarks/server.py --loop rloop --addr 127.0.0.1:25003 &
python benchmarks/client.py --addr 127.0.0.1:25003 --duration 10
```

Compare the results to ensure TokioLoop performs competitively.

---

## Summary

To verify TokioLoop TCP features are working:

1. **Quick Check:** Run `pytest tests/tcp/` - all tests should pass
2. **Performance Check:** Run benchmarks - should show good throughput
3. **Manual Check:** Create simple echo server and test with netcat
4. **Log Check:** Enable `RUST_LOG=debug` and verify expected log messages
5. **Code Check:** Review implementation in [`src/tokio_tcp.rs`](src/tokio_tcp.rs:1)

**Success Criteria:**
- All automated tests pass
- Benchmarks complete without errors
- Manual tests work as expected
- Logs show proper connection lifecycle
- Performance is competitive with other event loops

If all these checks pass, TokioLoop TCP features are working correctly!
