/// ICE agent — RFC 8445.
///
/// Simplified implementation covering:
///   - Candidate types: Host, ServerReflexive, Relayed
///   - Priority formula (RFC 8445 §5.1.2)
///   - Candidate pair formation and sorting
///   - Connectivity checks via STUN Binding Requests
///   - Role: Controlling (initiator) or Controlled (responder)
///
/// We omit the full ICE state machine (Frozen/Waiting trickling, STUN role
/// conflict resolution) for Phase 4 and instead run a simplified "try all
/// pairs in priority order, return first success" check.

use std::net::SocketAddr;

use anyhow::Context;
use tokio::net::UdpSocket;
use crate::stun::StunClient;

// ── Candidate ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateType {
    Host,
    ServerReflexive,
    Relayed,
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub cand_type:    CandidateType,
    pub addr:         SocketAddr,
    pub priority:     u32,
    pub foundation:   String,
    pub component_id: u8, // 1 for data, 2 for RTCP (we only use 1)
}

impl Candidate {
    /// Create a candidate and compute its priority per RFC 8445 §5.1.2.
    pub fn new(cand_type: CandidateType, addr: SocketAddr, local_pref: u32) -> Self {
        let type_pref = Self::type_preference(&cand_type);
        let priority  = Self::compute_priority(type_pref, local_pref, 1);
        let foundation = format!("{cand_type:?}-{}", addr.ip());
        Self { cand_type, addr, priority, foundation, component_id: 1 }
    }

    fn type_preference(t: &CandidateType) -> u32 {
        match t {
            CandidateType::Host            => 126,
            CandidateType::ServerReflexive => 100,
            CandidateType::Relayed         =>   0,
        }
    }

    /// priority = (2^24) × type_pref + (2^8) × local_pref + (256 − component_id)
    pub fn compute_priority(type_pref: u32, local_pref: u32, component_id: u32) -> u32 {
        (1 << 24) * type_pref + (1 << 8) * local_pref + (256 - component_id)
    }
}

// ── Candidate pair ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairState { Waiting, InProgress, Succeeded, Failed }

#[derive(Debug, Clone)]
pub struct CandidatePair {
    pub local:    Candidate,
    pub remote:   Candidate,
    pub priority: u64, // pair priority (RFC 8445 §6.1.2.3)
    pub state:    PairState,
}

impl CandidatePair {
    fn new(local: Candidate, remote: Candidate, role: &IceRole) -> Self {
        let priority = Self::pair_priority(local.priority, remote.priority, role);
        Self { local, remote, priority, state: PairState::Waiting }
    }

    /// RFC 8445 §6.1.2.3:  min(G,D)<<32 | max(G,D)<<1 | tiebreak
    fn pair_priority(g: u32, d: u32, role: &IceRole) -> u64 {
        let (controlling, controlled) = match role {
            IceRole::Controlling => (g as u64, d as u64),
            IceRole::Controlled  => (d as u64, g as u64),
        };
        (controlling.min(controlled) << 32)
            | (controlling.max(controlled) << 1)
            | if controlling > controlled { 1 } else { 0 }
    }
}

// ── ICE agent ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IceRole { Controlling, Controlled }

pub struct IceAgent {
    pub role:       IceRole,
    local_cands:    Vec<Candidate>,
    remote_cands:   Vec<Candidate>,
    pub check_list: Vec<CandidatePair>,
}

impl IceAgent {
    pub fn new(role: IceRole) -> Self {
        Self { role, local_cands: Vec::new(), remote_cands: Vec::new(), check_list: Vec::new() }
    }

    pub fn add_local_candidate(&mut self, c: Candidate) {
        self.local_cands.push(c);
    }

    pub fn set_remote_candidates(&mut self, candidates: Vec<Candidate>) {
        self.remote_cands = candidates;
    }

    /// Form all (local × remote) pairs and sort by priority descending.
    pub fn form_check_list(&mut self) {
        self.check_list.clear();
        for l in &self.local_cands {
            for r in &self.remote_cands {
                // Only pair candidates of the same IP family
                if l.addr.is_ipv4() != r.addr.is_ipv4() { continue; }
                self.check_list.push(CandidatePair::new(l.clone(), r.clone(), &self.role));
            }
        }
        self.check_list.sort_by(|a, b| b.priority.cmp(&a.priority));
    }

    /// Run STUN connectivity checks on each Waiting pair in priority order.
    /// Returns the first pair that succeeds, or `None` if all fail.
    pub async fn run_checks(
        &mut self,
        socket: &UdpSocket,
    ) -> anyhow::Result<Option<CandidatePair>> {
        for pair in &mut self.check_list {
            if pair.state != PairState::Waiting { continue; }
            pair.state = PairState::InProgress;

            match stun_check(socket, pair.remote.addr).await {
                Ok(_) => {
                    pair.state = PairState::Succeeded;
                    return Ok(Some(pair.clone()));
                }
                Err(_) => {
                    pair.state = PairState::Failed;
                }
            }
        }
        Ok(None)
    }

    pub fn selected_pair(&self) -> Option<&CandidatePair> {
        self.check_list.iter().find(|p| p.state == PairState::Succeeded)
    }
}

// ── STUN connectivity check (simplified) ─────────────────────────────────────

async fn stun_check(socket: &UdpSocket, target: SocketAddr) -> anyhow::Result<SocketAddr> {
    // In a real ICE check we'd include USERNAME, MESSAGE-INTEGRITY, PRIORITY,
    // and USE-CANDIDATE.  For Phase 4 we send a plain Binding Request.
    StunClient::discover_with_socket(socket, target)
        .await
        .context("ICE connectivity check failed")
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn sa(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    #[test]
    fn candidate_priority_ordering() {
        let host = Candidate::new(CandidateType::Host,            sa(1000), 65535);
        let srflx = Candidate::new(CandidateType::ServerReflexive, sa(1001), 65535);
        let relay = Candidate::new(CandidateType::Relayed,          sa(1002), 65535);

        assert!(host.priority  > srflx.priority, "Host must outrank SrFlx");
        assert!(srflx.priority > relay.priority,  "SrFlx must outrank Relay");
        assert_eq!(relay.priority & 0xFF, 255);  // 256 - component_id(1)
    }

    #[test]
    fn form_check_list_sorted() {
        let mut agent = IceAgent::new(IceRole::Controlling);

        agent.add_local_candidate(Candidate::new(CandidateType::Host,            sa(1000), 100));
        agent.add_local_candidate(Candidate::new(CandidateType::ServerReflexive, sa(1001), 100));

        agent.set_remote_candidates(vec![
            Candidate::new(CandidateType::Host, sa(2000), 100),
        ]);
        agent.form_check_list();

        assert_eq!(agent.check_list.len(), 2);
        // Host × Host pair must rank above SrFlx × Host pair
        assert!(agent.check_list[0].priority >= agent.check_list[1].priority);
        // The top pair involves a Host local candidate
        assert_eq!(agent.check_list[0].local.cand_type, CandidateType::Host);
    }

    /// Two ICE agents run checks over loopback: one acts as the "STUN responder"
    /// (it binds a UDP socket that replies to Binding Requests), the other is the
    /// checker.
    #[tokio::test]
    async fn connectivity_check_loopback() {
        use crate::stun::{BINDING_REQUEST, BINDING_SUCCESS, HEADER_LEN, MAGIC_COOKIE, StunMessage};

        // Spawn a simple STUN responder
        let responder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let responder_addr = responder.local_addr().unwrap();

        tokio::spawn(async move {
            let mut buf = vec![0u8; 512];
            loop {
                let Ok((n, src)) = responder.recv_from(&mut buf).await else { break };
                let Ok(msg) = StunMessage::parse(&buf[..n]) else { continue };
                if msg.msg_type != BINDING_REQUEST { continue }

                // Echo back a Binding Success with XOR-MAPPED-ADDRESS = src
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
                resp.extend_from_slice(&crate::stun::ATTR_XOR_MAPPED.to_be_bytes());
                resp.extend_from_slice(&(val.len() as u16).to_be_bytes());
                resp.extend_from_slice(&val);
                let _ = responder.send_to(&resp, src).await;
            }
        });

        // Checker side
        let checker_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let checker_local = checker_sock.local_addr().unwrap();

        let mut agent = IceAgent::new(IceRole::Controlling);
        agent.add_local_candidate(Candidate::new(
            CandidateType::Host, checker_local, 65535,
        ));
        agent.set_remote_candidates(vec![
            Candidate::new(CandidateType::Host, responder_addr, 65535),
        ]);
        agent.form_check_list();

        let result = agent.run_checks(&checker_sock).await.unwrap();
        assert!(result.is_some(), "expected at least one Succeeded pair");
        assert_eq!(result.unwrap().state, PairState::Succeeded);
    }
}
