/// STUN client — RFC 5389.
///
/// Implements just enough to send a Binding Request and read the
/// XOR-MAPPED-ADDRESS from the Binding Success Response.  This is
/// everything we need for NAT hole-punching and ICE candidate discovery.
///
/// Wire format:
///   [2B] message type (0x0001 Request, 0x0101 Success, 0x0111 Error)
///   [2B] message length  (bytes of attributes that follow, NOT including 20-B header)
///   [4B] magic cookie    (always 0x2112_A442)
///   [12B] transaction ID (random)
///   [attributes …]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::Context;
use rand::RngCore;
use tokio::net::UdpSocket;
use tokio::time::{timeout, Duration};

// ── Constants ─────────────────────────────────────────────────────────────────

pub const MAGIC_COOKIE:      u32 = 0x2112_A442;
pub const BINDING_REQUEST:   u16 = 0x0001;
pub const BINDING_SUCCESS:   u16 = 0x0101;
pub const BINDING_ERROR:     u16 = 0x0111;
pub const ATTR_MAPPED:       u16 = 0x0001;
pub const ATTR_XOR_MAPPED:   u16 = 0x0020;

pub const HEADER_LEN:   usize = 20;
const ATTR_HDR_LEN: usize = 4;   // type(2) + length(2)
const RECV_TIMEOUT: Duration = Duration::from_secs(3);

// ── Message types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum StunAttribute {
    XorMappedAddress(SocketAddr),
    MappedAddress(SocketAddr),
    Unknown { attr_type: u16, data: Vec<u8> },
}

#[derive(Debug, Clone)]
pub struct StunMessage {
    pub msg_type:       u16,
    pub transaction_id: [u8; 12],
    pub attributes:     Vec<StunAttribute>,
}

impl StunMessage {
    /// Create a new Binding Request with a random transaction ID.
    pub fn new_binding_request() -> Self {
        let mut tid = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut tid);
        Self { msg_type: BINDING_REQUEST, transaction_id: tid, attributes: Vec::new() }
    }

    /// Serialise to wire bytes.
    pub fn serialise(&self) -> Vec<u8> {
        let mut attrs = Vec::new();
        for attr in &self.attributes {
            serialise_attribute(attr, &mut attrs);
        }
        let mut out = Vec::with_capacity(HEADER_LEN + attrs.len());
        out.extend_from_slice(&self.msg_type.to_be_bytes());
        out.extend_from_slice(&(attrs.len() as u16).to_be_bytes());
        out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        out.extend_from_slice(&self.transaction_id);
        out.extend_from_slice(&attrs);
        out
    }

    /// Parse a STUN message from raw bytes.
    pub fn parse(buf: &[u8]) -> anyhow::Result<Self> {
        if buf.len() < HEADER_LEN {
            anyhow::bail!("STUN message too short: {} bytes", buf.len());
        }
        let msg_type   = u16::from_be_bytes([buf[0], buf[1]]);
        let attr_len   = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        let cookie     = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);

        if cookie != MAGIC_COOKIE {
            anyhow::bail!("STUN magic cookie mismatch: 0x{:08X}", cookie);
        }

        let mut tid = [0u8; 12];
        tid.copy_from_slice(&buf[8..20]);

        if buf.len() < HEADER_LEN + attr_len {
            anyhow::bail!("STUN buffer too short for declared attr_len");
        }

        let attrs_buf = &buf[HEADER_LEN..HEADER_LEN + attr_len];
        let attributes = parse_attributes(attrs_buf, &tid)?;

        Ok(Self { msg_type, transaction_id: tid, attributes })
    }

    /// Return the first XOR-MAPPED-ADDRESS or MAPPED-ADDRESS, in that preference order.
    pub fn mapped_addr(&self) -> Option<SocketAddr> {
        let mut fallback = None;
        for attr in &self.attributes {
            match attr {
                StunAttribute::XorMappedAddress(a) => return Some(*a),
                StunAttribute::MappedAddress(a)    => { fallback = Some(*a); }
                _                                  => {}
            }
        }
        fallback
    }
}

// ── Attribute serialisation / parsing ────────────────────────────────────────

fn serialise_attribute(attr: &StunAttribute, out: &mut Vec<u8>) {
    match attr {
        StunAttribute::Unknown { attr_type, data } => {
            out.extend_from_slice(&attr_type.to_be_bytes());
            out.extend_from_slice(&(data.len() as u16).to_be_bytes());
            out.extend_from_slice(data);
            // pad to 4-byte boundary
            let pad = (4 - (data.len() % 4)) % 4;
            out.extend(std::iter::repeat(0u8).take(pad));
        }
        // XOR-MAPPED-ADDRESS and MAPPED-ADDRESS are read-only in our client
        _ => {}
    }
}

fn parse_attributes(buf: &[u8], tid: &[u8; 12]) -> anyhow::Result<Vec<StunAttribute>> {
    let mut attrs = Vec::new();
    let mut pos = 0;
    while pos + ATTR_HDR_LEN <= buf.len() {
        let attr_type = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
        let attr_len  = u16::from_be_bytes([buf[pos + 2], buf[pos + 3]]) as usize;
        pos += ATTR_HDR_LEN;

        if pos + attr_len > buf.len() {
            anyhow::bail!("STUN attribute length overflows buffer");
        }
        let val = &buf[pos..pos + attr_len];

        let attr = match attr_type {
            ATTR_XOR_MAPPED => {
                let addr = decode_xor_mapped(val, tid)
                    .context("XOR-MAPPED-ADDRESS decode")?;
                StunAttribute::XorMappedAddress(addr)
            }
            ATTR_MAPPED => {
                let addr = decode_mapped(val)
                    .context("MAPPED-ADDRESS decode")?;
                StunAttribute::MappedAddress(addr)
            }
            _ => StunAttribute::Unknown {
                attr_type,
                data: val.to_vec(),
            },
        };
        attrs.push(attr);

        // advance past value + padding to 4-byte boundary
        let padded = attr_len + (4 - (attr_len % 4)) % 4;
        pos += padded;
    }
    Ok(attrs)
}

/// Decode XOR-MAPPED-ADDRESS value bytes.
fn decode_xor_mapped(val: &[u8], tid: &[u8; 12]) -> anyhow::Result<SocketAddr> {
    if val.len() < 4 { anyhow::bail!("XOR-MAPPED too short"); }
    let family = val[1];
    let x_port = u16::from_be_bytes([val[2], val[3]]);
    let port   = x_port ^ (MAGIC_COOKIE >> 16) as u16;

    match family {
        0x01 => {
            if val.len() < 8 { anyhow::bail!("XOR-MAPPED IPv4 too short"); }
            let x_ip = u32::from_be_bytes([val[4], val[5], val[6], val[7]]);
            let ip   = x_ip ^ MAGIC_COOKIE;
            Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port))
        }
        0x02 => {
            if val.len() < 20 { anyhow::bail!("XOR-MAPPED IPv6 too short"); }
            let mut x_bytes = [0u8; 16];
            x_bytes.copy_from_slice(&val[4..20]);
            // XOR with magic_cookie || transaction_id
            let xor_key: [u8; 16] = {
                let mut k = [0u8; 16];
                k[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
                k[4..].copy_from_slice(tid);
                k
            };
            for i in 0..16 { x_bytes[i] ^= xor_key[i]; }
            Ok(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(x_bytes)), port))
        }
        _ => anyhow::bail!("unknown XOR-MAPPED address family: {}", family),
    }
}

fn decode_mapped(val: &[u8]) -> anyhow::Result<SocketAddr> {
    if val.len() < 4 { anyhow::bail!("MAPPED-ADDRESS too short"); }
    let family = val[1];
    let port   = u16::from_be_bytes([val[2], val[3]]);
    match family {
        0x01 => {
            if val.len() < 8 { anyhow::bail!("MAPPED-ADDRESS IPv4 too short"); }
            let ip = u32::from_be_bytes([val[4], val[5], val[6], val[7]]);
            Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port))
        }
        _ => anyhow::bail!("MAPPED-ADDRESS IPv6 not implemented"),
    }
}

// ── STUN client ───────────────────────────────────────────────────────────────

pub struct StunClient;

impl StunClient {
    /// Bind a fresh UDP socket to 0.0.0.0:0, send a Binding Request to
    /// `stun_addr`, and return the reflexive SocketAddr.
    pub async fn discover_public_addr(stun_addr: SocketAddr) -> anyhow::Result<SocketAddr> {
        let socket = UdpSocket::bind("0.0.0.0:0").await.context("bind UDP for STUN")?;
        Self::discover_with_socket(&socket, stun_addr).await
    }

    /// Reuse an already-bound socket (so the same local port is probed).
    pub async fn discover_with_socket(
        socket: &UdpSocket,
        stun_addr: SocketAddr,
    ) -> anyhow::Result<SocketAddr> {
        let req = StunMessage::new_binding_request();
        let wire = req.serialise();

        socket.send_to(&wire, stun_addr).await.context("STUN send")?;

        let mut buf = vec![0u8; 512];
        let n = timeout(RECV_TIMEOUT, socket.recv(&mut buf))
            .await
            .context("STUN response timeout")?
            .context("STUN recv")?;

        let resp = StunMessage::parse(&buf[..n]).context("STUN parse response")?;
        if resp.msg_type != BINDING_SUCCESS {
            anyhow::bail!("STUN: expected Binding Success, got 0x{:04X}", resp.msg_type);
        }
        resp.mapped_addr()
            .ok_or_else(|| anyhow::anyhow!("STUN response has no MAPPED-ADDRESS"))
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_request_encode_decode() {
        let req = StunMessage::new_binding_request();
        let wire = req.serialise();

        assert_eq!(wire.len(), HEADER_LEN); // no attributes → exactly 20 bytes
        // First 2 bits of byte 0 must be 0 (RFC 5389 §6)
        assert_eq!(wire[0] & 0xC0, 0);
        // Magic cookie
        let cookie = u32::from_be_bytes([wire[4], wire[5], wire[6], wire[7]]);
        assert_eq!(cookie, MAGIC_COOKIE);
        // Transaction ID preserved
        assert_eq!(&wire[8..20], &req.transaction_id);

        // Parse it back
        let parsed = StunMessage::parse(&wire).unwrap();
        assert_eq!(parsed.msg_type, BINDING_REQUEST);
        assert_eq!(parsed.transaction_id, req.transaction_id);
        assert!(parsed.attributes.is_empty());
    }

    #[test]
    fn parse_xor_mapped_address_ipv4() {
        // Build a hand-crafted STUN Binding Success with one XOR-MAPPED-ADDRESS.
        // Address: 192.0.2.1:54321
        let ip: u32 = u32::from_be_bytes([192, 0, 2, 1]);
        let port: u16 = 54321;
        let tid = [0xABu8; 12];

        let x_port = port ^ (MAGIC_COOKIE >> 16) as u16;
        let x_ip   = ip ^ MAGIC_COOKIE;

        // Attribute value: padding(1) + family(1) + x_port(2) + x_ip(4) = 8 bytes
        let mut attr_val = vec![0u8, 0x01];
        attr_val.extend_from_slice(&x_port.to_be_bytes());
        attr_val.extend_from_slice(&x_ip.to_be_bytes());

        let mut msg = vec![0u8; HEADER_LEN];
        msg[0] = (BINDING_SUCCESS >> 8) as u8;
        msg[1] =  BINDING_SUCCESS       as u8;
        let attr_total = ATTR_HDR_LEN + attr_val.len();
        msg[2] = (attr_total >> 8) as u8;
        msg[3] =  attr_total       as u8;
        msg[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
        msg[8..20].copy_from_slice(&tid);

        // Append attribute header + value
        msg.extend_from_slice(&ATTR_XOR_MAPPED.to_be_bytes());
        msg.extend_from_slice(&(attr_val.len() as u16).to_be_bytes());
        msg.extend_from_slice(&attr_val);

        // Fix message length in header
        let body_len = msg.len() - HEADER_LEN;
        msg[2] = (body_len >> 8) as u8;
        msg[3] =  body_len       as u8;

        let parsed = StunMessage::parse(&msg).unwrap();
        let addr = parsed.mapped_addr().unwrap();
        assert_eq!(addr.port(), port);
        assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)));
    }

    /// Spawn a minimal STUN responder in-process and check the client gets its own addr back.
    #[tokio::test]
    async fn discover_loopback() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();

        tokio::spawn(async move {
            let mut buf = vec![0u8; 512];
            loop {
                let (n, src) = server.recv_from(&mut buf).await.unwrap();
                let req = StunMessage::parse(&buf[..n]).unwrap();
                if req.msg_type != BINDING_REQUEST { continue; }

                // Build XOR-MAPPED-ADDRESS for src
                let IpAddr::V4(ipv4) = src.ip() else { continue };
                let port = src.port();
                let x_port = port ^ (MAGIC_COOKIE >> 16) as u16;
                let x_ip   = u32::from(ipv4) ^ MAGIC_COOKIE;

                let mut attr_val = vec![0u8, 0x01];
                attr_val.extend_from_slice(&x_port.to_be_bytes());
                attr_val.extend_from_slice(&x_ip.to_be_bytes());

                let attr_body_len = ATTR_HDR_LEN + attr_val.len();
                let mut resp = vec![0u8; HEADER_LEN];
                resp[0] = (BINDING_SUCCESS >> 8) as u8;
                resp[1] =  BINDING_SUCCESS       as u8;
                resp[2] = (attr_body_len >> 8) as u8;
                resp[3] =  attr_body_len       as u8;
                resp[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
                resp[8..20].copy_from_slice(&req.transaction_id);
                resp.extend_from_slice(&ATTR_XOR_MAPPED.to_be_bytes());
                resp.extend_from_slice(&(attr_val.len() as u16).to_be_bytes());
                resp.extend_from_slice(&attr_val);

                let _ = server.send_to(&resp, src).await;
            }
        });

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_local = client.local_addr().unwrap();

        let discovered = StunClient::discover_with_socket(&client, server_addr)
            .await
            .unwrap();

        assert_eq!(discovered.port(), client_local.port());
    }
}
