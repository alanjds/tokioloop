use pyo3::prelude::*;
use std::sync::atomic;

use crate::{
    event_loop::EventLoop,
    tcp::TCPServer,
    tokio_event_loop::TEventLoop,
    tokio_tcp::{TokioTCPServer, TokioTCPServerRef},
};

enum ServerType {
    TCP(TCPServer),
    // UDP,
    // Unix,
}

enum TServerType {
    TokioTCP(TokioTCPServerRef),
    // TokioUDP,
    // TokioUnix,
}

#[pyclass(frozen, module = "rloop._rloop")]
pub(crate) struct Server {
    #[pyo3(get)]
    _loop: Py<EventLoop>,
    #[pyo3(get)]
    _sockets: Py<PyAny>,
    closed: atomic::AtomicBool,
    serving: atomic::AtomicBool,
    servers: Vec<ServerType>,
    // serve_forever_fut: RwLock<Option<Py<PyAny>>>,
}

#[pyclass(frozen, module = "rloop._rloop")]
pub(crate) struct TokioServer {
    #[pyo3(get)]
    _loop: Py<TEventLoop>,
    #[pyo3(get)]
    _sockets: Py<PyAny>,
    closed: atomic::AtomicBool,
    serving: atomic::AtomicBool,
    servers: Vec<TServerType>,
}

impl Server {
    pub(crate) fn tcp(event_loop: Py<EventLoop>, sockets: Py<PyAny>, servers: Vec<TCPServer>) -> Self {
        let srv: Vec<ServerType> = servers.into_iter().map(ServerType::TCP).collect();

        Self {
            _loop: event_loop,
            _sockets: sockets,
            closed: false.into(),
            serving: false.into(),
            servers: srv,
            // serve_forever_fut: RwLock::new(None),
            // waiters: RwLock::new(Vec::new()),
        }
    }
}

impl TokioServer {
    pub(crate) fn tcp(event_loop: Py<TEventLoop>, sockets: Py<PyAny>, servers: Vec<TokioTCPServerRef>) -> Self {
        let srv: Vec<TServerType> = servers.into_iter().map(TServerType::TokioTCP).collect();

        Self {
            _loop: event_loop,
            _sockets: sockets,
            closed: false.into(),
            serving: false.into(),
            servers: srv,
        }
    }

    /// Create a mock TokioServer with no actual servers for testing/development
    pub(crate) fn mock(event_loop: Py<TEventLoop>, sockets: Py<PyAny>) -> Self {
        Self {
            _loop: event_loop,
            _sockets: sockets,
            closed: false.into(),
            serving: false.into(),
            servers: vec![],  // Empty servers list for mock
        }
    }
}

#[pymethods]
impl Server {
    // needed?
    // fn _add_waiter(&self, waiter: Py<PyAny>) {
    //     let mut guard = self.waiters.write().unwrap();
    //     guard.push(waiter);
    // }

    // #[getter(_sff)]
    // fn _get_sff(&self, py: Python) -> Option<Py<PyAny>> {
    //     let guard = self.serve_forever_fut.read().unwrap();
    //     guard.as_ref().map(|v| v.clone_ref(py))
    // }

    // #[setter(_sff)]
    // fn _set_sff(&self, val: Py<PyAny>) {
    //     let mut guard = self.serve_forever_fut.write().unwrap();
    //     *guard = Some(val);
    // }

    fn _start_serving(&self, py: Python) -> PyResult<()> {
        for server in &self.servers {
            match server {
                ServerType::TCP(inner) => inner.listen(py, self._loop.clone_ref(py))?,
            }
        }
        self.serving.store(true, atomic::Ordering::Release);
        Ok(())
    }

    fn _is_serving(&self) -> bool {
        self.serving.load(atomic::Ordering::Relaxed)
    }

    fn _close(&self, py: Python) {
        if self
            .closed
            .compare_exchange(false, true, atomic::Ordering::Release, atomic::Ordering::Relaxed)
            .is_ok()
        {
            let event_loop = self._loop.get();
            for server in &self.servers {
                match server {
                    ServerType::TCP(inner) => inner.close(py, event_loop),
                    // _ => {}
                }
            }
        }
        self.serving.store(false, atomic::Ordering::Release);
        // Ok(())
    }

    fn _streams_close(&self, py: Python) {
        let event_loop = self._loop.get();
        for server in &self.servers {
            match server {
                ServerType::TCP(inner) => inner.streams_close(py, event_loop),
            }
        }
    }

    fn _streams_abort(&self, py: Python) {
        let event_loop = self._loop.get();
        for server in &self.servers {
            match server {
                ServerType::TCP(inner) => inner.streams_abort(py, event_loop),
            }
        }
    }
}

#[pymethods]
impl TokioServer {
    fn _start_serving(&self, py: Python) -> PyResult<()> {
        log::trace!("TokioServer::_start_serving called");

        for server in &self.servers {
            match server {
                TServerType::TokioTCP(inner) => {
                    log::debug!("Starting TokioTCP server");
                    inner.start_listening(py)?;
                }
            }
        }

        self.serving.store(true, atomic::Ordering::Release);
        log::trace!("TokioServer::_start_serving completed successfully");
        Ok(())
    }

    fn _is_serving(&self) -> bool {
        self.serving.load(atomic::Ordering::Relaxed)
    }

    fn _close(&self, py: Python) {
        if self
            .closed
            .compare_exchange(false, true, atomic::Ordering::Release, atomic::Ordering::Relaxed)
            .is_ok()
        {
            log::trace!("TokioServer::_close called");
            for server in &self.servers {
                match server {
                    TServerType::TokioTCP(inner) => {
                        inner.close(py);
                    }
                }
            }
        }
        self.serving.store(false, atomic::Ordering::Release);
    }

    fn _streams_close(&self, py: Python) {
        log::debug!("TokioServer::_streams_close called");
        for server in &self.servers {
            match server {
                TServerType::TokioTCP(inner) => {
                    inner.close(py);
                }
            }
        }
    }

    fn _streams_abort(&self, py: Python) {
        log::debug!("TokioServer::_streams_abort called");
        for server in &self.servers {
            match server {
                TServerType::TokioTCP(inner) => {
                    inner.close(py);
                }
            }
        }
    }
}

pub(crate) fn init_pymodule(module: &Bound<PyModule>) -> PyResult<()> {
    module.add_class::<Server>()?;
    module.add_class::<TokioServer>()?;

    Ok(())
}
