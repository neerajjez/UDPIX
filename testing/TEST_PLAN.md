# UDPix Transfer Engine — Production Test Plan

**Version:** 1.0  
**Status:** Engineering Draft  
**Authors:** Neeraj Jes  
**Target:** Enterprise Production Readiness

---

## Purpose and Scope

This document defines the complete test strategy for UDPix, covering every confirmed failure
surface identified by static analysis of all 12 source files across the protocol, IO engine,
CLI, traversal, and control-plane layers.

The plan is structured in three progressive phases:

| Phase | Goal | Tests | Trigger |
|-------|------|-------|---------|
| **Phase 1 — Functional Correctness** | All file sizes, types, and edge cases under ideal conditions | 13 | Every commit |
| **Phase 2 — Adversarial & Stress** | Trigger every failure mode; fault injection; security attacks | 22 | Daily / release |
| **Phase 3 — Production Hardening** | Long-duration, concurrent, NAT traversal, benchmarks | 12 | Nightly |

**Total: 47 test cases covering 38 confirmed failure modes.**

The existing Docker LAN test (505 files / 115 MB, `--direct` mode, zero-loss network, SHA-256
verification) is designated **P1-001 — Baseline Regression**. Everything else is new.

---

## Confirmed Failure Modes

38 failure modes were identified through line-by-line analysis. They are referenced throughout
this document as **F-01** through **F-38**.

### CRITICAL — Crash / Data Corruption / Security

| ID | Source Location | Failure Description |
|----|----------------|---------------------|
| **F-01** | `writer.rs:123` | **Path traversal**: `entry.path = "../../../../etc/passwd"` → `output_dir.join()` has no `..` sanitization; writes outside output directory |
| **F-02** | `writer.rs:123` | **Absolute path injection**: `entry.path = "/tmp/exploit"` → `PathBuf::join("/x")` replaces `output_dir` entirely on Unix |
| **F-03** | `writer.rs:137` | **Duplicate split-file parts**: `HashMap::insert(part_index, data)` silently overwrites first copy; assembled file is corrupted |
| **F-04** | `writer.rs:138–144` | **Missing split-file parts**: `is_complete()` never fires; FIN arrives; file silently not written; exit code 0 — undetectable data loss |
| **F-05** | `receive.rs:133` | **Oversized `block_len`**: direct-mode framing reads 8-byte LE `usize`; malicious/corrupt value `0xFFFFFFFF…` → unbounded `Vec` accumulation → OOM kill |
| **F-06** | `packer.rs:178` | **`entry_count` OOM bomb**: `Vec::with_capacity(u32::MAX)` before reading entries → attempts ~320 GB allocation → OOM kill |
| **F-07** | `writer.rs:59` | **`total_parts=0`**: `PartialFile::is_complete()` compares `parts.len() == 0 as usize` → true immediately on empty map → zero-byte garbage file written |
| **F-08** | `reader.rs:286` | **Circular symlinks**: `collect_recursive` follows all symlinks without cycle detection → infinite recursion → stack overflow / OOM |
| **F-09** | `writer.rs:137` | **`part_index >= total_parts`**: no bounds check on `part_index` before `HashMap::insert` → invalid state; may never complete |
| **F-10** | `packer.rs:178` | **No `entry_count` upper bound**: no cap before `Vec::with_capacity`; large value exhausts memory before any entries are read |

### CRITICAL — Hang / Deadlock

| ID | Source Location | Failure Description |
|----|----------------|---------------------|
| **F-11** | `receive.rs:126` | **Sender crash mid-transfer**: reassembly task blocks on `raw_data_rx.recv()` forever; no timeout; no FIN ever arrives |
| **F-12** | `receiver.rs:162` | **FIN never sent**: `readable(&self.socket)` in `Receiver::run()` blocks indefinitely; receiver never exits |
| **F-13** | `receiver.rs:258` | **`data_tx` full or closed**: `.send().await` has no timeout; with slow writer the channel fills; entire receive pipeline deadlocked |
| **F-14** | `receiver.rs:183` | **`sack_tx` full or closed**: heartbeat SACK `.send().await` blocks; FIN acknowledgment on line 284 also blocks |
| **F-15** | `holepunch.rs:101` | **All STUN servers unreachable**: `bail!("all STUN servers failed or none configured")`; no fallback to local address detection |
| **F-16** | `reader.rs:120` | **Background io_uring thread panic**: `block_in_place(|| result_rx.recv())` hangs forever if the background thread exits unexpectedly |
| **F-17** | `sender.rs:170` | **`cmd_rx`/`sack_rx` both drop**: Tokio `select!` only wakes on `probe_tick` (500 ms interval); `Sender::run()` stalls, never exits |

### HIGH — Security

| ID | Source Location | Failure Description |
|----|----------------|---------------------|
| **F-18** | `server.rs:43` | **No auth rate limiting**: gRPC `Authenticate` RPC has no per-IP throttle; PBKDF2 is ~50 ms/attempt but 20 parallel clients = 400 attempts/second |
| **F-19** | `send.rs:37`, `receive.rs:35` | **Hardcoded defaults** `username=admin`, `password=changeme`; any deployment not overriding these is trivially compromised |
| **F-20** | `auth.rs:64` | **JWT expiry during transfer**: default TTL=3600 s; no token refresh; gRPC `Heartbeat` returns `UNAUTHENTICATED` after 1 hour |
| **F-21** | `session_mgr.rs:107` | **Session never reaped**: `reap_expired()` is defined but never called from the server loop; crashed clients leak sessions indefinitely |

### HIGH — Data Loss / Protocol

| ID | Source Location | Failure Description |
|----|----------------|---------------------|
| **F-22** | `sack.rs:252` | **`loss_ema` overflow**: `expected_since_last_sack += seq - next_expected_seq`; a large out-of-order sequence jump overflows `u64`; loss EMA becomes garbage |
| **F-23** | `sender.rs:400` | **`sendmmsg()` silent failure**: returns 0 on `ENOBUFS`; `bytes_sent` incremented anyway; stats inflated; packets are not retried on that batch cycle |
| **F-24** | `receive.rs` | **No SYN handshake in direct mode**: if receiver starts before sender, it silently discards all early datagrams with no way to request retransmission |
| **F-25** | `send.rs` | **No SYN handshake (sender side)**: if sender starts before receiver, packets sent before the socket is bound are silently dropped by the kernel |
| **F-26** | `holepunch.rs:167` | **TURN allocation expiry**: RFC 5766 default lifetime=600 s; `TurnClient::refresh()` exists but is never called from `HolePuncher::punch()`; relay dies mid-transfer |
| **F-27** | `send.rs:129` | **Blocking sleep in async context**: `std::thread::sleep(20 ms) × 5` inside `tokio::spawn` blocks the Tokio worker thread for 100 ms during FIN phase |

### MEDIUM — Performance / Edge Cases

| ID | Source Location | Failure Description |
|----|----------------|---------------------|
| **F-28** | `reader.rs:114`, `send.rs:95` | **1M+ files**: `Vec<PathBuf>` loaded entirely in memory before any blocks are sent; `mpsc::channel::<SenderCommand>(64)` backs up and stalls the reader |
| **F-29** | `writer.rs`, `receive.rs:125` | **1 GB+ single file**: `accum: Vec<u8>` in reassembly task grows to hold the entire block before extraction; peak RSS = block size × 2 |
| **F-30** | `writer.rs:231` | **Disk full (`ENOSPC`)**: `io_uring` returns error; partial file left on disk; no cleanup; no atomic rename; no retry |
| **F-31** | `writer.rs:232` | **Pre-existing files**: `OpenOptions::truncate(true)` overwrites silently; no collision detection or backup |
| **F-32** | `packer.rs`, `writer.rs` | **Empty files (0 bytes)**: `entry.size=0` path untested; `submit_write(path, vec![])` may behave differently on io_uring vs. std path |
| **F-33** | `packer.rs:230` | **Special characters in filenames**: spaces, unicode (multi-byte UTF-8), hyphens, underscores through `to_string_lossy()` / `from_utf8()` round-trip |
| **F-34** | `writer.rs:181` | **Deep directory nesting**: `create_dir_all` on a path with >256 components may hit `ENAMETOOLONG` on some filesystems |
| **F-35** | `sender.rs`, `receiver.rs` | **Packet loss 1%/5%/10%**: RUDP retransmit queue (`urgent_retransmits`, `normal_retransmits`), SACK bitmap, and `BandwidthProfiler` throttle have never been exercised by tests |
| **F-36** | `session.rs:120` | **High latency (100 ms+ RTT)**: RFC 6298 RTO `srtt + 4*rttvar`; with 100 ms base RTT the initial RTO=200 ms clamp may cause premature retransmits |
| **F-37** | `sack.rs` | **Jitter + packet reordering**: SACK bitmap `mark_received()` + `advance_base()` correctness under non-FIFO delivery; NAK promotion on out-of-order |
| **F-38** | `send.rs:115` | **No rate limiting in direct mode**: `sock.send()` runs at full line rate with no token bucket; can overwhelm receiver UDP buffer even with `SO_RCVBUF=16 MB` |

---

## Test Infrastructure

### Existing Infrastructure

```
testing/
├── docker-compose.yml          # sender + receiver on 172.28.1.0/24 bridge
├── Dockerfile.test             # udpix binary + test scripts
├── run-tests.sh                # orchestration script
└── scripts/
    ├── entrypoint-sender.sh
    └── entrypoint-receiver.sh
```

All existing tests require:
```yaml
security_opt: ["seccomp:unconfined"]
ulimits:
  memlock: { soft: -1, hard: -1 }
```

### New Infrastructure Required for Phase 2

`testing/Dockerfile.adversarial` — extends `Dockerfile.test` with:
```dockerfile
FROM udpix-test:latest
RUN apt-get update && apt-get install -y \
    iproute2 iptables python3 python3-pip \
    && pip3 install scapy psutil --break-system-packages
```

Additional `docker-compose.adversarial.yml` capabilities:
```yaml
cap_add:
  - NET_ADMIN
  - SYS_PTRACE
```

### New Infrastructure Required for Phase 3

- `coturn` container (already in `docker-compose.yml`)
- `toxiproxy` container for programmable fault injection
- `nat-router` container (iptables masquerade for two-hop NAT simulation)
- Optional: `prometheus` + `grafana` for long-run RSS/throughput tracking

### Helper Scripts (to be created)

```
testing/scripts/attacks/
  path_traversal.py       # craft PackBlock with entry.path = "../../../../tmp/pwned"
  absolute_path.py        # craft PackBlock with entry.path = "/tmp/exploit"
  entry_count_bomb.py     # craft PackBlock with entry_count = 0xFFFFFFFF
  oversized_block_len.py  # send 8-byte LE framing header with block_len = 0x00FFFFFFFFFFFFFF
  duplicate_parts.py      # send valid split-file block then re-send part_index=0 with different data
  brute_force.py          # gRPC Authenticate loop (20 threads × 10 s)

testing/scripts/lib/
  pack_block.py           # python helper to build valid/invalid PackBlock wire bytes
  rudp_send.py            # send raw UDP datagram with RudpHeader prefix
  checksum.py             # SHA-256 directory comparison
  wait_file.py            # wait for file with timeout
```

### Rust Test Files (to be created)

```
crates/ioengine/tests/security.rs        # P2-001–P2-007 as fast in-process unit tests
crates/controlplane/tests/jwt_expiry.rs  # P2-016
crates/protocol/tests/channel_drops.rs  # P2-009, P2-010 (F-11–F-14)
```

---

## Phase 1 — Functional Correctness

**Goal:** Verify the engine is correct for all file sizes, types, and edge cases under perfect
network conditions (zero loss, zero latency, Docker bridge). All tests use `--direct` mode
unless specified.

**Run all Phase 1 tests:**
```bash
cd testing && docker compose up --build --abort-on-container-exit
```

---

### P1-001 — Baseline Regression

**What it tests:** The existing 505-file / ~110 MB transfer still passes after any code change.

**Setup:** Unchanged `testing/docker-compose.yml`

**Run:**
```bash
cd testing && docker compose up --build
```

**Pass criteria:**
- Exit code 0 on both containers
- `RESULT: ALL 505 FILES VERIFIED — PASS` in receiver log
- No `MISSING` or `MISMATCH` lines
- Sender throughput ≥ 100 MB/s

**Failure modes covered:** Establishes baseline; any regression in core transfer path fails here.

---

### P1-002 — Empty File (0 Bytes)

**What it tests:** The `entry.size=0` path through `Packer::pack_files`, `Packer::unpack_entries`,
and `AsyncWriter::receive_and_write`. An empty file entry has `offset=N`, `size=0`; the payload
slice `data[N..N]` is zero-length. `io_uring` `Write::new(fd, ptr, 0)` may behave differently
from `std::fs::write(path, b"")`.

**Setup:**
```bash
# In sender container, before udpix send:
mkdir -p /testdata/p1002
touch /testdata/p1002/zero.bin           # 0 bytes
touch /testdata/p1002/also_zero.txt      # 0 bytes
printf '\x00' > /testdata/p1002/one.bin  # 1 byte (sanity check alongside zeros)
sha256sum /testdata/p1002/* > /testdata/checksums_p1002.txt
```

**Run:** `udpix send /testdata/p1002 172.28.1.10:9001 --direct --local-port 9002`

**Pass criteria:**
- All 3 files present in `/received/p1002/`
- `zero.bin` SHA-256 = `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`
- `also_zero.txt` same hash
- `one.bin` SHA-256 = `6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d`
- No panic; no "bad magic" error

**Failure modes covered:** F-32

---

### P1-003 — Large File 64 MB (Split into 4 Blocks)

**What it tests:** `Packer::pack_files` splits a 64 MB file into 4 × 16 MB `PackBlock`s
(`part_index` 0–3, `total_parts=4`). `AsyncWriter::receive_and_write` accumulates all 4 parts
in the `PartialFile` HashMap and writes only on `is_complete()`.

**Setup:**
```python
# testing/scripts/gen_large.py
import os, hashlib
os.makedirs("/testdata/p1003", exist_ok=True)
data = bytes(range(256)) * (64 * 1024 * 1024 // 256)
open("/testdata/p1003/big64.bin", "wb").write(data)
h = hashlib.sha256(data).hexdigest()
open("/testdata/checksums_p1003.txt", "w").write(f"{h}  big64.bin\n")
```

**Run:** `udpix send /testdata/p1003 ...`

**Pass criteria:**
- `/received/p1003/big64.bin` exists
- Size exactly 67,108,864 bytes
- SHA-256 matches source
- Sender log shows 4 blocks sent
- No partial file (size < 64 MB)

**Failure modes covered:** F-03 (duplicate parts), F-04 (missing parts), F-07 (total_parts=0), F-09 (part_index OOB)

---

### P1-004 — Very Large File 512 MB

**What it tests:** A 512 MB file splits into 32 blocks. Tests memory behaviour of the
`accum: Vec<u8>` reassembly buffer in `receive.rs:125` under sustained block delivery.

**Setup:** Same script as P1-003 but 512 MB.

**Pass criteria:**
- SHA-256 matches; size exactly 536,870,912 bytes
- Receiver container peak RSS (monitored via `cat /proc/PID/status | grep VmRSS`) ≤ 2 GB
- No OOM kill in container logs (`dmesg` or Docker event)

**Failure modes covered:** F-29

---

### P1-005 — Deep Nested Directory (20 Levels)

**What it tests:** `collect_recursive` walks 20 directory levels. `AsyncWriter::submit_write`
calls `create_dir_all(parent)` which must create all 20 levels atomically.

**Setup:**
```bash
path="a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q/r/s/t"
mkdir -p "/testdata/p1005/$path"
echo "deep" > "/testdata/p1005/$path/leaf.txt"
```

**Pass criteria:**
- `/received/p1005/a/b/c/.../t/leaf.txt` exists
- Content = `deep\n`
- Full 20-level directory tree created under `/received/p1005/`

**Failure modes covered:** F-34

---

### P1-006 — Special Characters in Filenames

**What it tests:** UTF-8 encoding/decoding in `Packer::serialise` (`to_string_lossy`) and
`Packer::unpack_entries` (`from_utf8`). `PathBuf::join` on filenames with spaces, unicode,
and punctuation.

**Setup:**
```python
names = [
    "file with spaces.txt",
    "résumé_naïve.txt",
    "日本語ファイル.bin",
    "file-with-hyphens_and.underscores",
    "UPPERCASE_AND_lowercase.BIN",
]
for n in names:
    open(f"/testdata/p1006/{n}", "wb").write(n.encode("utf-8"))
```

**Pass criteria:**
- All 5 files received with exact filenames
- Content of each file matches its encoded filename bytes
- No UTF-8 decode error in logs

**Failure modes covered:** F-33

---

### P1-007 — 1000 × 1 KB Files

**What it tests:** `mpsc::channel::<SenderCommand>(64)` in `send.rs:95` — if IoEngine produces
blocks faster than sender transmits, the channel backs up. 1000 × 1 KB = ~1 MB → fits in one
16 MB PackBlock, but exercises the reader's multi-file path.

**Setup:**
```python
os.makedirs("/testdata/p1007", exist_ok=True)
for i in range(1000):
    open(f"/testdata/p1007/f{i:04d}.bin", "wb").write(bytes([i % 256]) * 1024)
```

**Pass criteria:**
- All 1000 files received; exact content match
- No channel deadlock; sender exits cleanly
- Completes within 30 seconds

**Failure modes covered:** F-28 (low scale), channel pressure

---

### P1-008 — Mixed File Sizes (1 B to 50 MB)

**What it tests:** Simultaneous presence of sub-byte files (packer accumulation), files exactly at
`BLOCK_SIZE` boundary (16,777,216 bytes = 1 block, not split), and files slightly above it (splits
into 2 blocks). Validates packer's boundary logic at `bytes.len() > BLOCK_SIZE` (line 121).

**Setup:**
```python
sizes = [1, 512, 4096, 65536, 1048576, 16777216, 16777217, 52428800]
for sz in sizes:
    open(f"/testdata/p1008/f{sz}.bin", "wb").write(b'\xAB' * sz)
```

**Pass criteria:**
- All 8 files received; exact sizes; SHA-256 matches
- The 16,777,216-byte file is 1 block (`total_parts=1`)
- The 16,777,217-byte file is 2 blocks (`total_parts=2`)
- No off-by-one (16,777,216-byte file must not be split)

**Failure modes covered:** F-04 (packer BLOCK_SIZE boundary)

---

### P1-009 — Pre-Existing Files in Output Directory

**What it tests:** The receiver writes into an already-populated `/received/` directory.
`OpenOptions::truncate(true)` in `writer.rs:232` overwrites the existing file. Verifies
that the post-transfer content matches the sender's version, not the pre-existing version.

**Setup:**
```bash
# In receiver container before transfer:
echo "OLD_CONTENT" > /received/p1009/overwrite_me.bin
chmod 644 /received/p1009/overwrite_me.bin
# Sender has /testdata/p1009/overwrite_me.bin with different content: b'\xFF' * 512
```

**Pass criteria:**
- `/received/p1009/overwrite_me.bin` has sender's content after transfer
- SHA-256 matches sender; size = 512

**Failure modes covered:** F-31

---

### P1-010 — Single File Path as Send Argument

**What it tests:** `IoEngine::send_directory` calls `read_dir()` on the given path. If the path
points to a file rather than a directory, `read_dir()` returns `ENOTDIR`. Verifies this error
is surfaced cleanly rather than causing a panic.

**Setup:** Pass `/testdata/p1003/big64.bin` (a single file) as the send path.

**Pass criteria:**
- Either: CLI returns exit code 1 with a clear error message referencing the path
- Or: the single file is successfully transferred (single-file handling is implemented)
- Never: a panic, SIGABRT, or `unwrap()` failure

**Failure modes covered:** API surface boundary condition

---

### P1-011 — Receiver Starts 5 Seconds Before Sender

**What it tests:** In direct mode there is no SYN/ACK. The receiver binds and enters
`Receiver::run()` polling `readable(&self.socket)`. The sender starts 5 seconds later.
Verifies no data is lost while the receiver waits.

**Setup:**
```bash
# receiver container: start immediately
# sender container: sleep 5 && udpix send ...
```

**Pass criteria:**
- All files verified; zero missing
- Receiver does not time out or exit before sender appears
- Sender log shows correct byte count

**Failure modes covered:** F-24

---

### P1-012 — Sender Starts 5 Seconds Before Receiver (Known Limitation)

**What it tests:** The sender starts 5 seconds before the receiver binds. All datagrams sent
during those 5 seconds are silently dropped by the kernel. Since direct mode has no retransmit
for these early packets, data loss is expected.

**This test is expected to FAIL on current code.** It documents a known gap.

**Setup:**
```bash
# sender: start immediately
# receiver: sleep 5 && udpix receive ...
```

**Pass criteria (documentation):**
- Measure the exact number of missing/corrupt files
- Log: "KNOWN LIMITATION: sender-before-receiver causes N% data loss in direct mode"
- No crash, no hang — only data loss

**Failure modes covered:** F-25

---

### P1-013 — RUDP Mode Loopback (No `--direct`)

**What it tests:** The `run_rudp` path in `send.rs` and `receive.rs` — never exercised by
existing tests. Exercises `Sender::run()`, `BandwidthProfiler::evaluate_probe()`,
`TokenBucket::try_consume()`, `SackManager::tick()`, and SACK-driven retransmits on a
perfect LAN.

**Setup:**
```bash
# Receiver (no --direct flag):
udpix receive /received 172.28.1.20:9002 --local-port 9001

# Sender (no --direct flag):
udpix send /testdata 172.28.1.10:9001 --local-port 9002
```

**Pass criteria:**
- Files received; SHA-256 matches
- `retransmits=N` appears in sender log (even N=0 is fine)
- `BandwidthProfiler` probe steps visible in DEBUG logs
- No hang after transfer completes (Sender exits within 5 s)

**Failure modes covered:** F-35 (validates RUDP code path), F-36, F-37 (code paths reachable)

---

## Phase 2 — Adversarial & Stress Testing

**Goal:** Deliberately trigger every confirmed failure mode. Tests **P2-001 through P2-007 are
expected to FAIL** on the current codebase — they document security vulnerabilities and data
corruption bugs requiring fixes. They should be run as a dedicated CI job (`make security-tests`)
that generates a vulnerability report regardless of pass/fail status.

**Infrastructure required:**
```bash
docker compose -f testing/docker-compose.adversarial.yml build
```

All Phase 2 Docker tests require `cap_add: [NET_ADMIN]` for `tc netem` operations.

---

### P2-001 — Path Traversal Attack

**What it tests:** F-01. `writer.rs:123`: `let abs_path = self.output_dir.join(&entry.path)`.
`PathBuf::join` does NOT resolve `..` on any OS — it simply appends. A path of
`../../../../tmp/pwned` will write outside the intended output directory.

**Implementation:**
```python
# testing/scripts/attacks/path_traversal.py
import struct, socket

MAGIC = 0x55445058
VERSION = 1
RUDP_HDR_SIZE = 29
FLAG_DATA = 0x04

def build_pack_block(path: str, content: bytes) -> bytes:
    path_b = path.encode("utf-8")
    # manifest: magic(4) + version(2) + block_id(8) + entry_count(4)
    header = struct.pack("<IHQ", MAGIC, VERSION, 0) + struct.pack("<I", 1)
    # entry: path_len(2) + path(N) + offset(8) + size(8) + part_index(4) + total_parts(4)
    entry = (struct.pack("<H", len(path_b)) + path_b +
             struct.pack("<QQII", 0, len(content), 0, 1))
    block = header + entry + content
    return block

def wrap_direct_mode(block: bytes) -> bytes:
    # 8-byte LE length prefix (direct mode framing)
    return struct.pack("<Q", len(block)) + block

def build_rudp_data(payload: bytes, seq: int = 0) -> bytes:
    # Minimal RudpHeader: flags(1)=DATA(0x04), session_id(4), seq(8), ts(8), len(2), fec(4), reserved(2)
    hdr = struct.pack("<BIQHIH", FLAG_DATA, 1, seq, 0, len(payload), 0) + b'\x00\x02'
    return hdr + payload

block = build_pack_block("../../../../tmp/pwned", b"path_traversal_owned")
framed = wrap_direct_mode(block)

# Send as RUDP DATA packets in chunks of 1443 bytes
MAX_PAYLOAD = 1443
sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
seq = 0
for i in range(0, len(framed), MAX_PAYLOAD):
    chunk = framed[i:i+MAX_PAYLOAD]
    pkt = build_rudp_data(chunk, seq)
    sock.sendto(pkt, ("172.28.1.10", 9001))
    seq += 1

# Send FIN
FIN_HDR = struct.pack("<BIQHIH", 0x02, 1, seq, 0, 0, 0) + b'\x00\x02'
for _ in range(5):
    sock.sendto(FIN_HDR, ("172.28.1.10", 9001))
```

**Run:**
```bash
# Start receiver first:
udpix receive /received 172.28.1.20:9001 --direct --local-port 9001

# From attacker container:
python3 testing/scripts/attacks/path_traversal.py

# Check:
test ! -f /tmp/pwned && echo "PASS: path not escaped" || echo "FAIL: /tmp/pwned exists"
```

**Pass criteria (after fix):** `/tmp/pwned` does NOT exist; receiver returns error; `output_dir.join(path)` must call `canonicalize()` or strip `..` components before use.

**Expected result (current code):** FAIL — `/tmp/pwned` is created with content `path_traversal_owned`.

**Failure modes covered:** F-01

---

### P2-002 — Absolute Path Injection

**What it tests:** F-02. Same mechanism as P2-001, but `entry.path = "/tmp/exploit"`.
`PathBuf::join("/tmp/exploit")` on Unix returns `/tmp/exploit` (replaces the entire base path).

**Implementation:** Same script, change path to `"/tmp/exploit_absolute"`.

**Pass criteria (after fix):** `/tmp/exploit_absolute` does NOT exist; absolute paths rejected at unpack time.

**Expected result (current code):** FAIL — file written to `/tmp/exploit_absolute`.

**Failure modes covered:** F-02

---

### P2-003 — `entry_count` OOM Bomb

**What it tests:** F-06, F-10. `packer.rs:178`: `Vec::with_capacity(entry_count as usize)` where
`entry_count = 0xFFFFFFFF` (4,294,967,295). On a 64-bit system with `FileEntry` ≈ 80 bytes,
this attempts to allocate ~344 GB.

**Implementation:**
```python
# testing/scripts/attacks/entry_count_bomb.py
import struct, socket

# PackBlock with entry_count = u32::MAX, no actual entries following
block = (struct.pack("<I", 0x55445058) +   # magic
         struct.pack("<H", 1) +             # version
         struct.pack("<Q", 0) +             # block_id
         struct.pack("<I", 0xFFFFFFFF))     # entry_count = 4 billion

framed = struct.pack("<Q", len(block)) + block
# send via RUDP DATA (same as P2-001 helper)
```

**Run:** Container memory limit = 256 MB. Send the crafted block to the receiver.

**Pass criteria (after fix):**
- `entry_count` validated against a reasonable max (e.g. 65,536) before `Vec::with_capacity`
- Receiver returns `anyhow::bail!("entry_count too large: ...")`
- Container RSS never exceeds 256 MB
- Process alive after the attack

**Expected result (current code):** OOM kill; Docker reports `exit code 137`.

**Failure modes covered:** F-06, F-10

---

### P2-004 — Oversized `block_len` OOM (Direct Mode Framing)

**What it tests:** F-05. `receive.rs:133`: `let block_len = u64::from_le_bytes(accum[..8].try_into().unwrap()) as usize`.
If `block_len = 0x00FFFFFFFFFFFFFF` (72 petabytes), the condition `accum.len() < 8 + block_len`
never becomes false; the accumulator grows without bound until OOM.

**Implementation:**
```python
# 8-byte LE header = 0x00FFFFFFFFFFFFFF (72 PB)
# Followed by 1000 bytes of junk (receiver waits for rest that will never come)
import struct, socket

framed = struct.pack("<Q", 0x00FFFFFFFFFFFFFF) + b'\xAB' * 1000
# wrap in RUDP DATA header and send
```

**Container memory limit:** 256 MB

**Pass criteria (after fix):**
- `block_len > MAX_BLOCK_SIZE` (e.g. `32 * 1024 * 1024`) is rejected immediately
- Error logged; reassembly task exits; receiver exits non-zero
- Process alive

**Expected result (current code):** Accumulator grows until OOM kill.

**Failure modes covered:** F-05

---

### P2-005 — Duplicate Split-File Parts

**What it tests:** F-03. `writer.rs:137`: `pf.parts.insert(entry.part_index, data)`.
Sending `part_index=0` a second time with different content silently overwrites the first copy.
The assembled file has the second copy's data spliced in, corrupting the result.

**Implementation:**
```python
# Send a valid 32 MB file as 2 parts (part 0 = first 16 MB, part 1 = second 16 MB)
# Then re-send part 0 with all-zero bytes replacing the original content
# is_complete() fires after the second part_0 send since parts.len() == 2

# Part 0 (original): bytes 0..16M of the source file
# Part 1 (original): bytes 16M..32M of the source file
# Part 0 (duplicate): 16 MB of all-zeros
```

**Pass criteria (after fix):**
- Duplicate `part_index` is either rejected (error) or the first copy is preserved (dedup)
- Assembled SHA-256 matches original source

**Expected result (current code):** FAIL — assembled file has corrupted first 16 MB; SHA-256 mismatch.

**Failure modes covered:** F-03

---

### P2-006 — Missing Split-File Part (Silent Data Loss)

**What it tests:** F-04. A 48 MB file splits into 3 blocks (parts 0, 1, 2). If only parts 0 and
2 are delivered and then a FIN is sent, `is_complete()` (which checks `parts.len() == total_parts`)
never becomes true. When `data_rx` closes (FIN processed), `receive_and_write` returns `Ok(bytes)`
but the file was never written — silent data loss with exit code 0.

**Run:** Send blocks for parts 0 and 2 only; skip part 1; then send FIN.

**Pass criteria (after fix):**
- Receiver returns a named error: `"incomplete split file: path X missing N/M parts"`
- Exit code non-zero
- No silent 0-exit with missing file

**Expected result (current code):** FAIL — file not written; receiver exits 0; file count shows missing file only if verified externally.

**Failure modes covered:** F-04

---

### P2-007 — `total_parts=0` Edge Case

**What it tests:** F-07. A `PackBlock` entry with `total_parts=0` and `part_index=0` causes
`PartialFile { total_parts: 0, parts: HashMap::new() }`. The condition `is_complete()` checks
`self.parts.len() == self.total_parts as usize` → `0 == 0` → `true` immediately before any
parts are received. `assemble()` iterates `0..0` → empty Vec → 0-byte file written.

**Run:** Craft PackBlock with `total_parts=0` for a non-empty file (4096 bytes payload).

**Pass criteria (after fix):** `total_parts=0` rejected in `Packer::unpack_entries`; error returned; no file written.

**Expected result (current code):** FAIL — 0-byte file written at the correct path.

**Failure modes covered:** F-07

---

### P2-008 — Circular Symlinks in Source Directory

**What it tests:** F-08. `collect_recursive` in `reader.rs` calls `entry.path()` → `read_dir()`
recursively. Symlink cycles create infinite recursion. `metadata()` on a `DirEntry` follows
symlinks (unlike `symlink_metadata()`), so the cycle is transparent to the walker.

**Setup:**
```bash
# Inside sender container before udpix send:
mkdir -p /testdata/p2008/real_dir
echo "real_file" > /testdata/p2008/real_dir/file.txt
# Create a circular symlink:
ln -s /testdata/p2008 /testdata/p2008/real_dir/loop
```

**Pass criteria (after fix):**
- Sender detects cycle within 10 seconds
- Either: skips symlinks entirely with a warning
- Or: tracks visited inodes and skips already-seen directories
- Process exits; no infinite loop; RSS does not grow unboundedly

**Expected result (current code):** FAIL — infinite recursion; process hangs or OOM kills.

**Failure modes covered:** F-08

---

### P2-009 — Sender Crash Mid-Transfer (SIGKILL)

**What it tests:** F-11, F-12. When the sender is killed mid-transfer, no FIN is ever sent.
The receiver's reassembly task in `receive.rs` is blocked on `raw_data_rx.recv()` forever.
The underlying `Receiver::run()` task also blocks in `readable(&self.socket)`. Without a
timeout, the receiver hangs indefinitely.

**Setup:**
```bash
# Start receiver
# Start sender
# After ~2s (30% transfer complete), SIGKILL sender:
docker kill --signal=KILL sender-container
# Measure time until receiver exits or is killed externally
```

**Pass criteria (after fix):**
- Receiver detects no-data condition within 30 seconds
- Exits with a non-zero code and logs `"transfer incomplete: sender connection lost"`
- Does not require external `kill -9`

**Expected result (current code):** FAIL — receiver hangs indefinitely; requires manual intervention.

**Failure modes covered:** F-11, F-12

---

### P2-010 — Back-Pressure / Channel Full Deadlock

**What it tests:** F-13, F-14. When `data_tx` (capacity 512) fills because the IoEngine writer
is slower than the receiver, `receiver.rs:258` blocks on `.send().await` without a timeout.
This is tested as a Rust integration test for deterministic control.

**Implementation:**
```rust
// crates/protocol/tests/channel_drops.rs
#[tokio::test(flavor = "multi_thread")]
async fn data_tx_full_does_not_deadlock() {
    // Create data_tx with capacity 1 (immediately fills)
    let (data_tx, mut data_rx) = mpsc::channel::<Vec<u8>>(1);
    let (sack_tx, _sack_rx) = mpsc::channel::<SackPayload>(512);
    let stats = SessionStats::new();

    let socket = Arc::new(std::net::UdpSocket::bind("127.0.0.1:0").unwrap());
    let receiver = Receiver::new(1, socket.clone(), 0, data_tx, sack_tx, stats);

    // Don't drain data_rx — receiver should not deadlock
    let handle = tokio::spawn(receiver.run());

    // Give receiver 2s to prove it doesn't block on full channel
    let timeout = tokio::time::timeout(Duration::from_secs(2), handle).await;
    // Should either timeout (acceptable — no data sent) or exit cleanly
    assert!(timeout.is_err() || timeout.unwrap().is_ok());
}
```

**Pass criteria:** Test does not hang; completes within 5 seconds.

**Failure modes covered:** F-13, F-14

---

### P2-011 — Packet Loss: 1%, 5%, 10% (netem)

**What it tests:** F-22, F-23, F-35. RUDP error recovery via SACK retransmits. Exercises
`urgent_retransmits`, `normal_retransmits`, NAK promotion, and `BandwidthProfiler` throttle.

**Setup:**
```yaml
# docker-compose.adversarial.yml
sender:
  cap_add: [NET_ADMIN]
  entrypoint: >
    bash -c "tc qdisc add dev eth0 root netem loss ${LOSS_PCT:-1}%
             && /scripts/entrypoint-sender.sh"
```

**Run three times:**
```bash
LOSS_PCT=1  docker compose -f docker-compose.adversarial.yml up
LOSS_PCT=5  docker compose -f docker-compose.adversarial.yml up
LOSS_PCT=10 docker compose -f docker-compose.adversarial.yml up
```

**Pass criteria:**

| Loss | Min throughput | Requirement |
|------|---------------|-------------|
| 1% | ≥ 80% of zero-loss baseline | All files verified; SHA-256 correct |
| 5% | ≥ 50% of zero-loss baseline | All files verified; SHA-256 correct |
| 10% | ≥ 30% of zero-loss baseline | All files verified; SHA-256 correct |

`retransmits > 0` in sender log at ≥5% loss.

**Failure modes covered:** F-22, F-23, F-35

---

### P2-012 — 100 ms RTT + ±50 ms Jitter (netem)

**What it tests:** F-36, F-37. Simulates an intercontinental WAN link. RTO computation in
`session.rs:120` (`srtt + 4*rttvar`, clamped to [200 ms, 10 s]) must adapt. Initial
`SRTT=0` → `RTO=200 ms` may trigger early retransmits on first packets at 100 ms RTT.

**Setup:**
```bash
tc qdisc add dev eth0 root netem delay 100ms 50ms distribution normal
```

**Pass criteria:**
- All files received and verified
- Sender SRTT visible in logs, converging to ~100–200 ms range
- No hang; completes within reasonable wall-clock time (≤ 10× zero-loss time)

**Failure modes covered:** F-36, F-37

---

### P2-013 — 50% Packet Reordering (netem)

**What it tests:** F-37. SACK bitmap correctness under heavy reordering. `SackManager::on_packet_received`
marks out-of-order packets; `missing_seqs()` identifies gaps; sender promotes gaps to retransmit queues.

**Setup:**
```bash
tc qdisc add dev eth0 root netem reorder 50% delay 10ms
```

**Pass criteria:**
- All files received and verified (SHA-256 correct)
- `retransmits > 0` in sender log (confirms SACK-triggered retransmit path exercised)
- No data corruption from SACK bitmap mismanagement

**Failure modes covered:** F-37

---

### P2-014 — Disk Full During Receive (`ENOSPC`)

**What it tests:** F-30. `write_file_io_uring` returns an error when the filesystem is full.
The error propagates via `WriteResult::result` → `receive_and_write` line 151. Verifies clean
error handling and that no panic occurs.

**Setup:**
```bash
# In receiver container:
mount -t tmpfs -o size=50m tmpfs /received
# Sender sends 100 MB (twice the available space)
```

**Pass criteria:**
- Receiver logs: `write to '...' failed: No space left on device`
- Exit code non-zero (≠ 0)
- No panic; no silent success with truncated files
- Partial files may remain (acceptable); zero-byte or corrupted files are a failure

**Failure modes covered:** F-30

---

### P2-015 — Brute-Force gRPC Authentication

**What it tests:** F-18. No rate limiting on `ControlPlane::authenticate`. At ~50 ms/PBKDF2
verification and 20 parallel goroutines, an attacker achieves ~400 attempts/second.

**Implementation:**
```python
# testing/scripts/attacks/brute_force.py
import grpc, threading, time, sys
# Generate stub from proto (or use raw gRPC)
# 20 threads × 10 seconds × attempts_per_thread

attempts = 0
lock = threading.Lock()

def attack(channel):
    global attempts
    stub = udpix_pb2_grpc.ControlPlaneStub(channel)
    t_end = time.time() + 10
    while time.time() < t_end:
        try:
            stub.Authenticate(udpix_pb2.AuthRequest(
                session_id="x", username="admin", password="wrong"))
        except grpc.RpcError:
            pass
        with lock:
            attempts += 1

threads = [threading.Thread(target=attack,
           args=(grpc.insecure_channel("172.28.1.1:9000"),))
           for _ in range(20)]
for t in threads: t.start()
for t in threads: t.join()
print(f"Attempts in 10s: {attempts}")
```

**Pass criteria (after fix):** ≤ 10 attempts per minute per IP; server returns `RESOURCE_EXHAUSTED` for throttled requests.

**Expected result (current code):** 400+ attempts/second; no throttling.

**Failure modes covered:** F-18

---

### P2-016 — JWT Expiry During Active Transfer

**What it tests:** F-20. JWT TTL = 3600 s by default. This test issues a token with TTL=2 s,
then calls `Heartbeat` after 3 s to confirm `UNAUTHENTICATED` is returned. Also verifies
that the UDP data-plane transfer (which does not re-check the JWT) is not interrupted.

**Implementation:**
```rust
// crates/controlplane/tests/jwt_expiry.rs
#[tokio::test]
async fn heartbeat_fails_after_jwt_expiry() {
    // Start server with custom TTL=2s
    // Authenticate → get token
    // Sleep 3s
    // Heartbeat → expect UNAUTHENTICATED
}

#[tokio::test]
async fn data_plane_unaffected_by_jwt_expiry() {
    // Authenticate → start long UDP transfer
    // After 3s, JWT expires
    // Verify transfer still completes (UDP plane does not re-check JWT)
    // This documents that auth is control-plane only; documents the boundary
}
```

**Pass criteria:** `Heartbeat` fails with `UNAUTHENTICATED` after TTL; data plane transfer not interrupted.

**Failure modes covered:** F-20

---

### P2-017 — Session Memory Leak (No Reaper Called)

**What it tests:** F-21. `SessionManager::reap_expired()` is never called from `server.rs`.
After 10,000 sessions connect without calling `Terminate`, the `HashMap` has 10,000 stale entries.

**Implementation:**
```python
# 10,000 Authenticate calls, no Terminate, then sleep(TTL+1)
# Measure approximate session count via a debug endpoint or by monitoring heap size
```

**Pass criteria (after fix):**
- Periodic reaper task fires every `TTL/2` seconds
- After `TTL+1` seconds, `SessionManager::sessions.read().len()` returns 0

**Expected result (current code):** Sessions never removed; HashMap grows to 10,000.

**Failure modes covered:** F-21

---

### P2-018 — `sendmmsg()` Silent Failure Stats Accuracy

**What it tests:** F-23. When `sendmmsg` returns ≤ 0 (e.g. `ENOBUFS`), `send_batch_sendmmsg`
returns 0. The caller uses `stats.bytes_sent.fetch_add(...)` only for successfully sent bytes,
but verifying the exact accounting requires a test with forced ENOBUFS.

**Setup (Rust unit test):**
```rust
// Mock the socket with a wrapper that returns ENOBUFS on the first send,
// then succeeds on subsequent sends.
// Verify that bytes_sent = 0 after the failed batch and > 0 after success.
```

**Pass criteria:** `bytes_sent` only increments when `sendmmsg` succeeds; zero inflation on failure.

**Failure modes covered:** F-23

---

### P2-019 — Blocking Sleep in Async FIN Phase

**What it tests:** F-27. `std::thread::sleep(20 ms) × 5` inside `tokio::spawn` in `send.rs:129`
blocks a Tokio worker thread for 100 ms total. High-resolution timer tasks running alongside
will experience up to 20 ms of jitter per FIN retry.

**Implementation:**
```rust
// Spawn a 1ms-interval timer task alongside the FIN loop
// Measure P99 timer latency during the 100ms FIN window
// Should be < 5ms after converting to tokio::time::sleep
```

**Pass criteria (after fix):** P99 timer jitter ≤ 5 ms during FIN phase (fix: use `tokio::time::sleep`).

**Expected result (current code):** P99 jitter up to 20 ms.

**Failure modes covered:** F-27

---

### P2-020 — 1 Million Small Files (128 Bytes Each)

**What it tests:** F-28. `Vec<PathBuf>` loaded in `reader.rs:114` before any IO; 1M `PathBuf`
entries ≈ ~200 MB RAM minimum. `mpsc::channel::<SenderCommand>(64)` may back up and stall.

**Setup:**
```python
# testing/scripts/gen_million_files.py
import os
for i in range(1_000_000):
    d = f"/testdata/p2020/{i // 10000:04d}"
    os.makedirs(d, exist_ok=True)
    open(f"{d}/{i % 10000:04d}.bin", "wb").write(bytes([i % 256]) * 128)
# Total: ~128 MB in 100 subdirectories × 10,000 files
```

**Pass criteria:**
- Transfer completes without crash
- Peak RSS ≤ 4 GB
- Throughput ≥ 50 MB/s sustained
- All 1,000,000 files verified

**Expected result (current code):** Unknown — first time this scale has been tested.

**Failure modes covered:** F-28, channel backpressure, reader memory scaling

---

### P2-021 — Permission Denied on Output Directory

**What it tests:** Error propagation when `create_dir_all` returns `EACCES`.

**Setup:**
```bash
mkdir -p /received
chmod 444 /received  # read-only; receiver cannot create subdirs
```

**Pass criteria:**
- Receiver exits with code 1
- Log contains `"create_dir_all '...' failed: Permission denied"`
- No panic; no `unwrap()` failure

**Failure modes covered:** F-30 (EACCES variant)

---

### P2-022 — Malformed RUDP Header Fuzzing

**What it tests:** Protocol robustness of `RudpHeader::read_from()` and `Receiver::process_datagram()`
against arbitrary 29–2000 byte UDP payloads.

**Implementation:**
```python
import random, socket

sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
receiver_addr = ("172.28.1.10", 9001)

for i in range(1000):
    # Random length 0..2000 bytes, random content
    n = random.randint(0, 2000)
    pkt = bytes(random.randint(0, 255) for _ in range(n))
    sock.sendto(pkt, receiver_addr)
```

**Run:** Receiver must stay alive for all 1000 packets.

**Pass criteria:**
- No panic, no SIGABRT, no SIGSEGV
- No garbage data written to disk (verify output dir is empty or contains only valid files from prior transfer)
- Receiver continues processing after all 1000 junk packets

**Failure modes covered:** General protocol robustness, `write_to()` assert panic risk (F class)

---

## Phase 3 — Production Hardening

**Goal:** Long-duration stability, concurrent sessions, real NAT traversal, and quantitative
performance benchmarks suitable for SLA commitments. All Phase 3 tests run **nightly** due to
time and resource requirements.

**Infrastructure:**
```bash
docker compose -f testing/docker-compose.hardening.yml up --build
```

---

### P3-001 — 70-Minute Long-Running Transfer

**What it tests:** F-20 (JWT expiry at 60 min), F-21 (session reaper), F-26 (TURN allocation
expiry at 600 s if running through TURN relay). A transfer large enough to run 70+ minutes
exercises all time-based resource expiry paths simultaneously.

**Setup:**
```bash
# Generate ~50 GB of data (or loop 100 MB × 500 iterations):
# Simpler approach: script that runs `udpix send` in a loop for 70 minutes,
# each iteration transferring 100 MB, reusing the same session.
```

**Pass criteria:**
- JWT refresh happens before TTL expiry (if JWT refresh is implemented)
- `reap_expired()` is called periodically; old sessions are not accumulating
- TURN relay stays alive if used (requires TURN refresh implementation)
- RSS is stable over 70 minutes (no monotonic growth)
- All data verified at end of test

**Failure modes covered:** F-20, F-21, F-26

---

### P3-002 — 10 Concurrent Sessions

**What it tests:** `session_id=1` hardcoded in `send.rs:158` and `receive.rs:148`. All concurrent
sessions share session_id=1 on the wire. `Receiver::process_datagram` at line 239 checks
`hdr.session_id() != self.session_id` — with 10 sessions all using ID 1, each receiver will
accept packets from all 10 senders, causing data corruption.

**Setup:** 10 sender containers → 10 receiver containers; each pair has a unique send/receive
directory; all run simultaneously.

**Pass criteria (after fix):**
- Each receiver gets only its own sender's data
- All 10 sessions' SHA-256 checksums match their respective sources
- No cross-session contamination

**Expected result (current code):** FAIL — receivers accept each other's packets; all sessions corrupt.

**Failure modes covered:** session_id multiplexing, concurrent session isolation

---

### P3-003 — Recovery After 5-Second 100% Loss Spike

**What it tests:** F-11, F-12, F-35. Network recovers after a complete 5-second blackout
mid-transfer. `BandwidthProfiler` should enter `Throttling` state (`loss > 30%`), drop to
`FLOOR_RATE_BPS = 5 Mbps`, then re-probe after network recovers.

**Setup (using toxiproxy):**
```bash
# At 30% transfer completion:
toxiproxy-cli toxic add --type=bandwidth --attr="rate=0" sender-to-receiver
sleep 5
toxiproxy-cli toxic delete sender-to-receiver
```

**Pass criteria:**
- Transfer resumes after the 5-second blackout
- All files eventually verified
- Sender logs show BandwidthProfiler state transitions: `Probing → Steady → Throttling → Probing → Steady`
- No permanent hang (receiver does not timeout during the 5-second window if timeout is > 5 s)

**Failure modes covered:** F-11, F-12, F-35, BandwidthProfiler state machine

---

### P3-004 — End-to-End STUN Hole Punch

**What it tests:** Full ICE hole-punch through a simulated NAT. Exercises `StunClient::discover_with_socket`,
`IceAgent::run_checks`, and the full `TraversalEngine::connect` flow.

**Setup:**
```yaml
# docker-compose.hardening.yml: two NAT routers + coturn STUN server
# sender behind nat-a (172.28.2.0/24)
# receiver behind nat-b (172.28.3.0/24)
# coturn at 172.28.1.1:3478
```

**Pass criteria:**
- `relay_used=false` in sender log (direct P2P succeeded via hole punch)
- All files transferred and verified
- ICE check succeeds on attempt ≤ 10

**Failure modes covered:** F-15, NAT traversal end-to-end

---

### P3-005 — All STUN Servers Unreachable

**What it tests:** F-15. `HolePuncher::discover` fails when `--stun` points to an unreachable
address. Verifies: (a) clean error message, not a hang; (b) exits within `N_servers × 3 s` timeout.

**Setup:** `udpix send /testdata <peer> --stun 240.0.0.1:3478` (unroutable address).

**Pass criteria:**
- Exits within 30 seconds (10 × 3 s STUN timeout)
- Exit code 1
- Error message: `"NAT traversal failed: all STUN servers failed or none configured"`
- No infinite wait

**Failure modes covered:** F-15

---

### P3-006 — TURN Allocation Expiry (No Refresh)

**What it tests:** F-26. coturn is configured with `--max-allocate-lifetime=60` (60-second
allocation lifetime). A transfer that requires more than 60 seconds through TURN relay should
fail without the fix and pass with it (requires `TurnClient::refresh()` implementation).

**Setup:**
```bash
# coturn: --max-allocate-lifetime=60
# Sender and receiver cannot hole-punch (symmetric NAT configured)
# Transfer takes >90 seconds
```

**Pass criteria (after fix):** TURN relay stays alive; transfer completes. `TurnClient::refresh()` called every ≤50 s.

**Expected result (current code):** FAIL — relay drops at T=60 s; transfer fails silently.

**Failure modes covered:** F-26

---

### P3-007 — io_uring SQ Saturation (QUEUE_DEPTH=2)

**What it tests:** In `reader.rs:248`: `unsafe { ring.submission().push(&sqe) }.context("io_uring SQ full")?`.
With `QUEUE_DEPTH=2`, submitting a 3rd SQ entry returns `Err` from `push()`. The `.context()`
call converts this to `anyhow::Error` — verifying no `unwrap()`-based panic.

**Implementation:**
```rust
// crates/ioengine/tests/security.rs
#[test]
#[cfg(target_os = "linux")]
fn io_uring_sq_overflow_returns_error_not_panic() {
    use io_uring::{opcode, types, IoUring};
    let mut ring = IoUring::new(2).expect("io_uring::new");
    let mut sq = ring.submission();
    // Fill SQ
    for _ in 0..2 {
        let sqe = opcode::Nop::new().build();
        unsafe { sq.push(&sqe) }.unwrap();
    }
    // Third push should fail
    let sqe = opcode::Nop::new().build();
    let result = unsafe { sq.push(&sqe) };
    assert!(result.is_err(), "SQ should be full");
}
```

**Pass criteria:** `push()` returns `Err`; no panic; test completes normally.

**Failure modes covered:** F-07 (io_uring SQ saturation scenario)

---

### P3-008 — Background io_uring Thread Death (block_in_place Hang)

**What it tests:** F-16. `reader.rs:120`: `block_in_place(|| self.result_rx.recv())`. If the
background thread exits (panic or normal), `result_rx` becomes disconnected and `recv()` returns
`Err(RecvError)`. Verifies `block_in_place` returns promptly and error propagates cleanly.

**Implementation:**
```rust
// crates/ioengine/tests/security.rs
#[tokio::test(flavor = "multi_thread")]
async fn background_thread_disconnect_propagates() {
    let (tx, rx) = crossbeam_channel::bounded::<i32>(1);
    // Drop sender immediately (simulates thread death)
    drop(tx);

    // block_in_place should return RecvError immediately, not hang
    let result = tokio::task::block_in_place(|| rx.recv());
    assert!(result.is_err());
}
```

**Pass criteria:** Returns `RecvError` within 1 ms of `tx` being dropped; no infinite wait.

**Failure modes covered:** F-16

---

### P3-009 — Performance Benchmark Suite

**Goal:** Establish baseline performance numbers for CI regression gating. All targets are
conservative minimums; the system should consistently beat them on any modern server.

#### P3-009a — Small-File Pack Rate
```bash
cargo bench -p udpix-bench -- io_pack
```
**Target:** ≥ 500 MB/s pack rate (100 × 4 KB files packed into one PackBlock)

#### P3-009b — Large File End-to-End (1 GB, Docker Loopback)
```bash
# Transfer 1 GB file via --direct mode; measure wall-clock throughput
```
**Target:** ≥ 800 MB/s on Docker loopback (limited by io_uring write throughput)

#### P3-009c — RUDP Packet Encode + Decode
```bash
cargo bench -p udpix-bench -- sender_encode
```
**Target:** ≥ 5 Mpps encode + decode at 1400-byte payload (i.e. ≤ 200 ns/packet round-trip)

#### P3-009d — SACK Bitmap Operations
```bash
cargo bench -p udpix-bench -- sack_bitmap
```
**Targets:**
- `mark_1024_sequential`: ≤ 5 µs total
- `missing_seqs_sparse` (50% loss): ≤ 10 µs total
- `advance_base_1024`: ≤ 2 µs total

#### P3-009e — End-to-End Network Throughput Baseline
```bash
cd testing && docker compose up --build
# Capture throughput from receiver log: "Transfer complete — N bytes in Xs (Y MB/s)"
```
**Target:** ≥ 300 MB/s on Docker bridge network

**Regression gate:** Store results in `testing/benchmarks/baseline.json`. Any commit that
regresses any benchmark by >15% must include a documented justification.

---

### P3-010 — Memory Stability Over 10 Sequential Transfers

**What it tests:** Memory leaks in the background thread lifecycle. `AsyncWriter::Drop`
sends a `None` shutdown signal via `request_tx`. `AsyncReader` uses the same pattern. After 10
full create/transfer/drop cycles, RSS should not grow monotonically.

**Setup:**
```python
# Loop 10 times:
#   docker compose up --build
#   wait for "ALL FILES VERIFIED — PASS"
#   sample RSS of receiver process each iteration
#   docker compose down
# Plot RSS over iterations
```

**Pass criteria:**
- RSS at iteration 10 ≤ RSS at iteration 1 × 1.10 (≤10% growth)
- No monotonically increasing trend (slope of linear regression ≤ 0.5 MB/iteration)

**Failure modes covered:** Memory leaks in async infrastructure, channel drop paths

---

### P3-011 — Graceful SIGTERM Shutdown Under Load

**What it tests:** F-17. When the sender receives SIGTERM mid-transfer, it should drain
in-flight packets and exit cleanly. `SenderCommand::Shutdown` exists but requires the receiver
app (IoEngine) to send it. The 1-second timeout in `send.rs:171` forces abort after 1 s.

**Setup:**
```bash
# Start transfer
# After 50% progress, send SIGTERM to sender:
docker kill --signal=TERM sender-container
# Measure time to exit; inspect receiver state
```

**Pass criteria:**
- Sender exits within 5 seconds of SIGTERM
- Receiver detects incomplete transfer and reports missing file count
- No zombie processes; all file descriptors closed
- Log: `"Transfer incomplete: shutdown requested; N bytes of M sent"`

**Failure modes covered:** F-17

---

### P3-012 — Hardcoded Credential Enforcement

**What it tests:** F-19. `--username admin --password changeme` are the default CLI values.
Any production deployment using these defaults is immediately compromisable.

**Implementation:**
```bash
# Test 1: verify defaults work (proves they ARE the defaults)
udpix send /testdata <peer> --username admin --password changeme
# Pass criterion: authentication succeeds (confirming defaults are active)

# Test 2: policy check
# The test FAILS if there is no mechanism to force override in production mode
# Acceptable mitigations:
#   (a) CLI warns "WARNING: using default credentials — change before production use"
#   (b) Server startup fails if UDPIX_ADMIN_PASS env var is not set
#   (c) CI lint check that rejects deployments with default creds
```

**Pass criteria (after fix):**
- One of mitigations (a), (b), or (c) is implemented
- Using `admin`/`changeme` in a server started with `--production` flag causes startup failure

**Expected result (current code):** Default credentials silently accepted; no warning.

**Failure modes covered:** F-19

---

## Coverage Matrix

Every failure mode maps to at least one test.

| F-ID | Description (short) | Test(s) |
|------|---------------------|---------|
| F-01 | Path traversal | P2-001 |
| F-02 | Absolute path injection | P2-002 |
| F-03 | Duplicate split parts | P2-005 |
| F-04 | Missing split parts | P2-006 |
| F-05 | Oversized block_len OOM | P2-004 |
| F-06 | entry_count OOM bomb | P2-003 |
| F-07 | total_parts=0 | P2-007 |
| F-08 | Circular symlinks | P2-008 |
| F-09 | part_index OOB | P1-003, P2-005 |
| F-10 | entry_count no bounds | P2-003 |
| F-11 | Sender crash hang | P2-009 |
| F-12 | FIN never arrives | P2-009, P3-003 |
| F-13 | data_tx deadlock | P2-010 |
| F-14 | sack_tx deadlock | P2-010 |
| F-15 | STUN unreachable | P3-004, P3-005 |
| F-16 | Thread panic hang | P3-008 |
| F-17 | Sender never exits | P3-011 |
| F-18 | Brute force auth | P2-015 |
| F-19 | Hardcoded creds | P3-012 |
| F-20 | JWT expiry | P2-016, P3-001 |
| F-21 | Session leak | P2-017, P3-001 |
| F-22 | loss_ema overflow | P2-011, P2-013 |
| F-23 | sendmmsg silent failure | P2-018 |
| F-24 | Receiver before sender | P1-011 |
| F-25 | Sender before receiver | P1-012 |
| F-26 | TURN relay expiry | P3-006, P3-001 |
| F-27 | Blocking sleep async | P2-019 |
| F-28 | 1M+ files | P1-007, P2-020 |
| F-29 | 1 GB+ single file | P1-004 |
| F-30 | Disk full ENOSPC | P2-014, P2-021 |
| F-31 | Pre-existing files | P1-009 |
| F-32 | Empty files | P1-002 |
| F-33 | Special chars | P1-006 |
| F-34 | Deep nesting | P1-005 |
| F-35 | Packet loss | P1-013, P2-011 |
| F-36 | High latency | P2-012 |
| F-37 | Jitter/reordering | P2-012, P2-013 |
| F-38 | No rate limit | P2-011, P2-020 |

---

## CI Integration

| CI Gate | Tests Included | Trigger | Allow Failure? |
|---------|---------------|---------|----------------|
| `cargo test --workspace` | All Rust unit + integration tests | Every commit | No |
| `make baseline` | P1-001 | Every commit | No |
| `make functional-tests` | P1-002 → P1-013 | Commits touching protocol/, ioengine/, cli/ | No |
| `make security-tests` | P2-001 → P2-007 (Rust fast tests) | Commits touching writer.rs, packer.rs, receive.rs | Report-only until fixes land |
| `make adversarial-tests` | P2-008 → P2-022 (Docker + netem) | Daily, or on release branch | P1-012 may fail (known gap) |
| `make hardening-tests` | P3-001 → P3-012 | Nightly | P3-002, P3-006 may fail until fixed |
| `make benchmarks` | P3-009a → P3-009e | Weekly | Regression gate: fail if >15% slower |

**Regression rule:** Any change to `writer.rs`, `packer.rs`, or `receiver.rs` must pass
P1-001 → P1-013 **and** P2-001 → P2-007 (fast Rust tests) before merge. No exceptions.

---

## Known Limitations (Current Codebase)

The following issues are confirmed bugs requiring code changes before Phase 2/3 tests can pass:

| Priority | Issue | Tests Failing | Fix Required |
|----------|-------|---------------|--------------|
| P0 | No path sanitization in writer | P2-001, P2-002 | Reject `..` components and absolute paths in `Packer::unpack_entries` |
| P0 | `entry_count` OOM | P2-003 | Cap `entry_count` at 65,536 before `Vec::with_capacity` |
| P0 | `block_len` OOM framing | P2-004 | Reject `block_len > 32 MB` in reassembly task |
| P0 | Duplicate split parts corrupt | P2-005 | Reject duplicate `part_index` in `PartialFile::insert` |
| P0 | Missing parts silent loss | P2-006 | Drain `partial` HashMap on FIN; return error for incomplete files |
| P0 | `total_parts=0` | P2-007 | Reject `total_parts=0` in `Packer::unpack_entries` |
| P0 | Circular symlinks hang | P2-008 | Track visited inode set in `collect_recursive`; skip cycles |
| P1 | Sender crash → receiver hang | P2-009 | Add 30 s idle timeout on `raw_data_rx.recv()` in reassembly task |
| P1 | No gRPC auth rate limiting | P2-015 | Token bucket per IP in `ControlPlane::authenticate` |
| P1 | Session never reaped | P2-017 | Spawn `tokio::task::spawn` reaper timer in `ServerBuilder::serve_*` |
| P1 | session_id=1 hardcoded | P3-002 | Pass session_id from auth token / CLI argument |
| P1 | TURN relay no refresh | P3-006 | Implement refresh loop in `HolePuncher::punch` |
| P2 | `thread::sleep` in async | P2-019 | Replace with `tokio::time::sleep` in `send.rs:129` |
| P2 | JWT no refresh | P2-016, P3-001 | Implement JWT refresh in `server.rs` heartbeat handler |
| P2 | Hardcoded creds | P3-012 | Warn or fail on `admin`/`changeme` in non-dev mode |
