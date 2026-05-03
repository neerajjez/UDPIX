/// Rate-based congestion control: token bucket pacer + bandwidth prober.
///
/// # Philosophy
///
/// TCP's AIMD halves throughput on every loss event.  At 22–26% loss that means
/// nearly every other RTT triggers a halving — the sender never gets out of
/// slow-start.  Our design inverts the question:
///
///   *"What is the highest rate at which goodput actually increases?"*
///
/// The `BandwidthProfiler` answers this by probing ascending rates (10 → 50 →
/// 100 → 200 → 500 → 1000 Mbps), measuring the resulting goodput at each step,
/// and locking onto the *sweet spot* — the rate where adding more speed stops
/// improving delivery.  Once found it holds that rate until conditions change.
///
/// If loss spikes above 30% at any moment, the profiler drops immediately to a
/// 5 Mbps floor to avoid making congestion worse, then re-probes from scratch.
///
/// # Token bucket
///
/// The `TokenBucket` converts the profiler's chosen rate into a packet-level
/// gate: a packet may only be sent if there are enough tokens.  Tokens refill
/// continuously at `rate_bytes_per_sec`.  This prevents micro-bursts that would
/// fill switch buffers and *increase* loss rather than decrease it.

use std::time::Instant;

// ── Token Bucket Pacer ────────────────────────────────────────────────────────

/// A token-bucket rate limiter for outgoing UDP datagrams.
///
/// One token = one byte of wire budget (header + payload).
/// Tokens accumulate at `rate_bytes_per_sec` and are consumed by `try_consume`.
pub struct TokenBucket {
    /// Maximum tokens to accumulate (caps burst to ~one MTU worth of tokens).
    capacity_bytes: u64,
    /// Currently available tokens (fractional bytes tracked as integer µ-tokens).
    tokens: f64,
    /// Target send rate in bytes / second.
    rate_bytes_per_sec: f64,
    /// Timestamp of the last `refill` call.
    last_refill: Instant,
}

impl TokenBucket {
    /// Create a pacer with an initial rate of `rate_bps` bits per second.
    ///
    /// Capacity is clamped to 2 MTU (2 × 1500 bytes) so we never accumulate
    /// enough tokens to send a large burst after an idle period.
    pub fn new(rate_bps: u64) -> Self {
        let rate_bytes = rate_bps as f64 / 8.0;
        Self {
            capacity_bytes: 3000,
            tokens: rate_bytes / 1000.0, // seed with ~1 ms worth
            rate_bytes_per_sec: rate_bytes,
            last_refill: Instant::now(),
        }
    }

    /// Update the target send rate (call when the profiler picks a new step).
    pub fn set_rate(&mut self, rate_bps: u64) {
        self.rate_bytes_per_sec = rate_bps as f64 / 8.0;
        // Don't carry over stale tokens when changing rate drastically.
        self.tokens = self.tokens.min(self.capacity_bytes as f64);
    }

    /// Add tokens proportional to elapsed time since the last refill.
    ///
    /// Call this at the top of the send loop iteration before `try_consume`.
    pub fn refill(&mut self) {
        let now     = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;
        self.tokens = (self.tokens + self.rate_bytes_per_sec * elapsed)
            .min(self.capacity_bytes as f64);
    }

    /// Try to consume `bytes` tokens for one datagram of that size.
    ///
    /// Returns `true` (send is allowed) if enough tokens were available and
    /// deducted.  Returns `false` (caller should wait) when the bucket is dry.
    pub fn try_consume(&mut self, bytes: usize) -> bool {
        if self.tokens >= bytes as f64 {
            self.tokens -= bytes as f64;
            true
        } else {
            false
        }
    }

    /// Remaining tokens as a byte count (diagnostic / test helper).
    pub fn available_bytes(&self) -> f64 {
        self.tokens
    }
}

// ── Bandwidth Profiler ────────────────────────────────────────────────────────

/// Probe rates in bits per second (10 Mbps → 1 Gbps).
const PROBE_STEPS_BPS: [u64; 7] = [
    10_000_000,
    50_000_000,
    100_000_000,
    200_000_000,
    500_000_000,
    1_000_000_000,
    2_000_000_000,
];

/// Minimum safe rate (5 Mbps).  We throttle to this on >30% instantaneous loss.
const FLOOR_RATE_BPS: u64 = 5_000_000;

/// Duration (in ms) spent at each probe step before evaluating.
const PROBE_STEP_MS: u64 = 500;

/// Loss threshold above which we abort the current probe and throttle.
const THROTTLE_LOSS_THRESHOLD: f64 = 0.30;

/// The three phases the profiler can be in.
#[derive(Debug, Clone, PartialEq)]
pub enum ProberPhase {
    /// Stepping through `PROBE_STEPS_BPS` looking for the goodput peak.
    Probing { step_index: usize },
    /// Locked onto the best rate; will monitor and re-probe if conditions degrade.
    Steady { rate_bps: u64 },
    /// Emergency floor: loss spiked above 30%.  Re-probe after a back-off.
    Throttling,
}

/// Measures goodput per probe step.
#[derive(Default, Clone)]
struct ProbeResult {
    bytes_sent:  u64,
    bytes_acked: u64,
}

impl ProbeResult {
    fn goodput_ratio(&self) -> f64 {
        if self.bytes_sent == 0 {
            return 0.0;
        }
        (self.bytes_acked as f64 / self.bytes_sent as f64).min(1.0)
    }
}

/// Slow-start bandwidth prober with loss-based throttling.
///
/// The prober works by running the token bucket at each of `PROBE_STEPS_BPS`
/// for `PROBE_STEP_MS` milliseconds, observing the resulting goodput, and
/// selecting the rate whose goodput is highest before it starts declining.
///
/// The prober also watches instantaneous loss via a smoothed EMA and will
/// immediately drop to `FLOOR_RATE_BPS` if loss exceeds 30%.
pub struct BandwidthProfiler {
    /// Current state machine phase.
    pub phase: ProberPhase,

    /// Goodput observations per step (indexed by `PROBE_STEPS_BPS`).
    probe_results: Vec<ProbeResult>,

    /// Bytes sent since the current step started.
    step_bytes_sent: u64,
    /// Bytes acked since the current step started.
    step_bytes_acked: u64,
    /// Wall time when the current step started.
    step_start: Instant,

    /// Smoothed loss EMA for throttle detection (α = 0.2).
    loss_ema: f64,

    /// Best rate discovered so far (bps).
    best_rate_bps: u64,
    /// Goodput ratio at `best_rate_bps`.
    best_goodput: f64,
}

impl BandwidthProfiler {
    pub fn new() -> Self {
        Self {
            phase: ProberPhase::Probing { step_index: 0 },
            probe_results: vec![ProbeResult::default(); PROBE_STEPS_BPS.len()],
            step_bytes_sent: 0,
            step_bytes_acked: 0,
            step_start: Instant::now(),
            loss_ema: 0.0,
            best_rate_bps: PROBE_STEPS_BPS[0],
            best_goodput: 0.0,
        }
    }

    /// Record bytes dispatched to the wire for the current step.
    pub fn on_bytes_sent(&mut self, n: u64) {
        self.step_bytes_sent += n;
    }

    /// Record bytes confirmed by the receiver; also checks for throttle trigger.
    ///
    /// Returns the new target rate in bps that the caller should set on the
    /// token bucket, or `None` if the rate hasn't changed.
    pub fn on_bytes_acked(&mut self, n: u64) -> Option<u64> {
        self.step_bytes_acked += n;

        // Update loss EMA.
        if self.step_bytes_sent > 0 {
            let step_loss = 1.0
                - (self.step_bytes_acked as f64 / self.step_bytes_sent as f64).min(1.0);
            self.loss_ema = 0.80 * self.loss_ema + 0.20 * step_loss;
        }

        // Throttle immediately if loss spikes above 30%.
        if self.loss_ema > THROTTLE_LOSS_THRESHOLD {
            return self.enter_throttle();
        }

        None
    }

    /// Called by the send loop on each timer tick.  Evaluates whether it is
    /// time to advance to the next probe step or commit to a steady rate.
    ///
    /// Returns the new target rate in bps if a transition occurred.
    pub fn evaluate_probe(&mut self) -> Option<u64> {
        let elapsed_ms = self.step_start.elapsed().as_millis() as u64;
        if elapsed_ms < PROBE_STEP_MS {
            return None; // still warming up
        }

        match &self.phase.clone() {
            ProberPhase::Probing { step_index } => {
                let idx = *step_index;
                let result = ProbeResult {
                    bytes_sent:  self.step_bytes_sent,
                    bytes_acked: self.step_bytes_acked,
                };
                let ratio = result.goodput_ratio();

                // Track the best goodput seen so far.
                if ratio > self.best_goodput {
                    self.best_goodput  = ratio;
                    self.best_rate_bps = PROBE_STEPS_BPS[idx];
                }
                self.probe_results[idx] = result;

                let next_idx = idx + 1;
                if next_idx >= PROBE_STEPS_BPS.len()
                    || (idx > 0 && ratio < self.probe_results[idx - 1].goodput_ratio() * 0.95)
                {
                    // Goodput peaked or we ran out of steps — commit to best rate.
                    self.phase = ProberPhase::Steady { rate_bps: self.best_rate_bps };
                    self.reset_step_counters();
                    return Some(self.best_rate_bps);
                }

                // Advance to the next step.
                self.phase = ProberPhase::Probing { step_index: next_idx };
                self.reset_step_counters();
                Some(PROBE_STEPS_BPS[next_idx])
            }

            ProberPhase::Steady { rate_bps } => {
                // In steady state, check if goodput has dropped significantly.
                let current_goodput = if self.step_bytes_sent > 0 {
                    (self.step_bytes_acked as f64 / self.step_bytes_sent as f64).min(1.0)
                } else {
                    1.0
                };
                if current_goodput < self.best_goodput * 0.80 {
                    // Conditions degraded; re-probe from scratch.
                    self.best_goodput = 0.0;
                    self.best_rate_bps = PROBE_STEPS_BPS[0];
                    self.phase = ProberPhase::Probing { step_index: 0 };
                    self.reset_step_counters();
                    return Some(PROBE_STEPS_BPS[0]);
                }
                self.reset_step_counters();
                // Stay at current rate.
                Some(*rate_bps)
            }

            ProberPhase::Throttling => {
                // After PROBE_STEP_MS at the floor, start re-probing.
                self.phase = ProberPhase::Probing { step_index: 0 };
                self.best_goodput  = 0.0;
                self.best_rate_bps = PROBE_STEPS_BPS[0];
                self.loss_ema      = 0.0;
                self.reset_step_counters();
                Some(PROBE_STEPS_BPS[0])
            }
        }
    }

    /// Current target rate in bps (the value that should be set on the bucket).
    pub fn current_rate_bps(&self) -> u64 {
        match &self.phase {
            ProberPhase::Probing { step_index } => PROBE_STEPS_BPS[*step_index],
            ProberPhase::Steady  { rate_bps }   => *rate_bps,
            ProberPhase::Throttling              => FLOOR_RATE_BPS,
        }
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn enter_throttle(&mut self) -> Option<u64> {
        if self.phase == ProberPhase::Throttling {
            return None; // already throttling
        }
        self.phase = ProberPhase::Throttling;
        self.reset_step_counters();
        Some(FLOOR_RATE_BPS)
    }

    fn reset_step_counters(&mut self) {
        self.step_bytes_sent  = 0;
        self.step_bytes_acked = 0;
        self.step_start       = Instant::now();
    }
}

impl Default for BandwidthProfiler {
    fn default() -> Self {
        Self::new()
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket_gates_on_empty() {
        let mut tb = TokenBucket::new(1_000_000); // 1 Mbps
        // Drain all tokens.
        tb.tokens = 0.0;
        assert!(!tb.try_consume(1400), "empty bucket should deny");
    }

    #[test]
    fn token_bucket_allows_when_full() {
        let mut tb = TokenBucket::new(1_000_000_000); // 1 Gbps
        tb.refill();
        assert!(tb.try_consume(1400), "full bucket should allow 1 packet");
    }

    #[test]
    fn token_bucket_set_rate_does_not_overflow() {
        let mut tb = TokenBucket::new(100_000_000);
        tb.tokens = 5000.0; // way above capacity
        tb.set_rate(100_000_000);
        assert!(tb.tokens <= tb.capacity_bytes as f64);
    }

    #[test]
    fn profiler_starts_at_10mbps() {
        let p = BandwidthProfiler::new();
        assert_eq!(p.current_rate_bps(), 10_000_000);
    }

    #[test]
    fn profiler_throttles_on_high_loss() {
        let mut p = BandwidthProfiler::new();
        // Simulate heavy loss: send 1 MB, ack 60 KB → ~94% loss → EMA will spike.
        for _ in 0..50 {
            p.on_bytes_sent(20_000);
            let r = p.on_bytes_acked(1_000); // ~5% goodput
            if r == Some(FLOOR_RATE_BPS) {
                assert_eq!(p.phase, ProberPhase::Throttling);
                return;
            }
        }
        panic!("expected throttle transition after sustained heavy loss");
    }
}
