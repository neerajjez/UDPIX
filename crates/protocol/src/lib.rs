/// # udpix-protocol
///
/// The custom Reliable UDP (RUDP) protocol engine — the core of UDPix's speed.
///
/// ## Design baseline: 22–26 % packet loss as *normal operating conditions*
///
/// Most reliable-UDP libraries treat high loss as an edge case and fall apart
/// above 5–10 %.  UDPix inverts this: 22–26 % loss on long-haul WAN paths is
/// the *expected* environment, not the exception.  Every component is designed
/// to deliver maximum goodput under these conditions.
///
/// ## Why not TCP?
///
/// TCP's AIMD halves its congestion window on every loss event.  At 25 % loss
/// that means nearly every other RTT triggers a halving — the sender is trapped
/// in slow-start permanently and throughput collapses to single-digit Mbps on
/// a link that physically supports gigabits.
///
/// ## What this crate implements
///
/// ### `packet`  — 29-byte binary header (`#[repr(C, packed)]`)
///   - session_id (u32)      — demultiplexes concurrent sessions
///   - sequence_number (u64) — monotone; used for SACK bitmap indexing
///   - timestamp_us (u64)    — sender µs epoch time, echoed in PONG for RTT
///   - flags (u8)            — SYN|FIN|DATA|ACK|NAK|PING|PONG|FEC bitmask
///   - payload_len (u16)     — encrypted payload byte count
///   - fec_group_id/index/size — reserved for Phase 2 Reed-Solomon FEC
///
/// ### `session`  — RTT estimators and session lifecycle state machine
///   Atomic `SessionStats` shared lock-free between Sender and Receiver.
///   RFC 6298 SRTT/RTTVAR updated with integer arithmetic; RTO clamped [200 ms, 10 s].
///
/// ### `sack`  — 1 024-bit sliding SACK bitmap + adaptive heartbeat
///   The receiver sends a SACK every 1–5 ms (adaptive to loss rate).
///   At > 20 % loss the interval drops to 1 ms so retransmits go out fast.
///   Fast retransmit fires when a sequence appears in ≥ 2 consecutive SAKs.
///
/// ### `congestion`  — Token bucket pacer + slow-start bandwidth prober
///   Probes the link at 10 → 50 → 100 → 200 → 500 → 1 000 → 2 000 Mbps,
///   locks onto the goodput peak, and throttles to a 5 Mbps floor at > 30 % loss.
///   The token bucket prevents micro-bursts that inflate switch-buffer loss.
///
/// ### `sender` / `receiver`  — Async loops with sendmmsg / recvmmsg batching
///   One `sendmmsg` / `recvmmsg` call handles up to 64 datagrams, reducing
///   context-switch overhead ~250× versus per-datagram `sendto` / `recvfrom`.

pub mod congestion;
pub mod packet;
pub mod receiver;
pub mod sack;
pub mod sender;
pub mod session;
