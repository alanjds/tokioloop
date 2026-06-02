use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;

use io_uring::{opcode, types, IoUring};
use pyo3::prelude::*;

use crate::tokio_event_loop::ScheduledTask;
use crate::tokio_handles::RustCallHandle;

// ---------------------------------------------------------------------------
// Request types sent from TEventLoop into the io_uring thread
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

    fn handle_completion(&mut self, op: InFlightOp, result: i32) {
        let scheduler_tx = self.scheduler_tx.clone();
        match op.op_type {
            OpType::Recv => {
                let fut = op.fut;
                let mut buf = op.buf;
                let res: Result<Vec<u8>, i32> = if result >= 0 {
                    buf.truncate(result as usize);
                    Ok(buf)
                } else {
                    Err(-result)
                };
                let handle = RustCallHandle::new(move |py| match res {
                    Ok(bytes) => {
                        let data = pyo3::types::PyBytes::new(py, &bytes);
                        let _ = fut.call_method1(py, "set_result", (data,));
                    }
                    Err(errno) => {
                        let e = std::io::Error::from_raw_os_error(errno);
                        let _ = fut.call_method1(py, "set_exception", (PyErr::from(e).into_value(py),));
                    }
                });
                let _ = scheduler_tx.try_send(ScheduledTask::Immediate { handle: Box::new(handle) });
            }
            OpType::SendAll => {
                if result < 0 {
                    let errno = -result;
                    let fut = op.fut;
                    let handle = RustCallHandle::new(move |py| {
                        let e = std::io::Error::from_raw_os_error(errno);
                        let _ = fut.call_method1(py, "set_exception", (PyErr::from(e).into_value(py),));
                    });
                    let _ = scheduler_tx.try_send(ScheduledTask::Immediate { handle: Box::new(handle) });
                } else {
                    let new_offset = op.offset + result as usize;
                    if new_offset >= op.buf.len() {
                        let fut = op.fut;
                        let handle = RustCallHandle::new(move |py| {
                            let _ = fut.call_method1(py, "set_result", (py.None(),));
                        });
                        let _ = scheduler_tx.try_send(ScheduledTask::Immediate { handle: Box::new(handle) });
                    } else {
                        // Partial send: re-submit the remaining bytes.
                        self.submit_send(op.fd, new_offset, op.buf, op.fut);
                    }
                }
            }
        }
    }

    fn run(mut self, request_rx: mpsc::Receiver<UringRequest>) {
        loop {
            // 1. Non-blocking drain: pick up all requests already queued.
            let mut shutdown = false;
            while let Ok(req) = request_rx.try_recv() {
                match req {
                    UringRequest::Shutdown => {
                        shutdown = true;
                        break;
                    }
                    UringRequest::Recv { fd, nbytes, fut } => self.submit_recv(fd, nbytes, fut),
                    UringRequest::SendAll { fd, data, fut } => self.submit_send(fd, 0, data, fut),
                }
            }
            if shutdown {
                break;
            }

            // 2. If nothing is in-flight, block until a new request arrives.
            //    This avoids spinning and keeps submit_and_wait from hanging forever.
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
            //    we call handle_completion (which may submit new SQEs via &mut self).
            let completions: Vec<(u64, i32)> = self
                .ring
                .completion()
                .map(|cqe| (cqe.user_data(), cqe.result()))
                .collect();

            // 5. Process completions; partial SendAll re-submits inside handle_completion.
            for (id, result) in completions {
                if let Some(op) = self.in_flight.remove(&id) {
                    self.handle_completion(op, result);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry point: spin up the io_uring thread
// ---------------------------------------------------------------------------

pub fn start_io_uring_thread(
    scheduler_tx: async_channel::Sender<ScheduledTask>,
) -> std::io::Result<(mpsc::SyncSender<UringRequest>, thread::JoinHandle<()>)> {
    let ring = IoUring::new(256)?;
    let (tx, rx) = mpsc::sync_channel::<UringRequest>(256);
    let executor = IoUringExecutor {
        ring,
        in_flight: HashMap::new(),
        next_id: 1,
        scheduler_tx,
    };
    let handle = thread::Builder::new()
        .name("tokioloop-io-uring".into())
        .spawn(move || executor.run(rx))?;
    Ok((tx, handle))
}
