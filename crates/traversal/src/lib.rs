// udpix-traversal — Phase 4 NAT traversal engine (STUN/TURN/ICE + UDP hole punching).
//
// Enterprise clients behind NAT use UDP hole punching for direct P2P connections.
// Symmetric NATs fall back to a TURN relay. ICE automates candidate selection.

pub mod stun;
pub mod turn;
pub mod ice;
pub mod holepunch;

use std::net::SocketAddr;

use holepunch::{HolePunchConfig, HolePunchResult, NatEndpoint};

// ── TraversalEngine ───────────────────────────────────────────────────────────

/// Top-level coordinator.  Create one per process; reuse across sessions.
pub struct TraversalEngine {
    config: HolePunchConfig,
}

impl TraversalEngine {
    pub fn new(config: HolePunchConfig) -> Self {
        Self { config }
    }

    /// Query STUN servers and return our public reflexive address.
    pub async fn discover_endpoint(&self, local_port: u16) -> anyhow::Result<NatEndpoint> {
        holepunch::HolePuncher::discover(&self.config, local_port).await
    }

    /// Establish a direct UDP path to `peer_public_addr` via hole punching.
    /// Falls back to TURN relay if ICE fails.
    pub async fn connect(
        &self,
        peer_public_addr: SocketAddr,
        local_port: u16,
    ) -> anyhow::Result<HolePunchResult> {
        holepunch::HolePuncher::punch(self.config.clone(), peer_public_addr, local_port).await
    }
}

// ── Integration tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use tokio::net::UdpSocket;

    use crate::stun::{
        ATTR_XOR_MAPPED, BINDING_REQUEST, BINDING_SUCCESS, HEADER_LEN, MAGIC_COOKIE,
        StunMessage,
    };

    async fn spawn_stun(bind: &str) -> SocketAddr {
        let srv = UdpSocket::bind(bind).await.unwrap();
        let addr = srv.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 512];
            loop {
                let Ok((n, src)) = srv.recv_from(&mut buf).await else { break };
                let Ok(msg) = StunMessage::parse(&buf[..n]) else { continue };
                if msg.msg_type != BINDING_REQUEST { continue }
                let IpAddr::V4(ipv4) = src.ip() else { continue };

                let x_port = src.port() ^ (MAGIC_COOKIE >> 16) as u16;
                let x_ip   = u32::from(ipv4) ^ MAGIC_COOKIE;
                let mut val = vec![0u8, 0x01];
                val.extend_from_slice(&x_port.to_be_bytes());
                val.extend_from_slice(&x_ip.to_be_bytes());
                let attr_len = 4 + val.len();
                let mut resp = vec![0u8; HEADER_LEN];
                resp[0] = (BINDING_SUCCESS >> 8) as u8;
                resp[1] =  BINDING_SUCCESS       as u8;
                resp[2] = (attr_len >> 8) as u8;
                resp[3] =  attr_len       as u8;
                resp[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
                resp[8..20].copy_from_slice(&msg.transaction_id);
                resp.extend_from_slice(&ATTR_XOR_MAPPED.to_be_bytes());
                resp.extend_from_slice(&(val.len() as u16).to_be_bytes());
                resp.extend_from_slice(&val);
                let _ = srv.send_to(&resp, src).await;
            }
        });
        addr
    }

    #[tokio::test]
    async fn traversal_engine_discover() {
        let stun_addr = spawn_stun("127.0.0.1:0").await;
        let engine = TraversalEngine::new(HolePunchConfig {
            stun_servers: vec![stun_addr],
            ..Default::default()
        });

        let endpoint = engine.discover_endpoint(0).await.unwrap();
        assert!(endpoint.public_addr.port() > 0);
    }

    #[tokio::test]
    async fn traversal_engine_connect_loopback() {
        // Spawn a STUN server (for discovery) and a "peer" socket that
        // responds to STUN Binding Requests (the ICE check).
        let stun_addr = spawn_stun("127.0.0.1:0").await;
        let peer_stun = spawn_stun("127.0.0.1:0").await;  // peer doubles as STUN responder

        let engine = TraversalEngine::new(HolePunchConfig {
            stun_servers: vec![stun_addr],
            attempts: 3,
            attempt_interval_ms: 50,
            turn: None,
        });

        // Connect to the peer's loopback STUN responder address.
        // ICE will send a Binding Request there and it will respond → Succeeded.
        let result = engine.connect(peer_stun, 0).await.unwrap();
        assert!(!result.relay_used, "should succeed without relay on loopback");
        assert_eq!(result.peer_addr, peer_stun);
    }
}
