/// Selective Acknowledgment (SACK) bitmap and adaptive heartbeat scheduler.
///
/// # Design
///
/// At 22–26% loss the receiver knows about gaps *immediately* but the sender
/// only learns about them when a SACK arrives.  The faster the SACK loop, the
/// faster retransmissions can go out.  We therefore send SACK heartbeats:
///   • Every 1 ms when loss > 20 %   — aggressive repair under heavy loss
///   • Every 3 ms when loss 10–20 %  — moderate pressure
///   • Every 5 ms when loss < 10 %   — low overhead in calm conditions
///
/// # Bitmap layout
///
/// `SackPayload` encodes which of the 1 024 sequence numbers above `base_seq`
/// have been received.  Each bit at position `k` covers `base_seq + k`.
/// When the receiver calls `advance_base` the window slides forward.
///
/// 1 024 bits = 16 × u64 words = 128 bytes on the wire (plus 8-byte base_seq
/// and 1-byte word_count → 137 bytes total payload, well inside a single MTU).

use std::time::Duration;

// ── Wire payload ──────────────────────────────────────────────────────────────

/// Maximum number of 64-bit words in a single SACK bitmap.
pub const SACK_WORDS: usize = 16;

/// Maximum sequence numbers tracked in one SACK payload (1 024 bits).
pub const SACK_WINDOW: u64 = (SACK_WORDS * 64) as u64;

/// Serialised SACK payload that follows a `RudpHeader` with the NAK flag set.
///
/// The receiver fills this struct and writes it into the UDP datagram; the
/// sender parses it to discover which packets need retransmission.
#[derive(Clone, Debug, Default)]
pub struct SackPayload {
    /// The lowest sequence number *not yet* confirmed received.
    /// All sequence numbers below `base_seq` are implicitly acknowledged.
    pub base_seq: u64,

    /// How many of the 16 words actually carry information (1–16).
    /// Words beyond `word_count` are treated as zero (all missing).
    pub word_count: u8,

    /// Received-bit bitmap: bit k of word[k/64] is set if `base_seq + k` was received.
    pub words: [u64; SACK_WORDS],
}

impl SackPayload {
    /// Total wire size in bytes when serialised with `serialise`.
    /// base_seq (8) + word_count (1) + words (word_count × 8)
    pub fn wire_size(&self) -> usize {
        8 + 1 + (self.word_count as usize * 8)
    }

    /// Mark sequence number `seq` as received in the local bitmap.
    ///
    /// No-op if `seq` is below `base_seq` (already acked) or more than
    /// `SACK_WINDOW` ahead of `base_seq` (outside the window).
    pub fn mark_received(&mut self, seq: u64) {
        if seq < self.base_seq {
            return;
        }
        let offset = seq - self.base_seq;
        if offset >= SACK_WINDOW {
            return;
        }
        let word_idx = (offset / 64) as usize;
        let bit_idx  =  offset % 64;
        self.words[word_idx] |= 1u64 << bit_idx;
        if word_idx + 1 > self.word_count as usize {
            self.word_count = (word_idx + 1) as u8;
        }
    }

    /// Returns `true` if `seq` is known to have been received.
    pub fn is_received(&self, seq: u64) -> bool {
        if seq < self.base_seq {
            return true; // implicitly acked
        }
        let offset = seq - self.base_seq;
        if offset >= SACK_WINDOW {
            return false;
        }
        let word_idx = (offset / 64) as usize;
        let bit_idx  =  offset % 64;
        (self.words[word_idx] >> bit_idx) & 1 == 1
    }

    /// Slide the base forward to `new_base`, clearing all bits below it.
    ///
    /// Call this when `base_seq` is confirmed delivered (cumulative ACK).
    /// After advancing, the bitmap is shifted so that `new_base` is at bit 0.
    pub fn advance_base(&mut self, new_base: u64) {
        if new_base <= self.base_seq {
            return;
        }
        let shift = (new_base - self.base_seq) as usize;
        if shift >= SACK_WINDOW as usize {
            // The entire window is being discarded.
            self.words = [0u64; SACK_WORDS];
            self.word_count = 0;
        } else {
            let word_shift = shift / 64;
            let bit_shift  = shift % 64;

            // Shift whole words left then adjust the remaining bits.
            if word_shift > 0 {
                self.words.copy_within(word_shift..SACK_WORDS, 0);
                for w in &mut self.words[SACK_WORDS - word_shift..] {
                    *w = 0;
                }
            }
            if bit_shift > 0 {
                let mut carry = 0u64;
                for w in self.words.iter_mut().rev() {
                    let new_carry = *w << (64 - bit_shift);
                    *w = (*w >> bit_shift) | carry;
                    carry = new_carry;
                }
            }
            // Recompute word_count to trim trailing zero words.
            self.word_count = SACK_WORDS as u8;
            while self.word_count > 0 && self.words[self.word_count as usize - 1] == 0 {
                self.word_count -= 1;
            }
        }
        self.base_seq = new_base;
    }

    /// Iterate over sequence numbers in `[base_seq, base_seq + SACK_WINDOW)`
    /// that are **not** marked received — i.e., the gaps the sender must fill.
    pub fn missing_seqs(&self) -> impl Iterator<Item = u64> + '_ {
        (0..SACK_WINDOW).filter_map(move |offset| {
            let word_idx = (offset / 64) as usize;
            let bit_idx  =  offset % 64;
            let received = (self.words[word_idx] >> bit_idx) & 1 == 1;
            if received { None } else { Some(self.base_seq + offset) }
        })
    }

    // ── Serialisation ─────────────────────────────────────────────────────────

    /// Append this payload's wire bytes to `buf`.
    ///
    /// Format:  base_seq (8 LE bytes) | word_count (1 byte) | words… (each 8 LE bytes)
    pub fn serialise(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.base_seq.to_le_bytes());
        buf.push(self.word_count);
        for i in 0..self.word_count as usize {
            buf.extend_from_slice(&self.words[i].to_le_bytes());
        }
    }

    /// Parse a `SackPayload` from the start of `data`.
    ///
    /// Returns `(payload, bytes_consumed)` or `None` if `data` is too short.
    pub fn deserialise(data: &[u8]) -> Option<(Self, usize)> {
        if data.len() < 9 {
            return None;
        }
        let base_seq   = u64::from_le_bytes(data[0..8].try_into().ok()?);
        let word_count = data[8];
        let needed = 9 + word_count as usize * 8;
        if data.len() < needed {
            return None;
        }
        let mut words = [0u64; SACK_WORDS];
        for i in 0..word_count as usize {
            let start = 9 + i * 8;
            words[i] = u64::from_le_bytes(data[start..start + 8].try_into().ok()?);
        }
        Some((Self { base_seq, word_count, words }, needed))
    }
}

// ── Adaptive SACK heartbeat scheduler ────────────────────────────────────────

/// Manages the local receive-side SACK state and decides *when* to send
/// the next heartbeat SACK to the sender.
///
/// The heartbeat interval adapts to the observed loss rate:
///   • loss > 20 % → 1 ms  (most aggressive; high retransmit pressure)
///   • loss 10–20 % → 3 ms
///   • loss < 10 %  → 5 ms (lowest overhead; quiet network)
pub struct SackManager {
    /// The local copy of the bitmap that we update as packets arrive.
    payload: SackPayload,

    /// How many packets arrived since the last heartbeat (for loss estimation).
    packets_since_last_sack: u64,

    /// How many sequence-number slots we *expected* to arrive since last SACK.
    /// `expected − received` gives gap count for loss estimation.
    expected_since_last_sack: u64,

    /// Current heartbeat interval.  Updated every time a heartbeat fires.
    heartbeat_interval: Duration,

    /// Next expected sequence number (used to detect gaps as packets arrive).
    next_expected_seq: u64,

    /// Running EMA loss estimate (0.0–1.0) for smooth interval adaptation.
    loss_ema: f64,
}

impl SackManager {
    /// `initial_seq` should be the sequence number of the first expected packet.
    pub fn new(initial_seq: u64) -> Self {
        let mut payload = SackPayload::default();
        payload.base_seq = initial_seq;
        Self {
            payload,
            packets_since_last_sack: 0,
            expected_since_last_sack: 0,
            heartbeat_interval: Duration::from_millis(5),
            next_expected_seq: initial_seq,
            loss_ema: 0.0,
        }
    }

    /// Record the arrival of a packet with sequence number `seq`.
    ///
    /// Updates the bitmap and advances the base when a contiguous prefix
    /// of received packets can be acked cumulatively.
    pub fn on_packet_received(&mut self, seq: u64) {
        // Count gap from the last expected sequence to this one as "expected".
        if seq >= self.next_expected_seq {
            self.expected_since_last_sack += seq - self.next_expected_seq + 1;
            self.next_expected_seq = seq + 1;
        }
        self.packets_since_last_sack += 1;
        self.payload.mark_received(seq);

        // Slide the base forward as far as the contiguous prefix allows.
        let mut new_base = self.payload.base_seq;
        while self.payload.is_received(new_base) {
            new_base += 1;
        }
        if new_base > self.payload.base_seq {
            self.payload.advance_base(new_base);
        }
    }

    /// Called by the heartbeat timer.  Returns the current SACK payload to
    /// send and resets internal interval counters.
    ///
    /// The caller should schedule the next heartbeat at `heartbeat_interval()`
    /// after this call returns.
    pub fn tick(&mut self) -> &SackPayload {
        // Update loss EMA from packets seen since the last tick.
        if self.expected_since_last_sack > 0 {
            let received = self.packets_since_last_sack;
            let expected = self.expected_since_last_sack;
            let interval_loss = if received >= expected {
                0.0
            } else {
                (expected - received) as f64 / expected as f64
            };
            // EMA with α = 0.25 for smooth adaptation.
            self.loss_ema = 0.75 * self.loss_ema + 0.25 * interval_loss;
        }
        self.packets_since_last_sack   = 0;
        self.expected_since_last_sack  = 0;

        // Adapt the heartbeat interval to current loss.
        self.heartbeat_interval = if self.loss_ema > 0.20 {
            Duration::from_millis(1)
        } else if self.loss_ema > 0.10 {
            Duration::from_millis(3)
        } else {
            Duration::from_millis(5)
        };

        &self.payload
    }

    /// Current heartbeat interval (call after `tick` to get the updated value).
    pub fn heartbeat_interval(&self) -> Duration {
        self.heartbeat_interval
    }

    /// Read-only view of the current bitmap (for diagnostics / testing).
    pub fn payload(&self) -> &SackPayload {
        &self.payload
    }

    /// The current smoothed loss estimate (0.0–1.0).
    pub fn loss_ema(&self) -> f64 {
        self.loss_ema
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_and_query() {
        let mut p = SackPayload::default();
        p.base_seq = 100;
        p.mark_received(100);
        p.mark_received(102);
        assert!( p.is_received(100));
        assert!(!p.is_received(101));
        assert!( p.is_received(102));
    }

    #[test]
    fn advance_base_shifts_window() {
        let mut p = SackPayload::default();
        p.base_seq = 0;
        // Mark 0 and 2 as received; 1 is a gap.
        p.mark_received(0);
        p.mark_received(2);
        // Advance past 0: new base = 1.
        p.advance_base(1);
        assert_eq!(p.base_seq, 1);
        // Bit 0 now corresponds to seq 1 (gap).
        assert!(!p.is_received(1));
        // Bit 1 now corresponds to seq 2 (received).
        assert!( p.is_received(2));
    }

    #[test]
    fn serialise_roundtrip() {
        let mut p = SackPayload::default();
        p.base_seq = 500;
        p.mark_received(500);
        p.mark_received(502);
        let mut buf = Vec::new();
        p.serialise(&mut buf);
        let (p2, consumed) = SackPayload::deserialise(&buf).unwrap();
        assert_eq!(consumed, buf.len());
        assert_eq!(p2.base_seq, 500);
        assert!( p2.is_received(500));
        assert!(!p2.is_received(501));
        assert!( p2.is_received(502));
    }

    #[test]
    fn sack_manager_adapts_interval() {
        let mut mgr = SackManager::new(0);
        // Simulate sustained 25% loss over 8 ticks.  The EMA (α=0.25) needs
        // ~6 iterations to climb above the 20% threshold that triggers 1ms.
        for round in 0u64..8 {
            let base = round * 40;
            for i in (0u64..40).filter(|i| i % 4 != 3) {
                mgr.on_packet_received(base + i);
            }
            mgr.tick();
        }
        assert_eq!(mgr.heartbeat_interval(), Duration::from_millis(1));
    }

    #[test]
    fn missing_seqs_finds_gaps() {
        let mut p = SackPayload::default();
        p.base_seq = 10;
        p.mark_received(10);
        p.mark_received(12);
        // Gaps: 11 and everything from 13 upward within SACK_WINDOW.
        let missing: Vec<u64> = p.missing_seqs().take(3).collect();
        assert_eq!(missing[0], 11);
        assert_eq!(missing[1], 13);
    }
}
