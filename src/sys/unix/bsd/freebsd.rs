use std::{
    io,
    mem,
    net::{SocketAddr, TcpStream as StdTcpStream},
    os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd},
    pin::Pin,
    task::{self, Poll, Waker},
    time::Duration,
};

use futures::ready;
use log::error;
use pin_project::pin_project;
use socket2::{SockAddr, Socket};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpSocket, TcpStream as TokioTcpStream},
};

use crate::sys::socket_take_error;

#[derive(Clone, Copy, Debug)]
enum TcpStreamState {
    Connected,
    FastOpenConnect,
    FastOpenConnecting,
}

#[pin_project(project = TcpStreamOptionProj)]
enum TcpStreamOption {
    Connected(#[pin] TokioTcpStream),
    Connecting {
        socket: TcpSocket,
        addr: SocketAddr,
        reader: Option<Waker>,
    },
    Empty,
}

impl TcpStreamOption {
    #[inline]
    fn connected(self: Pin<&mut Self>) -> Pin<&mut TokioTcpStream> {
        match self.project() {
            TcpStreamOptionProj::Connected(stream) => stream,
            _ => unreachable!("stream connected without a TcpStream instance"),
        }
    }
}

/// A `TcpStream` that supports TFO (TCP Fast Open)
#[pin_project(project = TcpStreamProj)]
pub struct TcpStream {
    state: TcpStreamState,
    #[pin]
    stream: TcpStreamOption,
}

macro_rules! call_socket_api {
    ($self:ident . $name:ident ( $($param:expr),* )) => {{
        let socket = unsafe { Socket::from_raw_fd($self.as_raw_fd()) };
        let result = socket.$name($($param,)*);
        socket.into_raw_fd();
        result
    }};
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

        Ok(TcpStream {
            // call sendto() with MSG_FASTOPEN in poll_write
            state: TcpStreamState::FastOpenConnect,
            stream: TcpStreamOption::Connecting {
                socket,
                addr,
                reader: None,
            },
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        call_socket_api!(self.local_addr()).map(|s| s.as_socket().unwrap())
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        match self.stream {
            TcpStreamOption::Connected(ref s) => s.peer_addr(),
            TcpStreamOption::Connecting { addr, .. } => Ok(addr),
            _ => unreachable!("stream must be either connecting or connected"),
        }
    }

    pub fn nodelay(&self) -> io::Result<bool> {
        call_socket_api!(self.nodelay())
    }

    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        call_socket_api!(self.set_nodelay(nodelay))
    }

    pub fn linger(&self) -> io::Result<Option<Duration>> {
        call_socket_api!(self.linger())
    }

    pub fn set_linger(&self, dur: Option<Duration>) -> io::Result<()> {
        call_socket_api!(self.set_linger(dur))
    }

    pub fn ttl(&self) -> io::Result<u32> {
        call_socket_api!(self.ttl())
    }

    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        call_socket_api!(self.set_ttl(ttl))
    }
}

impl From<TokioTcpStream> for TcpStream {
    fn from(s: TokioTcpStream) -> Self {
        TcpStream {
            state: TcpStreamState::Connected,
            stream: TcpStreamOption::Connected(s),
        }
    }
}

impl AsyncRead for TcpStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut task::Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.project();

        match this.stream.project() {
            TcpStreamOptionProj::Connected(stream) => stream.poll_read(cx, buf),
            TcpStreamOptionProj::Connecting { reader, .. } => {
                if let Some(w) = reader.take() {
                    w.wake();
                }
                *reader = Some(cx.waker().clone());
                Poll::Pending
            }
            TcpStreamOptionProj::Empty => unreachable!("stream must be either connecting or connected"),
        }
    }
}

impl AsyncWrite for TcpStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        loop {
            let TcpStreamProj { state, mut stream } = self.as_mut().project();

            match *state {
                TcpStreamState::Connected => return stream.connected().poll_write(cx, buf),

                TcpStreamState::FastOpenConnecting => {
                    // Waiting for `connect` finish if `connect` returns EINPROGRESS

                    let stream = stream.connected();
                    ready!(stream.poll_write_ready(cx))?;

                    // Get SO_ERROR checking `connect` error.
                    match socket_take_error(stream.get_mut()) {
                        Ok(Some(err)) | Err(err) => return Err(err).into(),
                        _ => {}
                    }

                    *state = TcpStreamState::Connected;
                }

                TcpStreamState::FastOpenConnect => {
                    // TCP_FASTOPEN was supported since FreeBSD 12.0
                    //
                    // Example program:
                    // <https://people.freebsd.org/~pkelsey/tfo-tools/tfo-client.c>

                    let ret = unsafe {
                        let (socket, addr) = match stream.as_mut().project() {
                            TcpStreamOptionProj::Connecting { socket, addr, .. } => (socket, *addr),
                            _ => unreachable!("stream connecting without address"),
                        };

                        let saddr = SockAddr::from(addr);

                        libc::sendto(
                            socket.as_raw_fd(),
                            buf.as_ptr() as *const libc::c_void,
                            buf.len(),
                            0, // Yes, BSD doesn't need MSG_FASTOPEN
                            saddr.as_ptr(),
                            saddr.len(),
                        )
                    };

                    if ret >= 0 {
                        // Connected to remote with TFO successfully with `ret` bytes of data sent

                        let new_stream = TcpStreamOption::Empty;
                        let old_stream = mem::replace(&mut *stream, new_stream);

                        let (socket, mut reader) = match old_stream {
                            TcpStreamOption::Connecting { socket, reader, .. } => (socket, reader),
                            _ => unreachable!("stream connecting without address"),
                        };

                        *stream = TcpStreamOption::Connected(TokioTcpStream::from_std(unsafe {
                            StdTcpStream::from_raw_fd(socket.into_raw_fd())
                        })?);
                        *state = TcpStreamState::Connected;

                        // Wake up the Future that pending on poll_read
                        if let Some(w) = reader.take() {
                            w.wake();
                        }

                        return Ok(ret as usize).into();
                    } else {
                        // Error occurs
                        let err = io::Error::last_os_error();

                        // EINPROGRESS
                        if let Some(libc::EINPROGRESS) = err.raw_os_error() {
                            // For non-blocking socket, it returns the number of bytes queued (and transmitted in the SYN-data packet) if cookie is available.
                            // If cookie is not available, it transmits a data-less SYN packet with Fast Open cookie request option and returns -EINPROGRESS like connect().
                            //
                            // So in this state. We have to loop again to call `poll_write` for sending the first packet.

                            let new_stream = TcpStreamOption::Empty;
                            let old_stream = mem::replace(&mut *stream, new_stream);

                            let (socket, mut reader) = match old_stream {
                                TcpStreamOption::Connecting { socket, reader, .. } => (socket, reader),
                                _ => unreachable!("stream connecting without address"),
                            };

                            // Register it into tokio's poll waiting for writable event (connected successfully).

                            *stream = TcpStreamOption::Connected(TokioTcpStream::from_std(unsafe {
                                StdTcpStream::from_raw_fd(socket.into_raw_fd())
                            })?);
                            *state = TcpStreamState::FastOpenConnecting;

                            // Wake up the Future that pending on poll_read
                            if let Some(w) = reader.take() {
                                w.wake();
                            }
                        } else {
                            // Other errors, including EAGAIN, EWOULDBLOCK
                            return Err(err).into();
                        }
                    }
                }
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<io::Result<()>> {
        match self.project().stream.project() {
            TcpStreamOptionProj::Connected(stream) => stream.poll_flush(cx),
            _ => Ok(()).into(),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<io::Result<()>> {
        match self.project().stream.project() {
            TcpStreamOptionProj::Connected(stream) => stream.poll_shutdown(cx),
            _ => Ok(()).into(),
        }
    }
}

impl AsRawFd for TcpStream {
    fn as_raw_fd(&self) -> RawFd {
        match self.stream {
            TcpStreamOption::Connected(ref s) => s.as_raw_fd(),
            TcpStreamOption::Connecting { ref socket, .. } => socket.as_raw_fd(),
            _ => unreachable!("stream connected without a TcpStream instance"),
        }
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
