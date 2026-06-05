use std::collections::HashMap;
use std::os::unix::io::{FromRawFd, OwnedFd};

use io_uring::{opcode, types, IoUring};
use pyo3::prelude::*;

use crate::tokio_handles::RustCallHandle;

// ---------------------------------------------------------------------------
// In-flight operation tracking
// ---------------------------------------------------------------------------

pub struct InFlightOp {
    pub fut: Py<PyAny>,
    pub buf: Vec<u8>,   // owned; must not be moved until CQE arrives
    pub offset: usize,  // bytes already sent (SendAll)
    pub op_type: OpType,
    pub fd: i32,
}

pub enum OpType {
    Recv,
    SendAll,
}

// ---------------------------------------------------------------------------
// UringState — io_uring ring + in-flight map, driven from the Tokio task.
//
// All access is serialised: push_recv/push_send are called under the GIL
// (inside attach_blocking), and drain_completions / flush are called from the
// same Tokio task after releasing the GIL. A Mutex<UringState> in TEventLoop
// provides safe exclusive access.
// ---------------------------------------------------------------------------

pub struct UringState {
    pub ring: IoUring,
    in_flight: HashMap<u64, InFlightOp>,
    next_id: u64,
    pub dirty: bool, // true when SQEs were pushed but not yet flushed
}

impl UringState {
    /// Create a new ring and a non-blocking eventfd that fires on every CQE batch.
    pub fn new() -> std::io::Result<(Self, OwnedFd)> {
        let ring = IoUring::new(512)?;

        let efd_raw = unsafe {
            libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC)
        };
        if efd_raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        ring.submitter().register_eventfd(efd_raw)?;
        let efd = unsafe { OwnedFd::from_raw_fd(efd_raw) };

        Ok((UringState { ring, in_flight: HashMap::new(), next_id: 1, dirty: false }, efd))
    }

    pub fn push_recv(&mut self, fd: i32, nbytes: usize, fut: Py<PyAny>) {
        let mut buf = vec![0u8; nbytes];
        let id = self.next_id;
        self.next_id += 1;
        let sqe = opcode::Recv::new(types::Fd(fd), buf.as_mut_ptr(), nbytes as u32)
            .build()
            .user_data(id);
        // SAFETY: `buf` is moved into `in_flight[id]` immediately; it will not
        // be accessed or dropped until handle_completions() removes the entry.
        unsafe {
            if self.ring.submission().push(&sqe).is_err() {
                let _ = self.ring.submit();
                let _ = self.ring.submission().push(&sqe);
            }
        }
        self.in_flight.insert(id, InFlightOp { fut, buf, offset: 0, op_type: OpType::Recv, fd });
        self.dirty = true;
    }

    pub fn push_send(&mut self, fd: i32, offset: usize, data: Vec<u8>, fut: Py<PyAny>) {
        let id = self.next_id;
        self.next_id += 1;
        let len = (data.len() - offset) as u32;
        let ptr = data[offset..].as_ptr();
        let sqe = opcode::Send::new(types::Fd(fd), ptr, len)
            .build()
            .user_data(id);
        // SAFETY: `data` is moved into `in_flight[id]` immediately.
        unsafe {
            if self.ring.submission().push(&sqe).is_err() {
                let _ = self.ring.submit();
                let _ = self.ring.submission().push(&sqe);
            }
        }
        self.in_flight.insert(id, InFlightOp { fut, buf: data, offset, op_type: OpType::SendAll, fd });
        self.dirty = true;
    }

    /// Flush pending SQEs to the kernel (non-blocking).
    pub fn flush(&mut self) {
        if self.dirty {
            let _ = self.ring.submit();
            self.dirty = false;
        }
    }

    pub fn has_in_flight(&self) -> bool {
        !self.in_flight.is_empty()
    }

    /// Drain all available CQEs. Partial sends are re-submitted immediately.
    /// Returns completed ops for the caller to resolve under the GIL.
    pub fn drain_completions(&mut self) -> Vec<(InFlightOp, i32)> {
        let raw: Vec<(u64, i32)> = self
            .ring
            .completion()
            .map(|cqe| (cqe.user_data(), cqe.result()))
            .collect();

        let mut out = Vec::with_capacity(raw.len());
        for (id, result) in raw {
            if let Some(op) = self.in_flight.remove(&id) {
                // Partial send: re-submit remaining bytes inline (avoids a GIL round-trip).
                if let OpType::SendAll = op.op_type {
                    if result > 0 {
                        let new_off = op.offset + result as usize;
                        if new_off < op.buf.len() {
                            self.push_send(op.fd, new_off, op.buf, op.fut);
                            continue;
                        }
                    }
                }
                out.push((op, result));
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Build a single RustCallHandle that resolves all futures in one GIL pass.
// ---------------------------------------------------------------------------

pub fn build_completion_handle(completions: Vec<(InFlightOp, i32)>) -> RustCallHandle {
    RustCallHandle::new(move |py: Python| {
        for (op, result) in completions {
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
                        let _ = fut.call_method1(
                            py,
                            "set_exception",
                            (PyErr::from(e).into_value(py),),
                        );
                    }
                }
                OpType::SendAll => {
                    let fut = op.fut;
                    if result < 0 {
                        let e = std::io::Error::from_raw_os_error(-result);
                        let _ = fut.call_method1(
                            py,
                            "set_exception",
                            (PyErr::from(e).into_value(py),),
                        );
                    } else {
                        let _ = fut.call_method1(py, "set_result", (py.None(),));
                    }
                }
            }
        }
    })
}
