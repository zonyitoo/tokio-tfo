use std::{
    io,
    os::unix::io::{AsRawFd, FromRawFd, IntoRawFd},
};

use cfg_if::cfg_if;
use socket2::Socket;

cfg_if! {
    if #[cfg(any(target_os = "linux", target_os = "android"))] {
        mod linux;
        pub use self::linux::*;
    } else if #[cfg(any(target_os = "freebsd",
                        target_os = "openbsd",
                        target_os = "netbsd",
                        target_os = "dragonfly",
                        target_os = "macos",
                        target_os = "ios",
                        target_os = "watchos",
                        target_os = "tvos"))] {
        mod bsd;
        pub use self::bsd::*;
    } else {
        compile_error!("Doesn't support TFO on the current platform");
    }
}

pub(crate) fn socket_take_error<S: AsRawFd>(fd: &S) -> io::Result<Option<io::Error>> {
    let socket = unsafe { Socket::from_raw_fd(fd.as_raw_fd()) };
    let result = socket.take_error();
    socket.into_raw_fd();
    result
}
