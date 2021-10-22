use std::{
    io::{self, ErrorKind},
    mem,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream as StdTcpStream},
    ops::{Deref, DerefMut},
    os::windows::io::{AsRawSocket, FromRawSocket, IntoRawSocket},
    pin::Pin,
    ptr,
    task::{self, Poll},
};

use futures::ready;
use log::{error, warn};
use once_cell::sync::Lazy;
use pin_project::pin_project;
use socket2::SockAddr;
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
            closesocket,
            setsockopt,
            socket,
            WSAGetLastError,
            WSAGetOverlappedResult,
            WSAIoctl,
            INVALID_SOCKET,
            SOCKET,
            SOCKET_ERROR,
            SOCK_STREAM,
            SOL_SOCKET,
            WSA_IO_INCOMPLETE,
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

        warn!("Failed to get ConnectEx function from WSA extension, error: {}", e);
    }

    let _ = closesocket(socket);

    connectex
});

enum TcpStreamState {
    Connected,
    FastOpenConnect(SocketAddr),
    FastOpenConnecting(Box<OVERLAPPED>),
}

// unsafe: OVERLAPPED can be sent between threads
unsafe impl Send for TcpStreamState {}
unsafe impl Sync for TcpStreamState {}

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
            match addr.ip() {
                IpAddr::V4(..) => socket.bind(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0))?,
                IpAddr::V6(..) => socket.bind(SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0))?,
            }
        }

        let stream = TokioTcpStream::from_std(unsafe { StdTcpStream::from_raw_socket(socket.into_raw_socket()) })?;

        Ok(TcpStream {
            inner: stream,
            state: TcpStreamState::FastOpenConnect(addr),
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
            let TcpStreamProj { inner, state } = self.as_mut().project();

            match *state {
                TcpStreamState::Connected => return inner.poll_write(cx, buf),

                TcpStreamState::FastOpenConnect(addr) => {
                    let saddr = SockAddr::from(addr);

                    unsafe {
                        // https://docs.microsoft.com/en-us/windows/win32/api/mswsock/nc-mswsock-lpfn_connectex
                        let connect_ex = PFN_CONNECTEX_OPT
                            .expect("LPFN_CONNECTEX function doesn't exist. It is only supported after Windows 10");

                        let sock = inner.as_raw_socket() as SOCKET;

                        let mut overlapped: Box<OVERLAPPED> = Box::new(mem::zeroed());

                        let mut bytes_sent: DWORD = 0;
                        let ret: BOOL = connect_ex(
                            sock,
                            saddr.as_ptr(),
                            saddr.len() as c_int,
                            buf.as_ptr() as PVOID,
                            buf.len() as DWORD,
                            &mut bytes_sent as LPDWORD,
                            overlapped.as_mut() as LPOVERLAPPED,
                        );

                        if ret == TRUE {
                            // Connected successfully.

                            // Make getpeername() works
                            set_update_connect_context(sock)?;

                            debug_assert!(bytes_sent as usize <= buf.len());

                            *state = TcpStreamState::Connected;
                            return Ok(bytes_sent as usize).into();
                        }

                        let err = WSAGetLastError();
                        if err != ERROR_IO_PENDING as c_int {
                            return Err(io::Error::from_raw_os_error(err)).into();
                        }

                        // ConnectEx pending (ERROR_IO_PENDING), check later in FastOpenConnecting
                        *state = TcpStreamState::FastOpenConnecting(overlapped);
                    }
                }

                TcpStreamState::FastOpenConnecting(ref mut overlapped) => {
                    let stream = inner.get_mut();

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
        self.project().inner.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<io::Result<()>> {
        self.project().inner.poll_shutdown(cx)
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
