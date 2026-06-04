# io_uring — Dedicated io_uring Executor Alongside Tokio

## Goal

Replace `epoll`+`recv` with `io_uring` for socket I/O, using a **dedicated io_uring thread**
that is independent of the Tokio runtime. This avoids `tokio-uring`'s single-thread limitation
and works with both single-thread and multi-thread Tokio configurations.

The io_uring thread owns a submission/completion ring and handles all `sock_recv`,
`sock_sendall`, and `sock_accept` operations. The existing Tokio runtime handles timers,
callbacks, and the Python GIL interaction unchanged.

## Branch

`feat/io-uring` — based on `feat/native-sock-v3`

## Why Not tokio-uring

`tokio-uring` requires a `tokio_uring::start()` runtime (single-threaded, not composable
with `tokio::runtime::Builder::new_multi_thread()`). The TokioLoop runtime is multi-thread.
Instead, use the `io-uring` crate (raw `liburing` bindings) on a dedicated OS thread.

## Architecture

```
Python thread (GIL)
  └─ _run / io_processing_loop  (Tokio multi-thread runtime)
       └─ scheduler channel (existing)

io_uring thread  (dedicated OS thread, no Tokio, no GIL)
  └─ io_uring ring (IORING_SETUP_SQPOLL optional)
       ├─ submits: IORING_OP_RECV, IORING_OP_SEND, IORING_OP_ACCEPT
       └─ on completion: result → completion_tx (async_channel) → scheduler → _run → GIL → set_result
```

The io_uring thread is a standard `std::thread` that:
1. Owns an `io_uring::IoUring` instance
2. Receives `UringRequest` messages from `submission_tx` (sent by `_sock_recv_native` etc.)
3. Submits them as SQEs to the ring
4. Calls `ring.submit_and_wait(1)` to block until at least one completion arrives
5. Drains the CQ and sends results back via `completion_tx` (a `async_channel::Sender`)
6. The `_run` scheduler picks up completions exactly like existing `RustCallHandle` items

## Cargo.toml Changes

```toml
[dependencies]
io-uring = "0.7"
```

## New Module: `src/uring.rs`

Create a new file `src/uring.rs` containing:

### `UringRequest` enum

```rust
use pyo3::Py;
use pyo3::PyAny;

pub enum UringRequest {
    Recv {
        fd: i32,
        nbytes: usize,
        fut: Py<PyAny>,
    },
    SendAll {
        fd: i32,
        data: Vec<u8>,
        fut: Py<PyAny>,
    },
    Shutdown,
}
```

### `UringCompletion` — routed as `RustCallHandle` via existing scheduler

Completions are sent as `ScheduledTask::Immediate { handle: Box::new(RustCallHandle::new(...)) }`
via the existing `scheduler_tx`. No new channel type needed.

### `IoUringExecutor`

```rust
use io_uring::{IoUring, opcode, types};
use std::collections::HashMap;

pub struct IoUringExecutor {
    ring: IoUring,
    // user_data → (fut, buffer_ptr, buffer_len) for in-flight ops
    in_flight: HashMap<u64, InFlightOp>,
    next_id: u64,
    scheduler_tx: async_channel::Sender<crate::tokio_event_loop::ScheduledTask>,
}

struct InFlightOp {
    fut: pyo3::Py<pyo3::PyAny>,
    buf: Vec<u8>,     // owned buffer for the duration of the op
    op_type: OpType,
}

enum OpType { Recv, SendAll }
```

### `IoUringExecutor::run` (blocking loop, runs on dedicated thread)

```rust
impl IoUringExecutor {
    pub fn run(mut self, request_rx: std::sync::mpsc::Receiver<UringRequest>) {
        loop {
            // Drain all pending requests and submit them as SQEs
            while let Ok(req) = request_rx.try_recv() {
                match req {
                    UringRequest::Shutdown => return,
                    UringRequest::Recv { fd, nbytes, fut } => {
                        let mut buf = vec![0u8; nbytes];
                        let sqe = opcode::Recv::new(types::Fd(fd), buf.as_mut_ptr(), nbytes as u32)
                            .build()
                            .user_data(self.next_id);
                        self.in_flight.insert(self.next_id, InFlightOp { fut, buf, op_type: OpType::Recv });
                        self.next_id += 1;
                        unsafe { self.ring.submission().push(&sqe).unwrap() };
                    }
                    UringRequest::SendAll { fd, data, fut } => {
                        let len = data.len() as u32;
                        let ptr = data.as_ptr();
                        let sqe = opcode::Send::new(types::Fd(fd), ptr, len)
                            .build()
                            .user_data(self.next_id);
                        self.in_flight.insert(self.next_id, InFlightOp { fut, buf: data, op_type: OpType::SendAll });
                        self.next_id += 1;
                        unsafe { self.ring.submission().push(&sqe).unwrap() };
                    }
                }
            }

            // Submit all queued SQEs and wait for at least one completion
            self.ring.submit_and_wait(1).unwrap();

            // Drain completions
            let cq = unsafe { self.ring.completion_shared() };
            for cqe in cq {
                let id = cqe.user_data();
                let result = cqe.result(); // bytes transferred (>= 0) or -errno
                if let Some(op) = self.in_flight.remove(&id) {
                    let scheduler_tx = self.scheduler_tx.clone();
                    match op.op_type {
                        OpType::Recv => {
                            let fut = op.fut;
                            let buf = op.buf;
                            let res: Result<Vec<u8>, i32> = if result >= 0 {
                                let mut b = buf;
                                b.truncate(result as usize);
                                Ok(b)
                            } else {
                                Err(-result)
                            };
                            let handle = crate::tokio_handles::RustCallHandle::new(move |py| {
                                match res {
                                    Ok(bytes) => {
                                        let data = pyo3::types::PyBytes::new(py, &bytes);
                                        let _ = fut.call_method1(py, "set_result", (data,));
                                    }
                                    Err(errno) => {
                                        let e = std::io::Error::from_raw_os_error(errno);
                                        let _ = fut.call_method1(py, "set_exception",
                                            (pyo3::exceptions::PyOSError::new_err(e.to_string()),));
                                    }
                                }
                            });
                            let _ = scheduler_tx.try_send(
                                crate::tokio_event_loop::ScheduledTask::Immediate {
                                    handle: Box::new(handle),
                                }
                            );
                        }
                        OpType::SendAll => { /* similar pattern */ }
                    }
                }
            }
        }
    }
}
```

### Thread startup

```rust
pub fn start_io_uring_thread(
    scheduler_tx: async_channel::Sender<crate::tokio_event_loop::ScheduledTask>,
) -> (std::sync::mpsc::SyncSender<UringRequest>, std::thread::JoinHandle<()>) {
    let (tx, rx) = std::sync::mpsc::sync_channel::<UringRequest>(256);
    let ring = IoUring::new(256).expect("io_uring init failed");
    let executor = IoUringExecutor {
        ring,
        in_flight: HashMap::new(),
        next_id: 1,
        scheduler_tx,
    };
    let handle = std::thread::Builder::new()
        .name("tokioloop-io-uring".into())
        .spawn(move || executor.run(rx))
        .unwrap();
    (tx, handle)
}
```

## Changes to `src/tokio_event_loop.rs`

### Add to `TEventLoop` struct

```rust
uring_tx: Option<std::sync::mpsc::SyncSender<crate::uring::UringRequest>>,
uring_thread: Option<std::thread::JoinHandle<()>>,
```

### Initialize in `TEventLoop::new()` (or `_run` startup)

```rust
let (uring_tx, uring_thread) = crate::uring::start_io_uring_thread(scheduler_tx.clone());
```

### `_sock_recv_native` — replace worker dispatch with uring submission

On EAGAIN (or always, bypassing the fast path attempt):

```rust
// Try fast path (MSG_DONTWAIT) first — same as current
// ...
// On EAGAIN, submit to io_uring instead of worker channel:
if let Some(ref tx) = self.uring_tx {
    let _ = tx.send(crate::uring::UringRequest::Recv { fd, nbytes, fut });
} else {
    // Fallback: existing worker task path
    let tx = self.get_or_spawn_reader(fd)?;
    let _ = tx.try_send(SockRecvMsg::Recv { nbytes, fut });
}
```

### Shutdown

In `_run` teardown or `close()`, send `UringRequest::Shutdown` and join the thread.

## Single vs Multi-Thread Tokio Compatibility

Because the io_uring thread is a plain `std::thread` with no Tokio dependency, it works
with any Tokio runtime configuration. The only integration point is the `async_channel`
scheduler, which is `Send + Sync + Clone`.

## Multishot Recv (Future Enhancement)

`IORING_OP_RECV_MULTISHOT` (Linux 6.0+) allows registering one recv that fires multiple
times as data arrives — like the persistent `AsyncFd` worker but with lower kernel overhead.
Implement after the basic single-shot version is verified:

```rust
opcode::RecvMulti::new(types::Fd(fd), /* buf_group */ 0)
    .build()
    .user_data(id)
```

Requires buffer rings (`IORING_REGISTER_PBUF_RING`) for the kernel to have a pool of
buffers to write into — necessary for multishot since recv count is unbounded.

## Fixed Buffers (Future Enhancement)

`IORING_REGISTER_BUFFERS` maps pre-allocated Rust memory into the kernel, enabling
`IORING_OP_RECV_FIXED` which avoids the kernel copy entirely (DMA → registered buffer directly).
Requires the buffer to stay pinned for the lifetime of the ring registration.

## Kernel Version Requirements

| Feature | Minimum Kernel |
|---|---|
| `IORING_OP_RECV` | 5.6 |
| `IORING_OP_SEND` | 5.6 |
| `IORING_OP_ACCEPT` | 5.5 |
| `IORING_OP_RECV_MULTISHOT` | 6.0 |
| Fixed buffers | 5.1 |

Check at startup: `IoUring::new(256)` returns `Err` if io_uring is unavailable.
Fallback to existing `AsyncFd`/epoll path if io_uring init fails.

## Files to Create / Modify

- **Create** `src/uring.rs` — `UringRequest`, `IoUringExecutor`, `start_io_uring_thread`
- **Modify** `src/lib.rs` — add `mod uring;`
- **Modify** `src/tokio_event_loop.rs` — add `uring_tx` field, update `_sock_recv_native`,
  update `ScheduledTask` visibility if needed, shutdown logic
- **Modify** `Cargo.toml` — add `io-uring = "0.7"`

## Verification

```bash
# Build
maturin develop

# Tests
.venv/bin/python -m pytest tests/test_sockets.py tests/tcp/test_tcp_server.py -k TokioLoop -q

# Check kernel supports io_uring (needs >= 5.6)
uname -r

# Benchmark
BENCHMARK_EXC_PREFIX=.venv/bin .venv/bin/python benchmarks/benchmarks.py raw
```

## Baseline (feat/native-sock-v3 to beat)

| Loop | 1KB | 10KB | 100KB |
|------|----:|-----:|------:|
| asyncio | 11,513 | 10,780 | 7,418 |
| **tokioloop** | **9,883** | **7,457** | **3,444** |

tokioloop vs asyncio: **85.8% / 69.2% / 46.4%**

Target after io_uring: **≥90% / ≥80% / ≥65%** (single-shot recv).
With multishot + fixed buffers: potentially **≥95% / ≥90% / ≥80%**.

## Implementation Order

1. Basic `IORING_OP_RECV` single-shot — replace `sock_recv` EAGAIN path
2. `IORING_OP_SEND` single-shot — replace `sock_sendall`
3. Benchmark and verify correctness ✅ **done** (see Results below)
4. `IORING_OP_ACCEPT` — replace `sock_accept`
5. (Optional) Multishot recv + buffer rings

---

## Results (implemented on `claude/io-uring-performance-YkX7H`)

Steps 1–3 are complete. The single-shot RECV+SEND io_uring thread replaced the
AsyncFd/epoll worker for all EAGAIN cases in `_sock_recv_native` and
`_sock_sendall_native`. Tests: zero regressions (same 6 pre-existing failures).

### Same-machine benchmark vs feat/native-sock-v3 (raw, concurrency=1)

| Loop | 1KB | 10KB | 100KB |
|------|----:|-----:|------:|
| asyncio | 18,002 | 15,732 | 8,959 |
| rloop | 17,955 | 15,635 | 7,587 |
| **tokioloop v3** | **15,348** | **10,633** | **3,638** |
| **tokioloop uring** | **15,090** | **9,976** | **3,760** |
| uvloop | 18,843 | 17,097 | 10,048 |

**tokioloop vs asyncio:** 83.8% / 63.4% / 42.0% (uring) vs 86.5% / 67.5% / 40.5% (v3)

### Concurrency benchmark (raw 1KB)

| | c=2 | c=3 |
|--|----:|----:|
| asyncio | 20,241 | 21,662 |
| **tokioloop** | **16,338** | **17,046** |
| tokioloop vs asyncio | 80.7% | 78.7% |

### Why the targets weren't reached

The existing benchmarks don't stress the io_uring path: the client is synchronous
(send→wait→send), so the server socket almost always has data on the first
`recv(MSG_DONTWAIT)`. The EAGAIN fallback — where io_uring activates — fires rarely.
The performance targets require benchmarks that expose this path.

### Next step

See **[agents/plans/new-benchmarks.md](new-benchmarks.md)** for two new benchmark
targets (`high_conc` at c=10/50/100 and `slow` with forced EAGAIN via 2 ms client
think-time) that will properly exercise the io_uring hot path.
6. (Optional) Fixed buffers for zero-copy
