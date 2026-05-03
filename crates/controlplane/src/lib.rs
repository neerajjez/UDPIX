// udpix-controlplane — Phase 3 gRPC control plane (auth, session management, policies).
// Modules: auth, server, session_mgr, policy — implemented in a future sprint.
//
// Overview:
//   Authenticates clients via PBKDF2+JWT over TLS 1.3, allocates SessionIds,
//   generates ephemeral AES-256-GCM session keys, and enforces bandwidth policies.
//   The data plane operates independently once it receives the session key.
