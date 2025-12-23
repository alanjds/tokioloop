use std::{
    collections::{BinaryHeap, VecDeque},
    sync::{atomic, Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use anyhow::Result;
use pyo3::prelude::*;
use mio::{Interest, Poll, Token, Waker, event, net::TcpListener};
use tokio::{runtime::Runtime, sync::mpsc, task::JoinHandle};

use crate::{
    tokio_handles::{TCBHandle, TTimerHandle, TBoxedHandle, THandle},
    py::copy_context,
    log::{LogExc, log_exc_to_py_ctx},
};

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
    Shutdown,
}

impl std::fmt::Debug for ScheduledTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScheduledTask::Immediate { .. } => write!(f, "ScheduledTask::Immediate"),
            ScheduledTask::Delayed { timer } => write!(f, "ScheduledTask::Delayed {{ when: {} }}", timer.when),
            ScheduledTask::Shutdown => write!(f, "ScheduledTask::Shutdown"),
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
    runtime: Arc<Runtime>,
    scheduler_tx: mpsc::UnboundedSender<ScheduledTask>,
    scheduler_rx: Mutex<Option<mpsc::UnboundedReceiver<ScheduledTask>>>,
    handles_ready: Mutex<VecDeque<TBoxedHandle>>,
    counter_ready: atomic::AtomicUsize,
    closed: atomic::AtomicBool,
    stopping: atomic::AtomicBool,
    epoch: Instant,
    exc_handler: Arc<RwLock<Py<PyAny>>>,
    exception_handler: Arc<RwLock<Py<PyAny>>>,
    #[pyo3(get)]
    _base_ctx: Py<PyAny>,
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
        match self.scheduler_tx.send(task) {
            Ok(()) => {
                log::debug!("Task sent successfully");
            }
            Err(_) => {
                log::error!("Failed to schedule task - channel closed");
                return Err(anyhow::anyhow!("Failed to schedule task - channel closed"));
            }
        }
        Ok(())
    }
}

#[pymethods]
impl TEventLoop {
    #[new]
    fn new(py: Python) -> PyResult<Self> {
        let runtime = Runtime::new()
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!("Failed to create Tokio runtime: {}", e)))?;

        let (scheduler_tx, scheduler_rx) = mpsc::unbounded_channel::<ScheduledTask>();

        Ok(Self {
            runtime: Arc::new(runtime),
            scheduler_tx,
            scheduler_rx: Mutex::new(Some(scheduler_rx)),
            handles_ready: Mutex::new(VecDeque::with_capacity(128)),
            counter_ready: atomic::AtomicUsize::new(0),
            closed: atomic::AtomicBool::new(false),
            stopping: atomic::AtomicBool::new(false),
            epoch: Instant::now(),
            exc_handler: Arc::new(RwLock::new(py.None())),
            exception_handler: Arc::new(RwLock::new(py.None())),
            _base_ctx: copy_context(py),
        })
    }

    #[getter(_clock)]
    fn _get_clock(&self) -> u128 {
        Instant::now().duration_since(self.epoch).as_micros()
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
        let runtime = self.runtime.clone();

        // Create LoopHandlers for use in the async task
        let loop_handlers = LoopHandlers {
            exc_handler: Arc::clone(&self.exc_handler),
            exception_handler: Arc::clone(&self.exception_handler),
        };

        // Extract the receiver BEFORE the async block
        let mut scheduler_rx = self.scheduler_rx.lock().unwrap().take().unwrap();

        // Keep the same sender - all scheduling uses the same channel
        // This ensures the receiver in _run() is connected to all tasks

        let stopping = Arc::new(atomic::AtomicBool::new(false));
        let stopping_clone = stopping.clone();

        // Main tokio task using proper async pattern
        let task_handle = runtime.spawn(async move {

            let mut immediate_tasks: VecDeque<Box<dyn THandle>> = VecDeque::new();
            let mut delayed_tasks: BinaryHeap<TokioTimer> = BinaryHeap::new();
            let mut current_handles = VecDeque::new();

            loop {
                // Use tokio::select! to handle multiple async sources concurrently
                tokio::select! {
                    // Handle incoming scheduled tasks
                    task = scheduler_rx.recv() => {
                        match task {
                            Some(ScheduledTask::Immediate { handle }) => {
                                current_handles.push_back(handle);
                            }
                            Some(ScheduledTask::Delayed { timer }) => {
                                delayed_tasks.push(timer);
                            }
                            Some(ScheduledTask::Shutdown) => {
                                log::debug!("Received shutdown signal");
                                break;
                            }
                            None => {
                                log::debug!("Scheduler channel closed");
                                break;
                            }
                        }
                    }

                    // Handle timer expiration
                    _ = async {
                        if let Some(next_timer) = delayed_tasks.peek() {
                            let next_time = next_timer.when;
                            let current = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_micros() as u128;
                            if next_time > current {
                                let micros_to_wait = next_time - current;
                                let sleep_duration = if micros_to_wait > 1000u128 { 1000u64 } else { micros_to_wait as u64 };
                                tokio::time::sleep(Duration::from_micros(sleep_duration)).await;
                            } else {
                                tokio::time::sleep(Duration::from_micros(100)).await;
                            }
                        } else {
                            tokio::time::sleep(Duration::from_millis(1)).await;
                        }
                    },

                    if !delayed_tasks.is_empty() => {
                        // Timer sleep completed, check for expired timers
                        let current_time = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_micros() as u128;
                        while let Some(timer) = delayed_tasks.peek() {
                            if timer.when <= current_time {
                                let timer = delayed_tasks.pop().unwrap();
                                if !timer.handle.cancelled() {
                                    current_handles.push_back(timer.handle);
                                }
                            } else {
                                break;
                            }
                        }
                    }

                    // Default case when no timers
                    _ = tokio::time::sleep(Duration::from_millis(1)), if delayed_tasks.is_empty() => {}
                }

                let mut state = TEventLoopRunState{};
                // Process immediate tasks
                while let Some(handle) = current_handles.pop_front() {
                    if !handle.cancelled() {
                        // Clone handlers for this handle execution
                        let handlers = loop_handlers.clone();

                        // Execute Python callback in GIL
                        Python::attach(|py| {
                            // if let Err(e) = std::panic::catch_unwind(|| {
                            //     log::debug!("Executing handle in tokio context");
                            //
                            //     // Execute the handle with proper context
                                   let _ = handle.run(py, &handlers, &mut state);
                            //     log::debug!("Handle execution completed");
                            // }) {
                            //   log::error!("Panic during handle execution: {:?}", e);
                            // }
                        });
                    }
                };

                // Check stop condition
                if stopping_clone.load(atomic::Ordering::Acquire) {
                    break;
                }
            };
        });

        // Block until completion
        match runtime.block_on(task_handle) {
            Ok(_) => {
                log::debug!("Tokio event loop completed successfully");
            }
            Err(e) => {
                log::error!("Tokio event loop task failed: {:?}", e);
                return Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(
                    format!("Tokio event loop failed: {:?}", e)
                ));
            }
        }

        Ok(())
    }

    #[pyo3(signature = (callback, *args, context=None))]
    fn call_soon(&self, py: Python, callback: Py<PyAny>, args: Py<PyAny>, context: Option<Py<PyAny>>) -> PyResult<Py<TCBHandle>> {
        let handle = TCBHandle::new(callback, args, context.unwrap_or_else(|| self._base_ctx.clone_ref(py)));
        let handle_py = Py::new(py, handle)?;

        self.schedule_handle(handle_py.clone_ref(py), None)?;
        Ok(handle_py)
    }

    #[pyo3(signature = (callback, *args, context=None))]
    fn call_soon_threadsafe(
        &self,
        py: Python,
        callback: Py<PyAny>,
        args: Py<PyAny>,
        context: Option<Py<PyAny>>,
    ) -> PyResult<Py<TCBHandle>> {
        let handle = TCBHandle::new(callback, args, context.unwrap_or_else(|| self._base_ctx.clone_ref(py)));
        let handle_py = Py::new(py, handle)?;

        self.schedule_handle(handle_py.clone_ref(py), None)?;
        Ok(handle_py)
    }

    fn _call_later(
        &self,
        py: Python,
        delay: u64,
        callback: Py<PyAny>,
        args: Py<PyAny>,
        context: Py<PyAny>,
    ) -> TTimerHandle {
        let when = Instant::now().duration_since(self.epoch).as_micros() + u128::from(delay);
        let handle = TCBHandle::new(callback, args, context);
        let handle_py = Py::new(py, handle).unwrap();

        let timer = TokioTimer {
            handle: Box::new(handle_py.clone_ref(py)),
            when,
        };

        let task = ScheduledTask::Delayed { timer };
        let _ = self.scheduler_tx.send(task);

        TTimerHandle::new(handle_py, when)
    }

    #[pyo3(signature = (fd, callback, *args, context=None))]
    fn add_reader(
        &self,
        py: Python,
        fd: usize,
        callback: Py<PyAny>,
        args: Py<PyAny>,
        context: Option<Py<PyAny>>,
    ) -> PyResult<Py<TCBHandle>> {
        // TODO: Implement tokio-based add_reader
        // This will use tokio::net::TcpListener/UnixListener for async I/O
        log::debug!("TokioEventLoop::add_reader called - not yet implemented");

        // For now, create a dummy handle
        let handle = TCBHandle::new(callback, args, context.unwrap_or_else(|| copy_context(py)));
        Py::new(py, handle)
    }

    fn remove_reader(&self, py: Python, fd: usize) -> bool {
        // TODO: Implement tokio-based remove_reader
        log::debug!("TokioEventLoop::remove_reader called - not yet implemented");
        false
    }

    #[pyo3(signature = (fd, callback, *args, context=None))]
    fn add_writer(
        &self,
        py: Python,
        fd: usize,
        callback: Py<PyAny>,
        args: Py<PyAny>,
        context: Option<Py<PyAny>>,
    ) -> PyResult<Py<TCBHandle>> {
        // TODO: Implement tokio-based add_writer
        log::debug!("TokioEventLoop::add_writer called - not yet implemented");

        // For now, create a dummy handle
        let handle = TCBHandle::new(callback, args, context.unwrap_or_else(|| copy_context(py)));
        Py::new(py, handle)
    }

    fn remove_writer(&self, py: Python, fd: usize) -> bool {
        // TODO: Implement tokio-based remove_writer
        log::debug!("TokioEventLoop::remove_writer called - not yet implemented");
        false
    }

    fn _tcp_conn(
        pyself: Py<Self>,
        py: Python,
        sock: (i32, i32),
        protocol_factory: Py<PyAny>,
        ssl_context: Option<Py<PyAny>>,
        server_hostname: Option<String>,
    ) -> PyResult<(Py<crate::tcp::TCPTransport>, Py<PyAny>)> {
        // TODO: Implement tokio-based TCP connection
        // This will use tokio::net::TcpStream and tokio-rustls for SSL
        log::debug!("TokioEventLoop::_tcp_conn called - not yet implemented");

        // For now, return an error to indicate not implemented
        Err(PyErr::new::<pyo3::exceptions::PyNotImplementedError, _>("TokioEventLoop::_tcp_conn not yet implemented"))
    }

    fn _tcp_server(
        pyself: Py<Self>,
        py: Python,
        socks: Py<PyAny>,
        rsocks: Vec<(i32, i32)>,
        protocol_factory: Py<PyAny>,
        backlog: i32,
    ) -> PyResult<Py<crate::server::Server>> {
        // TODO: Implement tokio-based TCP server
        // This will use tokio::net::TcpListener
        log::debug!("TokioEventLoop::_tcp_server called - not yet implemented");

        // For now, return an error to indicate not implemented
        Err(PyErr::new::<pyo3::exceptions::PyNotImplementedError, _>("TokioEventLoop::_tcp_server not yet implemented"))
    }

    fn _tcp_server_ssl(
        pyself: Py<Self>,
        py: Python,
        socks: Py<PyAny>,
        rsocks: Vec<(i32, i32)>,
        protocol_factory: Py<PyAny>,
        backlog: i32,
        ssl_context: Py<PyAny>,
    ) -> PyResult<Py<crate::server::Server>> {
        // TODO: Implement tokio-based TCP server with SSL
        // This will use tokio::net::TcpListener and tokio-rustls
        log::debug!("TokioEventLoop::_tcp_server_ssl called - not yet implemented");

        // For now, return an error to indicate not implemented
        Err(PyErr::new::<pyo3::exceptions::PyNotImplementedError, _>("TokioEventLoop::_tcp_server_ssl not yet implemented"))
    }

    fn _tcp_stream_bound(&self, fd: usize) -> bool {
        // TODO: Implement tokio-based TCP stream bound check
        log::debug!("TokioEventLoop::_tcp_stream_bound called - not yet implemented");
        false
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

    fn _sig_add(&self, py: Python, sig: u8, callback: Py<PyAny>, args: Py<PyAny>, context: Py<PyAny>) {
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
        // TODO: Implement tokio-based socket setup
        // For now, just log to call to make interface work
        log::debug!("TokioEventLoop::_ssock_set called with fd_r: {}, fd_w: {} - not yet implemented", fd_r, fd_w);
        Ok(())
    }

    fn _ssock_del(&self, fd_r: usize) -> PyResult<()> {
        // TODO: Implement tokio-based socket cleanup
        // For now, just log to call to make interface work
        log::debug!("TokioEventLoop::_ssock_del called with fd_r: {} - not yet implemented", fd_r);
        Ok(())
    }

    fn _signals_clear(&self) {
        // TODO: Implement tokio-based signal clearing
        // For now, just log to call to make interface work
        log::debug!("TokioEventLoop::_signals_clear called - not yet implemented");
    }
}

pub(crate) fn init_pymodule(module: &Bound<PyModule>) -> PyResult<()> {
    module.add_class::<TEventLoop>()?;
    Ok(())
}
