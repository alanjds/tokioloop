use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;

use io_uring::{opcode, types, IoUring};
use pyo3::prelude::*;

use crate::tokio_event_loop::ScheduledTask;
use crate::tokio_handles::RustCallHandle;

// ---------------------------------------------------------------------------
// Request types sent into each io_uring thread
// ---------------------------------------------------------------------------

pub enum UringRequest {
    Recv { fd: i32, nbytes: usize, fut: Py<PyAny> },
    SendAll { fd: i32, data: Vec<u8>, fut: Py<PyAny> },
    Shutdown,
}

// Safety: Py<PyAny> is Send when the GIL is not held at the point of transfer,
// which is guaranteed here (we only call Python methods under the scheduler).
unsafe impl Send for UringRequest {}

// ---------------------------------------------------------------------------
// In-flight operation tracking
// ---------------------------------------------------------------------------

struct InFlightOp {
    fut: Py<PyAny>,
    buf: Vec<u8>,   // owned buffer; must not be moved/dropped until CQE arrives
    offset: usize,  // bytes already sent (SendAll partial-send tracking)
    op_type: OpType,
    fd: i32,
}

enum OpType {
    Recv,
    SendAll,
}

// ---------------------------------------------------------------------------
// IoUringExecutor — runs on a dedicated OS thread, no Tokio dependency
// ---------------------------------------------------------------------------

struct IoUringExecutor {
    ring: IoUring,
    in_flight: HashMap<u64, InFlightOp>,
    next_id: u64,
    scheduler_tx: async_channel::Sender<ScheduledTask>,
}

impl IoUringExecutor {
    fn submit_recv(&mut self, fd: i32, nbytes: usize, fut: Py<PyAny>) {
        let mut buf = vec![0u8; nbytes];
        let id = self.next_id;
        self.next_id += 1;
        let sqe = opcode::Recv::new(types::Fd(fd), buf.as_mut_ptr(), nbytes as u32)
            .build()
            .user_data(id);
        // SAFETY: `buf` is moved into `in_flight[id]` immediately after this push.
        // It will not be accessed or dropped until handle_completion() removes the entry,
        // which only happens after the kernel signals completion via CQE.
        unsafe {
            if self.ring.submission().push(&sqe).is_err() {
                // Submission queue full: flush it first, then retry.
                let _ = self.ring.submit();
                let _ = self.ring.submission().push(&sqe);
            }
        }
        self.in_flight.insert(id, InFlightOp { fut, buf, offset: 0, op_type: OpType::Recv, fd });
    }

    fn submit_send(&mut self, fd: i32, offset: usize, data: Vec<u8>, fut: Py<PyAny>) {
        let id = self.next_id;
        self.next_id += 1;
        let len = (data.len() - offset) as u32;
        let ptr = data[offset..].as_ptr();
        let sqe = opcode::Send::new(types::Fd(fd), ptr, len)
            .build()
            .user_data(id);
        // SAFETY: `data` is moved into `in_flight[id]` immediately after this push.
        // Its memory remains pinned until handle_completion() finishes.
        unsafe {
            if self.ring.submission().push(&sqe).is_err() {
                let _ = self.ring.submit();
                let _ = self.ring.submission().push(&sqe);
            }
        }
        self.in_flight.insert(id, InFlightOp { fut, buf: data, offset, op_type: OpType::SendAll, fd });
    }

    fn build_completion_handles(completions: Vec<(u64, i32, InFlightOp)>) -> Box<dyn FnOnce(Python) + Send> {
        // Build a single closure that resolves all futures in one GIL acquisition.
        Box::new(move |py: Python| {
            for (_, result, op) in completions {
                match op.op_type {
                    OpType::Recv => {
                        let fut = op.fut;
                        let mut buf = op.buf;
                        if result >= 0 {
                            buf.truncate(result as usize);
                            let data = pyo3::types::PyBytes::new(py, &buf);
                            let _ = fut.call_method1(py, "set_result", (data,));
                        } else {
                            let e = std::io::Error::from_raw_os_error(-result);
                            let _ = fut.call_method1(py, "set_exception", (PyErr::from(e).into_value(py),));
                        }
                    }
                    OpType::SendAll => {
                        let fut = op.fut;
                        if result < 0 {
                            let e = std::io::Error::from_raw_os_error(-result);
                            let _ = fut.call_method1(py, "set_exception", (PyErr::from(e).into_value(py),));
                        } else {
                            // Fully sent (partial sends are re-submitted before this point).
                            let _ = fut.call_method1(py, "set_result", (py.None(),));
                        }
                    }
                }
            }
        })
    }

    fn run(mut self, request_rx: mpsc::Receiver<UringRequest>) {
        loop {
            // 1. Non-blocking drain: pick up all requests already queued.
            let mut shutdown = false;
            while let Ok(req) = request_rx.try_recv() {
                match req {
                    UringRequest::Shutdown => { shutdown = true; break; }
                    UringRequest::Recv { fd, nbytes, fut } => self.submit_recv(fd, nbytes, fut),
                    UringRequest::SendAll { fd, data, fut } => self.submit_send(fd, 0, data, fut),
                }
            }
            if shutdown { break; }

            // 2. If nothing is in-flight, block until a new request arrives.
            if self.in_flight.is_empty() {
                match request_rx.recv() {
                    Ok(UringRequest::Shutdown) | Err(_) => break,
                    Ok(UringRequest::Recv { fd, nbytes, fut }) => self.submit_recv(fd, nbytes, fut),
                    Ok(UringRequest::SendAll { fd, data, fut }) => self.submit_send(fd, 0, data, fut),
                }
            }

            // 3. Submit all pending SQEs and wait for at least one completion.
            if let Err(e) = self.ring.submit_and_wait(1) {
                log::error!("io_uring submit_and_wait: {e}");
                break;
            }

            // 4. Collect completions into a Vec so the borrow on `ring` ends before
            //    we call submit_send (which needs &mut self.ring).
            let raw_completions: Vec<(u64, i32)> = self
                .ring
                .completion()
                .map(|cqe| (cqe.user_data(), cqe.result()))
                .collect();

            if raw_completions.is_empty() {
                continue;
            }

            // 5. Resolve completions. Partial SendAll ops are re-submitted immediately;
            //    the rest are collected for a single batched GIL call.
            let mut to_deliver: Vec<(u64, i32, InFlightOp)> = Vec::with_capacity(raw_completions.len());
            for (id, result) in raw_completions {
                if let Some(op) = self.in_flight.remove(&id) {
                    // Re-submit partial sends inside this thread to avoid a GIL round-trip.
                    if let OpType::SendAll = op.op_type {
                        if result > 0 {
                            let new_offset = op.offset + result as usize;
                            if new_offset < op.buf.len() {
                                self.submit_send(op.fd, new_offset, op.buf, op.fut);
                                continue;
                            }
                        }
                    }
                    to_deliver.push((id, result, op));
                }
            }

            if to_deliver.is_empty() {
                continue;
            }

            // 6. Deliver all completions in ONE scheduler item → ONE GIL acquisition.
            let handle = RustCallHandle::new(Self::build_completion_handles(to_deliver));
            let _ = self.scheduler_tx.try_send(ScheduledTask::Immediate { handle: Box::new(handle) });
        }
    }
}

// ---------------------------------------------------------------------------
// UringPool — a fixed pool of io_uring threads with fd-based routing.
//
// Routing by (fd % pool_size) ensures all ops for a given fd stay on the
// same thread, keeping the per-thread in_flight map consistent while
// spreading load across N parallel rings.
// ---------------------------------------------------------------------------

pub struct UringPool {
    senders: Vec<mpsc::SyncSender<UringRequest>>,
    threads: Vec<thread::JoinHandle<()>>,
}

impl UringPool {
    pub fn new(
        scheduler_tx: async_channel::Sender<ScheduledTask>,
    ) -> std::io::Result<Self> {
        // Use one thread per available CPU, capped at 8 to avoid excessive rings.
        let n = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(4)
            .min(8)
            .max(1);

        let mut senders = Vec::with_capacity(n);
        let mut threads = Vec::with_capacity(n);
        for i in 0..n {
            let ring = IoUring::new(256)?;
            let (tx, rx) = mpsc::sync_channel::<UringRequest>(512);
            let executor = IoUringExecutor {
                ring,
                in_flight: HashMap::new(),
                next_id: 1,
                scheduler_tx: scheduler_tx.clone(),
            };
            let handle = thread::Builder::new()
                .name(format!("tokioloop-io-uring-{i}"))
                .spawn(move || executor.run(rx))?;
            senders.push(tx);
            threads.push(handle);
        }
        Ok(UringPool { senders, threads })
    }

    #[inline]
    fn sender_for(&self, fd: i32) -> &mpsc::SyncSender<UringRequest> {
        &self.senders[(fd as usize) % self.senders.len()]
    }

    pub fn submit_recv(&self, fd: i32, nbytes: usize, fut: Py<PyAny>) {
        let _ = self.sender_for(fd).send(UringRequest::Recv { fd, nbytes, fut });
    }

    pub fn submit_send(&self, fd: i32, data: Vec<u8>, fut: Py<PyAny>) {
        let _ = self.sender_for(fd).send(UringRequest::SendAll { fd, data, fut });
    }

    pub fn shutdown(self) {
        for sender in &self.senders {
            let _ = sender.send(UringRequest::Shutdown);
        }
        for handle in self.threads {
            let _ = handle.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn start_uring_pool(
    scheduler_tx: async_channel::Sender<ScheduledTask>,
) -> std::io::Result<UringPool> {
    UringPool::new(scheduler_tx)
}
