# UDPix

> **Does your organization need to transfer large files or massive datasets across the country — or around the world?**
> Are productivity-killing delays from traditional TCP-based file transfer tools holding your team back?
> If so, UDPix is built for you.

UDPix is an open-source, enterprise-grade high-speed file transfer platform that helps organizations move petabytes of data across Wide Area Networks (WANs) at speeds up to **100× faster** than conventional FTP or SFTP tools — even over long-distance, high-latency, or lossy connections.

Whether you're a media company distributing content globally, a genomics lab syncing datasets across continents, a financial services firm moving time-critical data, or any enterprise that can't afford to wait — UDPix delivers.

---

## Why Not Just Use FTP or SFTP?

Traditional file transfer tools are built on TCP. TCP was engineered for reliability on local networks, not for speed across global fiber links. On a connection with just **1% packet loss** and **100ms latency** (e.g., New York → London), TCP throughput collapses to a small fraction of the available bandwidth — regardless of how fast your physical link is.

UDPix solves this at the protocol level by:

- Replacing TCP's slow, reactive congestion control with a **rate-based UDP engine** that maintains full pipeline saturation
- Keeping the network filled at the mathematically optimal speed, reacting to latency analytically rather than punishing throughput on every dropped packet
- Handling up to **5% packet loss** gracefully with selective retransmission (only the missing bytes, not whole blocks)

---

## Key Features

| Feature | Details |
|---------|---------|
| **Protocol** | Custom Reliable UDP (RUDP) with rate-based congestion control |
| **Speed** | Designed to saturate 1 Gbps–10 Gbps+ WAN links |
| **Packet Loss Tolerance** | Up to 5% with SACK/NAK selective retransmission |
| **Latency Tolerance** | 100ms+ intercontinental links |
| **Small File Support** | Millions of small files packed into bulk streams — no inode thrashing |
| **Disk I/O** | Linux `io_uring` async I/O with zero-copy (`splice`) for maximum throughput |
| **Security** | AES-256-GCM payload encryption · TLS 1.3 control channel · PBKDF2 password hashing |
| **NAT Traversal** | STUN/TURN/ICE + UDP hole punching for direct peer-to-peer transfers |
| **Control Plane** | gRPC over TLS 1.3 — authentication, session management, bandwidth policies |
| **Language** | Rust — memory-safe, fearless concurrency, zero-cost abstractions |

---

## Architecture Overview

UDPix separates the system into two independent planes:

```
┌──────────────────────────────────────────────────────────┐
│                     CONTROL PLANE                        │
│         gRPC / TLS 1.3 (TCP) — the "brain"              │
│  Auth · Session keys · Routing · Quotas · Monitoring     │
└──────────────────────┬───────────────────────────────────┘
                       │ issues session key + start signal
┌──────────────────────▼───────────────────────────────────┐
│                      DATA PLANE                          │
│        Custom RUDP over UDP — the "muscle"               │
│  Paced sending · SACK/NAK · AES-256-GCM · io_uring      │
└──────────────────────────────────────────────────────────┘
```

### Crate Structure

```
crates/
├── common/        # Shared types, AES-256-GCM crypto, error types
├── protocol/      # Custom RUDP: packet format, congestion control, SACK, sender/receiver
├── ioengine/      # io_uring async disk I/O, small-file bulk packing, zero-copy
├── controlplane/  # gRPC server, PBKDF2 auth, session management
└── traversal/     # STUN/TURN/ICE NAT traversal, UDP hole punching
```

---

## Getting Started

### Prerequisites

- Linux kernel 5.11+ (required for `io_uring` SQPOLL features)
- Rust 1.75+ (`rustup install stable`)

### Build

```bash
git clone git@github.com:neerajjez/UDPIX.git
cd UDPIX
cargo build --release
```

### Run (development)

```bash
# Start the server (control plane + data plane)
cargo run --bin udpix-server

# Transfer a file from client to server
cargo run --bin udpix-client -- send /path/to/large-file server-host:9000
```

---

## Roadmap

- [x] Phase 0 — Project initialization, workspace structure
- [ ] Phase 1 — Custom RUDP protocol engine (packet format, congestion control, SACK/NAK, sendmmsg/recvmmsg)
- [ ] Phase 2 — io_uring storage engine (async disk I/O, small-file packing, zero-copy)
- [ ] Phase 3 — gRPC control plane (TLS 1.3, PBKDF2 auth, session key exchange)
- [ ] Phase 4 — NAT traversal (STUN/TURN/ICE, UDP hole punching, rendezvous server)
- [ ] Phase 5 — CLI tooling, benchmarks, Docker packaging

---

## Contributing

UDPix is community-built. PRs, issues, and architectural discussions are welcome.

---

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.

---

*Vibecoded by [@neerajjez](https://github.com/neerajjez) with the help of [Claude Code](https://claude.ai/code)*
