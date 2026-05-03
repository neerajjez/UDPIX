use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::RngCore;
use uuid::Uuid;
use zeroize::Zeroizing;

// ── Session data ──────────────────────────────────────────────────────────────

pub struct SessionInfo {
    pub session_uuid:   String,
    pub numeric_id:     u32,         // embedded in UDP packet headers
    pub username:       String,
    pub session_key:    Zeroizing<[u8; 32]>,
    pub session_nonce:  [u8; 12],
    pub max_bps:        u64,
    pub created_at:     Instant,
    pub last_heartbeat: Instant,
}

impl Clone for SessionInfo {
    fn clone(&self) -> Self {
        Self {
            session_uuid:   self.session_uuid.clone(),
            numeric_id:     self.numeric_id,
            username:       self.username.clone(),
            session_key:    Zeroizing::new(*self.session_key),
            session_nonce:  self.session_nonce,
            max_bps:        self.max_bps,
            created_at:     self.created_at,
            last_heartbeat: self.last_heartbeat,
        }
    }
}

// ── Manager ───────────────────────────────────────────────────────────────────

pub struct SessionManager {
    sessions:    parking_lot::RwLock<HashMap<String, SessionInfo>>,
    next_id:     AtomicU32,
    session_ttl: Duration,
}

impl SessionManager {
    pub fn new(session_ttl: Duration) -> Arc<Self> {
        Arc::new(Self {
            sessions:    parking_lot::RwLock::new(HashMap::new()),
            next_id:     AtomicU32::new(1),
            session_ttl,
        })
    }

    /// Allocate a new session: generate AES-256-GCM key + nonce, assign numeric id.
    pub fn create(&self, username: String, max_bps: u64) -> anyhow::Result<SessionInfo> {
        let mut rng = rand::thread_rng();

        let mut key_bytes = [0u8; 32];
        rng.fill_bytes(&mut key_bytes);

        let mut nonce_bytes = [0u8; 12];
        rng.fill_bytes(&mut nonce_bytes);

        let numeric_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let session_uuid = Uuid::new_v4().to_string();
        let now = Instant::now();

        let info = SessionInfo {
            session_uuid:   session_uuid.clone(),
            numeric_id,
            username,
            session_key:    Zeroizing::new(key_bytes),
            session_nonce:  nonce_bytes,
            max_bps,
            created_at:     now,
            last_heartbeat: now,
        };

        self.sessions.write().insert(session_uuid, info.clone());
        Ok(info)
    }

    /// Return a clone of the session or `None` if not found.
    pub fn get_clone(&self, uuid: &str) -> Option<SessionInfo> {
        self.sessions.read().get(uuid).cloned()
    }

    /// Update the heartbeat timestamp. Returns false if session not found.
    pub fn heartbeat(&self, uuid: &str) -> bool {
        let mut lock = self.sessions.write();
        if let Some(s) = lock.get_mut(uuid) {
            s.last_heartbeat = Instant::now();
            true
        } else {
            false
        }
    }

    /// Remove a session. Returns false if it did not exist.
    pub fn terminate(&self, uuid: &str) -> bool {
        self.sessions.write().remove(uuid).is_some()
    }

    /// Remove all sessions whose last heartbeat exceeds the TTL.
    /// Returns the number of sessions reaped.
    pub fn reap_expired(&self) -> usize {
        let now = Instant::now();
        let ttl = self.session_ttl;
        let mut lock = self.sessions.write();
        let before = lock.len();
        lock.retain(|_, s| now.duration_since(s.last_heartbeat) < ttl);
        before - lock.len()
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_get() {
        let mgr = SessionManager::new(Duration::from_secs(60));
        let info = mgr.create("alice".into(), 100_000_000).unwrap();
        assert_eq!(info.username, "alice");
        assert_eq!(info.session_key.len(), 32);
        assert_eq!(info.session_nonce.len(), 12);

        let got = mgr.get_clone(&info.session_uuid).unwrap();
        assert_eq!(got.numeric_id, info.numeric_id);
    }

    #[test]
    fn heartbeat_extends_session() {
        let mgr = SessionManager::new(Duration::from_millis(200));
        let info = mgr.create("bob".into(), 50_000_000).unwrap();

        std::thread::sleep(Duration::from_millis(150));
        assert!(mgr.heartbeat(&info.session_uuid), "heartbeat should succeed");

        std::thread::sleep(Duration::from_millis(150));
        // Still alive because heartbeat reset the timer
        assert!(mgr.get_clone(&info.session_uuid).is_some());
    }

    #[test]
    fn reap_expired() {
        let mgr = SessionManager::new(Duration::from_millis(100));
        let s1 = mgr.create("carol".into(), 0).unwrap();
        let s2 = mgr.create("dave".into(), 0).unwrap();

        std::thread::sleep(Duration::from_millis(150));
        let reaped = mgr.reap_expired();
        assert_eq!(reaped, 2);
        assert!(mgr.get_clone(&s1.session_uuid).is_none());
        assert!(mgr.get_clone(&s2.session_uuid).is_none());
    }

    #[test]
    fn terminate() {
        let mgr = SessionManager::new(Duration::from_secs(60));
        let info = mgr.create("eve".into(), 0).unwrap();
        assert!(mgr.terminate(&info.session_uuid));
        assert!(!mgr.terminate(&info.session_uuid), "double-terminate must return false");
        assert!(mgr.get_clone(&info.session_uuid).is_none());
    }
}
