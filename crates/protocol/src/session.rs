/// Per-session runtime state: RTT estimates, lifecycle, and loss statistics.
///
/// # Threading model
///
/// One `SessionStats` is shared between the `Sender` task and the `Receiver`
/// task for the same RUDP session.  Rather than protecting it with a `Mutex`
/// (which would add latency on the hot send/receive paths), we store each
/// field as an atomic.  The tradeoff is that reads are slightly stale, but
/// for statistics that are only used to *inform* rate decisions (not for
/// correctness) this is completely acceptable.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

// ── Session lifecycle ─────────────────────────────────────────────────────────

/// The state machine for a single RUDP session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    /// SYN sent / waiting for SYN-ACK from the receiver.
    Connecting,
    /// Handshake complete; data is flowing.
    Active,
    /// Sender has enqueued all data and sent FIN; waiting for the receiver
    /// to confirm it has received everything before we close the socket.
    Draining,
    /// Session terminated cleanly (all data delivered, FIN acknowledged).
    Closed,
    /// Session aborted due to timeout or unrecoverable error.
    Failed(String),
}

// ── RTT and loss statistics ───────────────────────────────────────────────────

/// Live statistics for an active RUDP session.
///
/// Updated concurrently by the Sender (bytes_sent, packets_sent) and the
/// Receiver (bytes_acked, packets_acked) using relaxed atomics.
pub struct SessionStats {
    /// Smoothed round-trip time in microseconds (RFC 6298 SRTT).
    /// Zero until the first PONG is received.
    pub srtt_us: AtomicU64,

    /// RTT variance in microseconds (RFC 6298 RTTVAR).
    /// Initialised to 5 ms (5 000 µs) as a conservative first estimate.
    pub rttvar_us: AtomicU64,

    /// Packets sent by the Sender (including retransmissions).
    pub packets_sent: AtomicU64,

    /// Packets confirmed received by the remote side (via SACK base advance).
    pub packets_acked: AtomicU64,

    /// Bytes written to the wire (header + payload, including retransmissions).
    pub bytes_sent: AtomicU64,

    /// Bytes confirmed received by the remote side.
    pub bytes_acked: AtomicU64,

    /// Number of retransmissions issued so far (diagnostic counter).
    pub retransmits: AtomicU64,
}

impl SessionStats {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            srtt_us:       AtomicU64::new(0),
            rttvar_us:     AtomicU64::new(5_000),
            packets_sent:  AtomicU64::new(0),
            packets_acked: AtomicU64::new(0),
            bytes_sent:    AtomicU64::new(0),
            bytes_acked:   AtomicU64::new(0),
            retransmits:   AtomicU64::new(0),
        })
    }

    // ── RTT update (RFC 6298) ─────────────────────────────────────────────────

    /// Feed a new RTT sample into the smoothed estimators.
    ///
    /// Uses the RFC 6298 algorithm:
    ///   - First measurement: SRTT = R, RTTVAR = R/2
    ///   - Subsequent:  RTTVAR = 3/4·RTTVAR + 1/4·|SRTT−R|
    ///                  SRTT   = 7/8·SRTT   + 1/8·R
    ///
    /// The integer arithmetic approximations (×7/8, ×3/4, etc.) avoid
    /// floating-point operations on the hot path.
    pub fn update_rtt(&self, rtt_us: u64) {
        let srtt   = self.srtt_us  .load(Ordering::Relaxed);
        let rttvar = self.rttvar_us.load(Ordering::Relaxed);

        let (new_srtt, new_rttvar) = if srtt == 0 {
            // First sample — bootstrap both estimators.
            (rtt_us, rtt_us / 2)
        } else {
            // |SRTT − R|: the absolute deviation of the new sample.
            let deviation = (srtt as i64 - rtt_us as i64).unsigned_abs();
            // RTTVAR = 3/4·RTTVAR + 1/4·|SRTT−R|
            let new_rttvar = (3 * rttvar / 4) + (deviation / 4);
            // SRTT = 7/8·SRTT + 1/8·R
            let new_srtt = (7 * srtt / 8) + (rtt_us / 8);
            (new_srtt, new_rttvar)
        };

        self.srtt_us  .store(new_srtt,   Ordering::Relaxed);
        self.rttvar_us.store(new_rttvar, Ordering::Relaxed);
    }

    // ── Derived metrics ───────────────────────────────────────────────────────

    /// Retransmission Timeout = SRTT + 4·RTTVAR (RFC 6298 §2.4).
    ///
    /// Clamped between 200 ms (practical minimum for WAN) and 10 s (hard max).
    /// At 22–26% loss a shorter RTO would cause spurious retransmissions on top
    /// of the already-heavy load, so we err conservatively.
    pub fn rto(&self) -> Duration {
        let srtt   = self.srtt_us  .load(Ordering::Relaxed);
        let rttvar = self.rttvar_us.load(Ordering::Relaxed);
        let rto_us = srtt.saturating_add(4 * rttvar);
        Duration::from_micros(rto_us.clamp(200_000, 10_000_000))
    }

    /// Instantaneous packet loss estimate: lost / sent.
    ///
    /// "lost" is approximated as sent − acked.  This overestimates if packets
    /// are still in flight, so use this as a trend signal rather than a precise
    /// measurement.
    pub fn loss_rate(&self) -> f64 {
        let sent  = self.packets_sent .load(Ordering::Relaxed);
        let acked = self.packets_acked.load(Ordering::Relaxed);
        if sent == 0 {
            return 0.0;
        }
        let lost = sent.saturating_sub(acked);
        lost as f64 / sent as f64
    }

    /// Effective goodput ratio: bytes confirmed received / bytes transmitted.
    ///
    /// Values below 1.0 reflect both packet loss and retransmission overhead.
    /// At 25% loss with one retransmit per lost packet, goodput ≈ 0.75.
    pub fn goodput_ratio(&self) -> f64 {
        let sent  = self.bytes_sent .load(Ordering::Relaxed);
        let acked = self.bytes_acked.load(Ordering::Relaxed);
        if sent == 0 {
            return 0.0;
        }
        (acked as f64 / sent as f64).min(1.0)
    }
}

impl Default for SessionStats {
    fn default() -> Self {
        Self {
            srtt_us:       AtomicU64::new(0),
            rttvar_us:     AtomicU64::new(5_000),
            packets_sent:  AtomicU64::new(0),
            packets_acked: AtomicU64::new(0),
            bytes_sent:    AtomicU64::new(0),
            bytes_acked:   AtomicU64::new(0),
            retransmits:   AtomicU64::new(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtt_bootstrap() {
        let stats = SessionStats::new();
        // Before any measurement, SRTT is zero.
        assert_eq!(stats.srtt_us.load(Ordering::Relaxed), 0);

        // First sample: 20 ms.
        stats.update_rtt(20_000);
        assert_eq!(stats.srtt_us.load(Ordering::Relaxed), 20_000);
        assert_eq!(stats.rttvar_us.load(Ordering::Relaxed), 10_000);
    }

    #[test]
    fn rto_clamps_to_minimum() {
        let stats = SessionStats::new();
        // With zero SRTT and zero RTTVAR the RTO should still be at least 200 ms.
        assert_eq!(stats.rto(), Duration::from_millis(200));
    }

    #[test]
    fn loss_rate_no_acks() {
        let stats = SessionStats::new();
        stats.packets_sent.store(100, Ordering::Relaxed);
        stats.packets_acked.store(74, Ordering::Relaxed); // simulated 26% loss
        let loss = stats.loss_rate();
        assert!((loss - 0.26).abs() < 0.001, "expected ~0.26, got {loss}");
    }
}
