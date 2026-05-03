// udpix-controlplane — Phase 3 gRPC control plane.
//
// Authenticates clients via PBKDF2+JWT over TLS 1.3, allocates SessionIds,
// generates ephemeral AES-256-GCM session keys, and enforces bandwidth policies.
// The data plane operates independently once it receives the session key.

pub mod auth;
pub mod policy;
pub mod server;
pub mod session_mgr;

pub mod proto {
    tonic::include_proto!("udpix.control");
}
