use std::{
    collections::{BinaryHeap, VecDeque},
    os::fd::{FromRawFd, OwnedFd},
    sync::{atomic, Arc, Mutex, OnceLock, RwLock},
    time::{Duration, Instant},
};

use anyhow::Result;
use pyo3::prelude::*;
use std::sync::atomic::AtomicBool;
use tokio::{runtime::Runtime, task::JoinHandle, net::UnixStream};
use tokio_util::sync::CancellationToken;

use crate::{
    tokio_handles::{TCBHandle, TTimerHandle, TBoxedHandle, THandle, PermitHandle},
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

enum ScheduledTask {
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

#[pyclass(frozen, subclass, module = "rloop._rloop")]
pub struct TEventLoop {
    runtime: OnceLock<Arc<Runtime>>,
    scheduler_tx: async_channel::Sender<ScheduledTask>,
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
            ScheduledTask::Immediate {
                handle: Box::new(handle)
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

        py.detach(|| {
            let task_handle: JoinHandle<std::result::Result<(), PyErr>> = runtime.spawn(async move {
                let mut delayed_tasks: BinaryHeap<TokioTimer> = BinaryHeap::new();
                // Ready handles collected each select! iteration, drained under one GIL
                let mut current_handles: VecDeque<TBoxedHandle> = VecDeque::new();

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
                    }

                    // Run ALL ready callbacks under a single GIL acquisition.
                    // Use attach_blocking so GC-triggered runtime drops don't panic.
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

                    if stopping_clone.load(atomic::Ordering::Acquire) {
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
        let context = context.unwrap_or_else(|| copy_context(py));

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
        let context = context.unwrap_or_else(|| copy_context(py));

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
