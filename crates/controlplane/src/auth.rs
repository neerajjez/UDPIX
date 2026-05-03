use std::collections::HashMap;
use std::num::NonZeroU32;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use pbkdf2::pbkdf2_hmac;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

const PBKDF2_ITERATIONS: NonZeroU32 = unsafe { NonZeroU32::new_unchecked(100_000) };
const SALT_LEN: usize = 32;
const HASH_LEN: usize = 32;

struct StoredUser {
    hash: [u8; HASH_LEN],
    salt: [u8; SALT_LEN],
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Claims {
    pub sub: String, // username
    pub sid: String, // session UUID
    pub exp: usize,  // Unix timestamp expiry
}

pub struct AuthEngine {
    users:          HashMap<String, StoredUser>,
    jwt_secret:     Vec<u8>,
    token_ttl_secs: u64,
}

impl AuthEngine {
    pub fn new(jwt_secret: Vec<u8>, token_ttl_secs: u64) -> Self {
        Self { users: HashMap::new(), jwt_secret, token_ttl_secs }
    }

    /// Hash `password` with PBKDF2-HMAC-SHA256 and store under `username`.
    pub fn add_user(&mut self, username: String, password: &str) -> anyhow::Result<()> {
        let mut salt = [0u8; SALT_LEN];
        rand::thread_rng().fill_bytes(&mut salt);

        let mut hash = [0u8; HASH_LEN];
        pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, PBKDF2_ITERATIONS.get(), &mut hash);

        self.users.insert(username, StoredUser { hash, salt });
        Ok(())
    }

    /// Constant-time PBKDF2 verify. Returns false for unknown usernames.
    pub fn authenticate(&self, username: &str, password: &str) -> bool {
        let user = match self.users.get(username) {
            Some(u) => u,
            None => return false,
        };
        let mut candidate = [0u8; HASH_LEN];
        pbkdf2_hmac::<Sha256>(password.as_bytes(), &user.salt, PBKDF2_ITERATIONS.get(), &mut candidate);
        // Constant-time comparison via XOR reduction
        let diff: u8 = candidate.iter().zip(user.hash.iter()).fold(0, |acc, (a, b)| acc | (a ^ b));
        diff == 0
    }

    /// Issue an HS256 JWT with `{ sub, sid, exp }` claims.
    pub fn issue_token(&self, username: &str, session_id: &str) -> anyhow::Result<String> {
        use jsonwebtoken::{encode, EncodingKey, Header};
        let exp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock error")?
            .as_secs()
            .saturating_add(self.token_ttl_secs) as usize;

        let claims = Claims { sub: username.to_owned(), sid: session_id.to_owned(), exp };
        encode(&Header::default(), &claims, &EncodingKey::from_secret(&self.jwt_secret))
            .context("JWT encode failed")
    }

    /// Verify token signature and expiry. Returns parsed `Claims` on success.
    pub fn verify_token(&self, token: &str) -> anyhow::Result<Claims> {
        use jsonwebtoken::{decode, DecodingKey, Validation};
        let mut validation = Validation::default();
        validation.leeway = 0; // no clock-skew tolerance
        let data = decode::<Claims>(token, &DecodingKey::from_secret(&self.jwt_secret), &validation)
            .context("JWT verification failed")?;
        Ok(data.claims)
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    fn engine() -> AuthEngine {
        AuthEngine::new(b"test-secret-key-32-bytes-long!!!".to_vec(), 3600)
    }

    #[test]
    fn hash_and_verify() {
        let mut e = engine();
        e.add_user("alice".into(), "correct-horse-battery-staple").unwrap();
        assert!(e.authenticate("alice", "correct-horse-battery-staple"));
        assert!(!e.authenticate("alice", "wrong-password"));
        assert!(!e.authenticate("unknown", "any-password"));
    }

    #[test]
    fn jwt_issue_and_verify() {
        let e = engine();
        let token = e.issue_token("bob", "session-uuid-123").unwrap();
        let claims = e.verify_token(&token).unwrap();
        assert_eq!(claims.sub, "bob");
        assert_eq!(claims.sid, "session-uuid-123");
    }

    #[test]
    fn jwt_expired_rejected() {
        // TTL = 1 second; sleep past expiry
        let e = AuthEngine::new(b"test-secret-key-32-bytes-long!!!".to_vec(), 1);
        let token = e.issue_token("carol", "sess-456").unwrap();
        sleep(Duration::from_secs(2));
        assert!(e.verify_token(&token).is_err(), "expired token must be rejected");
    }
}
