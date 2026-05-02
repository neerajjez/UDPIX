/// # udpix-traversal
///
/// NAT traversal — enables direct peer-to-peer transfers between clients that
/// are behind firewalls or NAT devices, without routing data through a relay.
///
/// ## The problem
/// Enterprise clients sit behind NAT routers. They have private IP addresses
/// (e.g., 192.168.x.x) that are not routable on the public internet. Two clients
/// behind different NATs cannot connect directly without help.
///
/// ## UDP hole punching (the solution for most NATs)
/// UDP is connectionless, which makes it ideal for hole punching:
///   1. Both clients connect to the public STUN server.
///   2. The STUN server observes each client's public IP:port as seen from outside
///      their NAT, and shares these mappings with the other peer.
///   3. Both clients simultaneously send UDP packets to each other's public address.
///   4. Each NAT device sees outbound traffic to that address and opens a temporary
///      "hole" — incoming packets from that address are now allowed through.
///   5. The two clients are now directly connected with no relay overhead.
///
/// ## Fallback: TURN relay (for symmetric NATs)
/// Symmetric NATs assign a different public port for every destination address,
/// making hole punching impossible. In this case the TURN server acts as a
/// high-bandwidth relay: both clients send encrypted UDP to the relay, which
/// forwards packets between them. The data is still encrypted end-to-end.
///
/// ## ICE: choosing the best path automatically
/// ICE (Interactive Connectivity Establishment) automates the negotiation.
/// It gathers all possible connection candidates (local, STUN-discovered, TURN)
/// and probes them in parallel, selecting the lowest-latency working path.
///
/// ## Modules
/// - `stun`       — STUN client (RFC 5389): discovers public IP:port via server
/// - `turn`       — TURN relay client (RFC 5766): fallback relay allocation
/// - `ice`        — ICE candidate gathering and connectivity checks
/// - `holepunch`  — Simultaneous UDP hole punch logic for Full Cone / Port Restricted NATs

pub mod holepunch;
pub mod ice;
pub mod stun;
pub mod turn;
