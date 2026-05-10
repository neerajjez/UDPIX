# UDPix

> **Does your organization need to transfer large files or massive datasets across the country — or around the world?**
> Are productivity-killing delays from traditional TCP-based file transfer tools holding your team back?
> If so, UDPix is built for you.

UDPix is an open-source, enterprise-grade high-speed file transfer platform that helps organizations move terabytes of data across Wide Area Networks (WANs) at speeds up to **100× faster** than conventional FTP or SFTP tools — even over long-distance, high-latency, or lossy connections.

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

| Requirement | Version | Notes |
|-------------|---------|-------|
| Linux kernel | 5.11+ | Required for `io_uring` SQPOLL features |
| Rust toolchain | 1.75+ | Install via `rustup` |
| protobuf compiler | any | `apt install protobuf-compiler` |

```bash
# Install Rust if you don't have it
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env

# Install protobuf compiler (Debian/Ubuntu)
sudo apt install protobuf-compiler
```

---

### 1. Clone and Build

```bash
git clone https://github.com/neerajjez/UDPIX.git
cd UDPIX

# Build all crates in release mode
cargo build --release

# Binary lands here:
./target/release/udpix --help
```

---

### 2. Start the Server

On the **receiving machine** (or any machine that will accept incoming transfers):

```bash
# Insecure mode — fine for local/dev/testing
./target/release/udpix server --addr 0.0.0.0:9000

# With custom credentials
UDPIX_ADMIN_USER=alice UDPIX_ADMIN_PASS=hunter2 \
  ./target/release/udpix server --addr 0.0.0.0:9000

# With TLS (production)
./target/release/udpix server \
  --addr 0.0.0.0:9000 \
  --cert /path/to/cert.pem \
  --key  /path/to/key.pem
```

> The server listens on TCP port 9000 (gRPC control plane) and UDP port 9000 (data plane).
> Open both in your firewall.

---

### 3. Send Files

On the **sending machine**:

```bash
# Send a single file
./target/release/udpix send /path/to/largefile.bin SERVER_IP:9000

# Send an entire directory (millions of small files work fine)
./target/release/udpix send /data/dataset/ SERVER_IP:9000

# With a STUN server for NAT traversal (when both sides are behind NAT)
./target/release/udpix send /data/dataset/ SERVER_IP:9000 \
  --stun stun.l.google.com:19302

# Full options
./target/release/udpix send /data/dataset/ SERVER_IP:9000 \
  --username alice \
  --password hunter2 \
  --local-port 0
```

---

### 4. Receive Files

On the **receiving machine** (in a second terminal, alongside the server):

```bash
# Receive into a directory
./target/release/udpix receive ./output/ SENDER_IP:9000

# With STUN (NAT traversal)
./target/release/udpix receive ./output/ SENDER_IP:9000 \
  --stun stun.l.google.com:19302
```

---

### 5. Docker (zero-dependency deploy)

```bash
# Build the image
docker build -t udpix:latest .

# Run the server
docker run -d \
  -p 9000:9000 -p 9000:9000/udp \
  -e UDPIX_ADMIN_PASS=hunter2 \
  -v $(pwd)/received:/received \
  udpix:latest server --addr 0.0.0.0:9000

# Full stack (server + coturn STUN/TURN relay)
docker compose up
```

---

### 6. Run Tests and Benchmarks

```bash
# Full test suite (55 tests across all crates)
cargo test

# Criterion benchmarks (RUDP encode/decode, SACK bitmap, I/O packing)
cargo bench -p udpix-bench
```

---

## Roadmap

- [x] Phase 0 — Project initialization, workspace structure
- [x] Phase 1 — Custom RUDP protocol engine (packet format, congestion control, SACK/NAK, sendmmsg/recvmmsg)
- [x] Phase 2 — io_uring storage engine (async disk I/O, small-file packing, zero-copy)
- [x] Phase 3 — gRPC control plane (TLS 1.3, PBKDF2 auth, session key exchange)
- [x] Phase 4 — NAT traversal (STUN/TURN/ICE, UDP hole punching, rendezvous server)
- [x] Phase 5 — CLI tooling, benchmarks, Docker packaging

---

## Integration Testing

The `testing/` directory contains a Docker Compose setup for end-to-end LAN transfer testing. It spins up two containers on a virtual 172.28.1.0/24 network — a sender and a receiver — transfers 505 files totalling ~115 MB, and verifies every file via SHA-256 checksums.

```bash
cd testing
docker compose up --build
# Expected: "RESULT: ALL 505 FILES VERIFIED — PASS"
```

> **io_uring in Docker** requires `security_opt: seccomp:unconfined` and `ulimits: memlock: -1` in the compose file.

---

## Bug Fixes — LAN Integration Test

Four bugs were found and fixed during the first real end-to-end transfer test:

### 1. PackBlock Framing Mismatch
**Symptom:** `bad magic 0x28805E78` error on receiver  
**Root cause:** The RUDP Receiver delivers individual ~1443-byte UDP payloads to the data channel. The IoEngine writer expected each `Vec<u8>` to be a *complete* PackBlock starting with the `UDPX` magic — but it was receiving fragments.  
**Fix:** Direct-mode sender now prefixes each PackBlock with an 8-byte LE `u64` length before splitting into datagrams. The receiver runs a reassembly task that accumulates fragments and extracts complete blocks before handing them to the IoEngine.

### 2. RUDP Sender Stall (Direct Mode)
**Symptom:** Sender transferred 0 bytes — transfer completed instantly with no data  
**Root cause:** The RUDP `Sender::run()` loop has a 500ms probe tick; when both `cmd_rx` and `sack_rx` are dropped simultaneously, the select loop stalls until the 1-second timeout fires — by which time essentially nothing was sent.  
**Fix:** For `--direct` (LAN) mode, the full RUDP congestion machinery is bypassed. A lightweight send loop writes packets directly to the socket at line rate, then sends FIN ×5 with 20ms gaps.

### 3. Split-File Reassembly Broken
**Symptom:** 4 large files (>16 MB) had wrong checksums; 1 was missing entirely  
**Root cause:** The `AsyncWriter` called `submit_write()` with `truncate(true)` for *each part* of a split file, overwriting prior parts. Additionally, `drop(partial); partial = HashMap::new()` inside the loop silently discarded the accumulator on every block.  
**Fix:** `Packer::unpack_entries()` now exposes `part_index` / `total_parts`. The writer uses a `PartialFile` accumulator and only calls `submit_write()` once all parts have arrived.

### 4. UDP Receive Buffer Overflow
**Symptom:** 1 large file still missing after fix #3 — `bytes_acked = 114,961,559` vs `bytes_sent = 115,169,351` (~207 KB of actual packet loss)  
**Root cause:** The Linux default UDP receive buffer is 208 KB. At 131 MB/s sender rate, the receiver's kernel buffer overflowed and silently dropped packets.  
**Fix:** Receiver calls `setsockopt(SO_RCVBUF, 16 MB)` after bind. The kernel clips this to `net.core.rmem_max` (typically 4 MB), which is sufficient to absorb any burst.

**Final result: 505/505 files verified — PASS. Sender 122 MB/s, receiver 53 MB/s, 2,065 ms.**

---

## Phase 1 Functional Correctness Test Suite

`testing/TEST_PLAN.md` defines 47 test cases across 6 phases. Phase 1 (13 tests) is now runnable:

```bash
# Run all 13 Phase 1 tests (empty files, large files, deep paths, unicode names, timing gaps, RUDP mode)
bash testing/run-phase1.sh

# Run a single scenario for debugging
P1_SCENARIO=p1-003 docker compose -f testing/docker-compose.phase1.yml up --build
```

Two additional bugs were found and fixed while building the Phase 1 test infrastructure:

### 5. Receiver Heartbeat Channel Deadlock

**Symptom:** With a 3+ second gap between receiver binding and sender connecting, `udpix receive` hung permanently — transfer never started.

**Root cause:** `send_heartbeat_sack()` called `self.sack_tx.send(sack).await` on a bounded channel (capacity 512). In direct mode, the consuming end (`_sack_rx`) is never read. After ~2.5 seconds (512 heartbeats × 5 ms), the channel filled and `.await` blocked forever — starving the `readable()` arm of the `tokio::select!` loop, so no incoming packets were ever processed.

**Fix:** Changed to `self.sack_tx.try_send(sack)` — non-blocking; drops the SACK payload if the channel is full rather than waiting. In direct mode the sender ignores SACKs anyway.

### 6. ICMP Socket State Corruption (Receiver-Before-Sender)

**Symptom:** When receiver bound its socket before the sender was listening, after ~3.5 seconds the socket stopped receiving data entirely even after the sender connected.

**Root cause:** The heartbeat loop sent UDP SACK packets to the peer's address even before any data had arrived. When the sender hadn't bound its port yet, the kernel generated ICMP "port unreachable" responses, which set `sk_err = ECONNREFUSED` on the receiver's connected UDP socket. After enough ICMP cycles, `recvmmsg` returned ECONNREFUSED on every call instead of actual data.

**Fix:** Added a `data_seen: bool` field to `Receiver`. Wire heartbeats (`socket.send()`) are suppressed until the first DATA packet is received, eliminating the ICMP flood while waiting for the sender.

### 7. Race: Empty `checksums.txt` Causes Premature Send Start

**Symptom:** P1-004 (512 MB transfer) — receiver showed 0 bytes written; sender completed before receiver ever bound its socket.

**Root cause:** Shell I/O redirect `sha256sum ... > checksums.txt` creates an empty file immediately (at the `>` operator), before `sha256sum` writes any output. The sender's polling loop detects the empty file, waits 3 seconds, then starts sending — while the receiver is still running `sha256sum` on 512 MB (which takes 2–3 s). By the time the receiver binds, the sender's UDP burst is already partly done.

**Fix:** Use an atomic write in all test receiver scripts: `sha256sum ... > checksums.txt.tmp && mv checksums.txt.tmp checksums.txt`. The `mv` (same-filesystem rename) is atomic; the sender only sees `checksums.txt` once it is complete.

### 8. `checksums.txt.tmp` Included in Checksum Manifest (find Glob Regression)

**Symptom:** P1-002, P1-005, P1-006 — receiver reported `MISSING: checksums.txt.tmp` during verification; 3/13 tests failed.

**Root cause:** The atomic-write fix (Bug #7) introduced a temporary file `checksums.txt.tmp`. The `find "$TESTDATA" -type f ! -name "checksums.txt"` patterns used to build the SHA-256 manifest did NOT exclude `checksums.txt.tmp`. Since the `>` redirect creates the `.tmp` file as empty before `find` runs, it was picked up and listed in the manifest. The receiver then checked for `/received/checksums.txt.tmp` — which was never transferred — and reported it missing.

**Fix:** Added `! -name "checksums.txt.tmp"` to all four `find` invocations in `entrypoint-receiver-p1.sh` that use a `! -name "checksums.txt"` exclusion. The `finalize_data_all()` helper and three inline scenario-specific `find` calls (P1-002, P1-005, P1-006) were all updated.

---

## Contributing

UDPix is community-built. PRs, issues, and architectural discussions are welcome.

---

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.

---

---

## A Note from the Author

I'm a systems and infrastructure engineer — not a software developer, definitely not a Rust developer.

The real reason this exists: at my job I've had to move 20–30 TB of data between on-prem environments, or cloud to cloud, over WAN. Every time it turned into a multi-day nightmare. Even getting 1 TB across a high-latency link with FTP or SFTP was painful — the kind of painful where you start the transfer, go home, come back in the morning, and it's still running. I started looking into what products actually solve this. There are a few, they work, and they cost as much as a car lease.

That felt like a common enough enterprise problem that someone should have built an open-source version of it. So I did a bit of research on why TCP falls apart on long-distance links, figured out what the right approach would be, and decided to build it — with a lot of help from Claude Code, which wrote essentially all of the Rust.

This is a vibe-coded project in the most literal sense. I brought the operational knowledge of what the problem actually is and what the solution needs to do. Claude brought the Rust. I'm giving it back to the community because I've taken a lot from open source over the years and this felt like something worth contributing.

If you're a real Rust engineer looking at this code — I know, I'm sorry, please open a PR.

*Vibecoded by [@neerajjez](https://github.com/neerajjez) with the help of [Claude Code](https://claude.ai/code)*
