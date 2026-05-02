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

// TODO(phase-1): Define UdpixError with thiserror #[derive(Error)]
