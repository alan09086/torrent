//! HTTP REST API server for the torrent engine.
//!
//! Provides a JSON API for managing torrents, querying session state,
//! and controlling the BitTorrent client over HTTP.
//!
//! # Usage
//!
//! ```no_run
//! use std::net::SocketAddr;
//! use torrent::session::SessionHandle;
//!
//! # async fn example(session: SessionHandle) -> std::io::Result<()> {
//! let addr: SocketAddr = "127.0.0.1:3000".parse().unwrap();
//! let server = torrent_api::ApiServer::bind(addr, session).await?;
//! println!("Listening on {}", server.local_addr());
//! server.run().await?;
//! # Ok(())
//! # }
//! ```

pub mod routes;

use std::net::SocketAddr;

use axum::Router;
use tokio::net::TcpListener;
use torrent::session::SessionHandle;

/// HTTP API server for the torrent engine.
///
/// Created via [`ApiServer::bind`], which binds a TCP listener without
/// starting to serve. Call [`ApiServer::run`] to begin accepting
/// connections.
pub struct ApiServer {
    local_addr: SocketAddr,
    listener: TcpListener,
    router: Router,
}

impl ApiServer {
    /// Bind the API server to the given address.
    ///
    /// Creates the router and binds a [`TcpListener`] but does **not**
    /// start serving requests. Call [`run`](Self::run) for that.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the TCP bind fails (e.g. address
    /// already in use, permission denied).
    pub async fn bind(addr: SocketAddr, session: SessionHandle) -> std::io::Result<Self> {
        let router = routes::build_router(session);
        let listener = TcpListener::bind(addr).await?;
        let local_addr = listener.local_addr()?;

        Ok(Self {
            local_addr,
            listener,
            router,
        })
    }

    /// Start serving HTTP requests.
    ///
    /// This future runs until the server shuts down (e.g. the listener
    /// is closed or a graceful shutdown signal is received).
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the underlying server encounters
    /// a fatal I/O error.
    pub async fn run(self) -> std::io::Result<()> {
        axum::serve(self.listener, self.router)
            .await
            .map_err(std::io::Error::other)
    }

    /// The local address the server is bound to.
    ///
    /// Useful when binding to port `0` to discover the OS-assigned port.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}
