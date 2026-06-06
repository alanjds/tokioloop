use std::{
    collections::{BinaryHeap, HashMap, VecDeque},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    pin::Pin,
    sync::{atomic, Arc, Mutex, OnceLock, RwLock},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use anyhow::Result;
use futures::Stream;
use pyo3::prelude::*;
use std::sync::atomic::AtomicBool;
use tokio::{runtime::Runtime, task::JoinHandle, net::UnixStream};
use tokio_util::sync::CancellationToken;

use crate::{
    tokio_handles::{TCBHandle, TTimerHandle, TBoxedHandle, THandle, PermitHandle, RustCallHandle},
    py::{copy_context, attach_blocking},
    log::{LogExc, log_exc_to_py_ctx},
    server::TokioServer,
    tokio_tcp::{TokioTCPServer, TokioTCPServerRef},
};
use pyo3::IntoPyObjectExt;

// Holds Python callback args behind a Mutex so they can be cleared with the GIL
// by remove_reader/remove_writer, while the watcher task only captures the Arc.
// This prevents Py<T> from being dropped on a tokio worker thread without the GIL.
type PyCallbackEntry = Arc<Mutex<Option<(Py<PyAny>, Py<PyAny>, Py<PyAny>)>>>;

// Non-owning fd wrapper — AsyncFd<BorrowedFd> tracks epoll interest without
// closing the underlying fd on drop (the Python socket owns the fd lifetime).
struct BorrowedFd(i32);

impl AsRawFd for BorrowedFd {
    fn as_raw_fd(&self) -> RawFd { self.0 }
}

// Persistent per-fd set for Option B: AsyncFd is created once per fd and
// reused across multiple EAGAIN events, saving dup+epoll_ctl ADD/DEL per EAGAIN.
struct ReadPollSet {
    // fd → (persistent AsyncFd, Option<pending (nbytes, fut)>)
    fds: HashMap<i32, (tokio::io::unix::AsyncFd<BorrowedFd>, Option<(usize, Py<PyAny>)>)>,
    pending_count: usize,
}

impl ReadPollSet {
    fn new() -> Self {
        Self { fds: HashMap::new(), pending_count: 0 }
    }

    fn add(&mut self, fd: i32, nbytes: usize, fut: Py<PyAny>) {
        match self.fds.get_mut(&fd) {
            Some(entry) => {
                // Reuse existing AsyncFd — no syscall needed
                if entry.1.is_none() { self.pending_count += 1; }
                entry.1 = Some((nbytes, fut));
            }
            None => {
                if let Ok(async_fd) = tokio::io::unix::AsyncFd::new(BorrowedFd(fd)) {
                    self.fds.insert(fd, (async_fd, Some((nbytes, fut))));
                    self.pending_count += 1;
                }
            }
        }
    }

    fn is_empty(&self) -> bool { self.pending_count == 0 }
}

impl futures::Stream for ReadPollSet {
    // (future, Ok(received data) or Err(io error))
    // recv() is done here while the ReadyGuard is held:
    // try_io clears ready on success but RETAINS it on EAGAIN (spurious wakeup),
    // so the next poll_read_ready fires immediately and we retry.
    type Item = (Py<PyAny>, Result<Vec<u8>, std::io::Error>);

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let me = self.get_mut();
        for (&fd, (async_fd, maybe_pending)) in me.fds.iter_mut() {
            if maybe_pending.is_none() { continue; }
            // Inner retry loop: after a spurious EAGAIN + clear_ready(), re-poll
            // immediately so that the waker is registered via the Poll::Pending path.
            loop {
                match async_fd.poll_read_ready(cx) {
                    Poll::Ready(Ok(mut guard)) => {
                        let nbytes = maybe_pending.as_ref().unwrap().0;
                        let mut avail: libc::c_int = 0;
                        unsafe { libc::ioctl(fd, libc::FIONREAD, &mut avail) };
                        let alloc_size = (avail as usize).min(nbytes).max(1);
                        let mut buf = vec![0u8; alloc_size];
                        let ptr = buf.as_mut_ptr() as *mut libc::c_void;
                        let io_result = guard.try_io(|_| {
                            let k = unsafe { libc::recv(fd, ptr, alloc_size, 0) };
                            if k >= 0 { Ok(k as usize) } else { Err(std::io::Error::last_os_error()) }
                        });
                        match io_result {
                            Ok(Ok(k)) => {
                                buf.truncate(k);
                                let (_, fut) = maybe_pending.take().unwrap();
                                me.pending_count -= 1;
                                return Poll::Ready(Some((fut, Ok(buf))));
                            }
                            Ok(Err(e)) => {
                                let (_, fut) = maybe_pending.take().unwrap();
                                me.pending_count -= 1;
                                return Poll::Ready(Some((fut, Err(e))));
                            }
                            Err(_would_block) => {
                                // Spurious wakeup (EPOLLIN fired but buffer is empty).
                                // clear_ready() resets internal readiness. Loop back to
                                // poll_read_ready so the Poll::Pending path registers
                                // the waker for the next real EPOLLIN event.
                                guard.clear_ready();
                                // continue inner loop → poll_read_ready again
                            }
                        }
                    }
                    Poll::Ready(Err(e)) => {
                        let (_, fut) = maybe_pending.take().unwrap();
                        me.pending_count -= 1;
                        return Poll::Ready(Some((fut, Err(e))));
                    }
                    Poll::Pending => {
                        break; // waker registered, move to next fd
                    }
                }
            }
        }
        Poll::Pending
    }
}

// Persistent per-fd set for sock_sendall EAGAIN handling, mirroring ReadPollSet.
// Uses OwnedFd (from dup) so its epoll registration doesn't conflict with
// ReadPollSet's BorrowedFd registration on the same original fd.
struct WritePollSet {
    // fd_orig → (AsyncFd on dup'd fd, Option<pending (data, offset, fut)>)
    fds: HashMap<i32, (tokio::io::unix::AsyncFd<OwnedFd>, Option<(Vec<u8>, usize, Py<PyAny>)>)>,
    pending_count: usize,
}

impl WritePollSet {
    fn new() -> Self { Self { fds: HashMap::new(), pending_count: 0 } }

    // Returns Some((fut, error)) if setup fails (dup or AsyncFd creation);
    // caller should resolve the future with that error.
    fn add(&mut self, fd: i32, data: Vec<u8>, fut: Py<PyAny>)
        -> Option<(Py<PyAny>, std::io::Error)>
    {
        match self.fds.get_mut(&fd) {
            Some(entry) => {
                if entry.1.is_none() { self.pending_count += 1; }
                entry.1 = Some((data, 0, fut));
                None
            }
            None => {
                let fd_dup = unsafe { libc::dup(fd) };
                if fd_dup < 0 {
                    return Some((fut, std::io::Error::last_os_error()));
                }
                let owned = unsafe { OwnedFd::from_raw_fd(fd_dup) };
                match tokio::io::unix::AsyncFd::with_interest(
                    owned, tokio::io::Interest::WRITABLE,
                ) {
                    Ok(async_fd) => {
                        self.fds.insert(fd, (async_fd, Some((data, 0, fut))));
                        self.pending_count += 1;
                        None
                    }
                    Err(e) => Some((fut, e)),
                }
            }
        }
    }

    fn is_empty(&self) -> bool { self.pending_count == 0 }
}

impl futures::Stream for WritePollSet {
    // (future, Ok(()) = all bytes sent  |  Err(e) = I/O error)
    type Item = (Py<PyAny>, Result<(), std::io::Error>);

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let me = self.get_mut();
        for (&fd, (async_fd, maybe_pending)) in me.fds.iter_mut() {
            if maybe_pending.is_none() { continue; }
            // Inner retry loop: after EAGAIN + clear_ready(), re-poll immediately
            // so that Poll::Pending registers the waker (same pattern as ReadPollSet).
            loop {
                match async_fd.poll_write_ready(cx) {
                    Poll::Ready(Ok(mut guard)) => {
                        let (data, offset, _) = maybe_pending.as_mut().unwrap();
                        // Greedy send: keep sending until EAGAIN or all bytes flushed.
                        // Avoid try_io so we don't prematurely clear_ready on partial sends.
                        let outcome = 'send: loop {
                            let n = unsafe {
                                libc::send(
                                    fd,
                                    data[*offset..].as_ptr() as *const libc::c_void,
                                    data.len() - *offset,
                                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                                )
                            };
                            if n > 0 {
                                *offset += n as usize;
                                if *offset >= data.len() { break 'send Ok(true); }
                                continue 'send; // more to send, loop immediately
                            }
                            let e = std::io::Error::last_os_error();
                            if e.kind() == std::io::ErrorKind::WouldBlock {
                                break 'send Ok(false); // EAGAIN
                            }
                            break 'send Err(e);
                        };
                        guard.clear_ready();
                        match outcome {
                            Ok(true) => {
                                let (_, _, fut) = maybe_pending.take().unwrap();
                                me.pending_count -= 1;
                                return Poll::Ready(Some((fut, Ok(()))));
                            }
                            Ok(false) => {
                                // EAGAIN: loop back → poll_write_ready → Poll::Pending
                            }
                            Err(e) => {
                                let (_, _, fut) = maybe_pending.take().unwrap();
                                me.pending_count -= 1;
                                return Poll::Ready(Some((fut, Err(e))));
                            }
                        }
                    }
                    Poll::Ready(Err(e)) => {
                        let (_, _, fut) = maybe_pending.take().unwrap();
                        me.pending_count -= 1;
                        return Poll::Ready(Some((fut, Err(e))));
                    }
                    Poll::Pending => {
                        break; // waker registered, move to next fd
                    }
                }
            }
        }
        Poll::Pending
    }
}

// Timer with absolute timestamp (like RLoop)
pub struct TokioTimer {
    pub handle: TBoxedHandle,
    when: u128,  // Absolute microseconds since epoch
}

impl std::fmt::Debug for TokioTimer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokioTimer")
            .field("when", &self.when)
            .finish()
    }
}

impl PartialEq for TokioTimer {
    fn eq(&self, other: &Self) -> bool {
        self.when == other.when
    }
}

impl Eq for TokioTimer {}

impl PartialOrd for TokioTimer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TokioTimer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reverse for min-heap behavior (earliest time first)
        other.when.cmp(&self.when)
    }
}

pub(crate) enum ScheduledTask {
    Immediate { handle: TBoxedHandle },
    Delayed { timer: TokioTimer },
}

impl std::fmt::Debug for ScheduledTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScheduledTask::Immediate { .. } => write!(f, "ScheduledTask::Immediate"),
            ScheduledTask::Delayed { timer } => write!(f, "ScheduledTask::Delayed {{ when: {} }}", timer.when),
        }
    }
}

pub struct TEventLoopRunState {
    // buf: Box<[u8]>,
    // events: event::Events,
    // pub read_buf: Box<[u8]>,
    // tick_last: u128,
}


#[derive(Clone)]
pub struct LoopHandlers {
    exc_handler: Arc<RwLock<Py<PyAny>>>,
    exception_handler: Arc<RwLock<Py<PyAny>>>,
}

impl LoopHandlers {
    pub fn log_exception(&self, py: Python, ctx: LogExc) -> PyResult<Py<PyAny>> {
        let handler = self.exc_handler.read().unwrap();
        handler.call1(
            py,
            (
                log_exc_to_py_ctx(py, ctx),
                self.exception_handler.read().unwrap().clone_ref(py),
            ),
        )
    }
}

// ---------------------------------------------------------------------------
// Stage 2a: inline-drain of woken continuations on the IO worker thread.
//
// For the STREAM path, `data_received` runs on a tokio worker and calls
// `StreamReader.feed_data` -> `loop.call_soon(continuation)`. Normally that
// continuation handle is sent across `scheduler_tx` to the `_run` thread, which
// re-acquires the GIL to run it: a cross-thread hop plus a *second* GIL section
// per echo. asyncio/uvloop avoid this by running the read, `data_received`, and
// the continuation in one GIL section on one thread.
//
// To match that, `io_processing_loop` installs a thread-local "inline sink"
// around its `data_received` call. While installed, `schedule_handle` diverts
// *immediate* handles into the sink instead of the channel. The worker then runs
// the captured handles in the SAME `Python::attach`, eliminating the hop and the
// second GIL acquisition. The sink is taken (set to None) before running, so any
// `call_soon` issued by the continuations themselves falls back to the channel
// and runs on `_run` next iteration -- preserving asyncio's deferral semantics
// and bounding inline work to the directly-woken continuations. PROTO is
// unaffected: its `data_received` schedules no immediate handle, so the sink
// stays empty and the inline run is a no-op.
thread_local! {
    static INLINE_SINK: std::cell::RefCell<Option<VecDeque<TBoxedHandle>>> =
        const { std::cell::RefCell::new(None) };
}

#[inline]
pub(crate) fn inline_sink_install() {
    INLINE_SINK.with(|s| {
        let mut b = s.borrow_mut();
        if b.is_none() {
            *b = Some(VecDeque::new());
        }
    });
}

#[inline]
pub(crate) fn inline_sink_take() -> Option<VecDeque<TBoxedHandle>> {
    INLINE_SINK.with(|s| s.borrow_mut().take())
}

/// Push an immediate handle into the inline sink if one is installed on this
/// thread. Returns the handle back in `Err` when no sink is active.
#[inline]
fn inline_sink_try_push(handle: TBoxedHandle) -> Result<(), TBoxedHandle> {
    INLINE_SINK.with(|s| {
        let mut b = s.borrow_mut();
        match b.as_mut() {
            Some(q) => {
                q.push_back(handle);
                Ok(())
            }
            None => Err(handle),
        }
    })
}

/// Gate for the Stage 2a inline path. **On by default**; set
/// `TOKIOLOOP_INLINE_STREAM=0` (or `false`) to fall back to the legacy
/// scheduler-hop path. Read once and runtime-toggleable (no rebuild).
pub(crate) fn inline_stream_enabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        std::env::var("TOKIOLOOP_INLINE_STREAM")
            .map(|v| !(v == "0" || v.eq_ignore_ascii_case("false")))
            .unwrap_or(true)
    })
}

#[pyclass(frozen, subclass, module = "rloop._rloop")]
pub struct TEventLoop {
    runtime: OnceLock<Arc<Runtime>>,
    pub(crate) scheduler_tx: async_channel::Sender<ScheduledTask>,
    scheduler_rx: async_channel::Receiver<ScheduledTask>,
    counter_ready: atomic::AtomicUsize,
    closed: atomic::AtomicBool,
    stopping: Arc<atomic::AtomicBool>,
    epoch: Instant,
    exc_handler: Arc<RwLock<Py<PyAny>>>,
    exception_handler: Arc<RwLock<Py<PyAny>>>,
    #[pyo3(get)]
    _base_ctx: Py<PyAny>,
    // Signal handling
    signal_socket_rx: async_channel::Receiver<u8>,
    signal_socket_tx: async_channel::Sender<u8>,
    sig_listening: Arc<AtomicBool>,
    sig_handlers: Arc<papaya::HashMap<u8, Py<PyAny>>>,
    // I/O watcher tasks, keyed by fd
    io_reader_entries: Arc<papaya::HashMap<usize, (CancellationToken, PyCallbackEntry)>>,
    io_writer_entries: Arc<papaya::HashMap<usize, (CancellationToken, PyCallbackEntry)>>,
    // Option B: pending sock_recv / sock_sendall calls queued on EAGAIN,
    // polled directly in _run's select! loop via ReadPollSet / WritePollSet.
    pending_reads: Arc<Mutex<HashMap<i32, (usize, Py<PyAny>)>>>,
    pending_reads_notify: Arc<tokio::sync::Notify>,
    pending_writes: Arc<Mutex<HashMap<i32, (Vec<u8>, Py<PyAny>)>>>,
    pending_writes_notify: Arc<tokio::sync::Notify>,
}

impl TEventLoop {
    pub(crate) fn log_exception(&self, py: Python, ctx: LogExc) -> PyResult<Py<PyAny>> {
        let handler = self.exc_handler.read().unwrap();
        handler.call1(
            py,
            (
                log_exc_to_py_ctx(py, ctx),
                self.exception_handler.read().unwrap().clone_ref(py),
            ),
        )
    }

    pub fn schedule0(&self, callback: Py<PyAny>, context: Option<Py<PyAny>>) -> Result<()> {
        let handle = Python::attach(|py| {
            Py::new(py, TCBHandle::new0(
                callback,
                context.unwrap_or_else(|| self._base_ctx.clone_ref(py)),
            ))
        })?;

        self.schedule_handle(handle, None)?;
        Ok(())
    }

    pub fn schedule1(&self, callback: Py<PyAny>, arg: Py<PyAny>, context: Option<Py<PyAny>>) -> Result<()> {
        let handle = Python::attach(|py| {
            Py::new(py, TCBHandle::new1(
                callback,
                arg,
                context.unwrap_or_else(|| self._base_ctx.clone_ref(py)),
            ))
        })?;

        self.schedule_handle(handle, None)?;
        Ok(())
    }

    pub fn schedule(&self, callback: Py<PyAny>, args: Py<PyAny>, context: Option<Py<PyAny>>) -> Result<()> {
        let handle = Python::attach(|py| {
            Py::new(py, TCBHandle::new(
                callback,
                args,
                context.unwrap_or_else(|| self._base_ctx.clone_ref(py)),
            ))
        })?;

        self.schedule_handle(handle, None)?;
        Ok(())
    }

    pub fn schedule_later0(&self, delay: Duration, callback: Py<PyAny>, context: Option<Py<PyAny>>) -> Result<()> {
        let handle = Python::attach(|py| {
            Py::new(py, TCBHandle::new0(
                callback,
                context.unwrap_or_else(|| self._base_ctx.clone_ref(py)),
            ))
        })?;

        self.schedule_handle(handle, Some(delay))?;
        Ok(())
    }

    pub fn schedule_later1(
        &self,
        delay: Duration,
        callback: Py<PyAny>,
        arg: Py<PyAny>,
        context: Option<Py<PyAny>>,
    ) -> Result<()> {
        let handle = Python::attach(|py| {
            Py::new(py, TCBHandle::new1(
                callback,
                arg,
                context.unwrap_or_else(|| self._base_ctx.clone_ref(py)),
            ))
        })?;

        self.schedule_handle(handle, Some(delay))?;
        Ok(())
    }

    pub fn schedule_later(
        &self,
        delay: Duration,
        callback: Py<PyAny>,
        args: Py<PyAny>,
        context: Option<Py<PyAny>>,
    ) -> Result<()> {
        let handle = Python::attach(|py| {
            Py::new(py, TCBHandle::new(
                callback,
                args,
                context.unwrap_or_else(|| self._base_ctx.clone_ref(py)),
            ))
        })?;

        self.schedule_handle(handle, Some(delay))?;
        Ok(())
    }

    pub fn schedule_handle(&self, handle: impl THandle + Send + 'static, delay: Option<Duration>) -> Result<()> {
        // Check if loop has stopped before attempting to schedule
        if self.stopping.load(atomic::Ordering::Acquire) || self.closed.load(atomic::Ordering::Acquire) {
            log::debug!("Loop is stopping or closed, ignoring task scheduling");
            return Ok(()); // Silently ignore tasks when loop is stopping
        }

        let task = if let Some(delay) = delay {
            // Calculate absolute time like RLoop
            let when = (Instant::now().duration_since(self.epoch) + delay).as_micros();
            let timer = TokioTimer {
                handle: Box::new(handle),
                when,
            };
            ScheduledTask::Delayed { timer }
        } else {
            // Stage 2a: when an inline sink is installed on this thread (only inside
            // io_processing_loop's data_received window), capture the immediate handle
            // for same-GIL inline execution instead of crossing the scheduler channel.
            let boxed: TBoxedHandle = Box::new(handle);
            match inline_sink_try_push(boxed) {
                Ok(()) => {
                    log::trace!("Immediate task captured by inline sink");
                    return Ok(());
                }
                Err(boxed) => ScheduledTask::Immediate { handle: boxed },
            }
        };

        log::debug!("Scheduling task: {:?}", task);
        if self.scheduler_tx.try_send(task).is_err() {
            log::debug!("Failed to schedule task - channel closed, ignoring");
            return Err(anyhow::anyhow!("Failed to schedule task - loop stopping & channel closed"));
        }
        log::debug!("Task sent successfully");
        Ok(())
    }

    /// Stage 2a: run handles captured by the inline sink in the current GIL
    /// section, using the loop's exception handlers. Mirrors the per-handle
    /// execution `_run` does, so semantics (cancellation check, exception
    /// logging) match. Re-entrancy is safe because `TEventLoop` is `frozen`.
    pub(crate) fn run_handles_inline(&self, py: Python, handles: VecDeque<TBoxedHandle>) {
        if handles.is_empty() {
            return;
        }
        let handlers = LoopHandlers {
            exc_handler: Arc::clone(&self.exc_handler),
            exception_handler: Arc::clone(&self.exception_handler),
        };
        let state = TEventLoopRunState {};
        for handle in handles {
            if !handle.cancelled() {
                handle.run(py, &handlers, &state);
            }
        }
    }

    pub fn get_runtime(&self) -> Arc<Runtime> {
        let runtime = self.runtime.get()
            .expect("Runtime not initialized - call initialize_runtime first")
            .clone();
        runtime
    }

}

#[pymethods]
impl TEventLoop {
    #[new]
    fn new(py: Python) -> PyResult<Self> {
        let (scheduler_tx, scheduler_rx) = async_channel::unbounded::<ScheduledTask>();
        let (signal_socket_tx, signal_socket_rx) = async_channel::unbounded::<u8>();

        Ok(Self {
            runtime: OnceLock::new(),
            scheduler_tx,
            scheduler_rx,
            counter_ready: atomic::AtomicUsize::new(0),
            closed: atomic::AtomicBool::new(false),
            stopping: Arc::new(atomic::AtomicBool::new(false)),
            epoch: Instant::now(),
            exc_handler: Arc::new(RwLock::new(py.None())),
            exception_handler: Arc::new(RwLock::new(py.None())),
            _base_ctx: copy_context(py),
            signal_socket_rx,
            signal_socket_tx,
            sig_listening: Arc::new(atomic::AtomicBool::new(false)),
            sig_handlers: Arc::new(papaya::HashMap::new()),
            io_reader_entries: Arc::new(papaya::HashMap::new()),
            io_writer_entries: Arc::new(papaya::HashMap::new()),
            pending_reads: Arc::new(Mutex::new(HashMap::new())),
            pending_reads_notify: Arc::new(tokio::sync::Notify::new()),
            pending_writes: Arc::new(Mutex::new(HashMap::new())),
            pending_writes_notify: Arc::new(tokio::sync::Notify::new()),
        })
    }

    fn initialize_runtime(&self, py: Python, loop_id: usize) -> PyResult<()> {
        use tokio::runtime::Builder;

        let loop_id_clone = loop_id;

        // Create custom runtime with automatic thread registration
        let runtime = Builder::new_multi_thread()
            .thread_name(format!("tokio-worker-{}", loop_id))
            .on_thread_start(move || {
                // This runs exactly once when each tokio worker thread starts
                Python::attach(|py| -> PyResult<()> {
                    let rloop_mod = py.import("rloop.loop")?;
                    let register_fn = rloop_mod.getattr("_register_tokio_thread")?;
                    register_fn.call1((loop_id_clone,))?;
                    log::trace!("Registered tokio thread with loop_id: {}", loop_id_clone);
                    Ok(())
                }).expect("Failed to register tokio thread");
            })
            .enable_all()
            .build()
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!("Failed to create Tokio runtime: {}", e)))?;

        // Use interior mutability to modify runtime field
        self.runtime.set(Arc::new(runtime))
            .map_err(|_| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(
                "Runtime already initialized"
            ))?;

        Ok(())
    }

    #[getter(_closed)]
    fn _get_closed(&self) -> bool {
        self.closed.load(atomic::Ordering::Acquire)
    }

    #[setter(_closed)]
    fn _set_closed(&self, val: bool) {
        self.closed.store(val, atomic::Ordering::Release);
    }

    #[getter(_stopping)]
    fn _get_stopping(&self) -> bool {
        self.stopping.load(atomic::Ordering::Acquire)
    }

    #[setter(_stopping)]
    fn _set_stopping(&self, val: bool) {
        self.stopping.store(val, atomic::Ordering::Release);
    }

    #[getter(_exc_handler)]
    fn _get_exc_handler(&self, py: Python) -> Py<PyAny> {
        self.exc_handler.read().unwrap().clone_ref(py)
    }

    #[setter(_exc_handler)]
    fn _set_exc_handler(&self, val: Py<PyAny>) {
        let mut guard = self.exc_handler.write().unwrap();
        *guard = val;
    }

    #[getter(_exception_handler)]
    fn _get_exception_handler(&self, py: Python) -> Py<PyAny> {
        self.exception_handler.read().unwrap().clone_ref(py)
    }

    #[setter(_exception_handler)]
    fn _set_exception_handler(&self, val: Py<PyAny>) {
        let mut guard = self.exception_handler.write().unwrap();
        *guard = val;
    }

    fn _run(&self, py: Python) -> PyResult<()> {
        let runtime = self.get_runtime();

        let loop_handlers = LoopHandlers {
            exc_handler: Arc::clone(&self.exc_handler),
            exception_handler: Arc::clone(&self.exception_handler),
        };

        let scheduler_rx = self.scheduler_rx.clone();

        let stopping_clone = Arc::clone(&self.stopping);
        let epoch = self.epoch;
        let signal_socket_rx = self.signal_socket_rx.clone();
        let sig_listening_clone = Arc::clone(&self.sig_listening);
        let pending_reads = Arc::clone(&self.pending_reads);
        let pending_reads_notify = Arc::clone(&self.pending_reads_notify);
        let pending_writes = Arc::clone(&self.pending_writes);
        let pending_writes_notify = Arc::clone(&self.pending_writes_notify);

        py.detach(|| {
            let task_handle: JoinHandle<std::result::Result<(), PyErr>> = runtime.spawn(async move {
                let mut delayed_tasks: BinaryHeap<TokioTimer> = BinaryHeap::new();
                // Ready handles collected each select! iteration, drained under one GIL
                let mut current_handles: VecDeque<TBoxedHandle> = VecDeque::new();
                // Option B: persistent AsyncFd sets + batches of ready results
                let mut read_poll_set = ReadPollSet::new();
                let mut ready_recvs: Vec<(Py<PyAny>, Result<Vec<u8>, std::io::Error>)> =
                    Vec::new();
                let mut write_poll_set = WritePollSet::new();
                let mut ready_writes: Vec<(Py<PyAny>, Result<(), std::io::Error>)> =
                    Vec::new();

                let mut scheduler_rx = scheduler_rx;

                loop {
                    tokio::select! {
                        // Drain all immediately available scheduled tasks in one shot
                        task = scheduler_rx.recv() => {
                            match task {
                                Ok(ScheduledTask::Immediate { handle }) => {
                                    log::trace!("Received: Immediate task");
                                    current_handles.push_back(handle);
                                    // Greedily drain any additional tasks already in the channel
                                    while let Ok(extra) = scheduler_rx.try_recv() {
                                        match extra {
                                            ScheduledTask::Immediate { handle } => current_handles.push_back(handle),
                                            ScheduledTask::Delayed { timer } => delayed_tasks.push(timer),
                                        }
                                    }
                                }
                                Ok(ScheduledTask::Delayed { timer }) => {
                                    log::trace!("Received: Delayed task");
                                    delayed_tasks.push(timer);
                                    while let Ok(extra) = scheduler_rx.try_recv() {
                                        match extra {
                                            ScheduledTask::Immediate { handle } => current_handles.push_back(handle),
                                            ScheduledTask::Delayed { timer } => delayed_tasks.push(timer),
                                        }
                                    }
                                }
                                Err(_) => {
                                    log::debug!("Scheduler channel closed");
                                    break;
                                }
                            }
                        }

                        // Timer branch: sleep exactly until the next deadline (no polling)
                        _ = async {
                            let next_us = delayed_tasks.peek().unwrap().when;
                            let elapsed_us = Instant::now().duration_since(epoch).as_micros();
                            if next_us > elapsed_us {
                                let wait_us = (next_us - elapsed_us) as u64;
                                tokio::time::sleep(Duration::from_micros(wait_us)).await;
                            }
                            // Drain all timers that have now expired
                            let now = Instant::now().duration_since(epoch).as_micros();
                            while let Some(timer) = delayed_tasks.peek() {
                                if timer.when <= now {
                                    let timer = delayed_tasks.pop().unwrap();
                                    if !timer.handle.cancelled() {
                                        log::trace!("Delayed task ready to run");
                                        current_handles.push_back(timer.handle);
                                    } else {
                                        log::trace!("Delayed task cancelled, skipping");
                                    }
                                } else {
                                    break;
                                }
                            }
                        }, if !delayed_tasks.is_empty() => {}

                        // Signal socket
                        sig = signal_socket_rx.recv() => {
                            if let Ok(signal_num) = sig {
                                log::trace!("Processing signal: {}", signal_num);
                                match signal_num {
                                    2 | 15 => {
                                        log::info!("Termination signal {} received, stopping event loop", signal_num);
                                        stopping_clone.store(true, atomic::Ordering::Release);
                                    }
                                    _ => {}
                                }
                            } else {
                                log::debug!("Signal channel closed");
                                break;
                            }
                            if !sig_listening_clone.load(atomic::Ordering::Acquire) {
                                log::debug!("Signal listening flag turned down");
                                break;
                            }
                        }

                        // Option B arm 1: drain newly-registered pending reads into the set
                        _ = pending_reads_notify.notified() => {
                            let mut map = pending_reads.lock().unwrap();
                            for (fd, (nbytes, fut)) in map.drain() {
                                read_poll_set.add(fd, nbytes, fut);
                            }
                        }

                        // Option B arm 2: an fd became readable — collect for GIL batch
                        Some(result) = futures::StreamExt::next(&mut read_poll_set),
                            if !read_poll_set.is_empty() =>
                        {
                            ready_recvs.push(result);
                        }

                        // Option B arm 3: drain newly-registered pending writes into the set
                        _ = pending_writes_notify.notified() => {
                            let mut map = pending_writes.lock().unwrap();
                            for (fd, (data, fut)) in map.drain() {
                                if let Some((err_fut, e)) = write_poll_set.add(fd, data, fut) {
                                    ready_writes.push((err_fut, Err(e)));
                                }
                            }
                        }

                        // Option B arm 4: an fd became writable — collect for GIL batch
                        Some(result) = futures::StreamExt::next(&mut write_poll_set),
                            if !write_poll_set.is_empty() =>
                        {
                            ready_writes.push(result);
                        }
                    }

                    // Run ALL ready callbacks + recv/send results under a single GIL acquisition.
                    // Use attach_blocking so GC-triggered runtime drops don't panic.
                    if !current_handles.is_empty() || !ready_recvs.is_empty() || !ready_writes.is_empty() {
                        let handlers = loop_handlers.clone();
                        let state = TEventLoopRunState {};
                        attach_blocking(|py| {
                            while let Some(handle) = current_handles.pop_front() {
                                if !handle.cancelled() {
                                    let _ = handle.run(py, &handlers, &state);
                                }
                                drop(handle);
                            }
                            // recv() was already done in poll_next; just wrap data as PyBytes
                            for (fut, result) in ready_recvs.drain(..) {
                                match result {
                                    Err(e) => {
                                        let _ = fut.call_method1(py, "set_exception",
                                            (PyErr::from(e).into_value(py),));
                                    }
                                    Ok(data) => {
                                        let bytes = pyo3::types::PyBytes::new(py, &data);
                                        let _ = fut.call_method1(py, "set_result", (bytes,));
                                    }
                                }
                            }
                            // sendall() was completed in poll_next; set result or exception
                            for (fut, result) in ready_writes.drain(..) {
                                match result {
                                    Err(e) => {
                                        let _ = fut.call_method1(py, "set_exception",
                                            (PyErr::from(e).into_value(py),));
                                    }
                                    Ok(()) => {
                                        let _ = fut.call_method1(py, "set_result", (py.None(),));
                                    }
                                }
                            }
                        });
                    }

                    if stopping_clone.load(atomic::Ordering::Acquire) {
                        // Drain teardown: wait for pending callbacks (e.g. connection_lost
                        // from io_processing_loop) using a proper recv() await so the Tokio
                        // I/O driver can deliver pending epoll events. 1 ms timeout is a
                        // safety net; in practice callbacks arrive within microseconds.
                        let teardown_timeout = tokio::time::sleep(Duration::from_millis(1));
                        tokio::pin!(teardown_timeout);
                        loop {
                            tokio::select! {
                                task = scheduler_rx.recv() => {
                                    match task {
                                        Ok(ScheduledTask::Immediate { handle }) => {
                                            current_handles.push_back(handle);
                                        }
                                        Ok(ScheduledTask::Delayed { .. }) => {}
                                        Err(_) => break,
                                    }
                                    // Greedy drain
                                    while let Ok(ScheduledTask::Immediate { handle }) =
                                        scheduler_rx.try_recv()
                                    {
                                        current_handles.push_back(handle);
                                    }
                                    if !current_handles.is_empty() {
                                        let handlers = loop_handlers.clone();
                                        let state = TEventLoopRunState {};
                                        attach_blocking(|py| {
                                            while let Some(handle) = current_handles.pop_front() {
                                                if !handle.cancelled() {
                                                    let _ = handle.run(py, &handlers, &state);
                                                }
                                                drop(handle);
                                            }
                                        });
                                    }
                                }
                                _ = &mut teardown_timeout => { break; }
                            }
                        }
                        break;
                    }
                }

                Ok(())
            });

            let result = match runtime.block_on(task_handle) {
                Ok(Ok(())) => {
                    log::info!("Tokio event loop completed successfully");
                    Ok(())
                }
                Ok(Err(e)) => {
                    log::error!("Tokio event loop task failed with PyErr: {:?}", e);
                    Err(e)
                }
                Err(e) => {
                    log::error!("Tokio event loop task failed with JoinError: {:?}", e);
                    Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(
                        format!("Tokio event loop failed: {:?}", e)
                    ))
                }
            };

            result
        })
    }

    #[pyo3(signature = (callback, *args, context=None))]
    fn call_soon(&self, py: Python, callback: Py<PyAny>, args: Py<PyAny>, context: Option<Py<PyAny>>) -> PyResult<Py<PyAny>> {
        // Stage 1: the multi-arg `TCBHandle::run()` calls the callback directly and
        // never enters `context` (only the 0/1-arg variants do), so the per-call_soon
        // `copy_context()` — a `PyContext_CopyCurrent` FFI plus a Python object
        // alloc/free — is dead work on this hot path. Skip it when no context is given.
        let context = context.unwrap_or_else(|| py.None());

        // Always use TCBHandle like the regular event loop
        let handle = TCBHandle::new(callback, args, context);
        let handle_obj = Py::new(py, handle)?;
        let handle_any = handle_obj.clone_ref(py).into_py_any(py)?;
        self.schedule_handle(handle_obj, None)?;
        Ok(handle_any)
    }

    #[pyo3(signature = (callback, *args, context=None))]
    fn call_soon_threadsafe(
        &self,
        py: Python,
        callback: Py<PyAny>,
        args: Py<PyAny>,
        context: Option<Py<PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        // Stage 1: see call_soon — TCBHandle::run() ignores `context`, so the
        // default copy_context() is dead work; skip it when none is supplied.
        let context = context.unwrap_or_else(|| py.None());

        // Always use TCBHandle like the regular event loop
        let handle = TCBHandle::new(callback, args, context);
        let handle_obj = Py::new(py, handle)?;
        let handle_any = handle_obj.clone_ref(py).into_py_any(py)?;
        self.schedule_handle(handle_obj, None)?;
        Ok(handle_any)
    }

    fn _call_later(
        &self,
        py: Python,
        delay: u64,
        callback: Py<PyAny>,
        args: Py<PyAny>,
        context: Py<PyAny>,
    ) -> PyResult<TTimerHandle> {
        let when = Instant::now().duration_since(self.epoch).as_micros() + u128::from(delay);

        // Create the handle
        let handle = Py::new(py, TCBHandle::new(callback, args, context))?;

        // Use schedule_handle instead of manually creating ScheduledTask
        self.schedule_handle(handle.clone_ref(py), Some(Duration::from_micros(delay)))?;

        Ok(TTimerHandle::new(handle, when))
    }

    fn _stop(&self) -> PyResult<()> {
        self.stopping.store(true, atomic::Ordering::Release);
        Ok(())
    }

    #[pyo3(signature = (fd, callback, *args, context=None))]
    fn add_reader(
        &self,
        py: Python,
        fd: usize,
        callback: Py<PyAny>,
        args: Py<PyAny>,
        context: Option<Py<PyAny>>,
    ) -> PyResult<crate::tokio_handles::TCBHandle> {
        log::debug!("TokioEventLoop::add_reader called for fd: {}", fd);

        // Cancel any existing reader watcher for this fd and clear its Python objects
        // (we hold the GIL here, so this is safe)
        if let Some((old_token, old_entry)) = self.io_reader_entries.pin().remove(&fd) {
            old_token.cancel();
            if let Ok(mut guard) = old_entry.lock() {
                guard.take();
            }
        }

        let context = context.unwrap_or_else(|| copy_context(py));

        // Wrap Python objects in a shared entry so the async task never holds Py<T> directly.
        // The task captures the Arc; clear() is called here (with GIL) via remove_reader().
        let entry: PyCallbackEntry = Arc::new(Mutex::new(Some((
            callback.clone_ref(py),
            args.clone_ref(py),
            context.clone_ref(py),
        ))));
        let entry_task = Arc::clone(&entry);

        let token = CancellationToken::new();
        self.io_reader_entries.pin().insert(fd, (token.clone(), entry));

        let scheduler_tx = self.scheduler_tx.clone();
        let runtime = self.get_runtime();

        // Semaphore(1): ensures at most one callback is in-flight per fd.
        // The permit is held by PermitHandle and released when the handle is dropped
        // (after the event loop runs or cancels it), preventing duplicate callbacks.
        let sem = Arc::new(tokio::sync::Semaphore::new(1));

        runtime.spawn(async move {
            let fd_dup = unsafe { libc::dup(fd as i32) };
            if fd_dup < 0 {
                log::error!("add_reader: dup({}) failed", fd);
                return;
            }
            let owned = unsafe { OwnedFd::from_raw_fd(fd_dup) };
            let async_fd = match tokio::io::unix::AsyncFd::new(owned) {
                Ok(a) => a,
                Err(e) => {
                    log::error!("add_reader: AsyncFd::new failed for fd {}: {}", fd, e);
                    return;
                }
            };

            loop {
                tokio::select! {
                    _ = token.cancelled() => {
                        log::trace!("add_reader watcher cancelled for fd {}", fd);
                        break;
                    }
                    result = async_fd.readable() => {
                        match result {
                            Ok(mut guard) => {
                                guard.clear_ready();
                                // Acquire permit: blocks until the previous callback was consumed
                                let permit = match Arc::clone(&sem).acquire_owned().await {
                                    Ok(p) => p,
                                    Err(_) => break,
                                };
                                let handle = Python::attach(|py| {
                                    let locked = entry_task.lock().map_err(|_| {
                                        PyErr::new::<pyo3::exceptions::PyRuntimeError, _>("entry_task lock poisoned")
                                    })?;
                                    let (cb, ag, cx) = locked.as_ref().ok_or_else(|| {
                                        PyErr::new::<pyo3::exceptions::PyRuntimeError, _>("reader entry removed")
                                    })?;
                                    Py::new(py, TCBHandle::new(
                                        cb.clone_ref(py),
                                        ag.clone_ref(py),
                                        cx.clone_ref(py),
                                    ))
                                });
                                match handle {
                                    Ok(h) => {
                                        let wrapped = Box::new(PermitHandle {
                                            inner: Box::new(h),
                                            _permit: permit,
                                        });
                                        if scheduler_tx.try_send(ScheduledTask::Immediate { handle: wrapped }).is_err() {
                                            log::debug!("add_reader: scheduler closed for fd {}", fd);
                                            break;
                                        }
                                    }
                                    Err(e) => {
                                        log::debug!("add_reader: entry removed or error for fd {}: {:?}", fd, e);
                                        drop(permit);
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                log::error!("add_reader: AsyncFd error for fd {}: {}", fd, e);
                                break;
                            }
                        }
                    }
                }
            }
            // entry_task (Arc) dropped here. If Option is None (cleared by remove_reader),
            // no Py<T> is freed. If Some, the map still holds the other Arc reference,
            // so Py<T> survives until TEventLoop is GC'd with the GIL held.
        });

        Ok(TCBHandle::new(callback, args, context))
    }

    #[pyo3(signature = (fd, callback, *args, context=None))]
    fn add_writer(
        &self,
        py: Python,
        fd: usize,
        callback: Py<PyAny>,
        args: Py<PyAny>,
        context: Option<Py<PyAny>>,
    ) -> PyResult<crate::tokio_handles::TCBHandle> {
        log::debug!("TokioEventLoop::add_writer called for fd: {}", fd);

        if let Some((old_token, old_entry)) = self.io_writer_entries.pin().remove(&fd) {
            old_token.cancel();
            if let Ok(mut guard) = old_entry.lock() {
                guard.take();
            }
        }

        let context = context.unwrap_or_else(|| copy_context(py));

        let entry: PyCallbackEntry = Arc::new(Mutex::new(Some((
            callback.clone_ref(py),
            args.clone_ref(py),
            context.clone_ref(py),
        ))));
        let entry_task = Arc::clone(&entry);

        let token = CancellationToken::new();
        self.io_writer_entries.pin().insert(fd, (token.clone(), entry));

        let scheduler_tx = self.scheduler_tx.clone();
        let runtime = self.get_runtime();

        let sem = Arc::new(tokio::sync::Semaphore::new(1));

        runtime.spawn(async move {
            let fd_dup = unsafe { libc::dup(fd as i32) };
            if fd_dup < 0 {
                log::error!("add_writer: dup({}) failed", fd);
                return;
            }
            let owned = unsafe { OwnedFd::from_raw_fd(fd_dup) };
            let async_fd = match tokio::io::unix::AsyncFd::new(owned) {
                Ok(a) => a,
                Err(e) => {
                    log::error!("add_writer: AsyncFd::new failed for fd {}: {}", fd, e);
                    return;
                }
            };

            loop {
                tokio::select! {
                    _ = token.cancelled() => {
                        log::trace!("add_writer watcher cancelled for fd {}", fd);
                        break;
                    }
                    result = async_fd.writable() => {
                        match result {
                            Ok(mut guard) => {
                                guard.clear_ready();
                                let permit = match Arc::clone(&sem).acquire_owned().await {
                                    Ok(p) => p,
                                    Err(_) => break,
                                };
                                let handle = Python::attach(|py| {
                                    let locked = entry_task.lock().map_err(|_| {
                                        PyErr::new::<pyo3::exceptions::PyRuntimeError, _>("entry_task lock poisoned")
                                    })?;
                                    let (cb, ag, cx) = locked.as_ref().ok_or_else(|| {
                                        PyErr::new::<pyo3::exceptions::PyRuntimeError, _>("writer entry removed")
                                    })?;
                                    Py::new(py, TCBHandle::new(
                                        cb.clone_ref(py),
                                        ag.clone_ref(py),
                                        cx.clone_ref(py),
                                    ))
                                });
                                match handle {
                                    Ok(h) => {
                                        let wrapped = Box::new(PermitHandle {
                                            inner: Box::new(h),
                                            _permit: permit,
                                        });
                                        if scheduler_tx.try_send(ScheduledTask::Immediate { handle: wrapped }).is_err() {
                                            log::debug!("add_writer: scheduler closed for fd {}", fd);
                                            break;
                                        }
                                    }
                                    Err(e) => {
                                        log::debug!("add_writer: entry removed or error for fd {}: {:?}", fd, e);
                                        drop(permit);
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                log::error!("add_writer: AsyncFd error for fd {}: {}", fd, e);
                                break;
                            }
                        }
                    }
                }
            }
        });

        Ok(TCBHandle::new(callback, args, context))
    }

    fn remove_reader(&self, _py: Python, fd: usize) -> bool {
        log::debug!("TokioEventLoop::remove_reader called for fd: {}", fd);
        if let Some((token, entry)) = self.io_reader_entries.pin().remove(&fd) {
            token.cancel();
            // Clear Py<T> objects with the GIL held (we're in a pymethods fn)
            if let Ok(mut guard) = entry.lock() {
                guard.take();
            }
            true
        } else {
            false
        }
    }

    fn remove_writer(&self, _py: Python, fd: usize) -> bool {
        log::debug!("TokioEventLoop::remove_writer called for fd: {}", fd);
        if let Some((token, entry)) = self.io_writer_entries.pin().remove(&fd) {
            token.cancel();
            if let Ok(mut guard) = entry.lock() {
                guard.take();
            }
            true
        } else {
            false
        }
    }

    fn _tcp_conn(
        pyself: Py<Self>,
        py: Python,
        sock: (i32, i32),
        protocol_factory: Py<PyAny>,
        ssl_context: Option<Py<PyAny>>,
        server_hostname: Option<String>,
    ) -> PyResult<(Py<crate::tokio_tcp::TokioTCPTransport>, Py<PyAny>)> {
        log::debug!("TokioEventLoop::_tcp_conn called");

        // Validate no SSL support for now
        if ssl_context.is_some() {
            return Err(PyErr::new::<pyo3::exceptions::PyNotImplementedError, _>(
                "SSL/TLS not yet supported in TokioLoop"
            ));
        }

        // Create protocol instance
        let protocol = protocol_factory.call0(py)?;

        // Create TokioTCPTransport
        let transport = crate::tokio_tcp::TokioTCPTransport::from_py(
            py,
            &pyself,
            sock, //(fd, family),
            protocol_factory,
        )?;

        // Convert to Py<TokioTCPTransport> for the attach method
        let transport_py = Py::new(py, transport)?;

        // Attach protocol to transport
        let _ = crate::tokio_tcp::TokioTCPTransport::attach(&transport_py, py)?;

        // Return transport and protocol
        Ok((transport_py, protocol))
    }

    fn _tcp_server(
        pyself: Py<Self>,
        py: Python,
        socks: Py<PyAny>,
        rsocks: Vec<(i32, i32)>,
        protocol_factory: Py<PyAny>,
        backlog: i32,
    ) -> PyResult<Py<crate::server::TokioServer>> {
        log::debug!("TokioEventLoop::_tcp_server called with {} sockets", rsocks.len());

        // Create tokio-based TCP servers
        let mut servers = Vec::new();

        for (fd, family) in rsocks {
            let server = crate::tokio_tcp::TokioTCPServer::from_fd(
                fd,
                family,
                backlog,
                protocol_factory.clone_ref(py),
                pyself.clone_ref(py),
            )?;

            // Start listening for connections
            TokioTCPServer::start_listening(server.clone(), py)?;
            servers.push(server);
        }

        // Create TokioServer with the TCP servers
        // The TokioTCPServer.from_fd() already creates proper socket objects
        // so we can use the original socks parameter as-is
        let tokio_server = crate::server::TokioServer::tcp(pyself.clone_ref(py), socks, servers);

        Py::new(py, tokio_server)
    }

    fn _tcp_server_ssl(
        pyself: Py<Self>,
        py: Python,
        socks: Py<PyAny>,
        rsocks: Vec<(i32, i32)>,
        protocol_factory: Py<PyAny>,
        backlog: i32,
        ssl_context: Py<PyAny>,
    ) -> PyResult<Py<crate::server::TokioServer>> {
        // TODO: Implement tokio-based TCP server with SSL
        // This will use tokio::net::TcpListener and tokio-rustls
        log::debug!("TokioEventLoop::_tcp_server_ssl called - not yet implemented");

        // For now, return an error to indicate not implemented
        Err(PyErr::new::<pyo3::exceptions::PyNotImplementedError, _>("TokioEventLoop::_tcp_server_ssl not yet implemented"))
    }

    fn _tcp_stream_bound(&self, fd: usize) -> bool {
        log::debug!("TokioEventLoop::_tcp_stream_bound called for fd: {}", fd);
        self.io_reader_entries.pin().contains_key(&fd)
            || self.io_writer_entries.pin().contains_key(&fd)
    }

    fn _udp_conn(
        pyself: Py<Self>,
        py: Python,
        sock: (i32, i32),
        protocol_factory: Py<PyAny>,
        remote_addr: Option<(String, u16)>,
    ) -> PyResult<(Py<crate::udp::UDPTransport>, Py<PyAny>)> {
        // TODO: Implement tokio-based UDP connection
        // This will use tokio::net::UdpSocket
        log::debug!("TokioEventLoop::_udp_conn called - not yet implemented");

        // For now, return an error to indicate not implemented
        Err(PyErr::new::<pyo3::exceptions::PyNotImplementedError, _>("TokioEventLoop::_udp_conn not yet implemented"))
    }

    fn _sig_add(&self, py: Python, sig: u8, callback: Py<PyAny>, args: Py<PyAny>, context: Option<Py<PyAny>>) {
        // TODO: Implement tokio-based signal handling
        // This may need special handling as tokio signal handling is different
        log::debug!("TokioEventLoop::_sig_add called with sig: {} - not yet implemented", sig);
    }

    fn _sig_rem(&self, sig: u8) -> bool {
        // TODO: Implement tokio-based signal removal
        log::debug!("TokioEventLoop::_sig_rem called with sig: {} - not yet implemented", sig);
        false
    }

    fn _ssock_set(&self, fd_r: usize, fd_w: usize) -> PyResult<()> {
        log::debug!("TokioEventLoop::_ssock_set called with fd_r: {}, fd_w: {}", fd_r, fd_w);

        // Spawn a background task to handle signal socket reading
        let runtime = self.get_runtime();
        let signal_socket_tx = self.signal_socket_tx.clone();
        let sig_listening = self.sig_listening.clone();
        let stopping_clone = self.stopping.clone();

        runtime.spawn(async move {
            // Duplicate file descriptors before converting to UnixStream to avoid IO safety violations
            let fd_r_dup = unsafe { libc::dup(fd_r as i32) };
            let fd_w_dup = unsafe { libc::dup(fd_w as i32) };

            if fd_r_dup == -1 {
                log::error!("Failed to duplicate signal socket RX file descriptor");
                return Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(
                    format!("Failed to duplicate signal socket RX file descriptor: {}", std::io::Error::last_os_error())
                ));
            }

            if fd_w_dup == -1 {
                log::error!("Failed to duplicate signal socket TX file descriptor");
                unsafe { libc::close(fd_r_dup); }
                return Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(
                    format!("Failed to duplicate signal socket TX file descriptor: {}", std::io::Error::last_os_error())
                ));
            }

            // Convert duplicated file descriptors to tokio UnixStream inside the runtime
            let std_socket_r = unsafe {
                std::os::unix::net::UnixStream::from_raw_fd(fd_r_dup)
            };
            let std_socket_w = unsafe {
                std::os::unix::net::UnixStream::from_raw_fd(fd_w_dup)
            };

            let tokio_socket_r = match UnixStream::from_std(std_socket_r) {
                Ok(socket) => socket,
                Err(e) => {
                    log::error!("Failed to convert signal socket RX to tokio: {}", e);
                    return Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!("Failed to convert signal socket RX to tokio: {}", e)));
                }
            };
            let tokio_socket_w = match UnixStream::from_std(std_socket_w) {
                Ok(socket) => socket,
                Err(e) => {
                    log::error!("Failed to convert signal socket TX to tokio: {}", e);
                    return Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!("Failed to convert signal socket TX to tokio: {}", e)));
                }
            };

            // Mark that we're listening for signals
            sig_listening.store(true, atomic::Ordering::Release);
            log::debug!("Signal socket setup completed successfully inside runtime");

            // Main signal reading loop
            let mut buf = [0; 1024];
            loop {
                // Wait for the socket to be readable
                if tokio_socket_r.readable().await.is_err() {
                    log::debug!("Signal socket not ready.");
                    tokio::time::sleep(Duration::from_micros(1)).await;
                    continue;
                }

                if !sig_listening.load(atomic::Ordering::Acquire) {
                    log::debug!("Signal listening flag turned down");
                    return Ok(());
                };

                // Try to read from the socket
                let result = tokio_socket_r.try_read(&mut buf);

                match result {
                    Ok(n) if n > 0 => {
                        log::debug!("Received signals: {} signals", n);
                        // Process signals received from Python
                        attach_blocking(|py| {
                            match py.check_signals() {
                                Ok(()) => {
                                    log::debug!("PyO3 signals processed successfully");
                                }
                                Err(e) => {
                                    log::warn!("Signal processing failed: {:?}", e);
                                    // Check if this is a critical signal that should stop the loop
                                    if e.is_instance_of::<pyo3::exceptions::PyKeyboardInterrupt>(py) ||
                                       e.is_instance_of::<pyo3::exceptions::PySystemExit>(py) {
                                                log::info!("Critical signal received, stopping event loop");
                                                stopping_clone.store(true, atomic::Ordering::Release);
                                            }
                                        }
                                }
                            });

                        // Trace individual signals from the buffer
                        for i in 0..n {
                            let signal_num = buf[i];
                            log::trace!("Received signal: {}", signal_num);

                            let is_sent = signal_socket_tx.send(signal_num).await;
                            if is_sent.is_err() {
                                log::debug!("Signal channel closed, stopping signal reading task");
                                return Ok(());
                            };
                        }
                    }
                    Ok(_) => {
                        log::debug!("Signal socket closed, stopping signal reading task");
                        return Ok(());
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        log::trace!("Signal socket WouldBlock");
                        continue;
                    }
                    Err(e) => {
                        log::trace!("Signal socket read error: {:?}", e);
                        continue;
                    }
                }
            }
        });
        Ok(())
    }

    fn _ssock_del(&self, fd_r: usize) -> PyResult<()> {
        log::debug!("TokioEventLoop::_ssock_del called with fd_r: {}", fd_r);

        // Mark that we're no longer listening for signals
        self.sig_listening.store(false, atomic::Ordering::Release);

        log::debug!("Signal socket cleanup completed successfully");
        Ok(())
    }

    // ── Native sock_* ops ──────────────────────────────────────────────────────
    // These bypass the Python add_reader/remove_reader machinery entirely.
    // Each uses a fast path (MSG_DONTWAIT with GIL) and, on EAGAIN, a persistent
    // per-fd worker task that holds a live AsyncFd (epoll stays registered between
    // calls — no dup/epoll_ctl/spawn overhead per request).

    fn _sock_recv_native(
        &self,
        py: Python,
        sock: Py<PyAny>,
        nbytes: usize,
        fut: Py<PyAny>,
    ) -> PyResult<()> {
        let fd = sock.call_method0(py, "fileno")?.extract::<i32>(py)?;

        // Fast path: attempt recv immediately while we hold the GIL.
        // Stack-allocate for small messages (≤ 4 KB) to avoid heap overhead.
        // For large requests, use FIONREAD first to avoid allocating a huge buffer
        // that gets freed immediately on EAGAIN.
        const STACK_CAP: usize = 4096;
        if nbytes <= STACK_CAP {
            let mut buf = [0u8; STACK_CAP];
            let n = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, nbytes, libc::MSG_DONTWAIT) };
            if n > 0 {
                fut.call_method1(py, "set_result", (pyo3::types::PyBytes::new(py, &buf[..n as usize]),))?;
                return Ok(());
            }
            if n == 0 {
                fut.call_method1(py, "set_result", (pyo3::types::PyBytes::new(py, b""),))?;
                return Ok(());
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::WouldBlock {
                fut.call_method1(py, "set_exception", (PyErr::from(e).into_value(py),))?;
                return Ok(());
            }
            // EAGAIN — fall through to EAGAIN path below
        } else {
            // Large buffer: check FIONREAD first to skip allocation when nothing ready
            let mut avail: libc::c_int = 0;
            unsafe { libc::ioctl(fd, libc::FIONREAD, &mut avail) };
            if avail > 0 {
                let alloc_size = (avail as usize).min(nbytes);
                let mut buf = vec![0u8; alloc_size];
                let n = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, alloc_size, libc::MSG_DONTWAIT) };
                if n > 0 {
                    fut.call_method1(py, "set_result", (pyo3::types::PyBytes::new(py, &buf[..n as usize]),))?;
                    return Ok(());
                }
                if n == 0 {
                    fut.call_method1(py, "set_result", (pyo3::types::PyBytes::new(py, b""),))?;
                    return Ok(());
                }
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::WouldBlock {
                    fut.call_method1(py, "set_exception", (PyErr::from(e).into_value(py),))?;
                    return Ok(());
                }
                // EAGAIN despite FIONREAD — fall through to EAGAIN path below
            }
            // FIONREAD == 0: definitely EAGAIN, go straight to EAGAIN path below
        }

        // EAGAIN: hand off to _run's ReadPollSet (Option B).
        // BorrowedFd keeps AsyncFd alive per-connection — no dup() needed.
        {
            let mut map = self.pending_reads.lock().unwrap();
            map.insert(fd, (nbytes, fut));
        }
        self.pending_reads_notify.notify_one();
        Ok(())
    }

    fn _sock_sendall_native(
        &self,
        py: Python,
        sock: Py<PyAny>,
        data: Py<PyAny>,
        fut: Py<PyAny>,
    ) -> PyResult<()> {
        let fd = sock.call_method0(py, "fileno")?.extract::<i32>(py)?;
        let bytes: &[u8] = data.bind(py).extract()?;
        if bytes.is_empty() {
            fut.call_method1(py, "set_result", (py.None(),))?;
            return Ok(());
        }

        // Fast path: send as much as possible without blocking.
        let n = unsafe {
            libc::send(
                fd,
                bytes.as_ptr() as *const libc::c_void,
                bytes.len(),
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::WouldBlock {
                return Err(PyErr::from(e));
            }
            // Nothing sent — hand entire buffer to _run's WritePollSet (Option B).
            {
                let mut map = self.pending_writes.lock().unwrap();
                map.insert(fd, (bytes.to_vec(), fut));
            }
            self.pending_writes_notify.notify_one();
            return Ok(());
        }
        let sent = n as usize;
        if sent == bytes.len() {
            fut.call_method1(py, "set_result", (py.None(),))?;
            return Ok(());
        }
        // Partial send — hand remaining bytes to _run's WritePollSet.
        {
            let mut map = self.pending_writes.lock().unwrap();
            map.insert(fd, (bytes[sent..].to_vec(), fut));
        }
        self.pending_writes_notify.notify_one();
        Ok(())
    }

    fn _sock_accept_native(
        &self,
        py: Python,
        sock: Py<PyAny>,
        fut: Py<PyAny>,
    ) -> PyResult<()> {
        let fd = sock.call_method0(py, "fileno")?.extract::<i32>(py)?;

        // Fast path: try accept() immediately.
        match sock.call_method0(py, "accept") {
            Ok(res) => {
                if let Ok(conn) = res.bind(py).get_item(0) {
                    let _ = conn.call_method1("setblocking", (false,));
                }
                fut.call_method1(py, "set_result", (res,))?;
                return Ok(());
            }
            Err(ref e) if e.is_instance_of::<pyo3::exceptions::PyBlockingIOError>(py) => {}
            Err(e) => return Err(e),
        }

        // Slow path: one-shot dup+AsyncFd (accept is called rarely — once per
        // connection — so the persistent-worker overhead is not worth it here).
        let scheduler_tx = self.scheduler_tx.clone();
        let runtime = self.get_runtime();
        runtime.spawn(async move {
            let accept_result: Result<Py<PyAny>, std::io::Error> = 'outer: {
                let fd_dup = unsafe { libc::dup(fd) };
                if fd_dup < 0 { break 'outer Err(std::io::Error::last_os_error()); }
                let owned = unsafe { OwnedFd::from_raw_fd(fd_dup) };
                match tokio::io::unix::AsyncFd::new(owned) {
                    Err(e) => break 'outer Err(e),
                    Ok(async_fd) => match async_fd.readable().await {
                        Err(e) => break 'outer Err(e),
                        Ok(mut guard) => {
                            guard.clear_ready();
                            drop(guard);
                            drop(async_fd);
                            let _ = scheduler_tx.try_send(ScheduledTask::Immediate {
                                handle: Box::new(RustCallHandle::new(move |py| {
                                    let result: PyResult<Py<PyAny>> = (|| {
                                        let res = sock.call_method0(py, "accept")?;
                                        if let Ok(conn) = res.bind(py).get_item(0) {
                                            let _ = conn.call_method1("setblocking", (false,));
                                        }
                                        Ok(res)
                                    })();
                                    drop(sock);
                                    match result {
                                        Ok(res) => { let _ = fut.call_method1(py, "set_result", (res,)); }
                                        Err(e) => { let _ = fut.call_method1(py, "set_exception", (e.into_value(py),)); }
                                    }
                                })),
                            });
                            return;
                        }
                    },
                }
            };
            // Error before scheduling (dup or AsyncFd::new failed)
            let e = match accept_result { Err(e) => e, Ok(_) => return };
            let _ = scheduler_tx.try_send(ScheduledTask::Immediate {
                handle: Box::new(RustCallHandle::new(move |py| {
                    drop(sock);
                    let _ = fut.call_method1(py, "set_exception", (PyErr::from(e).into_value(py),));
                })),
            });
        });
        Ok(())
    }

    fn _signals_clear(&self) {
        // TODO: Implement tokio-based signal clearing
        // For now, just log to call to make interface work
        log::debug!("TokioEventLoop::_signals_clear called - not yet implemented");
    }

    fn _sig_clear(&self) {
        // TODO: Implement tokio-based signal clearing
        // For now, just log to call to make interface work
        log::debug!("TokioEventLoop::_sig_clear called - not yet implemented");
    }
}

pub(crate) fn init_pymodule(module: &Bound<PyModule>) -> PyResult<()> {
    module.add_class::<TEventLoop>()?;
    Ok(())
}


