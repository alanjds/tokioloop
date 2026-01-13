use std::{
    collections::{HashMap, VecDeque}, net::SocketAddr, os::fd::{AsRawFd, FromRawFd}, sync::{atomic::{self, AtomicBool}, Arc, Mutex},
    time::{Duration},
};

use anyhow::Result;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use pyo3::IntoPyObjectExt;
use tokio::{
    net::{TcpStream, TcpListener},
    sync::mpsc,
    task::JoinHandle,
    io::{AsyncReadExt, AsyncWriteExt},
};

use crate::{
    handles::BoxedHandle,
    tokio_event_loop::{TEventLoop, LoopHandlers},
    tokio_handles::{TBoxedHandle, TCBHandle},
    py::{run_in_ctx, run_in_ctx0, run_in_ctx1},
    log::LogExc,
};

/// Internal state management for tokio TCP connections
pub(crate) struct TokioTCPTransportState {
    stream: TcpStream,
    read_buf: Vec<u8>,
    write_buf: VecDeque<Vec<u8>>,
    closing: bool,
    weof: bool,
    paused: bool,
}

/// Main transport class implementing asyncio transport interface
#[pyclass(frozen, unsendable, module = "rloop._rloop")]
pub(crate) struct TokioTCPTransport {
    fd: usize,
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
        sock: (i32, i32),
        protocol_factory: Py<PyAny>,
    ) -> PyResult<Self> {
        let (fd, _family) = sock;

        // Convert the socket file descriptor to a tokio TcpStream
        let std_stream = unsafe {
            std::net::TcpStream::from_raw_fd(fd)
        };
        let tokio_stream = tokio::net::TcpStream::from_std(std_stream)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!("Failed to convert socket to tokio stream: {}", e)))?;

        let protocol = protocol_factory.call0(py)?;

        let state = Arc::new(Mutex::new(TokioTCPTransportState {
            stream: tokio_stream,
            read_buf: Vec::with_capacity(8192),
            write_buf: VecDeque::new(),
            closing: false,
            weof: false,
            paused: false,
        }));

        let transport = Self {
            fd: fd as usize,
            state: state.clone(),
            pyloop: event_loop.clone_ref(py),
            protocol: protocol.clone_ref(py),
            extra: HashMap::new(),
            closing: false.into(),
            paused: false.into(),
            weof: false.into(),
            lfd: None,
        };

        Ok(transport)
    }

    pub fn attach(transport: &Py<Self>, py: Python) -> PyResult<Py<PyAny>> {
        let rself = transport.borrow(py);
        rself.protocol.call_method1(py, pyo3::intern!(py, "connection_made"), (transport.clone_ref(py),))?;
        Ok(rself.protocol.clone_ref(py))
    }

    #[inline]
    pub fn is_tls(&self) -> bool {
        false // No SSL support initially
    }

    #[inline]
    pub fn get_local_addr(&self) -> Result<String> {
        let state = self.state.lock().unwrap();
        let local_addr = state.stream.local_addr()
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;
        Ok(local_addr.to_string())
    }

    #[inline]
    pub fn get_remote_addr(&self) -> Result<String> {
        let state = self.state.lock().unwrap();
        let peer_addr = state.stream.peer_addr()
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;
        Ok(peer_addr.to_string())
    }

    fn call_connection_lost(&self, py: Python, err: Option<PyErr>) {
        let err_arg = match err {
            Some(e) => e.into_py_any(py).unwrap_or_else(|_| py.None()),
            None => py.None(),
        };
        let _ = self.protocol.call_method1(py, pyo3::intern!(py, "connection_lost"), (err_arg,));
    }
}

#[pymethods]
impl TokioTCPTransport {
    #[getter]
    fn fd(&self) -> usize {
        self.fd
    }

    fn get_extra_info(&self, py: Python, name: String, default: Option<Py<PyAny>>) -> PyResult<Py<PyAny>> {
        match name.as_str() {
            "socket" => {
                // Return a mock socket object for compatibility
                Ok(default.unwrap_or_else(|| py.None()))
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
                if let Some(value) = self.extra.get(&name) {
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

        // Call connection_lost if write buffer is empty
        if state.write_buf.is_empty() {
            drop(state);
            self.call_connection_lost(py, None);
        }
    }

    fn abort(&self, py: Python) {
        if self.closing.compare_exchange(false, true, atomic::Ordering::Relaxed, atomic::Ordering::Relaxed).is_ok() {
            let mut state = self.state.lock().unwrap();
            state.closing = true;
            state.write_buf.clear(); // Clear pending writes
        }

        self.call_connection_lost(py, None);
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
        let socket = _try_socket2_conversion(fd, backlog)?;

        // Convert to std TcpListener then to tokio
        let std_listener: std::net::TcpListener = socket.into();
        let tokio_listener = tokio::net::TcpListener::from_std(std_listener)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!("Failed to convert socket2 to tokio listener: {}", e)))?;

        log::debug!("TokioTCPServer::from_fd: Tokio listener created successfully");
        log::debug!("TokioTCPServer::from_fd: Will serve on the address: {:?}", tokio_listener.local_addr());

        let server = Arc::new(Self {
            listener: Some(tokio_listener),
            protocol_factory,
            pyloop,
            transports: Arc::new(Mutex::new(Vec::new())),
            closed: Arc::new(false.into()),
            socks: Python::attach(|py| py.None()),
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

        let transport = TokioTCPTransport {
            fd: stream.as_raw_fd() as usize,
            state: Arc::new(Mutex::new(TokioTCPTransportState {
                stream,
                read_buf: Vec::with_capacity(8192),
                write_buf: VecDeque::new(),
                closing: false,
                weof: false,
                paused: false,
            })),
            pyloop: pyloop.clone_ref(py),
            protocol: protocol.clone_ref(py),
            extra: HashMap::new(),
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
        if self.closed.compare_exchange(false, true, atomic::Ordering::Relaxed, atomic::Ordering::Relaxed).is_err() {
            return;
        }

        // Close all transports
        let transports = self.transports.lock().unwrap();
        for transport in transports.iter() {
            let _ = transport.borrow(py).close(py);
        }
    }

    fn is_serving(&self) -> bool {
        !self.closed.load(atomic::Ordering::Relaxed)
    }
}

fn _try_socket2_conversion(fd: i32, backlog: i32) -> Result<socket2::Socket, PyErr> {
    use socket2::{Socket, Domain, Type};

    log::trace!("Socket conversion via socket2: fd={}", fd);
    // Convert fd to socket2 Socket
    let socket = unsafe { Socket::from_raw_fd(fd) };
    // Ensure socket is in correct state
    socket.set_nonblocking(true)
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!("Failed to set socket nonblocking: {}", e)))?;
    log::trace!("Socket conversion via socket2 successful");
    // Set backlog if socket is not already listening
    if let Err(e) = socket.listen(backlog) {
        log::debug!("Socket conversion: listen failed (might already be listening): {}", e);
        // Don't fail if already listening, just continue
    }
    Ok(socket)
}

impl TokioTCPServer {
    pub fn start_listening(server: TokioTCPServerRef, py: Python) -> PyResult<()> {
        let server_clone = server.clone();

        let protocol_factory = server_clone.protocol_factory.clone_ref(py);
        let pyloop = server_clone.pyloop.clone_ref(py);
        let transports = server_clone.transports.clone();
        let closed = server_clone.closed.clone();

        let runtime = pyloop.get().runtime.clone();

        runtime.spawn(async move {
            log::debug!("TokioTCPServer: Starting listener loop");

            let listener = server_clone.listener.as_ref().unwrap().clone();
            log::debug!("TokioTCPServer: Will serve on the address: {:?}", listener.local_addr());

            loop {
                let is_closed = closed.load(atomic::Ordering::Relaxed);

                if is_closed {
                    log::debug!("TokioTCPServer: Server closed, stopping listener loop");
                    break;
                }

                match listener.accept().await {
                    Ok((stream, addr)) => {
                        log::debug!("TokioTCPServer: New connection accepted from {}", addr);

                        Python::attach(|py| {
                            let transport = TokioTCPServer::create_transport_from_stream(
                                py,
                                &pyloop,
                                stream,
                                protocol_factory.clone_ref(py),
                            );

                            match transport {
                                Ok(transport_py) => {
                                    // Store transport
                                    {
                                        let mut transports_lock = transports.lock().unwrap();
                                        transports_lock.push(transport_py.clone_ref(py));
                                    }

                                    // Attach protocol
                                    let _ = TokioTCPTransport::attach(&transport_py, py);
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
