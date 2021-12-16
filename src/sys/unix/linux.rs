use std::{
    io::{self, ErrorKind},
    mem,
    net::{SocketAddr, TcpStream as StdTcpStream},
    os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd},
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{self, Poll, Waker},
    time::Duration,
};

use futures::ready;
use log::error;
use pin_project::pin_project;
use socket2::{SockAddr, Socket};
use tokio::{
    io::{AsyncRead, AsyncWrite, Interest, ReadBuf},
    net::{TcpSocket, TcpStream as TokioTcpStream},
};

use crate::sys::socket_take_error;

#[derive(Clone, Copy, Debug)]
enum TcpStreamState {
    Connected,
    FastOpenConnect,
    FastOpenConnecting,
    FastOpenWrite,
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
        let mut connected = false;

        // TFO in Linux was supported since 3.7
        //
        // But TCP_FASTOPEN_CONNECT was supported since 4.1, so we have to be compatible with it
        static SUPPORT_TCP_FASTOPEN_CONNECT: AtomicBool = AtomicBool::new(true);
        if SUPPORT_TCP_FASTOPEN_CONNECT.load(Ordering::Relaxed) {
            unsafe {
                let enable: libc::c_int = 1;

                let ret = libc::setsockopt(
                    socket.as_raw_fd(),
                    libc::IPPROTO_TCP,
                    libc::TCP_FASTOPEN_CONNECT,
                    &enable as *const _ as *const libc::c_void,
                    mem::size_of_val(&enable) as libc::socklen_t,
                );

                if ret != 0 {
                    let err = io::Error::last_os_error();
                    if let Some(libc::ENOPROTOOPT) = err.raw_os_error() {
                        // `TCP_FASTOPEN_CONNECT` is not supported, maybe kernel version < 4.11
                        // Fallback to `sendto` with `MSG_FASTOPEN` (Supported after 3.7)
                        SUPPORT_TCP_FASTOPEN_CONNECT.store(false, Ordering::Relaxed);
                    } else {
                        error!("set TCP_FASTOPEN_CONNECT error: {}", err);
                        return Err(err);
                    }
                } else {
                    connected = true;
                }
            }
        }

        if connected {
            Ok(TcpStream {
                // call connect() if TCP_FASTOPEN_CONNECT is set
                state: TcpStreamState::FastOpenWrite,
                stream: TcpStreamOption::Connected(socket.connect(addr).await?),
            })
        } else {
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
                    // Fallback mode. Must be kernal < 4.11
                    //
                    // Uses sendto as BSD-like systems

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
                            libc::MSG_FASTOPEN,
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

                TcpStreamState::FastOpenWrite => {
                    // First `write` after `TCP_FASTOPEN_CONNECT`
                    // Kernel >= 4.11

                    let stream = stream.connected();

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
                                // EINPROGRESS
                                if let Some(libc::EINPROGRESS) = err.raw_os_error() {
                                    // For non-blocking socket, it returns the number of bytes queued (and transmitted in the SYN-data packet) no matter cookie is available or not.
                                    // When calling write() with an empty buffer, it transmits a data-less SYN packet with Fast Open cookie request option and returns -EINPROGRESS like connect().
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
                                *state = TcpStreamState::FastOpenConnecting;
                            }
                        }
                        Err(err) => return Err(err).into(),
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
/// `TCP_FASTOPEN` was supported since Linux 3.7
pub fn set_tcp_fastopen<S: AsRawFd>(socket: &S) -> io::Result<()> {
    // https://lwn.net/Articles/508865/
    //
    // The option value, qlen, specifies this server's limit on the size of the queue of TFO requests that have
    // not yet completed the three-way handshake (see the remarks on prevention of resource-exhaustion attacks above).
    //
    // It was recommended to be `5` in this document.
    //
    // But since mio's TcpListener sets backlogs to 1024, it would be nice to have 1024 slots for handshaking TFO requests.
    let queue: libc::c_int = 1024;

    unsafe {
        let ret = libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_FASTOPEN,
            &queue as *const _ as *const libc::c_void,
            mem::size_of_val(&queue) as libc::socklen_t,
        );

        if ret != 0 {
            let err = io::Error::last_os_error();
            error!("set TCP_FASTOPEN error: {}", err);
            return Err(err);
        }
    }

    Ok(())
}
