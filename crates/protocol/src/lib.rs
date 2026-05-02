/// # udpix-protocol
///
/// The custom Reliable UDP (RUDP) protocol engine — the core of UDPix's speed.
///
/// ## Why not TCP?
/// On a global WAN link with 1% packet loss and 100ms RTT, TCP's AIMD algorithm
/// causes throughput to collapse because it halves the window on every lost packet
/// and recovers slowly. A 10 Gbps fiber link degrades to megabits per second.
///
/// ## What this crate implements
///
/// ### `packet`  — Binary UDP packet format
///   Custom header layout (`#[repr(C, packed)]`) designed for minimal overhead:
///   - session_id (u32)   — identifies which transfer session this packet belongs to
///   - seq (u64)          — monotonically increasing sequence number for ordering
///   - timestamp_us (u64) — sender's microsecond timestamp, echoed back for RTT math
///   - flags (u8)         — bitmask: SYN | ACK | NAK | FIN | PING
///   - payload_len (u16)  — byte length of the encrypted payload that follows
///
/// ### `congestion`  — Rate-based token bucket controller
///   Instead of TCP's window-based control, we compute the ideal send rate:
///     rate = bandwidth_estimate × (1.0 - loss_factor)
///   Packets are injected at exactly this rate using a token bucket timer,
///   preventing micro-bursts that overflow switch buffers on the path.
///
/// ### `sack`  — Selective Acknowledgment and Negative Acknowledgment
///   The receiver maintains a sliding bitmap of received sequence numbers.
///   Periodically it sends NAK packets listing the exact missing sequences.
///   The sender only retransmits those specific bytes — never whole windows.
///   This keeps throughput high even with up to 5% steady-state packet loss.
///
/// ### `sender` / `receiver`  — Async event loops using sendmmsg / recvmmsg
///   Standard `sendto()` / `recvfrom()` syscalls require one context switch
///   per datagram. At 10 Gbps with 1500-byte MTU that is ~833,000 syscalls/sec —
///   enough to pin a CPU core just on syscall overhead.
///   `sendmmsg` / `recvmmsg` batch up to 256 datagrams per syscall, reducing
///   context switches by ~250× and unlocking true multi-gigabit throughput.

pub mod congestion;
pub mod packet;
pub mod receiver;
pub mod sack;
pub mod sender;
pub mod session;
