/// Errors that can occur during NAT port mapping.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No default gateway found.
    #[error("no default gateway found")]
    NoGateway,

    /// UPnP discovery failed.
    #[error("UPnP discovery: {0}")]
    UpnpDiscovery(String),

    /// UPnP control action failed.
    #[error("UPnP control: {0}")]
    UpnpControl(String),

    /// NAT-PMP protocol error.
    #[error("NAT-PMP: {0}")]
    NatPmp(String),

    /// PCP protocol error.
    #[error("PCP: {0}")]
    Pcp(String),

    /// Unsupported protocol version.
    #[error("unsupported protocol version")]
    UnsupportedVersion,

    /// Gateway refused the port mapping request.
    #[error("mapping refused by gateway")]
    MappingRefused,

    /// Operation timed out.
    #[error("timeout")]
    Timeout,

    /// Actor shut down.
    #[error("NAT actor shut down")]
    Shutdown,

    /// I/O error.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience result type for NAT operations.
pub type Result<T> = std::result::Result<T, Error>;
