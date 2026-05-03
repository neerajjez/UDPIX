/// Unified error enum for the entire UDPix system.
///
/// Each crate maps its internal errors into this type so that the top-level
/// binaries (udpix-server, udpix-client) only need to handle one error kind.
///
/// Variant naming convention:
///   - `Io(...)` — OS-level I/O failures (disk, socket, syscall)
///   - `Protocol(...)` — malformed or unexpected protocol messages
///   - `Crypto(...)` — encryption/decryption failures (wrong key, bad tag)
///   - `Auth(...)` — authentication / authorization rejections
///   - `Traversal(...)` — NAT traversal negotiation failures
#[derive(Debug)]
pub enum UdpixError {
    Io(std::io::Error),
    Protocol(String),
    Crypto(String),
    Auth(String),
    Traversal(String),
}

impl std::fmt::Display for UdpixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e)         => write!(f, "io: {e}"),
            Self::Protocol(s)   => write!(f, "protocol: {s}"),
            Self::Crypto(s)     => write!(f, "crypto: {s}"),
            Self::Auth(s)       => write!(f, "auth: {s}"),
            Self::Traversal(s)  => write!(f, "traversal: {s}"),
        }
    }
}

impl std::error::Error for UdpixError {}
