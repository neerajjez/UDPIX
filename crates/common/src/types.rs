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

// TODO(phase-1): Define these as newtype wrappers, e.g.:
//   pub struct SessionId(pub u32);
//   pub struct SequenceNumber(pub u64);
//   pub struct Timestamp(pub u64);  // microseconds since Unix epoch
