/// # udpix-controlplane
///
/// The orchestration brain — manages authentication, session lifecycle, and
/// bandwidth policies over a secure gRPC / TLS 1.3 channel.
///
/// ## Separation of concerns
/// The control plane never touches file bytes. Its sole job is to:
///   1. Authenticate the requesting client (PBKDF2 + JWT)
///   2. Allocate a unique SessionId for the transfer
///   3. Generate a fresh AES-256-GCM session key for that transfer
///   4. Securely deliver the session key to both sender and receiver over TLS
///   5. Enforce bandwidth quotas and routing policies
///   6. Monitor transfer progress reported by the data plane workers
///
/// The data plane (udpix-protocol) then operates independently, using only
/// the session key it received — no further contact with the control plane
/// until the transfer completes or an error occurs.
///
/// ## Security model
/// - Passwords stored as PBKDF2-HMAC-SHA256, 100,000 iterations, random salt
///   (meets OWASP 2024 minimum). The raw password never leaves the client.
/// - JWT session tokens with 15-minute expiry, signed with a server secret.
/// - All control traffic encrypted by TLS 1.3; the server presents a certificate
///   and the client validates it to prevent man-in-the-middle attacks.
/// - Session keys are ephemeral — a new AES key is generated for every transfer
///   and discarded immediately after. Past captures cannot decrypt future transfers.
///
/// ## Modules
/// - `server`      — tonic gRPC server, registers all service handlers
/// - `auth`        — PBKDF2 hashing, JWT issue/verify, token refresh
/// - `session_mgr` — SessionId allocation, key generation, lifecycle tracking
/// - `policy`      — Bandwidth quota enforcement and routing rule evaluation

pub mod auth;
pub mod server;
pub mod session_mgr;
pub mod policy;
