/// Core domain types shared across all UDPix crates.
///
/// SessionId  — unique 32-bit ID assigned by the control plane per transfer
///              session. Embedded in every UDP packet header so the receiver
///              can demultiplex packets from concurrent sessions on one port.
///
/// TransferId — UUID v4 assigned at transfer initiation. Used for resumable
///              transfers and progress tracking in the control plane.
///
/// BlockId    — sequential 64-bit identifier for a 16 MB bulk block produced
///              by the ioengine packer. The protocol layer only sees blocks,
///              never individual files.
///
/// Timestamp  — microsecond-precision u64 used in packet headers for RTT
///              measurement. Sender writes current time; receiver echoes it
///              back in the ACK so the sender can compute round-trip time.

/// 32-bit session identifier assigned by the control plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(pub u32);

/// UUID v4 transfer identifier for resumable transfers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransferId(pub u128);

/// Sequential 64-bit block identifier produced by the ioengine packer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId(pub u64);

/// Microsecond-precision Unix timestamp used in packet headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(pub u64);
