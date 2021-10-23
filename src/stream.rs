//! TFO stream

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, RawSocket};
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use pin_project::pin_project;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpSocket, TcpStream as TokioTcpStream},
};

use crate::sys::TcpStream as SysTcpStream;

/// TCP stream connecting remote server with TFO
#[pin_project]
pub struct TfoStream {
    #[pin]
    inner: SysTcpStream,
}

impl TfoStream {
    /// Connect to `addr` with TCP Fast Open
    ///
    /// `connect` errors may be returned from write operations like `poll_write` because
    /// if TFO cookie is available, the actual `SYN` packet will be sent with data when
    /// calling `poll_write`.
    ///
    /// On some platforms, this is a no-op method only recording the target address.
    pub async fn connect(addr: SocketAddr) -> io::Result<TfoStream> {
        SysTcpStream::connect(addr).await.map(|inner| TfoStream { inner })
    }

    /// Connect to `addr` with half constructed `TcpSocket`
    ///
    /// This is for customizing the whole process of `connect`, users could set any flags
    /// before performing the actual TFO `connect`.
    pub async fn connect_with_socket(socket: TcpSocket, addr: SocketAddr) -> io::Result<TfoStream> {
        SysTcpStream::connect_with_socket(socket, addr)
            .await
            .map(|inner| TfoStream { inner })
    }

    /// Returns the local address that this stream is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    /// Returns the remote address that this stream is connected to.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.inner.peer_addr()
    }

    /// Gets the value of the `TCP_NODELAY` option on this socket.
    pub fn nodelay(&self) -> io::Result<bool> {
        self.inner.nodelay()
    }

    /// Sets the value of the `TCP_NODELAY` option on this socket.
    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        self.inner.set_nodelay(nodelay)
    }

    /// Reads the linger duration for this socket by getting the `SO_LINGER`
    /// option.
    pub fn linger(&self) -> io::Result<Option<Duration>> {
        self.inner.linger()
    }

    /// Sets the linger duration of this socket by setting the SO_LINGER option.
    pub fn set_linger(&self, dur: Option<Duration>) -> io::Result<()> {
        self.inner.set_linger(dur)
    }

    /// Gets the value of the `IP_TTL` option for this socket.
    pub fn ttl(&self) -> io::Result<u32> {
        self.inner.ttl()
    }

    /// Sets the value for the `IP_TTL` option on this socket.
    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        self.inner.set_ttl(ttl)
    }
}

impl AsyncRead for TfoStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        self.project().inner.poll_read(cx, buf)
    }
}

impl AsyncWrite for TfoStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.project().inner.poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.project().inner.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.project().inner.poll_shutdown(cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.project().inner.poll_write_vectored(cx, bufs)
    }
}

impl From<TokioTcpStream> for TfoStream {
    fn from(s: TokioTcpStream) -> Self {
        TfoStream {
            inner: SysTcpStream::from(s),
        }
    }
}

#[cfg(unix)]
impl AsRawFd for TfoStream {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

#[cfg(windows)]
impl AsRawSocket for TfoStream {
    fn as_raw_socket(&self) -> RawSocket {
        self.inner.as_raw_socket()
    }
}
