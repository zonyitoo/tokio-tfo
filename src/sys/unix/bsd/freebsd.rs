use std::{
    io::{self, ErrorKind},
    mem,
    net::{SocketAddr, TcpStream as StdTcpStream},
    ops::{Deref, DerefMut},
    os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd},
    pin::Pin,
    task::{self, Poll},
};

use futures::ready;
use log::error;
use pin_project::pin_project;
use socket2::SockAddr;
use tokio::{
    io::{AsyncRead, AsyncWrite, Interest, ReadBuf},
    net::{TcpSocket, TcpStream as TokioTcpStream},
};

enum TcpStreamState {
    Connected,
    FastOpenConnect(SocketAddr),
}

/// A `TcpStream` that supports TFO (TCP Fast Open)
#[pin_project(project = TcpStreamProj)]
pub struct TcpStream {
    #[pin]
    inner: TokioTcpStream,
    state: TcpStreamState,
}

impl TcpStream {
    pub async fn connect(addr: SocketAddr) -> io::Result<TcpStream> {
        let socket = match addr {
            SocketAddr::V4(..) => TcpSocket::new_v4()?,
            SocketAddr::V6(..) => TcpSocket::new_v6()?,
        };

        TcpStream::connect_with_socket(socket, addr).await
    }

    pub async fn connect_with_socket(socket: TcpSocket, addr: SocketAddr) -> io::Result<TcpStream> {
        unsafe {
            let enable: libc::c_int = 1;

            let ret = libc::setsockopt(
                socket.as_raw_fd(),
                libc::IPPROTO_TCP,
                libc::TCP_FASTOPEN,
                &enable as *const _ as *const libc::c_void,
                mem::size_of_val(&enable) as libc::socklen_t,
            );

            if ret != 0 {
                let err = io::Error::last_os_error();
                error!("set TCP_FASTOPEN error: {}", err);
                return Err(err);
            }
        }

        let stream = TokioTcpStream::from_std(unsafe { StdTcpStream::from_raw_fd(socket.into_raw_fd()) })?;

        Ok(TcpStream {
            inner: stream,
            state: TcpStreamState::FastOpenConnect(addr),
        })
    }
}

impl Deref for TcpStream {
    type Target = TokioTcpStream;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for TcpStream {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl From<TokioTcpStream> for TcpStream {
    fn from(s: TokioTcpStream) -> Self {
        TcpStream {
            inner: s,
            state: TcpStreamState::Connected,
        }
    }
}

impl AsyncRead for TcpStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut task::Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        self.project().inner.poll_read(cx, buf)
    }
}

impl AsyncWrite for TcpStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        loop {
            let TcpStreamProj { inner, state } = self.as_mut().project();

            match *state {
                TcpStreamState::Connected => return inner.poll_write(cx, buf),

                TcpStreamState::FastOpenConnect(addr) => {
                    // TCP_FASTOPEN was supported since FreeBSD 12.0
                    //
                    // Example program:
                    // <https://people.freebsd.org/~pkelsey/tfo-tools/tfo-client.c>

                    let saddr = SockAddr::from(addr);

                    let stream = inner.get_mut();

                    let mut connecting = false;
                    let send_result = stream.try_io(Interest::WRITABLE, || {
                        unsafe {
                            let ret = libc::sendto(
                                stream.as_raw_fd(),
                                buf.as_ptr() as *const libc::c_void,
                                buf.len(),
                                0, // Yes, BSD doesn't need MSG_FASTOPEN
                                saddr.as_ptr(),
                                saddr.len(),
                            );

                            if ret >= 0 {
                                Ok(ret as usize)
                            } else {
                                // Error occurs
                                let err = io::Error::last_os_error();

                                // EINPROGRESS
                                if let Some(libc::EINPROGRESS) = err.raw_os_error() {
                                    // For non-blocking socket, it returns the number of bytes queued (and transmitted in the SYN-data packet) if cookie is available.
                                    // If cookie is not available, it transmits a data-less SYN packet with Fast Open cookie request option and returns -EINPROGRESS like connect().
                                    //
                                    // So in this state. We have to loop again to call `poll_write` for sending the first packet.
                                    connecting = true;

                                    // Let `poll_write_io` clears the write readiness.
                                    Err(ErrorKind::WouldBlock.into())
                                } else {
                                    // Other errors, including EAGAIN, EWOULDBLOCK
                                    Err(err)
                                }
                            }
                        }
                    });

                    match send_result {
                        Ok(n) => {
                            // Connected successfully with fast open
                            *state = TcpStreamState::Connected;
                            return Ok(n).into();
                        }
                        Err(ref err) if err.kind() == ErrorKind::WouldBlock => {
                            if connecting {
                                // Connecting with normal TCP handshakes, write the first packet after connected
                                *state = TcpStreamState::Connected;
                            }
                        }
                        Err(err) => return Err(err).into(),
                    }
                }
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<io::Result<()>> {
        self.project().inner.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<io::Result<()>> {
        self.project().inner.poll_shutdown(cx)
    }
}

impl AsRawFd for TcpStream {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

/// Enable `TCP_FASTOPEN`
///
/// TCP_FASTOPEN was supported since FreeBSD 12.0
///
/// Example program: <https://people.freebsd.org/~pkelsey/tfo-tools/tfo-srv.c>
pub fn set_tcp_fastopen<S: AsRawFd>(socket: &S) -> io::Result<()> {
    let enable: libc::c_int = 1;

    unsafe {
        let ret = libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_FASTOPEN,
            &enable as *const _ as *const libc::c_void,
            mem::size_of_val(&enable) as libc::socklen_t,
        );

        if ret != 0 {
            let err = io::Error::last_os_error();
            error!("set TCP_FASTOPEN error: {}", err);
            return Err(err);
        }
    }

    Ok(())
}
