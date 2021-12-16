use std::{
    io::{self, ErrorKind},
    mem,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream as StdTcpStream},
    os::windows::io::{AsRawSocket, FromRawSocket, IntoRawSocket, RawSocket},
    pin::Pin,
    ptr,
    task::{self, Poll, Waker},
    time::Duration,
};

use futures::ready;
use log::{error, warn};
use once_cell::sync::Lazy;
use pin_project::pin_project;
use socket2::{SockAddr, Socket};
use tokio::{
    io::{AsyncRead, AsyncWrite, Interest, ReadBuf},
    net::{TcpSocket, TcpStream as TokioTcpStream},
};
use winapi::{
    ctypes::{c_char, c_int},
    shared::{
        minwindef::{BOOL, DWORD, FALSE, LPDWORD, LPVOID, TRUE},
        winerror::ERROR_IO_PENDING,
        ws2def::{AF_INET, IPPROTO_TCP, SIO_GET_EXTENSION_FUNCTION_POINTER},
    },
    um::{
        minwinbase::{LPOVERLAPPED, OVERLAPPED},
        mswsock::{LPFN_CONNECTEX, SO_UPDATE_CONNECT_CONTEXT, WSAID_CONNECTEX},
        winnt::PVOID,
        winsock2::{
            closesocket, setsockopt, socket, WSAGetLastError, WSAGetOverlappedResult, WSAIoctl, INVALID_SOCKET, SOCKET,
            SOCKET_ERROR, SOCK_STREAM, SOL_SOCKET, WSAEINVAL, WSA_IO_INCOMPLETE,
        },
    },
};

// ws2ipdef.h
// FIXME: Use winapi's definition if issue resolved
// https://github.com/retep998/winapi-rs/issues/856
const TCP_FASTOPEN: DWORD = 15;

static PFN_CONNECTEX_OPT: Lazy<LPFN_CONNECTEX> = Lazy::new(|| unsafe {
    let socket = socket(AF_INET, SOCK_STREAM, 0);
    if socket == INVALID_SOCKET {
        return None;
    }

    let mut guid = WSAID_CONNECTEX;
    let mut num_bytes: DWORD = 0;

    let mut connectex: LPFN_CONNECTEX = None;

    let ret = WSAIoctl(
        socket,
        SIO_GET_EXTENSION_FUNCTION_POINTER,
        &mut guid as *mut _ as LPVOID,
        mem::size_of_val(&guid) as DWORD,
        &mut connectex as *mut _ as LPVOID,
        mem::size_of_val(&connectex) as DWORD,
        &mut num_bytes as *mut _,
        ptr::null_mut(),
        None,
    );

    if ret != 0 {
        let err = WSAGetLastError();
        let e = io::Error::from_raw_os_error(err);

        warn!("failed to get ConnectEx function from WSA extension, error: {}", e);
    }

    let _ = closesocket(socket);

    connectex
});

enum TcpStreamState {
    Connected,
    FastOpenConnect,
    FastOpenConnecting(Box<OVERLAPPED>),
}

// unsafe: OVERLAPPED can be sent between threads
unsafe impl Send for TcpStreamState {}
unsafe impl Sync for TcpStreamState {}

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
    #[pin]
    stream: TcpStreamOption,
    state: TcpStreamState,
}

macro_rules! call_socket_api {
    ($self:ident . $name:ident ( $($param:expr),* )) => {{
        let socket = unsafe { Socket::from_raw_socket($self.as_raw_socket()) };
        let result = socket.$name($($param,)*);
        socket.into_raw_socket();
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
        let sock = socket.as_raw_socket() as SOCKET;

        unsafe {
            // TCP_FASTOPEN was supported since Windows 10

            // Enable TCP_FASTOPEN option

            let enable: DWORD = 1;

            let ret = setsockopt(
                sock,
                IPPROTO_TCP as c_int,
                TCP_FASTOPEN as c_int,
                &enable as *const _ as *const c_char,
                mem::size_of_val(&enable) as c_int,
            );

            if ret == SOCKET_ERROR {
                let err = io::Error::from_raw_os_error(WSAGetLastError());
                error!("set TCP_FASTOPEN error: {}", err);
                return Err(err);
            }

            // Bind to a dummy address (required for TFO socket)
            let result = match addr.ip() {
                IpAddr::V4(..) => socket.bind(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0)),
                IpAddr::V6(..) => socket.bind(SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0)),
            };

            if let Err(err) = result {
                // https://docs.microsoft.com/en-us/windows/win32/api/winsock/nf-winsock-bind
                // This error is returned of the socket `s` is already bound to an address.
                if let Some(WSAEINVAL) = err.raw_os_error() {
                    // It is Ok if socket have already bound to an address.
                } else {
                    return Err(err);
                }
            }
        }

        Ok(TcpStream {
            stream: TcpStreamOption::Connecting {
                socket,
                addr,
                reader: None,
            },
            state: TcpStreamState::FastOpenConnect,
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
            stream: TcpStreamOption::Connected(s),
            state: TcpStreamState::Connected,
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

fn set_update_connect_context(sock: SOCKET) -> io::Result<()> {
    unsafe {
        // Make getpeername work
        // https://docs.microsoft.com/en-us/windows/win32/api/mswsock/nc-mswsock-lpfn_connectex
        let ret = setsockopt(sock, SOL_SOCKET, SO_UPDATE_CONNECT_CONTEXT, ptr::null(), 0);
        if ret == SOCKET_ERROR {
            let err = WSAGetLastError();
            return Err(io::Error::from_raw_os_error(err));
        }
    }

    Ok(())
}

impl AsyncWrite for TcpStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        loop {
            let TcpStreamProj { state, mut stream } = self.as_mut().project();

            match *state {
                TcpStreamState::Connected => return stream.connected().poll_write(cx, buf),

                TcpStreamState::FastOpenConnect => {
                    unsafe {
                        // https://docs.microsoft.com/en-us/windows/win32/api/mswsock/nc-mswsock-lpfn_connectex
                        let connect_ex = PFN_CONNECTEX_OPT
                            .expect("LPFN_CONNECTEX function doesn't exist. It is only supported since Windows 10");

                        let mut overlapped: Box<OVERLAPPED> = Box::new(mem::zeroed());
                        let mut bytes_sent: DWORD = 0;

                        let ret: BOOL = {
                            let (socket, addr) = match stream.as_mut().project() {
                                TcpStreamOptionProj::Connecting { socket, addr, .. } => (socket, *addr),
                                _ => unreachable!("stream connecting without address"),
                            };

                            let sock = socket.as_raw_socket() as SOCKET;
                            let saddr = SockAddr::from(addr);

                            connect_ex(
                                sock,
                                saddr.as_ptr(),
                                saddr.len() as c_int,
                                buf.as_ptr() as PVOID,
                                buf.len() as DWORD,
                                &mut bytes_sent as LPDWORD,
                                overlapped.as_mut() as LPOVERLAPPED,
                            )
                        };

                        if ret == TRUE {
                            // Connected successfully.

                            // Make getpeername() works

                            debug_assert!(bytes_sent as usize <= buf.len());

                            let new_stream = TcpStreamOption::Empty;
                            let old_stream = mem::replace(&mut *stream, new_stream);

                            let (socket, mut reader) = match old_stream {
                                TcpStreamOption::Connecting { socket, reader, .. } => (socket, reader),
                                _ => unreachable!("stream connecting without address"),
                            };

                            let sock = socket.as_raw_socket() as SOCKET;
                            set_update_connect_context(sock)?;

                            *stream = TcpStreamOption::Connected(TokioTcpStream::from_std(
                                StdTcpStream::from_raw_socket(socket.into_raw_socket()),
                            )?);
                            *state = TcpStreamState::Connected;

                            // Wake up the Future that pending on poll_read
                            if let Some(w) = reader.take() {
                                w.wake();
                            }

                            return Ok(bytes_sent as usize).into();
                        }

                        let err = WSAGetLastError();
                        if err != ERROR_IO_PENDING as c_int {
                            return Err(io::Error::from_raw_os_error(err)).into();
                        }

                        // ConnectEx pending (ERROR_IO_PENDING), check later in FastOpenConnecting
                        let new_stream = TcpStreamOption::Empty;
                        let old_stream = mem::replace(&mut *stream, new_stream);

                        let (socket, mut reader) = match old_stream {
                            TcpStreamOption::Connecting { socket, reader, .. } => (socket, reader),
                            _ => unreachable!("stream connecting without address"),
                        };

                        *stream = TcpStreamOption::Connected(TokioTcpStream::from_std(StdTcpStream::from_raw_socket(
                            socket.into_raw_socket(),
                        ))?);
                        *state = TcpStreamState::FastOpenConnecting(overlapped);

                        // Wake up the Future that pending on poll_read
                        if let Some(w) = reader.take() {
                            w.wake();
                        }
                    }
                }

                TcpStreamState::FastOpenConnecting(ref mut overlapped) => {
                    let stream = stream.connected();

                    ready!(stream.poll_write_ready(cx))?;

                    let write_result = stream.try_io(Interest::WRITABLE, || {
                        unsafe {
                            let sock = stream.as_raw_socket() as SOCKET;

                            let mut bytes_sent: DWORD = 0;
                            let mut flags: DWORD = 0;

                            // Fetch ConnectEx's result in a non-blocking way.
                            let ret: BOOL = WSAGetOverlappedResult(
                                sock,
                                overlapped.as_mut() as LPOVERLAPPED,
                                &mut bytes_sent as LPDWORD,
                                FALSE, // fWait = false, non-blocking, returns WSA_IO_INCOMPLETE
                                &mut flags as LPDWORD,
                            );

                            if ret == TRUE {
                                // Get ConnectEx's result successfully. Socket is connected

                                // Make getpeername() works
                                set_update_connect_context(sock)?;

                                debug_assert!(bytes_sent as usize <= buf.len());

                                return Ok(bytes_sent as usize);
                            }

                            let err = WSAGetLastError();
                            if err == WSA_IO_INCOMPLETE {
                                // ConnectEx is still not connected. Wait for the next round
                                //
                                // Let `try_io` clears the write readiness.
                                Err(ErrorKind::WouldBlock.into())
                            } else {
                                Err(io::Error::from_raw_os_error(err))
                            }
                        }
                    });

                    match write_result {
                        Ok(n) => {
                            // Check SO_ERROR after `connect`
                            if let Err(err) = socket_take_error(stream.get_mut()) {
                                return Err(err).into();
                            }

                            // Connect successfully with fast open
                            *state = TcpStreamState::Connected;
                            return Ok(n).into();
                        }
                        Err(ref err) if err.kind() == ErrorKind::WouldBlock => {
                            // Wait again for writable event.
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

impl AsRawSocket for TcpStream {
    fn as_raw_socket(&self) -> RawSocket {
        match self.stream {
            TcpStreamOption::Connected(ref s) => s.as_raw_socket(),
            TcpStreamOption::Connecting { ref socket, .. } => socket.as_raw_socket(),
            _ => unreachable!("stream connected without a TcpStream instance"),
        }
    }
}

/// Enable `TCP_FASTOPEN`
///
/// Program borrowed from
/// https://social.msdn.microsoft.com/Forums/en-US/94d1fe8e-4f17-4b28-89eb-1ac776a2e134/how-to-create-tcp-fast-open-connections-with-winsock-?forum=windowsgeneraldevelopmentissues
///
/// TCP_FASTOPEN document
/// https://docs.microsoft.com/en-us/windows/win32/winsock/ipproto-tcp-socket-options
///
/// TCP_FASTOPEN is supported since Windows 10
pub fn set_tcp_fastopen<S: AsRawSocket>(socket: &S) -> io::Result<()> {
    let enable: DWORD = 1;

    unsafe {
        let ret = setsockopt(
            socket.as_raw_socket() as SOCKET,
            IPPROTO_TCP as c_int,
            TCP_FASTOPEN as c_int,
            &enable as *const _ as *const c_char,
            mem::size_of_val(&enable) as c_int,
        );

        if ret == SOCKET_ERROR {
            let err = io::Error::from_raw_os_error(WSAGetLastError());
            error!("set TCP_FASTOPEN error: {}", err);
            return Err(err);
        }
    }

    Ok(())
}

pub(crate) fn socket_take_error<S: AsRawSocket>(fd: &S) -> io::Result<Option<io::Error>> {
    let socket = unsafe { Socket::from_raw_socket(fd.as_raw_socket()) };
    let result = socket.take_error();
    socket.into_raw_socket();
    result
}
