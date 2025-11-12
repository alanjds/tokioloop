use std::{
    borrow::Cow,
    cell::RefCell,
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    sync::atomic,
};

use anyhow::Result;
use mio::Interest;
use openssl::ssl::{Ssl, SslContext, SslMethod, SslStream};
use pyo3::{buffer::PyBuffer, prelude::*, types::PyBytes, IntoPyObjectExt};
use std::os::fd::{AsRawFd, FromRawFd};

use crate::{
    event_loop::EventLoop,
    handles::{BoxedHandle, CBHandle},
    log::LogExc,
    py::{asyncio_proto_buf, copy_context},
    sock::SocketWrapper,
    utils::syscall,
};

pub(crate) struct SSLTransportState {
    ssl_stream: SslStream<std::net::TcpStream>,
    write_buf: VecDeque<Box<[u8]>>,
    write_buf_dsize: usize,
    handshake_complete: bool,
}

#[pyclass(frozen, unsendable, module = "rloop._rloop")]
pub(crate) struct SSLTransport {
    pub fd: usize,
    pub lfd: Option<usize>,
    state: RefCell<SSLTransportState>,
    pyloop: Py<EventLoop>,
    // atomics
    closing: atomic::AtomicBool,
    paused: atomic::AtomicBool,
    water_hi: atomic::AtomicUsize,
    water_lo: atomic::AtomicUsize,
    weof: atomic::AtomicBool,
    // py protocol fields
    pub proto: Py<PyAny>,
    proto_buffered: bool,
    proto_paused: atomic::AtomicBool,
    protom_buf_get: Py<PyAny>,
    protom_conn_lost: Py<PyAny>,
    protom_recv_data: Py<PyAny>,
    // py extras
    extra: HashMap<String, Py<PyAny>>,
    sock: Py<SocketWrapper>,
}

impl SSLTransport {
    pub(crate) fn new(
        py: Python,
        pyloop: Py<EventLoop>,
        sock: (i32, i32),
        ssl_context: Py<PyAny>,
        protocol_factory: Py<PyAny>,
        server: bool,
    ) -> Result<Self> {
        let pyproto = protocol_factory.bind(py).call0().unwrap();

        let fd = sock.0 as usize;

        // Create SSL context from Python SSL context
        let ssl_ctx = Self::create_ssl_context(py, &ssl_context)?;
        let mut ssl = Ssl::new(&ssl_ctx)?;

        // Set SSL mode based on server parameter
        if server {
            ssl.set_accept_state();
        } else {
            ssl.set_connect_state();
        }

        // Create TCP stream from socket
        let std_sock: std::net::TcpStream = unsafe { std::os::fd::FromRawFd::from_raw_fd(sock.0) };
        let mut ssl_stream = SslStream::new(ssl, std_sock)?;

        // Set non-blocking mode
        ssl_stream.get_mut().set_nonblocking(true)?;

        // For non-blocking SSL, we don't try to complete handshake here
        // It will be handled during read/write operations

        let state = SSLTransportState {
            ssl_stream,
            write_buf: VecDeque::new(),
            write_buf_dsize: 0,
            handshake_complete: false,
        };

        let wh = 1024 * 64;
        let wl = wh / 4;

        let mut proto_buffered = false;
        let protom_buf_get: Py<PyAny>;
        let protom_recv_data: Py<PyAny>;
        if pyproto.is_instance(asyncio_proto_buf(py).unwrap()).unwrap() {
            proto_buffered = true;
            protom_buf_get = pyproto.getattr(pyo3::intern!(py, "get_buffer")).unwrap().unbind();
            protom_recv_data = pyproto.getattr(pyo3::intern!(py, "buffer_updated")).unwrap().unbind();
        } else {
            protom_buf_get = py.None();
            protom_recv_data = pyproto.getattr(pyo3::intern!(py, "data_received")).unwrap().unbind();
        }
        let protom_conn_lost = pyproto.getattr(pyo3::intern!(py, "connection_lost")).unwrap().unbind();
        let proto = pyproto.unbind();

        Ok(Self {
            fd,
            lfd: None,
            state: RefCell::new(state),
            pyloop,
            closing: false.into(),
            paused: false.into(),
            water_hi: wh.into(),
            water_lo: wl.into(),
            weof: false.into(),
            proto,
            proto_buffered,
            proto_paused: false.into(),
            protom_buf_get,
            protom_conn_lost,
            protom_recv_data,
            extra: HashMap::new(),
            sock: SocketWrapper::from_fd(py, fd, sock.1, socket2::Type::STREAM, 0),
        })
    }

    fn create_ssl_context(py: Python, ssl_context: &Py<PyAny>) -> Result<SslContext> {
        let mut ctx = SslContext::builder(SslMethod::tls())?;
        ctx.set_verify(openssl::ssl::SslVerifyMode::NONE); // For testing

        // Try to load certificates if available
        if let Ok(certfile) = ssl_context.getattr(py, "_certfile") {
            if let Ok(keyfile) = ssl_context.getattr(py, "_keyfile") {
                let certfile_str: String = certfile.extract(py)?;
                let keyfile_str: String = keyfile.extract(py)?;
                ctx.set_private_key_file(&keyfile_str, openssl::ssl::SslFiletype::PEM)?;
                ctx.set_certificate_chain_file(&certfile_str)?;
            }
        }

        Ok(ctx.build())
    }

    pub(crate) fn attach(pyself: &Py<Self>, py: Python) -> PyResult<Py<PyAny>> {
        let rself = pyself.borrow(py);
        rself
            .proto
            .call_method1(py, pyo3::intern!(py, "connection_made"), (pyself.clone_ref(py),))?;
        Ok(rself.proto.clone_ref(py))
    }

    #[inline]
    fn write_buf_size_decr(pyself: &Py<Self>, py: Python) {
        let rself = pyself.borrow(py);
        if rself.state.borrow().write_buf_dsize <= rself.water_lo.load(atomic::Ordering::Relaxed)
            && rself
                .proto_paused
                .compare_exchange(true, false, atomic::Ordering::Relaxed, atomic::Ordering::Relaxed)
                .is_ok()
        {
            Self::proto_resume(pyself, py);
        }
    }

    #[inline]
    fn close_from_read_handle(&self, py: Python, event_loop: &EventLoop) -> bool {
        if self
            .closing
            .compare_exchange(false, true, atomic::Ordering::Relaxed, atomic::Ordering::Relaxed)
            .is_err()
        {
            return false;
        }

        if !self.state.borrow().write_buf.is_empty() {
            return false;
        }

        event_loop.ssl_stream_rem(self.fd, Interest::WRITABLE);
        _ = self.protom_conn_lost.call1(py, (py.None(),));
        true
    }

    #[inline]
    fn close_from_write_handle(&self, py: Python, errored: bool) -> Option<bool> {
        if self.closing.load(atomic::Ordering::Relaxed) {
            _ = self.protom_conn_lost.call1(
                py,
                #[allow(clippy::obfuscated_if_else)]
                (errored
                    .then(|| {
                        pyo3::exceptions::PyRuntimeError::new_err("ssl transport failed")
                            .into_py_any(py)
                            .unwrap()
                    })
                    .unwrap_or_else(|| py.None()),),
            );
            return Some(true);
        }
        self.weof.load(atomic::Ordering::Relaxed).then_some(false)
    }

    #[inline(always)]
    fn call_conn_lost(&self, py: Python, err: Option<PyErr>) {
        _ = self.protom_conn_lost.call1(py, (err,));
        self.pyloop.get().ssl_stream_close(py, self.fd);
    }

    fn try_write(pyself: &Py<Self>, py: Python, data: &[u8]) -> PyResult<()> {
        let rself = pyself.borrow(py);

        if rself.weof.load(atomic::Ordering::Relaxed) {
            return Err(pyo3::exceptions::PyRuntimeError::new_err("Cannot write after EOF"));
        }
        if data.is_empty() {
            return Ok(());
        }

        let mut state = rself.state.borrow_mut();
        let buf_added = match state.write_buf_dsize {
            0 => match state.ssl_stream.write(data) {
                Ok(written) if written as usize == data.len() => 0,
                Ok(written) => {
                    let written = written as usize;
                    state.write_buf.push_back((&data[written..]).into());
                    data.len() - written
                }
                Err(err)
                    if err.kind() == std::io::ErrorKind::Interrupted
                        || err.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    state.write_buf.push_back(data.into());
                    data.len()
                }
                Err(err) => {
                    if state.write_buf_dsize > 0 {
                        rself.pyloop.get().ssl_stream_rem(rself.fd, Interest::WRITABLE);
                    }
                    if rself
                        .closing
                        .compare_exchange(false, true, atomic::Ordering::Relaxed, atomic::Ordering::Relaxed)
                        .is_ok()
                    {
                        rself.pyloop.get().ssl_stream_rem(rself.fd, Interest::READABLE);
                    }
                    rself.call_conn_lost(py, Some(pyo3::exceptions::PyRuntimeError::new_err(err.to_string())));
                    0
                }
            },
            _ => {
                state.write_buf.push_back(data.into());
                data.len()
            }
        };
        if buf_added > 0 {
            if state.write_buf_dsize == 0 {
                rself.pyloop.get().ssl_stream_add(rself.fd, Interest::WRITABLE);
            }
            state.write_buf_dsize += buf_added;
            if state.write_buf_dsize > rself.water_hi.load(atomic::Ordering::Relaxed)
                && rself
                    .proto_paused
                    .compare_exchange(false, true, atomic::Ordering::Relaxed, atomic::Ordering::Relaxed)
                    .is_ok()
            {
                Self::proto_pause(pyself, py);
            }
        }

        Ok(())
    }

    fn proto_pause(pyself: &Py<Self>, py: Python) {
        let rself = pyself.borrow(py);
        if let Err(err) = rself.proto.call_method0(py, pyo3::intern!(py, "pause_writing")) {
            let err_ctx = LogExc::transport(
                err,
                "protocol.pause_writing() failed".into(),
                rself.proto.clone_ref(py),
                pyself.clone_ref(py).into_any(),
            );
            _ = rself.pyloop.get().log_exception(py, err_ctx);
        }
    }

    fn proto_resume(pyself: &Py<Self>, py: Python) {
        let rself = pyself.borrow(py);
        if let Err(err) = rself.proto.call_method0(py, pyo3::intern!(py, "resume_writing")) {
            let err_ctx = LogExc::transport(
                err,
                "protocol.resume_writing() failed".into(),
                rself.proto.clone_ref(py),
                pyself.clone_ref(py).into_any(),
            );
            _ = rself.pyloop.get().log_exception(py, err_ctx);
        }
    }
}

#[pymethods]
impl SSLTransport {
    #[pyo3(signature = (name, default = None))]
    fn get_extra_info(&self, py: Python, name: &str, default: Option<Py<PyAny>>) -> Option<Py<PyAny>> {
        match name {
            "socket" => Some(self.sock.clone_ref(py).into_any()),
            "sockname" => self.sock.call_method0(py, pyo3::intern!(py, "getsockname")).ok(),
            "peername" => self.sock.call_method0(py, pyo3::intern!(py, "getpeername")).ok(),
            "sslcontext" => Some(py.None()), // TODO: return actual SSL context
            "peercert" => Some(py.None()),   // TODO: return peer certificate
            _ => self.extra.get(name).map(|v| v.clone_ref(py)).or(default),
        }
    }

    fn is_closing(&self) -> bool {
        self.closing.load(atomic::Ordering::Relaxed)
    }

    pub fn close(&self, py: Python) {
        if self
            .closing
            .compare_exchange(false, true, atomic::Ordering::Relaxed, atomic::Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        let event_loop = self.pyloop.get();
        event_loop.ssl_stream_rem(self.fd, Interest::READABLE);
        if self.state.borrow().write_buf_dsize == 0 {
            event_loop.ssl_stream_rem(self.fd, Interest::WRITABLE);
            self.call_conn_lost(py, None);
        }
    }

    fn set_protocol(&self, _protocol: Py<PyAny>) -> PyResult<()> {
        Err(pyo3::exceptions::PyNotImplementedError::new_err(
            "SSLTransport protocol cannot be changed",
        ))
    }

    fn get_protocol(&self, py: Python) -> Py<PyAny> {
        self.proto.clone_ref(py)
    }

    fn is_reading(&self) -> bool {
        !self.closing.load(atomic::Ordering::Relaxed) && !self.paused.load(atomic::Ordering::Relaxed)
    }

    fn pause_reading(&self) {
        if self.closing.load(atomic::Ordering::Relaxed) {
            return;
        }
        if self
            .paused
            .compare_exchange(false, true, atomic::Ordering::Relaxed, atomic::Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        self.pyloop.get().ssl_stream_rem(self.fd, Interest::READABLE);
    }

    fn resume_reading(&self) {
        if self.closing.load(atomic::Ordering::Relaxed) {
            return;
        }
        if self
            .paused
            .compare_exchange(true, false, atomic::Ordering::Relaxed, atomic::Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        self.pyloop.get().ssl_stream_add(self.fd, Interest::READABLE);
    }

    #[pyo3(signature = (high = None, low = None))]
    fn set_write_buffer_limits(pyself: Py<Self>, py: Python, high: Option<usize>, low: Option<usize>) -> PyResult<()> {
        let wh = match high {
            None => match low {
                None => 1024 * 64,
                Some(v) => v * 4,
            },
            Some(v) => v,
        };
        let wl = match low {
            None => wh / 4,
            Some(v) => v,
        };

        if wh < wl {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "high must be >= low must be >= 0",
            ));
        }

        let rself = pyself.borrow(py);
        rself.water_hi.store(wh, atomic::Ordering::Relaxed);
        rself.water_lo.store(wl, atomic::Ordering::Relaxed);

        if rself.state.borrow().write_buf_dsize > wh
            && rself
                .proto_paused
                .compare_exchange(false, true, atomic::Ordering::Relaxed, atomic::Ordering::Relaxed)
                .is_ok()
        {
            Self::proto_pause(&pyself, py);
        }

        Ok(())
    }

    fn get_write_buffer_size(&self) -> usize {
        self.state.borrow().write_buf_dsize
    }

    fn get_write_buffer_limits(&self) -> (usize, usize) {
        (
            self.water_lo.load(atomic::Ordering::Relaxed),
            self.water_hi.load(atomic::Ordering::Relaxed),
        )
    }

    fn write(pyself: Py<Self>, py: Python, data: Cow<[u8]>) -> PyResult<()> {
        Self::try_write(&pyself, py, &data)
    }

    fn writelines(pyself: Py<Self>, py: Python, data: &Bound<PyAny>) -> PyResult<()> {
        let pybytes = PyBytes::new(py, &[0; 0]);
        let pybytesj = pybytes.call_method1(pyo3::intern!(py, "join"), (data,))?;
        let bytes = pybytesj.extract::<Cow<[u8]>>()?;
        Self::try_write(&pyself, py, &bytes)
    }

    fn write_eof(&self) {
        if self.closing.load(atomic::Ordering::Relaxed) {
            return;
        }
        if self
            .weof
            .compare_exchange(false, true, atomic::Ordering::Relaxed, atomic::Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        let mut state = self.state.borrow_mut();
        if state.write_buf_dsize == 0 {
            _ = state.ssl_stream.shutdown();
        }
    }

    fn can_write_eof(&self) -> bool {
        true
    }

    pub fn abort(&self, py: Python) {
        if self.state.borrow().write_buf_dsize > 0 {
            self.pyloop.get().ssl_stream_rem(self.fd, Interest::WRITABLE);
        }
        if self
            .closing
            .compare_exchange(false, true, atomic::Ordering::Relaxed, atomic::Ordering::Relaxed)
            .is_ok()
        {
            self.pyloop.get().ssl_stream_rem(self.fd, Interest::READABLE);
        }
        self.call_conn_lost(py, None);
    }
}

pub(crate) struct SSLReadHandle {
    pub fd: usize,
}

impl SSLReadHandle {
    #[inline]
    fn recv_direct(&self, py: Python, transport: &SSLTransport, buf: &mut [u8]) -> (Option<Py<PyAny>>, bool) {
        let (read, closed) = self.read_into(&mut transport.state.borrow_mut().ssl_stream, buf);
        if read > 0 {
            let rbuf = &buf[..read];
            let pydata = unsafe { PyBytes::from_ptr(py, rbuf.as_ptr(), read) };
            return (Some(pydata.into_any().unbind()), closed);
        }
        (None, closed)
    }

    #[inline]
    fn recv_buffered(&self, py: Python, transport: &SSLTransport) -> (Option<Py<PyAny>>, bool) {
        let pybuf: PyBuffer<u8> = PyBuffer::get(&transport.protom_buf_get.bind(py).call1((-1,)).unwrap()).unwrap();
        let mut vbuf = pybuf.to_vec(py).unwrap();
        let (read, closed) = self.read_into(&mut transport.state.borrow_mut().ssl_stream, vbuf.as_mut_slice());
        if read > 0 {
            _ = pybuf.copy_from_slice(py, &vbuf[..]);
            return (Some(read.into_py_any(py).unwrap()), closed);
        }
        (None, closed)
    }

    #[inline(always)]
    fn read_into(&self, ssl_stream: &mut SslStream<std::net::TcpStream>, buf: &mut [u8]) -> (usize, bool) {
        let mut len = 0;
        let mut closed = false;

        loop {
            match ssl_stream.read(&mut buf[len..]) {
                Ok(0) => {
                    if len < buf.len() {
                        closed = true;
                    }
                    break;
                }
                Ok(readn) => len += readn,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break, // For now, break on any error including SSL handshake issues
            }
        }

        (len, closed)
    }

    #[inline]
    fn recv_eof(&self, py: Python, event_loop: &EventLoop, transport: &SSLTransport) -> bool {
        event_loop.ssl_stream_rem(self.fd, Interest::READABLE);
        if let Ok(pyr) = transport.proto.call_method0(py, pyo3::intern!(py, "eof_received"))
            && let Ok(true) = pyr.is_truthy(py)
        {
            return false;
        }
        transport.close_from_read_handle(py, event_loop)
    }
}

impl crate::handles::Handle for SSLReadHandle {
    fn run(&self, py: Python, event_loop: &EventLoop, state: &mut crate::event_loop::EventLoopRunState) {
        if let Some(pytransport) = event_loop.get_ssl_transport(self.fd, py) {
            let transport = pytransport.borrow(py);

            let mut close = false;
            loop {
                let (data, eof) = match transport.proto_buffered {
                    true => self.recv_buffered(py, &transport),
                    false => self.recv_direct(py, &transport, &mut state.read_buf),
                };

                if let Some(data) = data {
                    _ = transport.protom_recv_data.call1(py, (data,));
                    if !eof {
                        continue;
                    }
                }

                if eof {
                    close = self.recv_eof(py, event_loop, &transport);
                }

                break;
            }

            if close {
                event_loop.ssl_stream_close(py, self.fd);
            }
        }
    }
}

pub(crate) struct SSLWriteHandle {
    pub fd: usize,
}

impl SSLWriteHandle {
    #[inline]
    fn write(&self, transport: &SSLTransport) -> Option<usize> {
        let mut ret = 0;
        let mut state = transport.state.borrow_mut();
        while let Some(data) = state.write_buf.pop_front() {
            match state.ssl_stream.write(&data) {
                Ok(written) if (written as usize) < data.len() => {
                    let written = written as usize;
                    state.write_buf.push_front((&data[written..]).into());
                    ret += written;
                    break;
                }
                Ok(written) => ret += written as usize,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {
                    state.write_buf.push_front(data);
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    state.write_buf.push_front(data);
                    break;
                }
                _ => {
                    state.write_buf.clear();
                    state.write_buf_dsize = 0;
                    return None;
                }
            }
        }
        state.write_buf_dsize -= ret;
        Some(ret)
    }
}

impl crate::handles::Handle for SSLWriteHandle {
    fn run(&self, py: Python, event_loop: &EventLoop, _state: &mut crate::event_loop::EventLoopRunState) {
        if let Some(pytransport) = event_loop.get_ssl_transport(self.fd, py) {
            let transport = pytransport.borrow(py);
            let stream_close;

            if let Some(written) = self.write(&transport) {
                if written > 0 {
                    SSLTransport::write_buf_size_decr(&pytransport, py);
                }
                stream_close = match transport.state.borrow().write_buf.is_empty() {
                    true => transport.close_from_write_handle(py, false),
                    false => None,
                };
            } else {
                stream_close = transport.close_from_write_handle(py, true);
            }

            if transport.state.borrow().write_buf.is_empty() {
                event_loop.ssl_stream_rem(self.fd, Interest::WRITABLE);
            }

            match stream_close {
                Some(true) => event_loop.ssl_stream_close(py, self.fd),
                Some(false) => {
                    _ = transport.state.borrow_mut().ssl_stream.shutdown();
                }
                _ => {}
            }
        }
    }
}

pub(crate) fn init_pymodule(module: &Bound<PyModule>) -> PyResult<()> {
    module.add_class::<SSLTransport>()?;
    Ok(())
}
