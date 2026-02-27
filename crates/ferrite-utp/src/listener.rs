use std::net::SocketAddr;

use tokio::sync::mpsc;

use crate::error::{Error, Result};
use crate::stream::UtpStream;

/// Accepts incoming uTP connections.
pub struct UtpListener {
    pub(crate) incoming_rx: mpsc::Receiver<(UtpStream, SocketAddr)>,
    pub(crate) local_addr: SocketAddr,
}

impl UtpListener {
    /// Accept the next incoming connection.
    ///
    /// Returns the stream and the remote peer's address.
    pub async fn accept(&mut self) -> Result<(UtpStream, SocketAddr)> {
        self.incoming_rx.recv().await.ok_or(Error::Shutdown)
    }

    /// Returns the local address this listener is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}
