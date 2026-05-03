/// AES-256-GCM authenticated encryption for UDP payload data.
///
/// Every UDP data packet is encrypted with a unique 96-bit nonce derived from
/// the session key + sequence number. This ensures:
///   1. Confidentiality   — payload is unreadable without the session key
///   2. Integrity         — GCM authentication tag detects any tampering
///   3. Replay protection — sequence-derived nonces prevent replayed packets
///
/// The session key itself is generated fresh per transfer by the control plane
/// (gRPC server) and delivered to both peers over TLS 1.3 — it never travels
/// over the UDP data channel.
// Phase 3: implement encrypt/decrypt using the `aes-gcm` crate.
pub mod crypto {}
