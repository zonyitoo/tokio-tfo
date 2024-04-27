//! TFO listener

use std::{
    io,
    net::{SocketAddr, TcpListener as StdTcpListener},
    task::{Context, Poll},
};

use cfg_if::cfg_if;
use futures::{future, ready};
use tokio::net::{TcpListener as TokioTcpListener, TcpSocket};

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "watchos", target_os = "tvos"))]
use crate::sys::set_tcp_fastopen_force_enable;
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

        // On all other platforms, TCP_FASTOPEN can be set before bind(), between bind() and listen(), or after listen().
        // We prefer setting it before bind() as this feels most like the natural order of socket initialization sequence.
        //
        // On macOS, setting TCP_FASTOPEN_FORCE_ENABLE requires the socket to be in the TCPS_CLOSED state.
        // TCP_FASTOPEN, on the other hand, can only be set when the socket is in the TCPS_LISTEN state.
        cfg_if! {
            if #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "watchos", target_os = "tvos")))] {
                set_tcp_fastopen(&socket)?;
            } else {
                if let Err(err) = set_tcp_fastopen_force_enable(&socket) {
                    log::debug!("failed to set TCP_FASTOPEN_FORCE_ENABLE: {:?}", err);
                }
            }
        }

        socket.bind(addr)?;

        // mio's default backlog is 1024
        let inner = socket.listen(1024)?;

        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "watchos", target_os = "tvos"))]
        set_tcp_fastopen(&inner)?;

        Ok(TfoListener { inner })
    }

    /// Creates new `TfoListener` from a `std::net::TcpListener`.
    ///
    /// This will enable the TCP listener with TFO enabled.
    ///
    /// The `std::net::TcpListener` must be in TCPS_LISTEN.
    pub fn from_std(listener: StdTcpListener) -> io::Result<TfoListener> {
        // The listener must set_nonblocking for TokioTcpListener::from_std.
        listener.set_nonblocking(true)?;
        Self::from_tokio(TokioTcpListener::from_std(listener)?)
    }

    /// Creates new `TfoListener` from a `tokio::net::TcpListener`.
    ///
    /// This will enable the TCP listener with TFO enabled.
    pub fn from_tokio(listener: TokioTcpListener) -> io::Result<TfoListener> {
        set_tcp_fastopen(&listener)?;
        Ok(TfoListener { inner: listener })
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
