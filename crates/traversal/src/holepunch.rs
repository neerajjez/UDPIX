/// UDP hole-punch orchestrator.
///
/// 1. Bind a SO_REUSEADDR + SO_REUSEPORT socket on `local_port` (via socket2)
/// 2. Discover our public address via STUN
/// 3. Run ICE connectivity checks against the peer's public address
/// 4. Fall back to TURN relay if ICE fails and a TURN server is configured
///
/// The returned `HolePunchResult` contains a ready `Arc<UdpSocket>` that
/// the Phase 1 Sender/Receiver can use directly.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::time::{sleep, Duration};

use crate::ice::{Candidate, CandidateType, IceAgent, IceRole};
use crate::stun::StunClient;

// ── Config types ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct TurnConfig {
    pub server:   SocketAddr,
    pub username: String,
    pub password: String,
}

#[derive(Clone)]
pub struct HolePunchConfig {
    pub stun_servers:        Vec<SocketAddr>,
    pub turn:                Option<TurnConfig>,
    pub attempts:            u32,
    pub attempt_interval_ms: u64,
}

impl Default for HolePunchConfig {
    fn default() -> Self {
        Self {
            stun_servers:        vec![],
            turn:                None,
            attempts:            10,
            attempt_interval_ms: 250,
        }
    }
}

// ── Result types ──────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct NatEndpoint {
    pub local_addr:  SocketAddr,
    pub public_addr: SocketAddr,
}

pub struct HolePunchResult {
    pub socket:     Arc<UdpSocket>,
    pub peer_addr:  SocketAddr,
    pub relay_used: bool,
}

// ── HolePuncher ───────────────────────────────────────────────────────────────

pub struct HolePuncher;

impl HolePuncher {
    /// Create a UDP socket bound to `local_port` with SO_REUSEADDR + SO_REUSEPORT.
    ///
    /// Both options must be set before `bind()` so that hole-punching can reuse
    /// the same port for STUN discovery and P2P communication.
    pub fn bind_reusable(local_port: u16) -> anyhow::Result<Arc<UdpSocket>> {
        let s2 = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
            .context("socket2::new")?;
        s2.set_reuse_address(true).context("SO_REUSEADDR")?;
        #[cfg(target_os = "linux")]
        s2.set_reuse_port(true).context("SO_REUSEPORT")?;
        s2.set_nonblocking(true).context("set_nonblocking")?;

        let bind_addr: std::net::SocketAddr = format!("0.0.0.0:{local_port}").parse().unwrap();
        s2.bind(&bind_addr.into()).context("socket2::bind")?;

        let std_sock: std::net::UdpSocket = s2.into();
        let tokio_sock = UdpSocket::from_std(std_sock).context("tokio UdpSocket::from_std")?;
        Ok(Arc::new(tokio_sock))
    }

    /// Discover our public (reflexive) address by querying the first STUN server
    /// that responds.
    pub async fn discover(config: &HolePunchConfig, local_port: u16) -> anyhow::Result<NatEndpoint> {
        let socket = Self::bind_reusable(local_port)?;
        let local_addr = socket.local_addr().context("local_addr")?;

        for &stun_addr in &config.stun_servers {
            match StunClient::discover_with_socket(&socket, stun_addr).await {
                Ok(public) => return Ok(NatEndpoint { local_addr, public_addr: public }),
                Err(e) => tracing::warn!("STUN server {stun_addr} failed: {e}"),
            }
        }
        anyhow::bail!("all STUN servers failed or none configured")
    }

    /// Full punch pipeline:
    ///   1. Discover our reflexive address
    ///   2. Build ICE check list: local host + reflexive candidates vs. peer addr
    ///   3. Run ICE checks (simultaneous UDP send)
    ///   4. If ICE fails and TURN is configured → fall back to relay
    pub async fn punch(
        config:          HolePunchConfig,
        peer_public_addr: SocketAddr,
        local_port:       u16,
    ) -> anyhow::Result<HolePunchResult> {
        let socket = Self::bind_reusable(local_port)?;
        let local_addr = socket.local_addr().context("local_addr")?;

        // Discover reflexive address via STUN
        let mut public_addr_opt: Option<SocketAddr> = None;
        for &stun_addr in &config.stun_servers {
            if let Ok(pub_addr) = StunClient::discover_with_socket(&socket, stun_addr).await {
                public_addr_opt = Some(pub_addr);
                break;
            }
        }

        // Build ICE agent
        let mut agent = IceAgent::new(IceRole::Controlling);
        agent.add_local_candidate(Candidate::new(CandidateType::Host, local_addr, 65535));
        if let Some(pub_addr) = public_addr_opt {
            agent.add_local_candidate(Candidate::new(
                CandidateType::ServerReflexive, pub_addr, 65535,
            ));
        }
        agent.set_remote_candidates(vec![
            Candidate::new(CandidateType::Host, peer_public_addr, 65535),
        ]);
        agent.form_check_list();

        // Run ICE connectivity checks with retry spacing
        for attempt in 0..config.attempts {
            if let Ok(Some(pair)) = agent.run_checks(&socket).await {
                tracing::info!(
                    "ICE succeeded on attempt {attempt}: local={} remote={}",
                    pair.local.addr,
                    pair.remote.addr
                );
                return Ok(HolePunchResult {
                    socket,
                    peer_addr:  pair.remote.addr,
                    relay_used: false,
                });
            }
            sleep(Duration::from_millis(config.attempt_interval_ms)).await;
        }

        // ICE failed — try TURN relay
        if let Some(turn_cfg) = &config.turn {
            tracing::warn!("ICE failed after {} attempts; falling back to TURN relay", config.attempts);
            let mut turn = crate::turn::TurnClient::new(
                turn_cfg.server,
                turn_cfg.username.clone(),
                turn_cfg.password.clone(),
            )
            .await
            .context("TURN client init")?;

            let alloc = turn.allocate().await.context("TURN allocate")?;
            turn.create_permission(peer_public_addr).await.context("TURN create_permission")?;

            tracing::info!("TURN relay allocated: {}", alloc.relayed_addr);
            return Ok(HolePunchResult {
                socket,
                peer_addr:  alloc.relayed_addr,
                relay_used: true,
            });
        }

        anyhow::bail!(
            "NAT traversal failed: ICE ({} attempts) and no TURN fallback configured",
            config.attempts
        )
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    use crate::stun::{ATTR_XOR_MAPPED, BINDING_REQUEST, BINDING_SUCCESS, HEADER_LEN, MAGIC_COOKIE};
    use crate::stun::StunMessage;

    /// Spawn a minimal STUN responder on a random port and return its address.
    async fn spawn_stun_server() -> SocketAddr {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 512];
            loop {
                let Ok((n, src)) = server.recv_from(&mut buf).await else { break };
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
                let _ = server.send_to(&resp, src).await;
            }
        });
        addr
    }

    #[tokio::test]
    async fn bind_reusable_succeeds() {
        let sock = HolePuncher::bind_reusable(0).unwrap();
        let addr = sock.local_addr().unwrap();
        assert!(addr.port() > 0);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bind_reusable_same_port_twice() {
        let s1 = HolePuncher::bind_reusable(0).unwrap();
        let port = s1.local_addr().unwrap().port();
        let s2 = HolePuncher::bind_reusable(port);
        assert!(s2.is_ok(), "SO_REUSEPORT must allow second bind");
    }

    #[tokio::test]
    async fn discover_via_loopback_stun() {
        let stun_addr = spawn_stun_server().await;
        let config = HolePunchConfig {
            stun_servers: vec![stun_addr],
            ..Default::default()
        };

        let endpoint = HolePuncher::discover(&config, 0).await.unwrap();
        assert!(endpoint.public_addr.port() > 0);
        // On loopback the reflexive addr == local addr
        assert_eq!(endpoint.public_addr.port(), endpoint.local_addr.port());
    }
}
