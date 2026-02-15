use std::{
    collections::{HashMap, VecDeque}, net::SocketAddr, os::fd::{AsRawFd, FromRawFd}, sync::{atomic::{self, AtomicBool}, Arc, Mutex},
    time::{Duration},
};

use anyhow::Result;
use pyo3::{ffi::c_str, prelude::*};
use pyo3::types::PyBytes;
use pyo3::IntoPyObjectExt;
use tokio::{
    net::{TcpStream, TcpListener},
    io::{AsyncReadExt, AsyncWriteExt},
};
use socket2::{Domain, Type};

use crate::{
    tokio_event_loop::TEventLoop,
    py::sock,
    utils::python_spawn,
};

/// Internal state management for tokio TCP connections
pub(crate) struct TokioTCPTransportState {
    stream: Option<TcpStream>,
    read_buf: Vec<u8>,
    write_buf: VecDeque<Vec<u8>>,
    closing: bool,
    weof: bool,
    read_eof: bool,
    write_shutdown_done: bool,
    paused: bool,
    io_task: Option<tokio::task::JoinHandle<()>>,
    local_addr: Option<SocketAddr>,
    peer_addr: Option<SocketAddr>,
}

/// Main transport class implementing asyncio transport interface
#[pyclass(frozen, module = "rloop._rloop")]
pub(crate) struct TokioTCPTransport {
    pub(crate) fd: usize,
    state: Arc<Mutex<TokioTCPTransportState>>,
    pyloop: Py<TEventLoop>,
    protocol: Py<PyAny>,
    extra: HashMap<String, Py<PyAny>>,
    // Atomic flags for thread safety
    closing: AtomicBool,
    paused: AtomicBool,
    weof: AtomicBool,
    #[pyo3(get)]
    lfd: Option<usize>,
}

impl TokioTCPTransport {
    pub fn from_py(
        py: Python,
        event_loop: &Py<TEventLoop>,
        sock_tuple: (i32, i32),
        protocol_factory: Py<PyAny>,
    ) -> PyResult<Self> {
        log::trace!("TokioTCPTransport::from_py called");
        let (fd, _family) = sock_tuple;

        // Convert the socket file descriptor to a tokio TcpStream
        let socket = crate::utils::_try_socket2_conversion(fd, 0)?;

        // Convert to std TcpStream then to tokio
        let std_stream: std::net::TcpStream = socket.into();
        let tokio_stream = tokio::net::TcpStream::from_std(std_stream)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!("Failed to convert socket to tokio stream: {}", e)))?;

        log::debug!("TokioTCPTransport::from_py: Tokio stream created successfully");

        // Store the addresses before moving the strean
        let local_addr = tokio_stream.local_addr().ok();
        let peer_addr = tokio_stream.peer_addr().ok();

        let protocol = protocol_factory.call0(py)?;

        let mut extra = HashMap::new();

        log::debug!("TokioTCPTransport::from_py: About to create socket for fd {}", fd);
        let socket_factory = match sock(py) {
            Ok(sf) => sf,
            Err(e) => {
                log::error!("TokioTCPTransport::from_py: Failed to get socket factory: {}", e);
                return Err(e);
            }
        };
        let py_socket = match socket_factory.call1((
            i32::from(Domain::IPV4),
            i32::from(Type::STREAM),
            0,
            fd
        )) {
            Ok(ps) => ps,
            Err(e) => {
                log::error!("TokioTCPTransport::from_py: Failed to create Python socket: {}", e);
                return Err(e);
            }
        };
        log::debug!("TokioTCPTransport::from_py: Created Python socket for fd {}", fd);
        extra.insert("socket".to_string(), py_socket.unbind());
        log::debug!("TokioTCPTransport::from_py: Inserted socket into extra map, size: {}", extra.len());

        let state = Arc::new(Mutex::new(TokioTCPTransportState {
            stream: Some(tokio_stream),
            read_buf: Vec::with_capacity(8192),
            write_buf: VecDeque::new(),
            closing: false,
            weof: false,
            read_eof: false,
            write_shutdown_done: false,
            paused: false,
            io_task: None,
            local_addr,
            peer_addr,
        }));

        let transport = Self {
            fd: fd as usize,
            state,
            pyloop: event_loop.clone_ref(py),
            protocol: protocol.clone_ref(py),
            extra,
            closing: false.into(),
            paused: false.into(),
            weof: false.into(),
            lfd: None,
        };

        Ok(transport)
    }

    pub fn attach(transport: &Py<Self>, py: Python) -> PyResult<Py<PyAny>> {
        let rself = transport.borrow(py);

        // Start the I/O processing task
        Self::start_io_task(transport, py)?;

        rself.protocol.call_method1(py, pyo3::intern!(py, "connection_made"), (transport.clone_ref(py),))?;
        Ok(rself.protocol.clone_ref(py))
    }

    fn start_io_task(transport: &Py<Self>, py: Python) -> PyResult<()> {
        let transport_clone = transport.clone_ref(py);
        let protocol = transport.borrow(py).protocol.clone_ref(py);
        let pyloop = transport.borrow(py).pyloop.clone_ref(py);
        let state = transport.borrow(py).state.clone();

        let runtime = pyloop.borrow(py).get_runtime();

        // Spawn the I/O processing task with proper cleanup on cancellation
        let state_clone = state.clone();
        let io_task = runtime.spawn(async move {
            // Wrap Python objects in Option for take() semantics
            let mut transport_opt = Some(transport_clone);
            let mut protocol_opt = Some(protocol);

            // Run the main I/O loop with references
            // Store fd value for cleanup logging
            let fd: i32;
            {
                let transport_ref = transport_opt.as_ref().expect("transport should exist");
                let protocol_ref = protocol_opt.as_ref().expect("protocol should exist");
                fd = Self::io_processing_loop(transport_ref, state_clone, protocol_ref).await;
            }

            // Cleanup phase - ALWAYS runs regardless of exit path
            log::trace!("TASK CLEANUP: Dropping Python objects for fd={}", fd);
            Python::attach(|_py| {
                drop(transport_opt.take());
                drop(protocol_opt.take());
            });
        });

        // Store the task handle
        {
            let mut state_lock = state.lock().unwrap();
            state_lock.io_task = Some(io_task);
        }

        Ok(())
    }

    async fn io_processing_loop(
        transport: &Py<TokioTCPTransport>,
        state: Arc<Mutex<TokioTCPTransportState>>,
        protocol: &Py<PyAny>,
    ) -> i32 {
        let mut connection_lost_called = false;
        let mut fd: i32 = 0;

        // Main async logic block - cleanup runs after regardless of how this exits
        let fd = async {
            let mut read_buf = [0u8; 8192];

            // Extract the stream and split it for reading and writing
            let stream_opt = {
                let mut state_lock = state.lock().unwrap();
                std::mem::take(&mut state_lock.stream)
            };

            let Some(stream) = stream_opt else {
                log::error!("No stream available for I/O processing");
                return 0;
            };

            // For logs
            fd = stream.as_raw_fd() as i32;

            let (mut reader, mut writer) = tokio::io::split(stream);

            loop {
                // Check if we should stop
                let (is_closing, is_paused, read_eof) = {
                    let state_lock = state.lock().unwrap();
                    (state_lock.closing, state_lock.paused, state_lock.read_eof)
                };

                if is_closing {
                    log::trace!("TCP connection is_closing. Exiting the loop [fd={}]", fd);
                    break;
                }

                // If paused, just wait then recheck (loop)
                if is_paused {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                }

                tokio::select! {
                    // Handle reading: only if we have not received EOF yet
                    result = reader.read(&mut read_buf), if !read_eof => {
                        match result {
                            Ok(0) => {
                                // EOF received
                                log::trace!("TCP connection EOF received [fd={}]", fd);
                                {
                                    let mut state_lock = state.lock().unwrap();
                                    state_lock.read_eof = true;
                                }
                                Python::attach(|py| {
                                    let _ = protocol.call_method0(py, pyo3::intern!(py, "eof_received"));
                                });
                            }
                            Ok(n) => {
                                // Data received, forward to protocol
                                let data = read_buf[..n].to_vec();
                                Python::attach(|py| {
                                    let _ = protocol.call_method1(
                                        py,
                                        pyo3::intern!(py, "data_received"),
                                        (PyBytes::new(py, &data),)
                                    );
                                });
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                // No data available, continue
                            }
                            Err(e) => {
                                log::error!("TCP read error: [fd={}] {}", fd, e);
                                break;
                            }
                        }
                    }

                    _ = async {
                        // Writing & shutdown should be handled with both locked
                        let (write_data, should_shutdown_now) = {
                            let mut state_lock = state.lock().unwrap();
                            let write_data = state_lock.write_buf.pop_front();
                            let should_shutdown = state_lock.weof
                                && state_lock.write_buf.is_empty()
                                && !state_lock.write_shutdown_done;
                            (write_data, should_shutdown)
                        };

                        // Handle writing first
                        if let Some(data) = write_data {
                            if let Err(e) = writer.write_all(&data).await {
                                log::error!("TCP write error: [fd={}] {}", fd, e);
                            } else {
                                log::trace!("TCP wrote {} bytes [fd={}]", data.len(), fd);
                                if let Err(e) = writer.flush().await {
                                    log::error!("TCP flush error: [fd={}] {}", fd, e);
                                }
                            }
                        }

                        // Shutdown only with empty buffer
                        if should_shutdown_now {
                            log::trace!("TCP writer shutdown starting [fd={}]", fd);
                            if let Err(e) = writer.shutdown().await {
                                log::debug!("TCP writer shutdown error (may be expected): [fd={}] {}", fd, e);
                            }

                            let mut state_lock = state.lock().unwrap();
                            state_lock.write_shutdown_done = true;
                            log::debug!("TCP writer shutdown completed [fd={}]", fd);
                        }
                    }, if {
                        let state_lock = state.lock().unwrap();
                        !state_lock.write_buf.is_empty() ||
                        (state_lock.weof && !state_lock.write_shutdown_done)
                    } => {}

                    // Prevent a busy loop
                    _ = tokio::time::sleep(Duration::from_millis(1)) => {}
                }

                // Graceful shutdown only after everything else. May never occur
                let should_exit = {
                    let state_lock = state.lock().unwrap();
                    (state_lock.read_eof && state_lock.write_buf.is_empty())
                    // ^-- Exit when client closes without server doing write_eof()
                        || (state_lock.read_eof
                            && state_lock.write_shutdown_done
                            && state_lock.write_buf.is_empty())
                };

                if should_exit {
                    log::trace!("TCP connection gracefully closed (read EOF + no pending writes) [fd={}]", fd);
                    break;
                }
            }
            fd  // Return fd from async block
        }.await;

        // Cleanup phase - ALWAYS executed after the main loop, regardless of exit path
        log::trace!("io_processing_loop: Cleaning up Python objects for fd={}", fd);

        Python::attach(|py| {
            if fd > 0 {
                let transport_ref = transport.borrow(py);
                if !connection_lost_called {
                    log::trace!("Calling connection_lost for fd={}", fd);
                    let _ = transport_ref.call_connection_lost(py, None);
                }
                drop(transport_ref);
            } else {
                log::trace!("Calling connection_lost for fd={} ? No.", fd);
            }

            log::trace!("io_processing_loop: Cleanup complete for fd={}", fd);
        });

        // Note: transport and protocol are dropped by the caller (start_io_task) with GIL held
        fd  // Return fd from function
    }

    #[inline]
    pub fn is_tls(&self) -> bool {
        false // No SSL support initially
    }

    #[inline]
    pub fn get_local_addr(&self) -> Result<String> {
        let state = self.state.lock().unwrap();
        state.local_addr
            .map(|addr| addr.to_string())
            .ok_or_else(|| anyhow::anyhow!("Local address not available"))
    }

    #[inline]
    pub fn get_remote_addr(&self) -> Result<String> {
        let state = self.state.lock().unwrap();
        state.peer_addr
            .map(|addr| addr.to_string())
            .ok_or_else(|| anyhow::anyhow!("Peer address not available"))
    }

    fn call_connection_lost(&self, py: Python, err: Option<PyErr>) {
        let err_arg = match err {
            Some(e) => e.into_py_any(py).unwrap_or_else(|_| py.None()),
            None => py.None(),
        };
        log::trace!("TokioTCPTransport::call_connection_lost calling {}.connection_lost", self.protocol);
        let _ = self.protocol.call_method1(py, pyo3::intern!(py, "connection_lost"), (err_arg,));
    }
}

#[pymethods]
impl TokioTCPTransport {
    #[getter]
    fn fd(&self) -> usize {
        self.fd
    }

    #[pyo3(signature = (name, default=None))]
    fn get_extra_info(&self, py: Python, name: &str, default: Option<Py<PyAny>>) -> PyResult<Py<PyAny>> {
        match name {
            "socket" => {
                match self.extra.get("socket") {
                    Some(py_obj) => Ok(py_obj.clone_ref(py)),
                    None => Ok(default.unwrap_or_else(|| py.None())),
                }
            }
            "sockname" => {
                match self.get_local_addr() {
                    Ok(addr) => {
                        // Parse address string to return a tuple (host, port)
                        if let Some((host_part, port_part)) = addr.rsplit_once(':') {
                            let port: u16 = port_part.parse().unwrap_or(0);
                            let addr_tuple = (host_part, port);
                            Ok(addr_tuple.into_py_any(py)?)
                        } else {
                            Ok(default.unwrap_or_else(|| py.None()))
                        }
                    }
                    Err(_) => Ok(default.unwrap_or_else(|| py.None())),
                }
            }
            "peername" => {
                match self.get_remote_addr() {
                    Ok(addr) => {
                        // Parse address string to return a tuple (host, port)
                        if let Some((host_part, port_part)) = addr.rsplit_once(':') {
                            let port: u16 = port_part.parse().unwrap_or(0);
                            let addr_tuple = (host_part, port);
                            Ok(addr_tuple.into_py_any(py)?)
                        } else {
                            Ok(default.unwrap_or_else(|| py.None()))
                        }
                    }
                    Err(_) => Ok(default.unwrap_or_else(|| py.None())),
                }
            }
            _ => {
                let default_val = default.unwrap_or_else(|| py.None());
                if let Some(value) = self.extra.get(name) {
                    Ok(value.clone_ref(py))
                } else {
                    Ok(default_val)
                }
            }
        }
    }

    fn write(&self, py: Python, data: Py<PyAny>) -> PyResult<()> {
        if self.weof.load(atomic::Ordering::Relaxed) {
            return Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>("Cannot write after EOF"));
        }

        let bytes = data.extract::<Vec<u8>>(py)?;

        {
            let mut state = self.state.lock().unwrap();
            state.write_buf.push_back(bytes);
        }

        Ok(())
    }

    fn writelines(&self, py: Python, list_of_data: Py<PyAny>) -> PyResult<()> {
        let list = list_of_data.extract::<Vec<Py<PyAny>>>(py)?;
        for item in list {
            self.write(py, item)?;
        }
        Ok(())
    }

    fn write_eof(&self, py: Python) -> PyResult<()> {
        if self.closing.load(atomic::Ordering::Relaxed) {
            return Ok(());
        }

        if self.weof.compare_exchange(false, true, atomic::Ordering::Relaxed, atomic::Ordering::Relaxed).is_ok() {
            // Set EOF flag - actual shutdown will happen when write buffer is empty
            let mut state = self.state.lock().unwrap();
            state.weof = true;
        }

        Ok(())
    }

    fn can_write_eof(&self) -> bool {
        !self.weof.load(atomic::Ordering::Relaxed)
    }

    fn pause_reading(&self, py: Python) {
        if self.closing.load(atomic::Ordering::Relaxed) {
            return;
        }

        if self.paused.compare_exchange(false, true, atomic::Ordering::Relaxed, atomic::Ordering::Relaxed).is_ok() {
            let mut state = self.state.lock().unwrap();
            state.paused = true;
        }
    }

    fn resume_reading(&self, py: Python) {
        if self.closing.load(atomic::Ordering::Relaxed) {
            return;
        }

        if self.paused.compare_exchange(true, false, atomic::Ordering::Relaxed, atomic::Ordering::Relaxed).is_ok() {
            let mut state = self.state.lock().unwrap();
            state.paused = false;
        }
    }

    fn close(&self, py: Python) {
        if self.closing.compare_exchange(false, true, atomic::Ordering::Relaxed, atomic::Ordering::Relaxed).is_err() {
            return;
        }

        let mut state = self.state.lock().unwrap();
        state.closing = true;

        // Remove the transport from tcp_transports map
        // This allows the FD to be used for raw socket operations after the transport is closed
        let fd = self.fd;
        let pyloop = self.pyloop.clone_ref(py);
        {
            let loop_ref = pyloop.borrow(py);
            let tcp_transports = loop_ref.tcp_transports.pin();
            tcp_transports.remove(&fd);
            log::debug!("Removed TCP transport for fd: {}", fd);
        }

        // Don't call connection_lost immediately: let the I/O loop handle it
        // Allows for any pending data to be received
    }

    fn abort(&self, py: Python) {
        // Let io_processing_loop call connection_lost: Avoid dup calls
        if self.closing.compare_exchange(false, true, atomic::Ordering::Relaxed, atomic::Ordering::Relaxed).is_ok() {
            let mut state = self.state.lock().unwrap();
            state.closing = true;
            state.write_buf.clear();
        }
    }

    fn is_reading(&self) -> bool {
        !self.closing.load(atomic::Ordering::Relaxed) && !self.paused.load(atomic::Ordering::Relaxed)
    }

    fn is_closing(&self) -> bool {
        self.closing.load(atomic::Ordering::Relaxed)
    }
}

/// Server implementation using tokio::net::TcpListener
#[pyclass(frozen, subclass, module = "rloop._rloop")]
pub(crate) struct TokioTCPServer {
    listener: Option<TcpListener>,
    protocol_factory: Py<PyAny>,
    pyloop: Py<TEventLoop>,
    transports: Arc<Mutex<Vec<Py<TokioTCPTransport>>>>,
    closed: Arc<AtomicBool>,
    #[pyo3(get)]
    socks: Py<PyAny>,
    #[pyo3(get)]
    transports_py: Vec<Py<TokioTCPTransport>>,
}

pub type TokioTCPServerRef = Arc<TokioTCPServer>;

impl TokioTCPServer {
    pub fn from_fd(
        fd: i32,
        family: i32,
        backlog: i32,
        protocol_factory: Py<PyAny>,
        pyloop: Py<TEventLoop>,
    ) -> PyResult<TokioTCPServerRef> {
        log::debug!("TokioTCPServer::from_fd called with fd: {}", fd);

        // Convert via socket2 for better state management
        let socket = crate::utils::_try_socket2_conversion(fd, backlog)?;

        // Convert to std TcpListener then to tokio
        let std_listener: std::net::TcpListener = socket.into();
        let tokio_listener = tokio::net::TcpListener::from_std(std_listener)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!("Failed to convert socket2 to tokio listener: {}", e)))?;

        log::debug!("TokioTCPServer::from_fd: Tokio listener created successfully");
        log::debug!("TokioTCPServer::from_fd: Will serve on the address: {:?}", tokio_listener.local_addr());

        // Create an empty list for sockets (will be populated by Python side)
        let socks = Python::with_gil(|py| {
            let socket_list = py.eval(c_str!("[]"), None, None)?;
            Ok::<Py<PyAny>, PyErr>(socket_list.into())
        })?;

        let server = Arc::new(Self {
            listener: Some(tokio_listener),
            protocol_factory,
            pyloop,
            transports: Arc::new(Mutex::new(Vec::new())),
            closed: Arc::new(false.into()),
            socks,
            transports_py: Vec::new(),
        });

        Ok(server)
    }

    pub fn create_transport_from_stream(
        py: Python,
        pyloop: &Py<TEventLoop>,
        stream: TcpStream,
        protocol_factory: Py<PyAny>,
    ) -> PyResult<Py<TokioTCPTransport>> {
        let protocol = protocol_factory.call0(py)?;

        let local_addr = stream.local_addr().ok();
        let peer_addr = stream.peer_addr().ok();
        let fd = stream.as_raw_fd();

        let mut extra = HashMap::new();

        let socket_factory = sock(py)?;
        let py_socket = socket_factory.call1((
            i32::from(Domain::IPV4),
            i32::from(Type::STREAM),
            0,
            fd
        ))?;
        extra.insert("socket".to_string(), py_socket.unbind());

        let transport = TokioTCPTransport {
            fd: fd as usize,
            state: Arc::new(Mutex::new(TokioTCPTransportState {
                stream: Some(stream),
                read_buf: Vec::with_capacity(8192),
                write_buf: VecDeque::new(),
                closing: false,
                weof: false,
                read_eof: false,
                write_shutdown_done: false,
                paused: false,
                io_task: None,
                local_addr,
                peer_addr,
            })),
            pyloop: pyloop.clone_ref(py),
            protocol: protocol.clone_ref(py),
            extra,
            closing: false.into(),
            paused: false.into(),
            weof: false.into(),
            lfd: None,
        };

        let pytransport = Py::new(py, transport)?;

        Ok(pytransport)
    }

    #[inline]
    pub fn fd(&self) -> usize {
        self.listener.as_ref().unwrap().as_raw_fd() as usize
    }

    pub fn close(&self, py: Python) {
        log::trace!("TokioTCPServer::close called");
        if self.closed.compare_exchange(false, true, atomic::Ordering::Relaxed, atomic::Ordering::Relaxed).is_err() {
            return;
        }

        // Close all transports
        let transports = self.transports.lock().unwrap();
        for transport in transports.iter() {
            log::trace!("TokioTCPServer: closing transport {}", transport);
            let _ = transport.borrow(py).close(py);
        }
        log::trace!("TokioTCPServer: closed all transports");
    }

    fn is_serving(&self) -> bool {
        !self.closed.load(atomic::Ordering::Relaxed)
    }
}

impl TokioTCPServer {
    pub fn start_listening(server: TokioTCPServerRef, py: Python) -> PyResult<()> {
        let server_clone = server.clone();

        let protocol_factory = server_clone.protocol_factory.clone_ref(py);
        let pyloop = server_clone.pyloop.clone_ref(py);
        let transports = server_clone.transports.clone();
        let closed = server_clone.closed.clone();

        let runtime = pyloop.borrow(py).get_runtime();

        runtime.spawn(async move {
            log::debug!("TokioTCPServer: Starting listener loop");

            let listener = server_clone.listener.as_ref().unwrap().clone();
            log::debug!("TokioTCPServer: Will listen on the address: {:?}", listener.local_addr());

            loop {
                let is_closed = closed.load(atomic::Ordering::Relaxed);

                if is_closed {
                    log::debug!("TokioTCPServer: Server closed, stopping listener loop");
                    break;
                }

                match listener.accept().await {
                    Ok((stream, addr)) => {
                        log::debug!("TokioTCPServer: New connection accepted from client on {}", addr);

                        Python::attach(|py| {
                            let transport = TokioTCPServer::create_transport_from_stream(
                                py,
                                &pyloop,
                                stream,
                                protocol_factory.clone_ref(py),
                            );

                            match transport {
                                Ok(transport_py) => {
                                    log::trace!("Transport created: {}", transport_py);
                                    // Store transport
                                    {
                                        let mut transports_lock = transports.lock().unwrap();
                                        transports_lock.push(transport_py.clone_ref(py));
                                    }

                                    // Attach protocol
                                    match TokioTCPTransport::attach(&transport_py, py) {
                                        Ok(result) => log::trace!("Transport attached: {}", result),
                                        Err(e) => log::error!("Transport not attached: {}", e)
                                    };
                                }
                                Err(e) => {
                                    log::error!("Failed to create transport for connection: {}", e);
                                }
                            }
                        });
                    }
                    Err(e) => {
                        log::error!("TokioTCPServer: Error accepting connection: {}", e);
                        tokio::time::sleep(Duration::from_millis(1)).await;
                        // Continue listening
                    }
                }
            }

            log::debug!("TokioTCPServer: Listener loop stopped");
        });

        Ok(())
    }
}

pub(crate) fn init_pymodule(module: &Bound<PyModule>) -> PyResult<()> {
    module.add_class::<TokioTCPTransport>()?;
    module.add_class::<TokioTCPServer>()?;
    Ok(())
}
