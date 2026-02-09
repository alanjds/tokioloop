macro_rules! syscall {
    ($fn: ident ( $($arg: expr),* $(,)* ) ) => {{
        let res = unsafe { libc::$fn($($arg, )*) };
        if res < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }};
}

pub(crate) use syscall;

use anyhow::Result;
use pyo3::prelude::*;

use std::os::fd::FromRawFd;

use socket2::Socket;

pub(crate) fn _try_socket2_conversion(fd: i32, backlog: i32) -> Result<socket2::Socket, PyErr> {
    log::trace!("Socket conversion via socket2: fd={}", fd);
    // Convert fd to socket2 Socket
    let socket = unsafe { Socket::from_raw_fd(fd) };
    // Ensure socket is in correct state
    socket.set_nonblocking(true)
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!("Failed to set socket nonblocking: {}", e)))?;
    log::trace!("Socket conversion via socket2 successful");
    // Set backlog if socket is not already listening
    if backlog != 0 {
        if let Err(e) = socket.listen(backlog) {
            log::debug!("Socket conversion: listen failed (might already be listening): {}", e);
            // Don't fail if already listening, just continue
        }
    }
    Ok(socket)
}
