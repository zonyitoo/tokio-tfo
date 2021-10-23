//! TFO listener

use std::{
    io,
    net::SocketAddr,
    task::{Context, Poll},
};

use futures::{future, ready};
use tokio::net::{TcpListener as TokioTcpListener, TcpSocket};

use crate::{stream::TfoStream, sys::set_tcp_fastopen};

/// TCP listener with TFO enabled
pub struct TfoListener {
    inner: TokioTcpListener,
}

impl TfoListener {
    /// Bind the socket to the given address.
    ///
    /// This will create a TCP listener with TFO enabled.
    pub async fn bind(addr: SocketAddr) -> io::Result<TfoListener> {
        let socket = match addr {
            SocketAddr::V4(..) => TcpSocket::new_v4()?,
            SocketAddr::V6(..) => TcpSocket::new_v6()?,
        };

        // On platforms with Berkeley-derived sockets, this allows to quickly
        // rebind a socket, without needing to wait for the OS to clean up the
        // previous one.
        //
        // On Windows, this allows rebinding sockets which are actively in use,
        // which allows “socket hijacking”, so we explicitly don't set it here.
        // https://docs.microsoft.com/en-us/windows/win32/winsock/using-so-reuseaddr-and-so-exclusiveaddruse
        #[cfg(not(windows))]
        socket.set_reuseaddr(true)?;

        socket.bind(addr)?;

        // mio's default backlog is 1024
        let inner = socket.listen(1024)?;

        set_tcp_fastopen(&inner)?;

        Ok(TfoListener { inner })
    }

    /// Polls to accept a new incoming connection to this listener.
    pub fn poll_accept(&self, cx: &mut Context<'_>) -> Poll<io::Result<(TfoStream, SocketAddr)>> {
        let (stream, peer_addr) = ready!(self.inner.poll_accept(cx))?;
        Poll::Ready(Ok((TfoStream::from(stream), peer_addr)))
    }

    /// Accept a new incoming connection to this listener
    pub async fn accept(&self) -> io::Result<(TfoStream, SocketAddr)> {
        future::poll_fn(|cx| self.poll_accept(cx)).await
    }

    /// Returns the local address that this listener is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}
