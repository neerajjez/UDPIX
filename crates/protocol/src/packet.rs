/// Wire-format packet definitions for the UDPix Reliable UDP protocol.
///
/// # Design rationale
///
/// At 22–26% packet loss our baseline, the header must be:
///   1. Small — every header byte wasted is a byte less of useful payload.
///   2. Rich in control bits — we need SYN/FIN handshake, NAK/SACK for loss
///      recovery, PING/PONG for RTT measurement, and a hook for future FEC.
///   3. Zero-copy safe — we cast raw UDP receive buffer bytes directly into
///      this struct, so `#[repr(C, packed)]` is mandatory.
///
/// # Packed struct field access
///
/// `#[repr(C, packed)]` strips all alignment padding.  Direct field access on
/// misaligned memory is **undefined behaviour** in Rust, so every multi-byte
/// field MUST be read through `ptr::read_unaligned`.  Use the accessor methods
/// (`header.sequence_number()`, etc.) rather than `header.sequence_number`
/// directly.
///
/// # Header layout (29 bytes total)
///
/// ```text
/// offset  len  field
///  0       4   session_id       — demultiplexes concurrent sessions
///  4       8   sequence_number  — monotone; used for ordering & SACK
/// 12       8   timestamp_us     — µs sender timestamp; echoed for RTT
/// 20       1   flags            — SYN|FIN|DATA|ACK|NAK|PING|PONG|FEC
/// 21       2   payload_len      — bytes of encrypted payload following
/// 23       4   fec_group_id     — FEC group (Phase 2; zero for now)
/// 27       1   fec_index        — index in FEC group (Phase 2; zero)
/// 28       1   fec_group_size   — data packets per group (Phase 2; zero)
/// ```

use std::mem;

// ── Flags bitmask constants ───────────────────────────────────────────────────

pub mod flags {
    /// Session initiation (first packet from sender).
    pub const SYN:  u8 = 0b0000_0001;
    /// Session termination (sender is done, no more data).
    pub const FIN:  u8 = 0b0000_0010;
    /// This packet carries encrypted payload data.
    pub const DATA: u8 = 0b0000_0100;
    /// Cumulative acknowledgment (all seqs up to sequence_number received).
    pub const ACK:  u8 = 0b0000_1000;
    /// Heartbeat SACK/NAK — payload is a `SackPayload` bitmap.
    /// Sent by the receiver every 1–5 ms regardless of data arrival.
    pub const NAK:  u8 = 0b0001_0000;
    /// RTT probe: sender writes timestamp_us, no payload.
    pub const PING: u8 = 0b0010_0000;
    /// RTT probe response: receiver echoes sender's timestamp_us back.
    pub const PONG: u8 = 0b0100_0000;
    /// Forward Error Correction parity block (Phase 2 — unused, always 0).
    pub const FEC:  u8 = 0b1000_0000;
}

// ── Wire-format header ────────────────────────────────────────────────────────

/// 29-byte binary header prepended to every RUDP UDP datagram.
///
/// All fields are little-endian on the wire.  Use the constructor methods
/// to build outgoing headers, and the `read_from` / accessor methods when
/// parsing incoming datagrams.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct RudpHeader {
    /// Identifies which transfer session owns this packet.
    /// Assigned by the control plane; allows multiple concurrent sessions
    /// to share a single UDP port without confusion.
    pub session_id: u32,

    /// Monotonically increasing sequence number (never wraps in practice).
    /// At 1 Gbps with 1 400-byte payloads a u64 takes ~580 years to overflow.
    pub sequence_number: u64,

    /// Microseconds since Unix epoch written by the sender at transmit time.
    /// DATA packets: the receiver echoes this in its next PONG so the sender
    /// can compute RTT without storing per-packet send times.
    pub timestamp_us: u64,

    /// Control flags bitmask — see the `flags` module constants above.
    pub flags: u8,

    /// Byte length of the encrypted payload immediately following this header.
    /// Zero for PING / PONG / SYN / FIN packets (no payload).
    pub payload_len: u16,

    // ── FEC hook (Phase 2 — zeroed until FEC is implemented) ─────────────────
    //
    // When FEC is active, the sender groups N data packets and produces M parity
    // blocks (Reed-Solomon or XOR).  All share the same fec_group_id.
    // The receiver can reconstruct up to M lost packets from the parity blocks,
    // reducing the effective NAK/retransmit overhead under extreme loss.

    /// FEC group this packet belongs to (0 = FEC disabled).
    pub fec_group_id: u32,

    /// Index within the FEC group (0..N-1 = data; N..N+M-1 = parity).
    pub fec_index: u8,

    /// Total data packets in this FEC group (needed for reconstruction math).
    pub fec_group_size: u8,
}

// Compile-time guard: the struct must be exactly 29 bytes with no padding.
const _HEADER_SIZE_CHECK: () = assert!(mem::size_of::<RudpHeader>() == 29);

// ── Constructors ──────────────────────────────────────────────────────────────

impl RudpHeader {
    /// Wire size of the header in bytes.
    pub const SIZE: usize = mem::size_of::<Self>(); // 29

    /// Build a header for an outgoing DATA packet.
    #[inline]
    pub fn new_data(session_id: u32, seq: u64, ts_us: u64, payload_len: u16) -> Self {
        Self {
            session_id,
            sequence_number: seq,
            timestamp_us: ts_us,
            flags: flags::DATA,
            payload_len,
            fec_group_id: 0,
            fec_index: 0,
            fec_group_size: 0,
        }
    }

    /// Build a Heartbeat-SACK header (receiver → sender, every 1–5 ms).
    /// `payload_len` must equal the serialized byte length of the SackPayload.
    #[inline]
    pub fn new_heartbeat_sack(session_id: u32, seq: u64, ts_us: u64, payload_len: u16) -> Self {
        Self {
            session_id,
            sequence_number: seq,
            timestamp_us: ts_us,
            flags: flags::NAK,
            payload_len,
            fec_group_id: 0,
            fec_index: 0,
            fec_group_size: 0,
        }
    }

    /// Build a PING probe header (no payload; used for RTT measurement).
    #[inline]
    pub fn new_ping(session_id: u32, seq: u64, ts_us: u64) -> Self {
        Self {
            session_id,
            sequence_number: seq,
            timestamp_us: ts_us,
            flags: flags::PING,
            payload_len: 0,
            fec_group_id: 0,
            fec_index: 0,
            fec_group_size: 0,
        }
    }

    /// Build a PONG response (echoes the sender's timestamp_us for RTT math).
    #[inline]
    pub fn new_pong(session_id: u32, seq: u64, sender_ts_us: u64) -> Self {
        Self {
            session_id,
            sequence_number: seq,
            // Echo the exact timestamp the sender wrote — they subtract it
            // from 'now' to get the round-trip time.
            timestamp_us: sender_ts_us,
            flags: flags::PONG,
            payload_len: 0,
            fec_group_id: 0,
            fec_index: 0,
            fec_group_size: 0,
        }
    }

    /// Build a SYN (session-open) header.
    #[inline]
    pub fn new_syn(session_id: u32, ts_us: u64) -> Self {
        Self {
            session_id,
            sequence_number: 0,
            timestamp_us: ts_us,
            flags: flags::SYN,
            payload_len: 0,
            fec_group_id: 0,
            fec_index: 0,
            fec_group_size: 0,
        }
    }

    /// Build a FIN (session-close) header.
    #[inline]
    pub fn new_fin(session_id: u32, seq: u64, ts_us: u64) -> Self {
        Self {
            session_id,
            sequence_number: seq,
            timestamp_us: ts_us,
            flags: flags::FIN,
            payload_len: 0,
            fec_group_id: 0,
            fec_index: 0,
            fec_group_size: 0,
        }
    }
}

// ── Safe unaligned field accessors ───────────────────────────────────────────
//
// These are needed because `#[repr(C, packed)]` may place multi-byte fields at
// non-natural alignments.  Accessing them through a reference would be UB;
// `ptr::read_unaligned` performs a safe byte-by-byte copy instead.

impl RudpHeader {
    // `#[repr(C, packed)]` may misalign multi-byte fields, so taking a reference
    // is UB.  `std::ptr::addr_of!` gives a raw pointer without creating a reference,
    // which `read_unaligned` can then dereference safely.
    #[inline]
    pub fn session_id(&self) -> u32 {
        unsafe { std::ptr::read_unaligned(std::ptr::addr_of!(self.session_id)) }
    }
    #[inline]
    pub fn sequence_number(&self) -> u64 {
        unsafe { std::ptr::read_unaligned(std::ptr::addr_of!(self.sequence_number)) }
    }
    #[inline]
    pub fn timestamp_us(&self) -> u64 {
        unsafe { std::ptr::read_unaligned(std::ptr::addr_of!(self.timestamp_us)) }
    }
    #[inline]
    pub fn payload_len(&self) -> u16 {
        unsafe { std::ptr::read_unaligned(std::ptr::addr_of!(self.payload_len)) }
    }
    #[inline]
    pub fn fec_group_id(&self) -> u32 {
        unsafe { std::ptr::read_unaligned(std::ptr::addr_of!(self.fec_group_id)) }
    }

    // Flag testers — single-byte field, no alignment issue.
    #[inline] pub fn is_syn(&self)  -> bool { self.flags & flags::SYN  != 0 }
    #[inline] pub fn is_fin(&self)  -> bool { self.flags & flags::FIN  != 0 }
    #[inline] pub fn is_data(&self) -> bool { self.flags & flags::DATA != 0 }
    #[inline] pub fn is_ack(&self)  -> bool { self.flags & flags::ACK  != 0 }
    #[inline] pub fn is_nak(&self)  -> bool { self.flags & flags::NAK  != 0 }
    #[inline] pub fn is_ping(&self) -> bool { self.flags & flags::PING != 0 }
    #[inline] pub fn is_pong(&self) -> bool { self.flags & flags::PONG != 0 }
    #[inline] pub fn is_fec(&self)  -> bool { self.flags & flags::FEC  != 0 }
}

// ── Serialisation / deserialisation ──────────────────────────────────────────

impl RudpHeader {
    /// Write this header into the first `RudpHeader::SIZE` bytes of `buf`.
    ///
    /// Panics if `buf` is shorter than 29 bytes.
    pub fn write_to(&self, buf: &mut [u8]) {
        assert!(buf.len() >= Self::SIZE, "buffer too small for RudpHeader");
        // SAFETY: self is a valid RudpHeader; we copy exactly SIZE bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self as *const Self as *const u8,
                buf.as_mut_ptr(),
                Self::SIZE,
            );
        }
    }

    /// Parse the first 29 bytes of a received UDP datagram into a header.
    ///
    /// Returns `None` if the datagram is shorter than the header.
    pub fn read_from(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::SIZE {
            return None;
        }
        // SAFETY: we verified the buffer is at least SIZE bytes; we write
        // into a zero-initialised local copy via unaligned pointer copy.
        let mut h = Self {
            session_id: 0,
            sequence_number: 0,
            timestamp_us: 0,
            flags: 0,
            payload_len: 0,
            fec_group_id: 0,
            fec_index: 0,
            fec_group_size: 0,
        };
        unsafe {
            std::ptr::copy_nonoverlapping(
                buf.as_ptr(),
                &mut h as *mut Self as *mut u8,
                Self::SIZE,
            );
        }
        Some(h)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Return the current time as microseconds since the Unix epoch.
/// Used to populate `timestamp_us` when building outgoing packets.
#[inline]
pub fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip() {
        // Build a DATA header, serialise it, then parse it back.
        let orig = RudpHeader::new_data(0xDEAD_BEEF, 42, 1_000_000, 1400);
        let mut buf = [0u8; RudpHeader::SIZE + 4]; // extra bytes to check bounds
        orig.write_to(&mut buf);

        let parsed = RudpHeader::read_from(&buf).unwrap();
        assert_eq!(parsed.session_id(),      0xDEAD_BEEF);
        assert_eq!(parsed.sequence_number(), 42);
        assert_eq!(parsed.timestamp_us(),    1_000_000);
        assert_eq!(parsed.payload_len(),     1400);
        assert!(parsed.is_data());
        assert!(!parsed.is_nak());
    }

    #[test]
    fn header_size_is_29() {
        assert_eq!(RudpHeader::SIZE, 29);
    }

    #[test]
    fn nak_flag_roundtrip() {
        let h = RudpHeader::new_heartbeat_sack(1, 7, 500, 137);
        assert!(h.is_nak());
        assert!(!h.is_data());
        assert!(!h.is_ping());
    }
}
