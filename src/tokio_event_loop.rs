use std::{
    collections::{BinaryHeap, VecDeque},
    sync::{atomic, Arc, Mutex, RwLock},
    os::fd::{FromRawFd, IntoRawFd},
    time::{Duration, Instant},
};

use anyhow::Result;
use pyo3::prelude::*;
use mio::{Interest, Poll, Token, Waker, event, net::TcpListener};
use std::sync::atomic::AtomicBool;
use tokio::{runtime::Runtime, sync::mpsc, task::JoinHandle, net::UnixStream};

use crate::{
    tokio_handles::{TCBHandle, TTimerHandle, TBoxedHandle, THandle},
    py::copy_context,
    log::{LogExc, log_exc_to_py_ctx},
    server::TokioServer,
    tokio_tcp::TokioTCPServer,
};
use pyo3::IntoPyObjectExt;
use socket2::Socket;

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
    pub(crate) runtime: Arc<Runtime>,
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
    // Signal handling fields for Phase 3 implementation
    signal_socket_rx: Arc<Mutex<Option<UnixStream>>>,
    signal_socket_tx: Arc<Mutex<Option<UnixStream>>>,
    sig_listening: Arc<AtomicBool>,
    sig_handlers: Arc<papaya::HashMap<u8, Py<PyAny>>>,
    // Pending signal socket setup (stored as raw FDs to be converted inside runtime)
    pending_signal_sockets: Arc<Mutex<Option<(usize, usize)>>>,
    // I/O handling fields for future tokio integration
    io_callbacks: Arc<papaya::HashMap<usize, (Py<PyAny>, Py<PyAny>, Py<PyAny>)>>,
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

    /// Get a clone of the tokio runtime for spawning tasks
    pub fn runtime(&self) -> Arc<Runtime> {
        Arc::clone(&self.runtime)
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
        match self.scheduler_tx.send(task) {
            Ok(()) => {
                log::debug!("Task sent successfully");
            }
            Err(_) => {
                log::debug!("Failed to schedule task - channel closed, ignoring");
                return Err(anyhow::anyhow!("Failed to schedule task - loop stopping & channel closed"));
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
            // Initialize signal handling fields
            signal_socket_rx: Arc::new(Mutex::new(None)),
            signal_socket_tx: Arc::new(Mutex::new(None)),
            sig_listening: Arc::new(atomic::AtomicBool::new(false)),
            sig_handlers: Arc::new(papaya::HashMap::new()),
            pending_signal_sockets: Arc::new(Mutex::new(None)),
            // I/O handling fields for future tokio integration
            io_callbacks: Arc::new(papaya::HashMap::new()),
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
        let stopping_clone = Arc::clone(&stopping);
        let epoch = self.epoch;

        // Get a copy of the scheduler sender for signal handling
        let scheduler_tx = self.scheduler_tx.clone();

        // Get signal socket references for use in async task
        let signal_socket_rx = Arc::clone(&self.signal_socket_rx);
        let signal_socket_tx = Arc::clone(&self.signal_socket_tx);
        let sig_listening_clone = Arc::clone(&self.sig_listening);
        let pending_signal_sockets = Arc::clone(&self.pending_signal_sockets);

        // Release GIL to allow tokio tasks to acquire it
        py.detach(|| {
            // Main tokio task
            let task_handle: JoinHandle<std::result::Result<(), PyErr>> = runtime.spawn(async move {
                let mut delayed_tasks: BinaryHeap<TokioTimer> = BinaryHeap::new();
                let (current_handles_tx, mut current_handles_rx) = tokio::sync::mpsc::unbounded_channel::<TBoxedHandle>();

                // Check for pending signal socket setup and convert to tokio streams
                {
                    let mut pending_guard = pending_signal_sockets.lock().unwrap();
                    if let Some((fd_r, fd_w)) = pending_guard.take() {
                        log::debug!("Converting pending signal sockets to tokio streams");

                        // Convert raw file descriptors to tokio UnixStream inside the runtime
                        let std_socket_r = unsafe {
                            std::os::unix::net::UnixStream::from_raw_fd(fd_r as i32)
                        };

                        let std_socket_w = unsafe {
                            std::os::unix::net::UnixStream::from_raw_fd(fd_w as i32)
                        };

                        // Convert to tokio UnixStream
                        match UnixStream::from_std(std_socket_r) {
                            Ok(tokio_socket_r) => {
                                match UnixStream::from_std(std_socket_w) {
                                    Ok(tokio_socket_w) => {
                                        // Store the converted sockets
                                        {
                                            let mut rx_guard = signal_socket_rx.lock().unwrap();
                                            *rx_guard = Some(tokio_socket_r);
                                        }
                                        {
                                            let mut tx_guard = signal_socket_tx.lock().unwrap();
                                            *tx_guard = Some(tokio_socket_w);
                                        }

                                        // Mark that we're listening for signals
                                        sig_listening_clone.store(true, atomic::Ordering::Release);
                                        log::debug!("Signal socket setup completed successfully inside runtime");
                                    }
                                    Err(e) => {
                                        log::error!("Failed to convert signal socket TX to tokio: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                log::error!("Failed to convert signal socket RX to tokio: {}", e);
                            }
                        }
                    }
                }

                loop {
                    // Check signals before entering tokio::select!
                    // Python::attach(|py| {
                    //     let _ = py.check_signals();
                    // });

                    tokio::select! {
                        // Handle incoming scheduled tasks
                        task = scheduler_rx.recv() => {
                            match task {
                                Some(ScheduledTask::Immediate { handle }) => {
                                    log::trace!("Received: Immediate task");
                                    let _ = current_handles_tx.send(handle);
                                }
                                Some(ScheduledTask::Delayed { timer }) => {
                                    log::trace!("Received: Delayed task");
                                    delayed_tasks.push(timer);
                                }
                                Some(ScheduledTask::Shutdown) => {
                                    log::debug!("Received: Shutdown signal");
                                    break;
                                }
                                None => {
                                    log::debug!("Received: None. Scheduler channel closed");
                                    break;
                                }
                            }
                        }

                        // Handle timer expiration
                        _ = async {
                            if let Some(next_timer) = delayed_tasks.peek() {
                                let next_time = next_timer.when;
                                let current = Instant::now().duration_since(epoch).as_micros();
                                if next_time > current {
                                    let micros_to_wait = next_time - current;
                                    let sleep_duration = if micros_to_wait > 100u128 { 100u64 } else { micros_to_wait as u64 };
                                    tokio::time::sleep(Duration::from_micros(sleep_duration)).await;
                                } else {
                                    let current_time = Instant::now().duration_since(epoch).as_micros();
                                    // Timer sleep completed, check for expired timers
                                    while let Some(timer) = delayed_tasks.peek() {
                                        if timer.when <= current_time {
                                            log::trace!("Delayed task: selected to run");
                                            let timer = delayed_tasks.pop().unwrap();
                                            if !timer.handle.cancelled() {
                                                let _ = current_handles_tx.send(timer.handle);
                                                log::trace!("Delayed task: sent to run");
                                            } else {
                                                log::trace!("Delayed task: cancelled. Not sent to run");
                                            }
                                        } else {
                                            break;
                                        }
                                    }
                                }
                            } else {
                                tokio::time::sleep(Duration::from_millis(1)).await;
                            }
                        }, if !delayed_tasks.is_empty() => {}

                        // Signal socket reading
                        _ = async {
                            // Take the socket out of the mutex to avoid holding the lock across await
                            let socket_opt = signal_socket_rx.lock().unwrap().take();

                            if let Some(socket) = socket_opt {
                                // Wait for the socket to be readable
                                if socket.readable().await.is_err() {
                                    log::debug!("Signal socket not ready.");
                                    // Put the socket back before returning
                                    let _ = signal_socket_rx.lock().unwrap().insert(socket);
                                    return;
                                }

                                let mut buf = [0; 1024];
                                let result = socket.try_read(&mut buf);

                                // Put the socket back before processing results
                                let _ = signal_socket_rx.lock().unwrap().insert(socket);

                                match result {
                                    Ok(n) if n > 0 => {
                                        log::debug!("Received {} bytes from signal socket", n);
                                        // Process signals received from Python
                                        Python::attach(|py| {
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
                                                        stopping.store(true, atomic::Ordering::Release);
                                                        let _ = scheduler_tx.send(ScheduledTask::Shutdown);
                                                    }
                                                }
                                            }
                                        });
                                        // Trace individual signals from the buffer
                                        for i in 0..n {
                                            let signal_num = buf[i];
                                            let signal_name = match signal_num {
                                                1 => "SIGHUP",
                                                2 => "SIGINT",
                                                3 => "SIGQUIT",
                                                6 => "SIGABRT",
                                                8 => "SIGFPE",
                                                9 => "SIGKILL",
                                                10 => "SIGUSR1",
                                                11 => "SIGSEGV",
                                                12 => "SIGUSR2",
                                                13 => "SIGPIPE",
                                                14 => "SIGALRM",
                                                15 => "SIGTERM",
                                                17 => "SIGCHLD",
                                                18 => "SIGCONT",
                                                19 => "SIGSTOP",
                                                20 => "SIGTSTP",
                                                21 => "SIGTTIN",
                                                22 => "SIGTTOU",
                                                _ => "UNKNOWN",
                                            };
                                            log::debug!("Signal received: {} ({})", signal_num, signal_name);

                                            // Auto-stop on termination signals
                                            match signal_num {
                                                2 | 15 => {  // SIGINT or SIGTERM
                                                    log::info!("Termination signal {} ({}) received, stopping event loop", signal_num, signal_name);
                                                    stopping.store(true, atomic::Ordering::Release);
                                                    // Also send shutdown task to ensure clean termination
                                                    let _ = scheduler_tx.send(ScheduledTask::Shutdown);
                                                }
                                                _ => {
                                                    // Other signals are handled by PyO3 signal processing
                                                }
                                            }
                                        }
                                    }
                                    Ok(_) => {
                                        // No data available, continue
                                    }
                                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                        // Ignore WouldBlock for this socket
                                        // log::trace!("Signal socket WouldBlock");
                                    }
                                    Err(e) => {
                                        log::trace!("Signal socket read error: {:?}", e);
                                    }
                                }
                            }
                        }, if sig_listening_clone.load(atomic::Ordering::Acquire) => {}

                        // Default case when no timers
                        _ = async {
                            tokio::time::sleep(Duration::from_millis(1)).await;
                        }, if delayed_tasks.is_empty() => {}

                        handle = current_handles_rx.recv() => {
                            if let Some(handle) = handle {
                                log::trace!("Task received to run");
                                if !handle.cancelled() {
                                    // Clone handlers for this handle execution
                                    let handlers = loop_handlers.clone();
                                    let mut state = TEventLoopRunState{};

                                    // Execute Python callback in GIL
                                    log::trace!("PyO3: attaching Python to run the task");
                                    Python::attach(|py| {
                                        // if let Err(e) = std::panic::catch_unwind(|| {
                                            log::debug!("Executing handle in tokio context");
                                        //
                                        //     // Execute the handle with proper context
                                            let _ = handle.run(py, &handlers, &state);
                                            log::debug!("Handle execution completed");
                                        // }) {
                                        //   log::error!("Panic during handle execution: {:?}", e);
                                        // }
                                    });
                                }
                            } else {
                                log::debug!("Handle channel closed, breaking from select");
                                break;
                            }
                        }
                    }

                    // Check stop condition
                    if stopping_clone.load(atomic::Ordering::Acquire) {
                        break;
                    }
                };

                Ok(())
            });

            // Block until completion
            match runtime.block_on(task_handle) {
                Ok(Ok(())) => {
                    log::info!("Tokio event loop completed successfully");
                    return Ok(());
                }
                Ok(Err(e)) => {
                    log::error!("Tokio event loop task failed with PyErr: {:?}", e);
                    return Err(e);
                }
                Err(e) => {
                    log::error!("Tokio event loop task failed with JoinError: {:?}", e);
                    return Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(
                        format!("Tokio event loop failed: {:?}", e)
                    ));
                }
            };
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

        /* For timers, always use the multi-argument TCBHandle for simplicity */
        let handle = Py::new(py, TCBHandle::new(callback, args, context))?;

        let timer = TokioTimer {
            handle: Box::new(handle.clone_ref(py)),
            when,
        };

        let task = ScheduledTask::Delayed { timer };
        let _ = self.scheduler_tx.send(task);

        Ok(TTimerHandle::new(handle, when))
    }

    fn _stop(&self) -> PyResult<()> {
        self.stopping.store(true, atomic::Ordering::Release);
        let _ = self.scheduler_tx.send(ScheduledTask::Shutdown);
        Ok(())
    }

    // I/O methods for socket operations - these are critical for TCP functionality
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

        // For now, implement a simple version that schedules the callback
        // In a full implementation, this would register the fd with tokio's interest system
        let context = context.unwrap_or_else(|| copy_context(py));
        let handle = TCBHandle::new(callback.clone_ref(py), args.clone_ref(py), context.clone_ref(py));
        let handle_obj = Py::new(py, handle)?;

        // Store the callback for later execution when fd becomes readable
        {
            let mut callbacks = self.io_callbacks.pin();
            callbacks.insert(fd, (handle_obj.clone_ref(py).into(), py.None(), context.clone_ref(py)));
        }

        // Schedule an immediate task to check readability
        self.schedule_handle(handle_obj.clone_ref(py), None)?;

        // Return a new handle with the same parameters
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

        // For now, implement a simple version that schedules the callback
        // In a full implementation, this would register the fd with tokio's interest system
        let context = context.unwrap_or_else(|| copy_context(py));
        let handle = TCBHandle::new(callback.clone_ref(py), args.clone_ref(py), context.clone_ref(py));
        let handle_obj = Py::new(py, handle)?;

        // Store the callback for later execution when fd becomes writable
        {
            let mut callbacks = self.io_callbacks.pin();
            callbacks.insert(fd, (py.None(), handle_obj.clone_ref(py).into(), context.clone_ref(py)));
        }

        // Schedule an immediate task to check writability
        self.schedule_handle(handle_obj.clone_ref(py), None)?;

        // Return a new handle with the same parameters
        Ok(TCBHandle::new(callback, args, context))
    }

    fn remove_reader(&self, py: Python, fd: usize) -> bool {
        log::debug!("TokioEventLoop::remove_reader called for fd: {}", fd);

        // Remove the reader callback
        let mut callbacks = self.io_callbacks.pin();
        if let Some((reader_cb, writer_cb, ctx)) = callbacks.remove(&fd) {
            // Only return true if there was a reader callback
            !reader_cb.is_none(py)
        } else {
            false
        }
    }

    fn remove_writer(&self, py: Python, fd: usize) -> bool {
        log::debug!("TokioEventLoop::remove_writer called for fd: {}", fd);

        // Remove the writer callback
        let mut callbacks = self.io_callbacks.pin();
        if let Some((reader_cb, writer_cb, ctx)) = callbacks.remove(&fd) {
            // Only return true if there was a writer callback
            !writer_cb.is_none(py)
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

        // Convert socket to tokio TcpStream
        let (fd, family) = sock;
        let std_stream = unsafe { std::net::TcpStream::from_raw_fd(fd) };
        let tokio_stream = tokio::net::TcpStream::from_std(std_stream)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!(
                "Failed to convert socket to tokio stream: {}", e
            )))?;

        // Create protocol instance
        let protocol = protocol_factory.call0(py)?;

        // Create TokioTCPTransport
        let transport = crate::tokio_tcp::TokioTCPTransport::from_py(
            py,
            &pyself,
            (fd, family),
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
            server.start_listening(py)?;

            // Create a TCPServer wrapper for the TokioTCPServer
            let tcp_server = crate::tcp::TCPServer::from_fd(fd, family, backlog, protocol_factory.clone_ref(py));
            servers.push(tcp_server);
        }

        // Create TokioServer with the TCP servers
        let tokio_server = crate::server::TokioServer::tcp(pyself.clone_ref(py), socks.clone_ref(py), servers);

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
        log::debug!("TokioEventLoop::_ssock_set called with fd_r: {}, fd_w: {}", fd_r, fd_w);

        // Store the raw file descriptors for later conversion inside the tokio runtime
        {
            let mut pending = self.pending_signal_sockets.lock().unwrap();
            *pending = Some((fd_r, fd_w));
        }

        log::debug!("Signal socket FDs stored for later conversion");
        Ok(())
    }

    fn _ssock_del(&self, fd_r: usize) -> PyResult<()> {
        log::debug!("TokioEventLoop::_ssock_del called with fd_r: {}", fd_r);

        // Remove and close signal sockets
        {
            let mut rx_guard = self.signal_socket_rx.lock().unwrap();
            if let Some(socket) = rx_guard.take() {
                // Drop the socket to close it
                drop(socket);
                log::debug!("Signal socket RX closed");
            }
        }

        {
            let mut tx_guard = self.signal_socket_tx.lock().unwrap();
            if let Some(socket) = tx_guard.take() {
                // Drop the socket to close it
                drop(socket);
                log::debug!("Signal socket TX closed");
            }
        }

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
