use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct BandwidthPolicy {
    pub max_bps:      u64,
    pub burst_factor: f64,
}

pub struct PolicyEngine {
    default:  BandwidthPolicy,
    per_user: HashMap<String, BandwidthPolicy>,
}

impl PolicyEngine {
    /// Create with a default ceiling. `burst_factor` defaults to 1.5.
    pub fn new(default_max_bps: u64) -> Self {
        Self {
            default:  BandwidthPolicy { max_bps: default_max_bps, burst_factor: 1.5 },
            per_user: HashMap::new(),
        }
    }

    /// Return the policy for `username`, falling back to the default.
    pub fn resolve(&self, username: &str) -> BandwidthPolicy {
        self.per_user.get(username).cloned().unwrap_or_else(|| self.default.clone())
    }

    pub fn set_user_policy(&mut self, username: &str, max_bps: u64, burst_factor: f64) {
        self.per_user.insert(username.to_owned(), BandwidthPolicy { max_bps, burst_factor });
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_default_and_override() {
        let mut engine = PolicyEngine::new(100_000_000);
        let default = engine.resolve("unknown_user");
        assert_eq!(default.max_bps, 100_000_000);
        assert!((default.burst_factor - 1.5).abs() < f64::EPSILON);

        engine.set_user_policy("vip", 1_000_000_000, 2.0);
        let vip = engine.resolve("vip");
        assert_eq!(vip.max_bps, 1_000_000_000);
        assert!((vip.burst_factor - 2.0).abs() < f64::EPSILON);

        // Other users still get the default
        let other = engine.resolve("alice");
        assert_eq!(other.max_bps, 100_000_000);
    }
}
