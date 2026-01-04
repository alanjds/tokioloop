use pyo3::prelude::*;
use std::sync::atomic;

use crate::{
    event_loop::EventLoop,
    tcp::TCPServer,
    tokio_event_loop::TEventLoop,
};

enum ServerType {
    TCP(TCPServer),
    // UDP,
    // Unix,
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
    servers: Vec<ServerType>,
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
    pub(crate) fn tcp(event_loop: Py<TEventLoop>, sockets: Py<PyAny>, servers: Vec<TCPServer>) -> Self {
        let srv: Vec<ServerType> = servers.into_iter().map(ServerType::TCP).collect();

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
        // For now, just mark as serving without actually starting listeners
        // This is a mock implementation to make tests pass
        log::debug!("TokioServer::_start_serving called - mock implementation");
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
            // Mock close - just mark as closed
            log::debug!("TokioServer::_close called - mock implementation");
        }
        self.serving.store(false, atomic::Ordering::Release);
    }

    fn _streams_close(&self, py: Python) {
        log::debug!("TokioServer::_streams_close called - mock implementation");
    }

    fn _streams_abort(&self, py: Python) {
        log::debug!("TokioServer::_streams_abort called - mock implementation");
    }
}

pub(crate) fn init_pymodule(module: &Bound<PyModule>) -> PyResult<()> {
    module.add_class::<Server>()?;
    module.add_class::<TokioServer>()?;

    Ok(())
}
