/// Async sender task: token-bucket pacing, lock-free retransmit queue, sendmmsg batching.
///
/// # Architecture
///
/// ```text
///  Application
///      │  SenderCommand::{SendChunk, Shutdown}
///      ▼
///  ┌─────────────────────────────────────────────────┐
///  │  Sender::run() — async Tokio task               │
///  │                                                 │
///  │  ① Pop urgent retransmits (lock-free SegQueue)  │
///  │  ② Pop new data from the application queue      │
///  │  ③ Gate both through TokenBucket pacer          │
///  │  ④ Batch into iovec array (up to 64 packets)    │
///  │  ⑤ Call sendmmsg — one syscall, many datagrams  │
///  │  ⑥ Feed bytes_sent to BandwidthProfiler         │
///  │  ⑦ On timer: evaluate probe, update rate        │
///  └─────────────────────────────────────────────────┘
/// ```
///
/// # Retransmit priority
///
/// When the receiver's SACK reveals gaps the `Sender` is given a list of
/// missing sequence numbers.  Sequences that have appeared in **≥ 2 consecutive
/// SACK messages** are promoted to `urgent_retransmits` (fast retransmit, like
/// TCP's three-dup-ACK rule).  All other gaps are queued in `normal_retransmits`
/// and sent only after urgent work is exhausted.

use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_queue::SegQueue;
use tokio::sync::mpsc;

use crate::congestion::{BandwidthProfiler, TokenBucket};
use crate::packet::{now_us, RudpHeader};
use crate::sack::SackPayload;
use crate::session::SessionStats;

// ── Public API types ──────────────────────────────────────────────────────────

/// Commands the application sends to the Sender task.
pub enum SenderCommand {
    /// Transmit a chunk of encrypted payload bytes.
    SendChunk(Vec<u8>),
    /// Shut down cleanly: drain in-flight data then stop.
    Shutdown,
}

/// Metadata stored for every in-flight packet (needed for retransmission).
struct InFlightEntry {
    /// Raw datagram bytes (header + encrypted payload) ready to re-send.
    datagram: Vec<u8>,
    /// Wall time this packet was *last* transmitted.
    last_sent: Instant,
    /// How many times this packet has appeared in consecutive SACK gaps.
    nak_count: u8,
}

// ── Maximum datagrams per sendmmsg call ──────────────────────────────────────

const BATCH_SIZE: usize = 64;

/// Maximum payload bytes per DATA packet (leaves room for the 29-byte header
/// inside a standard 1 500-byte Ethernet MTU with 20+8-byte IP/UDP headers).
pub const MAX_PAYLOAD: usize = 1_443;

// ── Sender ────────────────────────────────────────────────────────────────────

/// The async send-side task for one RUDP session.
///
/// Constructed by the control plane and driven by `run()`.
pub struct Sender {
    /// Session identifier (multiplexes concurrent transfers on one port).
    session_id: u32,

    /// Underlying UDP socket (non-blocking; wraps the OS fd).
    socket: Arc<UdpSocket>,

    /// Sequence numbers that need urgent retransmission (appeared in ≥ 2 SAKs).
    urgent_retransmits: Arc<SegQueue<u64>>,

    /// Sequence numbers needing ordinary retransmission (appeared in 1 SACK).
    normal_retransmits: Arc<SegQueue<u64>>,

    /// In-flight packet store: seq → datagram bytes + metadata.
    in_flight: HashMap<u64, InFlightEntry>,

    /// Application-to-sender command channel.
    cmd_rx: mpsc::Receiver<SenderCommand>,

    /// Channel through which the receiver delivers parsed SACK payloads.
    sack_rx: mpsc::Receiver<SackPayload>,

    /// Monotonically increasing sequence number for outgoing DATA packets.
    next_seq: u64,

    /// Token bucket pacer — gates all outgoing bytes.
    pacer: TokenBucket,

    /// Bandwidth prober — finds the sweet-spot send rate.
    profiler: BandwidthProfiler,

    /// Shared RTT / loss statistics (also read by the receiver task).
    stats: Arc<SessionStats>,
}

impl Sender {
    /// Construct a new Sender.  All channels are created externally so the
    /// control plane can wire Sender ↔ Receiver before either task starts.
    pub fn new(
        session_id: u32,
        socket: Arc<UdpSocket>,
        cmd_rx: mpsc::Receiver<SenderCommand>,
        sack_rx: mpsc::Receiver<SackPayload>,
        stats: Arc<SessionStats>,
    ) -> Self {
        let initial_rate = 10_000_000u64; // 10 Mbps — start of the probe ladder
        Self {
            session_id,
            socket,
            urgent_retransmits: Arc::new(SegQueue::new()),
            normal_retransmits: Arc::new(SegQueue::new()),
            in_flight: HashMap::new(),
            cmd_rx,
            sack_rx,
            next_seq: 0,
            pacer: TokenBucket::new(initial_rate),
            profiler: BandwidthProfiler::new(),
            stats,
        }
    }

    /// Return a handle to the urgent-retransmit queue so the receiver task can
    /// push fast-retransmit requests without going through the command channel.
    pub fn urgent_queue(&self) -> Arc<SegQueue<u64>> {
        Arc::clone(&self.urgent_retransmits)
    }

    // ── Main async run loop ───────────────────────────────────────────────────

    /// Drive the sender until a `Shutdown` command is received and the
    /// in-flight window drains to zero.
    ///
    /// This is a Tokio task; spawn it with `tokio::spawn(sender.run())`.
    pub async fn run(mut self) {
        let mut probe_tick = tokio::time::interval(Duration::from_millis(500));
        let mut shutdown_requested = false;

        loop {
            tokio::select! {
                biased; // poll in declaration order for predictable priority

                // ── Probe timer ───────────────────────────────────────────────
                _ = probe_tick.tick() => {
                    if let Some(new_rate) = self.profiler.evaluate_probe() {
                        self.pacer.set_rate(new_rate);
                    }
                }

                // ── Incoming SACK from receiver ───────────────────────────────
                Some(sack) = self.sack_rx.recv() => {
                    self.process_sack(&sack);
                }

                // ── Application commands ──────────────────────────────────────
                Some(cmd) = self.cmd_rx.recv() => {
                    match cmd {
                        SenderCommand::SendChunk(payload) => {
                            self.enqueue_data(payload);
                        }
                        SenderCommand::Shutdown => {
                            shutdown_requested = true;
                        }
                    }
                }

                else => break,
            }

            // Refill the token bucket from elapsed time.
            self.pacer.refill();

            // Send as many packets as the pacer allows this iteration.
            self.send_batch();

            // If we were asked to shut down and the window is empty, we're done.
            if shutdown_requested && self.in_flight.is_empty() {
                break;
            }
        }
    }

    // ── SACK processing ───────────────────────────────────────────────────────

    /// Parse the receiver's SACK, remove acked packets from `in_flight`, and
    /// promote persistent gaps to the urgent-retransmit queue.
    fn process_sack(&mut self, sack: &SackPayload) {
        // Remove every in-flight packet whose sequence is below the SACK base
        // (they are cumulatively acked) or explicitly marked received.
        let acked_seqs: Vec<u64> = self
            .in_flight
            .keys()
            .copied()
            .filter(|&seq| seq < sack.base_seq || sack.is_received(seq))
            .collect();

        let mut total_acked_bytes = 0u64;
        for seq in acked_seqs {
            if let Some(entry) = self.in_flight.remove(&seq) {
                total_acked_bytes += entry.datagram.len() as u64;
                self.stats.packets_acked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        self.stats
            .bytes_acked
            .fetch_add(total_acked_bytes, std::sync::atomic::Ordering::Relaxed);
        self.profiler.on_bytes_acked(total_acked_bytes);

        // For gaps still in flight, increment their NAK counter.
        // Sequences with nak_count ≥ 2 go to the urgent queue (fast retransmit).
        for (seq, entry) in self.in_flight.iter_mut() {
            let seq = *seq;
            if seq >= sack.base_seq && !sack.is_received(seq) {
                entry.nak_count += 1;
                if entry.nak_count >= 2 {
                    self.urgent_retransmits.push(seq);
                    entry.nak_count = 0; // reset so we don't re-push every SACK
                } else {
                    self.normal_retransmits.push(seq);
                }
            }
        }
    }

    // ── Data enqueuing ────────────────────────────────────────────────────────

    /// Slice `payload` into MTU-sized chunks and build DATA packets for each,
    /// storing them in `in_flight` so they can be retransmitted if needed.
    fn enqueue_data(&mut self, payload: Vec<u8>) {
        for chunk in payload.chunks(MAX_PAYLOAD) {
            let seq    = self.next_seq;
            let ts_us  = now_us();
            let header = RudpHeader::new_data(
                self.session_id,
                seq,
                ts_us,
                chunk.len() as u16,
            );
            let mut datagram = vec![0u8; RudpHeader::SIZE + chunk.len()];
            header.write_to(&mut datagram);
            datagram[RudpHeader::SIZE..].copy_from_slice(chunk);

            self.in_flight.insert(seq, InFlightEntry {
                datagram,
                last_sent: Instant::now() - Duration::from_secs(1), // mark as unsent
                nak_count: 0,
            });
            self.next_seq += 1;
        }
    }

    // ── Batch send ────────────────────────────────────────────────────────────

    /// Collect up to `BATCH_SIZE` packets that are ready to send and dispatch
    /// them in a single `sendmmsg` syscall.
    ///
    /// Priority: urgent retransmits > normal retransmits > new (unsent) data.
    fn send_batch(&mut self) {
        let mut batch: Vec<Vec<u8>> = Vec::with_capacity(BATCH_SIZE);

        // ① Urgent retransmits first.
        while batch.len() < BATCH_SIZE {
            match self.urgent_retransmits.pop() {
                Some(seq) => {
                    if let Some(entry) = self.in_flight.get_mut(&seq) {
                        let bytes = entry.datagram.len();
                        if self.pacer.try_consume(bytes) {
                            batch.push(entry.datagram.clone());
                            entry.last_sent = Instant::now();
                            self.stats.retransmits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        } else {
                            // Put it back — we ran out of tokens.
                            self.urgent_retransmits.push(seq);
                            break;
                        }
                    }
                }
                None => break,
            }
        }

        // ② Normal retransmits next.
        while batch.len() < BATCH_SIZE {
            match self.normal_retransmits.pop() {
                Some(seq) => {
                    if let Some(entry) = self.in_flight.get_mut(&seq) {
                        let bytes = entry.datagram.len();
                        if self.pacer.try_consume(bytes) {
                            batch.push(entry.datagram.clone());
                            entry.last_sent = Instant::now();
                            self.stats.retransmits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        } else {
                            self.normal_retransmits.push(seq);
                            break;
                        }
                    }
                }
                None => break,
            }
        }

        // ③ New (never-sent) in-flight entries — those with last_sent far in the past.
        let rto = self.stats.rto();
        let unsent: Vec<u64> = self
            .in_flight
            .iter()
            .filter(|(_, e)| e.last_sent.elapsed() > rto && e.nak_count == 0)
            .map(|(seq, _)| *seq)
            .take(BATCH_SIZE - batch.len())
            .collect();

        for seq in unsent {
            if let Some(entry) = self.in_flight.get_mut(&seq) {
                let bytes = entry.datagram.len();
                if self.pacer.try_consume(bytes) {
                    batch.push(entry.datagram.clone());
                    entry.last_sent = Instant::now();
                } else {
                    break;
                }
            }
        }

        if batch.is_empty() {
            return;
        }

        // Dispatch the batch via sendmmsg (one syscall).
        let total_bytes = send_batch_sendmmsg(&self.socket, &batch);
        self.stats
            .bytes_sent
            .fetch_add(total_bytes, std::sync::atomic::Ordering::Relaxed);
        self.stats
            .packets_sent
            .fetch_add(batch.len() as u64, std::sync::atomic::Ordering::Relaxed);
        self.profiler.on_bytes_sent(total_bytes);
    }
}

// ── sendmmsg wrapper ──────────────────────────────────────────────────────────

/// Send all datagrams in `batch` via a single `sendmmsg(2)` syscall.
///
/// Falls back to sequential `send()` calls if `sendmmsg` is not available
/// (non-Linux build).  Returns the total bytes actually sent.
///
/// # Safety
///
/// We build the `mmsghdr` array on the stack.  All pointers are valid for the
/// duration of the syscall because `batch` is borrowed immutably throughout.
fn send_batch_sendmmsg(socket: &UdpSocket, batch: &[Vec<u8>]) -> u64 {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;

        let fd = socket.as_raw_fd();
        let n  = batch.len();

        // Build one iovec and one mmsghdr per datagram.
        let mut iovecs: Vec<libc::iovec>   = Vec::with_capacity(n);
        let mut hdrs:   Vec<libc::mmsghdr> = Vec::with_capacity(n);

        for dg in batch {
            iovecs.push(libc::iovec {
                iov_base: dg.as_ptr() as *mut libc::c_void,
                iov_len:  dg.len(),
            });
        }
        for i in 0..n {
            hdrs.push(libc::mmsghdr {
                msg_hdr: libc::msghdr {
                    msg_name:       std::ptr::null_mut(),
                    msg_namelen:    0,
                    msg_iov:        &mut iovecs[i] as *mut libc::iovec,
                    msg_iovlen:     1,
                    msg_control:    std::ptr::null_mut(),
                    msg_controllen: 0,
                    msg_flags:      0,
                },
                msg_len: 0,
            });
        }

        // SAFETY: fd is valid, all iov pointers live for the duration of the call.
        let sent = unsafe {
            libc::sendmmsg(fd, hdrs.as_mut_ptr(), n as libc::c_uint, 0)
        };

        if sent <= 0 {
            return 0;
        }
        hdrs[..sent as usize]
            .iter()
            .map(|h| h.msg_len as u64)
            .sum()
    }

    #[cfg(not(target_os = "linux"))]
    {
        // Portable fallback: one syscall per datagram.
        let mut total = 0u64;
        for dg in batch {
            if let Ok(n) = socket.send(dg) {
                total += n as u64;
            }
        }
        total
    }
}

// ── RTT probe helpers ─────────────────────────────────────────────────────────

/// Build and immediately send a PING probe packet.
///
/// The sender calls this periodically; the receiver echoes the timestamp in a
/// PONG so we can compute round-trip time.
pub fn send_ping(socket: &UdpSocket, session_id: u32, seq: u64) {
    let ts = now_us();
    let hdr = RudpHeader::new_ping(session_id, seq, ts);
    let mut buf = [0u8; RudpHeader::SIZE];
    hdr.write_to(&mut buf);
    let _ = socket.send(&buf);
}

/// Process a received PONG packet: compute RTT and feed it into `SessionStats`.
pub fn handle_pong(stats: &SessionStats, pong_ts_us: u64) {
    let now = now_us();
    if now >= pong_ts_us {
        stats.update_rtt(now - pong_ts_us);
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionStats;

    #[test]
    fn handle_pong_updates_stats() {
        let stats = SessionStats::new();
        let sent_ts = now_us().saturating_sub(10_000); // pretend 10 ms ago
        handle_pong(&stats, sent_ts);
        let srtt = stats.srtt_us.load(std::sync::atomic::Ordering::Relaxed);
        // RTT should be in the ballpark of 10 000 µs.
        assert!(srtt >= 9_000 && srtt <= 50_000, "srtt={srtt}");
    }

    #[test]
    fn enqueue_splits_large_payload() {
        // We cannot run the full async loop in a unit test, but we can verify
        // that enqueue_data slices correctly by counting in-flight entries.
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let (sack_tx, sack_rx) = tokio::sync::mpsc::channel(16);
        drop(cmd_tx);
        drop(sack_tx);

        // Build a dummy non-blocking UDP socket so we can construct Sender.
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        socket.set_nonblocking(true).unwrap();

        let mut sender = Sender::new(
            1,
            socket,
            cmd_rx,
            sack_rx,
            SessionStats::new(),
        );

        // 3 KB payload should split into 3 DATA packets (each ≤ 1 443 bytes).
        sender.enqueue_data(vec![0xABu8; 3 * MAX_PAYLOAD]);
        assert_eq!(sender.in_flight.len(), 3);
    }
}
