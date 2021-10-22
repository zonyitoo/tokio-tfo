use std::{
    io::{self, ErrorKind},
    mem,
    net::{SocketAddr, TcpStream as StdTcpStream},
    ops::{Deref, DerefMut},
    os::unix::io::{AsRawFd, FromRawFd, IntoRawFd},
    pin::Pin,
    ptr,
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
    FastOpenWrite,
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

        // TFO in macos uses connectx

        unsafe {
            let raddr = SockAddr::from(addr);

            let mut endpoints: libc::sa_endpoints_t = mem::zeroed();
            endpoints.sae_dstaddr = raddr.as_ptr();
            endpoints.sae_dstaddrlen = raddr.len();

            let ret = libc::connectx(
                socket.as_raw_fd(),
                &endpoints as *const _,
                libc::SAE_ASSOCID_ANY,
                libc::CONNECT_DATA_IDEMPOTENT /* Enable TFO */ | libc::CONNECT_RESUME_ON_READ_WRITE, /* Send SYN with subsequence send/recv */
                ptr::null(),
                0,
                ptr::null_mut(),
                ptr::null_mut(),
            );

            if ret != 0 {
                return Err(io::Error::last_os_error());
            }
        }

        let stream = TokioTcpStream::from_std(unsafe { StdTcpStream::from_raw_fd(socket.into_raw_fd()) })?;

        Ok(TcpStream {
            inner: stream,
            state: TcpStreamState::FastOpenWrite,
        })
    }

    pub fn from_std(stream: StdTcpStream) -> io::Result<TcpStream> {
        TokioTcpStream::from_std(stream).map(|inner| TcpStream {
            inner,
            state: TcpStreamState::Connected,
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

                TcpStreamState::FastOpenWrite => {
                    // `CONNECT_RESUME_ON_READ_WRITE` is set when calling `connectx`,
                    // so the first call of `send` will perform the actual SYN with TFO cookie.
                    //
                    // (NOT SURE) If remote server doesn't support TFO or this is the first connection,
                    // it may return EINPROGRESS just like other platforms (Linux, FreeBSD).

                    let stream = inner.get_mut();

                    // Ensure socket is writable
                    ready!(stream.poll_write_ready(cx))?;

                    let mut connecting = false;
                    let send_result = stream.try_io(Interest::WRITABLE, || {
                        unsafe {
                            let ret = libc::send(stream.as_raw_fd(), buf.as_ptr() as *const libc::c_void, buf.len(), 0);
                            if ret >= 0 {
                                Ok(ret as usize)
                            } else {
                                let err = io::Error::last_os_error();
                                // EAGAIN and EWOULDBLOCK should have been handled by tokio
                                //
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
                                    // Other errors, including EAGAIN
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

/// Enable `TCP_FASTOPEN`
///
/// `TCP_FASTOPEN` was supported since
/// macosx(10.11), ios(9.0), tvos(9.0), watchos(2.0)
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
