use pyo3::{IntoPyObjectExt, prelude::*};

use std::{
    sync::{atomic, Arc},
    time::Duration,
};

use anyhow::Result;
use pyo3::prelude::*;
use tokio::{
    task::JoinHandle as TokioJoinHandle,
    time::sleep as tokio_sleep,
};

use crate::{
    tokio_event_loop::{TEventLoop, TEventLoopRunState, LoopHandlers},
    log::LogExc,
    py::{run_in_ctx, run_in_ctx0},
};

#[cfg(not(PyPy))]
use crate::py::run_in_ctx1;


pub trait THandle: Send + Sync {
    fn run(&self, py: Python, handlers: &LoopHandlers, state: &mut TEventLoopRunState);
    fn cancelled(&self) -> bool {
        false
    }
}

pub(crate) type TBoxedHandle = Box<dyn THandle + Send + Sync>;



#[pyclass(frozen, module = "rloop._rloop")]
pub(crate) struct TCBHandle {
    callback: Py<PyAny>,
    args: Py<PyAny>,
    context: Py<PyAny>,
    cancelled: atomic::AtomicBool,
}

#[pyclass(frozen, module = "rloop._rloop", name = "TCBHandle0")]
pub(crate) struct TCBHandleNoArgs {
    callback: Py<PyAny>,
    context: Py<PyAny>,
    cancelled: atomic::AtomicBool,
}

#[pyclass(frozen, module = "rloop._rloop", name = "TCBHandle1")]
pub(crate) struct TCBHandleOneArg {
    callback: Py<PyAny>,
    arg: Py<PyAny>,
    context: Py<PyAny>,
    cancelled: atomic::AtomicBool,
}

impl TCBHandle {
    pub(crate) fn new(callback: Py<PyAny>, args: Py<PyAny>, context: Py<PyAny>) -> Self {
        Self {
            callback,
            args,
            context,
            cancelled: false.into(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn new0(callback: Py<PyAny>, context: Py<PyAny>) -> TCBHandleNoArgs {
        TCBHandleNoArgs {
            callback,
            context,
            cancelled: false.into(),
        }
    }

    pub(crate) fn new1(callback: Py<PyAny>, arg: Py<PyAny>, context: Py<PyAny>) -> TCBHandleOneArg {
        TCBHandleOneArg {
            callback,
            arg,
            context,
            cancelled: false.into(),
        }
    }
}

macro_rules! tcbhandle_cancel_impl {
    ($handle:ident) => {
        #[pymethods]
        impl $handle {
            fn cancel(&self) {
                self.cancelled.store(true, atomic::Ordering::Relaxed);
            }
        }
    };
}

macro_rules! tcbhandle_cancelled_impl {
    () => {
        #[inline]
        fn cancelled(&self) -> bool {
            self.get().cancelled.load(atomic::Ordering::Relaxed)
        }
    };
}

tcbhandle_cancel_impl!(TCBHandle);
tcbhandle_cancel_impl!(TCBHandleNoArgs);
tcbhandle_cancel_impl!(TCBHandleOneArg);

impl THandle for Py<TCBHandle> {
    tcbhandle_cancelled_impl!();

    fn run(&self, py: Python, handlers: &LoopHandlers, _state: &mut TEventLoopRunState) {
        log::trace!("TCBHandle.run: starting");
        let rself = self.get();
        let ctx = rself.context.as_ptr();
        let cb = rself.callback.as_ptr();
        let args = rself.args.as_ptr();

        if let Err(err) = run_in_ctx!(py, ctx, cb, args) {
            log::trace!("TCBHandle.run: failed");
            let err_ctx = LogExc::cb_handle(
                err,
                format!("Exception in callback {:?}", rself.callback.bind(py)),
                self.clone_ref(py).into_py_any(py).unwrap(),
            );
            _ = handlers.log_exception(py, err_ctx);
        }
        log::trace!("TCBHandle.run: finished");
    }
}

impl THandle for Py<TCBHandleNoArgs> {
    tcbhandle_cancelled_impl!();

    fn run(&self, py: Python, handlers: &LoopHandlers, _state: &mut TEventLoopRunState) {
        log::trace!("TCBHandleNoArgs.run: starting");
        let rself = self.get();
        let ctx = rself.context.as_ptr();
        let cb = rself.callback.as_ptr();

        if let Err(err) = run_in_ctx0!(py, ctx, cb) {
            log::trace!("TCBHandleNoArgs.run: failed");
            let err_ctx = LogExc::cb_handle(
                err,
                format!("Exception in callback {:?}", rself.callback.bind(py)),
                self.clone_ref(py).into_py_any(py).unwrap(),
            );
            _ = handlers.log_exception(py, err_ctx);
        }
        log::trace!("TCBHandleNoArgs.run: finished");
    }
}

impl THandle for Py<TCBHandleOneArg> {
    #[cfg(not(PyPy))]
    fn run(&self, py: Python, handlers: &LoopHandlers, _state: &mut TEventLoopRunState) {
        log::trace!("TCBHandleOneArg.run: starting");
        let rself = self.get();
        let ctx = rself.context.as_ptr();
        let cb = rself.callback.as_ptr();
        let arg = rself.arg.as_ptr();

        if let Err(err) = run_in_ctx1!(py, ctx, cb, arg) {
            log::trace!("TCBHandleOneArg.run: failed");
            let err_ctx = LogExc::cb_handle(
                err,
                format!("Exception in callback {:?}", rself.callback.bind(py)),
                self.clone_ref(py).into_py_any(py).unwrap(),
            );
            _ = handlers.log_exception(py, err_ctx);
        }
        log::trace!("TCBHandleOneArg.run: finished");
    }

    #[cfg(PyPy)]
    fn run(&self, py: Python, handlers: &LoopHandlers, _state: &mut TEventLoopRunState) {
        log::trace!("TCBHandleOneArg.run: starting");
        let rself = self.get();
        let ctx = rself.context.as_ptr();
        let cb = rself.callback.as_ptr();
        let args = (rself.arg.clone_ref(py),).into_py_any(py).unwrap().into_ptr();

        if let Err(err) = run_in_ctx!(py, ctx, cb, args) {
            log::trace!("TCBHandleOneArg.run: failed");
            let err_ctx = LogExc::cb_handle(
                err,
                format!("Exception in callback {:?}", rself.callback.bind(py)),
                self.clone_ref(py).into_py_any(py).unwrap(),
            );
            _ = handlers.log_exception(py, err_ctx);
        }
        log::trace!("TCBHandleOneArg.run: finished");
    }
}

#[pyclass(frozen, module = "rloop._rloop")]
pub(crate) struct TTimerHandle {
    pub handle: Py<TCBHandle>,
    #[pyo3(get)]
    when: f64,
}

impl TTimerHandle {
    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn new(handle: Py<TCBHandle>, when: u128) -> Self {
        Self {
            handle,
            when: (when as f64) / 1_000_000.0,
        }
    }
}

#[pymethods]
impl TTimerHandle {
    fn cancel(&self) {
        self.handle.get().cancel();
    }

    fn cancelled(&self) -> bool {
        self.handle.cancelled()
    }
}


pub(crate) fn init_pymodule(module: &Bound<PyModule>) -> PyResult<()> {
    // module.add_class::<THandle>()?;
    module.add_class::<TTimerHandle>()?;
    module.add_class::<TCBHandle>()?;
    Ok(())
}
