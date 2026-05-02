/// # udpix-common
///
/// Shared foundation used by every other UDPix crate.
///
/// Responsibilities:
/// - `crypto`   — AES-256-GCM encrypt/decrypt helpers for UDP payload encryption
/// - `error`    — Unified error type so all crates speak the same error language
/// - `types`    — Core domain types: SessionId, TransferId, BlockId, timestamps
///
/// Nothing in this crate depends on networking or disk I/O.
/// It is kept minimal so it compiles fast and stays easy to audit.

pub mod crypto;
pub mod error;
pub mod types;
