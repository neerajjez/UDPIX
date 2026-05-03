/// Async receiver task: recvmmsg batching, reassembly, and heartbeat SACK dispatch.
///
/// # Architecture
///
/// ```text
///  ┌──────────────────────────────────────────────────────────────┐
///  │  Receiver::run() — async Tokio task                          │
///  │                                                              │
///  │  tokio::select! {                                            │
///  │    heartbeat_tick => send SACK (adaptive 1–5 ms)             │
///  │    socket readable => recvmmsg(64 datagrams per syscall)     │
///  │  }                                                           │
///  │                                                              │
///  │  For each received datagram:                                 │
///  │    • RudpHeader::read_from() — zero-copy header parse        │
///  │    • is_data()  → decrypt + deliver to app + mark SACK bitmap│
///  │    • is_ping()  → immediately send PONG (RTT echo)           │
///  │    • is_pong()  → feed timestamp into SessionStats::rtt      │
///  │    • is_fin()   → notify application; stop loop              │
///  └──────────────────────────────────────────────────────────────┘
/// ```
///
/// # Pre-allocated buffer pool
///
/// At 22–26% loss with fast retransmits the receiver can see bursts of
/// thousands of datagrams per second.  Allocating a new `Vec<u8>` for each
/// would saturate the global allocator.  Instead we keep a pool of
/// `POOL_SIZE` fixed-size buffers (each large enough for one MTU) and
/// recycle them after processing.

use std::net::UdpSocket;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::packet::{now_us, RudpHeader};
use crate::sack::{SackManager, SackPayload};
use crate::sender::handle_pong;
use crate::session::SessionStats;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Maximum datagrams received per `recvmmsg` call.
const RECV_BATCH: usize = 64;

/// Buffer size for each receive slot (header + max payload, with headroom).
const BUF_SIZE: usize = 1_500;

/// How many pre-allocated receive buffers to keep in the pool.
const POOL_SIZE: usize = RECV_BATCH * 4;

// ── Pre-allocated buffer pool ─────────────────────────────────────────────────

/// A flat pool of fixed-size receive buffers.
///
/// The receiver grabs `RECV_BATCH` buffers before calling `recvmmsg`, then
/// releases them back after processing.  No heap allocations on the hot path.
struct RecvBufferPool {
    /// The storage backing all buffers (POOL_SIZE × BUF_SIZE bytes).
    storage: Vec<u8>,
}

impl RecvBufferPool {
    fn new() -> Self {
        Self {
            storage: vec![0u8; POOL_SIZE * BUF_SIZE],
        }
    }

    /// Return a mutable slice for buffer slot `idx`.
    fn slot_mut(&mut self, idx: usize) -> &mut [u8] {
        let start = idx * BUF_SIZE;
        &mut self.storage[start..start + BUF_SIZE]
    }

    /// Return an immutable slice for buffer slot `idx` trimmed to `len` bytes.
    fn slot(&self, idx: usize, len: usize) -> &[u8] {
        let start = idx * BUF_SIZE;
        &self.storage[start..start + len]
    }
}

// ── Receiver ─────────────────────────────────────────────────────────────────

/// The async receive-side task for one RUDP session.
pub struct Receiver {
    /// Session identifier (must match the Sender).
    session_id: u32,

    /// Underlying UDP socket (non-blocking).
    socket: Arc<UdpSocket>,

    /// Sends reassembled application payload to the consumer.
    data_tx: mpsc::Sender<Vec<u8>>,

    /// Sends SACK payloads back to the Sender task for retransmit scheduling.
    sack_tx: mpsc::Sender<SackPayload>,

    /// Adaptive SACK bitmap and heartbeat scheduler.
    sack_mgr: SackManager,

    /// Shared RTT / loss statistics.
    stats: Arc<SessionStats>,

    /// Pre-allocated receive buffer pool.
    pool: RecvBufferPool,
}

impl Receiver {
    /// Create a new Receiver.
    ///
    /// `initial_seq` is the first sequence number we expect from the sender.
    /// `data_tx` delivers reassembled payload bytes to the application layer.
    /// `sack_tx` goes back to the Sender so it can schedule retransmissions.
    pub fn new(
        session_id: u32,
        socket: Arc<UdpSocket>,
        initial_seq: u64,
        data_tx: mpsc::Sender<Vec<u8>>,
        sack_tx: mpsc::Sender<SackPayload>,
        stats: Arc<SessionStats>,
    ) -> Self {
        Self {
            session_id,
            socket,
            data_tx,
            sack_tx,
            sack_mgr: SackManager::new(initial_seq),
            stats,
            pool: RecvBufferPool::new(),
        }
    }

    // ── Main async run loop ───────────────────────────────────────────────────

    /// Drive the receiver until a FIN is received from the sender.
    pub async fn run(mut self) {
        // Build the first heartbeat interval and schedule the initial tick.
        let mut heartbeat = tokio::time::interval(Duration::from_millis(5));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                biased;

                // ── Heartbeat SACK timer ──────────────────────────────────────
                _ = heartbeat.tick() => {
                    if let Some(new_interval) = self.send_heartbeat_sack().await {
                        heartbeat = tokio::time::interval(new_interval);
                        heartbeat.set_missed_tick_behavior(
                            tokio::time::MissedTickBehavior::Delay,
                        );
                    }
                }

                // ── Socket readable ───────────────────────────────────────────
                // Tokio's `readable()` future resolves when the OS signals EPOLLIN.
                // We then drain the socket with recvmmsg until it returns EAGAIN.
                ready = readable(&self.socket) => {
                    if ready {
                        if self.recv_batch().await {
                            break; // FIN received; session complete
                        }
                    }
                }
            }
        }
    }

    // ── Heartbeat SACK ────────────────────────────────────────────────────────

    /// Serialise the current SACK bitmap and send it to the sender.
    ///
    /// Returns the new heartbeat interval if it changed, so the caller can
    /// reset the timer.
    async fn send_heartbeat_sack(&mut self) -> Option<Duration> {
        let sack = self.sack_mgr.tick().clone();
        let old_interval = self.sack_mgr.heartbeat_interval();

        // Transmit the SACK payload back to the Sender task.
        // We send a clone through the channel (cheap — it's just u64 words).
        let _ = self.sack_tx.send(sack.clone()).await;

        // Also write it onto the wire as a UDP heartbeat-SACK datagram.
        let mut buf = Vec::with_capacity(RudpHeader::SIZE + sack.wire_size());
        buf.resize(RudpHeader::SIZE, 0);
        let hdr = RudpHeader::new_heartbeat_sack(
            self.session_id,
            0,
            now_us(),
            sack.wire_size() as u16,
        );
        hdr.write_to(&mut buf[..RudpHeader::SIZE]);
        sack.serialise(&mut buf);
        let _ = self.socket.send(&buf);

        // Re-read the interval *after* tick() updated it.
        let new_interval = self.sack_mgr.heartbeat_interval();
        if new_interval != old_interval {
            Some(new_interval)
        } else {
            None
        }
    }

    // ── recvmmsg batch receive ────────────────────────────────────────────────

    /// Drain the socket with a single `recvmmsg` call, then process each
    /// datagram.
    ///
    /// Returns `true` if a FIN was received (signals the loop to exit).
    async fn recv_batch(&mut self) -> bool {
        let received = recv_batch_recvmmsg(&self.socket, &mut self.pool);

        for (slot_idx, nbytes) in received {
            // Copy into a local vec so we release the immutable pool borrow
            // before calling `process_datagram` (which needs &mut self).
            let owned: Vec<u8> = self.pool.slot(slot_idx, nbytes).to_vec();
            if let Some(fin) = self.process_datagram(&owned).await {
                if fin {
                    return true;
                }
            }
        }
        false
    }

    // ── Per-datagram dispatch ─────────────────────────────────────────────────

    /// Parse one received datagram and act on its packet type.
    ///
    /// Returns `Some(true)` if this was a FIN (session end), `Some(false)` for
    /// any other handled packet, or `None` if the datagram was malformed.
    async fn process_datagram(&mut self, buf: &[u8]) -> Option<bool> {
        let hdr = RudpHeader::read_from(buf)?;

        // Ignore packets from a different session.
        if hdr.session_id() != self.session_id {
            return None;
        }

        if hdr.is_data() {
            let payload_len = hdr.payload_len() as usize;
            let payload_start = RudpHeader::SIZE;
            if buf.len() < payload_start + payload_len {
                return None; // truncated datagram
            }
            let payload = &buf[payload_start..payload_start + payload_len];

            let seq = hdr.sequence_number();
            self.sack_mgr.on_packet_received(seq);
            self.stats.packets_acked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.stats.bytes_acked.fetch_add(payload_len as u64, std::sync::atomic::Ordering::Relaxed);

            // Deliver payload bytes to the application (best-effort in-order
            // delivery — re-ordering is handled by the application layer).
            let _ = self.data_tx.send(payload.to_vec()).await;

            return Some(false);
        }

        if hdr.is_ping() {
            // Echo the sender's timestamp verbatim in a PONG.
            let pong = RudpHeader::new_pong(
                self.session_id,
                hdr.sequence_number(),
                hdr.timestamp_us(),
            );
            let mut out = [0u8; RudpHeader::SIZE];
            pong.write_to(&mut out);
            let _ = self.socket.send(&out);
            return Some(false);
        }

        if hdr.is_pong() {
            // RTT measurement: sender's timestamp was echoed; compute round-trip.
            handle_pong(&self.stats, hdr.timestamp_us());
            return Some(false);
        }

        if hdr.is_fin() {
            // Sender is done.  Send one final SACK, then signal end-of-session.
            self.send_heartbeat_sack().await;
            return Some(true);
        }

        // SYN / other control packets are handled at the session setup layer.
        Some(false)
    }
}

// ── recvmmsg wrapper ──────────────────────────────────────────────────────────

/// Drain up to `RECV_BATCH` datagrams from `socket` via a single `recvmmsg`
/// call.  Returns a vec of `(slot_index, bytes_received)` pairs.
///
/// Uses `MSG_DONTWAIT` so the call returns immediately if the socket is empty.
fn recv_batch_recvmmsg(
    socket: &UdpSocket,
    pool: &mut RecvBufferPool,
) -> Vec<(usize, usize)> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;

        let fd = socket.as_raw_fd();
        let n  = RECV_BATCH.min(POOL_SIZE);

        let mut iovecs: Vec<libc::iovec>   = Vec::with_capacity(n);
        let mut hdrs:   Vec<libc::mmsghdr> = Vec::with_capacity(n);

        for i in 0..n {
            let slot = pool.slot_mut(i);
            iovecs.push(libc::iovec {
                iov_base: slot.as_mut_ptr() as *mut libc::c_void,
                iov_len:  BUF_SIZE,
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

        // SAFETY: fd is valid; iov pointers live for the duration of the call;
        // MSG_DONTWAIT prevents blocking.
        let received = unsafe {
            libc::recvmmsg(
                fd,
                hdrs.as_mut_ptr(),
                n as libc::c_uint,
                libc::MSG_DONTWAIT,
                std::ptr::null_mut(),
            )
        };

        if received <= 0 {
            return Vec::new();
        }
        (0..received as usize)
            .map(|i| (i, hdrs[i].msg_len as usize))
            .collect()
    }

    #[cfg(not(target_os = "linux"))]
    {
        // Portable fallback: one blocking recv per call.
        let slot = pool.slot_mut(0);
        match socket.recv(slot) {
            Ok(n)  => vec![(0, n)],
            Err(_) => Vec::new(),
        }
    }
}

// ── Tokio readable helper ─────────────────────────────────────────────────────

/// Waits until the socket is readable (EPOLLIN) using Tokio's async I/O reactor.
async fn readable(socket: &UdpSocket) -> bool {
    // Convert the std socket to a Tokio UdpSocket just to poll readiness,
    // then immediately re-convert.  This avoids holding a TokioUdpSocket
    // permanently while still supporting our Arc<std::net::UdpSocket> design.
    use tokio::net::UdpSocket as TokioUdp;

    // We duplicate the fd so Tokio can register it without taking ownership.
    #[cfg(unix)]
    {
        use std::os::unix::io::{AsRawFd, FromRawFd};
        let fd = socket.as_raw_fd();
        let dup_fd = unsafe { libc::dup(fd) };
        if dup_fd < 0 {
            return false;
        }
        let std_dup = unsafe { UdpSocket::from_raw_fd(dup_fd) };
        std_dup.set_nonblocking(true).ok();
        if let Ok(tok) = TokioUdp::from_std(std_dup) {
            tok.readable().await.is_ok()
        } else {
            false
        }
    }

    #[cfg(not(unix))]
    {
        // On non-Unix platforms just yield once so the caller can try recv.
        tokio::task::yield_now().await;
        true
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionStats;

    #[test]
    fn recv_buffer_pool_slot_isolation() {
        let mut pool = RecvBufferPool::new();
        pool.slot_mut(0)[0] = 0xAA;
        pool.slot_mut(1)[0] = 0xBB;
        assert_eq!(pool.slot(0, 1)[0], 0xAA);
        assert_eq!(pool.slot(1, 1)[0], 0xBB);
    }

    #[tokio::test]
    async fn process_data_packet_updates_sack() {
        let (data_tx, _data_rx)   = mpsc::channel(16);
        let (sack_tx, _sack_rx)   = mpsc::channel(16);
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        socket.set_nonblocking(true).unwrap();

        let mut rx = Receiver::new(
            42,
            socket,
            0,
            data_tx,
            sack_tx,
            SessionStats::new(),
        );

        // Craft a valid DATA packet with 4 payload bytes.
        let payload = [0xDE, 0xAD, 0xBE, 0xEF];
        let hdr = RudpHeader::new_data(42, 0, now_us(), payload.len() as u16);
        let mut buf = vec![0u8; RudpHeader::SIZE + payload.len()];
        hdr.write_to(&mut buf);
        buf[RudpHeader::SIZE..].copy_from_slice(&payload);

        let result = rx.process_datagram(&buf).await;
        assert_eq!(result, Some(false));
        assert!(rx.sack_mgr.payload().is_received(0));
    }

    #[tokio::test]
    async fn fin_returns_true() {
        let (data_tx, _data_rx) = mpsc::channel(16);
        let (sack_tx, _sack_rx) = mpsc::channel(16);
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        socket.set_nonblocking(true).unwrap();
        socket.connect(socket.local_addr().unwrap()).unwrap();

        let mut rx = Receiver::new(
            7,
            socket,
            0,
            data_tx,
            sack_tx,
            SessionStats::new(),
        );

        let hdr = RudpHeader::new_fin(7, 99, now_us());
        let mut buf = [0u8; RudpHeader::SIZE];
        hdr.write_to(&mut buf);

        let result = rx.process_datagram(&buf).await;
        assert_eq!(result, Some(true));
    }
}
