use std::{
    io,
    net::{SocketAddr, TcpStream as StdTcpStream},
    pin::Pin,
    task::{Context, Poll},
};

use pin_project::pin_project;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream as TokioTcpStream,
};

use crate::sys::TcpStream as SysTcpStream;

#[pin_project]
pub struct TfoStream {
    #[pin]
    inner: SysTcpStream,
}

impl TfoStream {
    pub async fn connect(addr: SocketAddr) -> io::Result<TfoStream> {
        SysTcpStream::connect(addr).await.map(|inner| TfoStream { inner })
    }

    pub fn from_std(stream: StdTcpStream) -> io::Result<TfoStream> {
        SysTcpStream::from_std(stream).map(|inner| TfoStream { inner })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.inner.peer_addr()
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
