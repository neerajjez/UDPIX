use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Context;
use clap::Args;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::time::{sleep, Duration, Instant};

use udpix_ioengine::IoEngine;
use udpix_protocol::packet::{now_us, RudpHeader};
use udpix_protocol::sender::{Sender, SenderCommand, MAX_PAYLOAD};
use udpix_protocol::sack::SackPayload;
use udpix_protocol::session::SessionStats;
use udpix_traversal::{TraversalEngine, holepunch::HolePunchConfig};

#[derive(Args)]
pub struct SendArgs {
    /// File or directory to send
    pub path: PathBuf,

    /// Receiver address (host:port)
    pub peer: SocketAddr,

    /// Skip NAT traversal — connect directly (use on LAN / same subnet)
    #[arg(long)]
    pub direct: bool,

    /// Local UDP port (0 = OS-assigned; for --direct, set a fixed port)
    #[arg(long, default_value = "0")]
    pub local_port: u16,

    /// STUN server for NAT traversal (host:port) — ignored with --direct
    #[arg(long)]
    pub stun: Option<SocketAddr>,

    /// Username for gRPC authentication
    #[arg(long, default_value = "admin")]
    pub username: String,

    /// Password for gRPC authentication
    #[arg(long, default_value = "changeme")]
    pub password: String,
}

pub async fn run(args: SendArgs) -> anyhow::Result<()> {
    let SendArgs { path, peer, direct, local_port, stun, .. } = args;

    let proto_socket: Arc<std::net::UdpSocket> = if direct {
        let bind_addr = format!("0.0.0.0:{local_port}");
        let sock = std::net::UdpSocket::bind(&bind_addr)
            .with_context(|| format!("bind UDP on {bind_addr}"))?;
        sock.connect(peer)
            .with_context(|| format!("connect to {peer}"))?;
        tracing::info!(
            "Direct mode: {} → {}",
            sock.local_addr().unwrap(),
            peer
        );
        Arc::new(sock)
    } else {
        let stun_servers = stun.into_iter().collect::<Vec<_>>();
        let engine = TraversalEngine::new(HolePunchConfig {
            stun_servers,
            ..Default::default()
        });
        tracing::info!("NAT traversal: connecting to {peer}");
        let result = engine
            .connect(peer, local_port)
            .await
            .context("NAT traversal failed")?;
        let peer_addr = result.peer_addr;
        tracing::info!("Traversal succeeded (relay={})", result.relay_used);
        let tokio_sock = Arc::try_unwrap(result.socket)
            .map_err(|_| anyhow::anyhow!("unexpected shared socket Arc after punch"))?;
        let std_sock = tokio_sock.into_std().context("into_std")?;
        std_sock.connect(peer_addr).context("connect to peer")?;
        Arc::new(std_sock)
    };

    if direct {
        tcp_await_ready(peer).await?;
        run_direct(path, proto_socket).await
    } else {
        run_rudp(path, proto_socket).await
    }
}

// Wait for the receiver's TCP "READY" signal before blasting UDP data.
// Retries the TCP connect every 500 ms until the receiver comes up or 60 s elapses.
async fn tcp_await_ready(peer: SocketAddr) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    tracing::info!("TCP handshake: waiting for receiver at {peer} (60s timeout)");
    loop {
        if Instant::now() >= deadline {
            anyhow::bail!("TCP handshake: receiver did not become ready within 60s");
        }
        match TcpStream::connect(peer).await {
            Ok(mut stream) => {
                let mut buf = [0u8; 8];
                let n = stream.read(&mut buf).await
                    .context("TCP handshake: read error")?;
                if n >= 5 && &buf[..5] == b"READY" {
                    tracing::info!("TCP handshake: receiver ready — starting transfer");
                    return Ok(());
                }
                anyhow::bail!("TCP handshake: unexpected response from receiver");
            }
            Err(_) => {
                // Receiver not up yet — retry in 500 ms
                sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

// ── Direct-mode send (no rate limiting — designed for LAN / zero-loss paths) ──
//
// Each PackBlock is wrapped in an 8-byte LE length prefix before being split
// into RUDP DATA datagrams.  This lets the receiver's reassembly task know
// exactly where each block boundary is, since RUDP delivers per-datagram
// payloads (~1443 bytes) rather than whole blocks.
async fn run_direct(path: PathBuf, socket: Arc<std::net::UdpSocket>) -> anyhow::Result<()> {
    let io = IoEngine::new(std::env::temp_dir()).context("IoEngine::new")?;
    let (io_tx, mut io_rx) = tokio::sync::mpsc::channel::<SenderCommand>(64);

    let sock = Arc::clone(&socket);
    let send_handle = tokio::spawn(async move {
        let mut seq: u64 = 0;
        let mut bytes_sent: u64 = 0;

        while let Some(cmd) = io_rx.recv().await {
            match cmd {
                SenderCommand::SendChunk(data) => {
                    // 8-byte LE length prefix so receiver can reconstruct block boundaries
                    let mut framed = Vec::with_capacity(8 + data.len());
                    framed.extend_from_slice(&(data.len() as u64).to_le_bytes());
                    framed.extend_from_slice(&data);

                    for chunk in framed.chunks(MAX_PAYLOAD) {
                        let hdr = RudpHeader::new_data(1, seq, now_us(), chunk.len() as u16);
                        let mut pkt = vec![0u8; RudpHeader::SIZE + chunk.len()];
                        hdr.write_to(&mut pkt);
                        pkt[RudpHeader::SIZE..].copy_from_slice(chunk);
                        let _ = sock.send(&pkt);
                        seq += 1;
                        bytes_sent += chunk.len() as u64;
                    }
                }
                SenderCommand::Shutdown => break,
            }
        }

        // FIN sent 5× to survive light packet loss
        let fin = RudpHeader::new_fin(1, seq, now_us());
        let mut fin_buf = [0u8; RudpHeader::SIZE];
        fin.write_to(&mut fin_buf);
        for _ in 0..5 {
            let _ = sock.send(&fin_buf);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        bytes_sent
    });

    let t0 = std::time::Instant::now();
    tracing::info!("Sending {:?}", path);
    io.send_directory(path, io_tx)
        .await
        .context("send_directory")?;

    let bytes_sent = send_handle.await.context("send task panicked")?;
    let elapsed = t0.elapsed();
    let mbps = bytes_sent as f64 / elapsed.as_secs_f64() / 1_000_000.0;
    tracing::info!(
        "Transfer complete — {bytes_sent} bytes in {:.2}s ({mbps:.1} MB/s)",
        elapsed.as_secs_f64(),
    );
    Ok(())
}

// ── RUDP send (NAT traversal mode with congestion control) ────────────────────
async fn run_rudp(path: PathBuf, socket: Arc<std::net::UdpSocket>) -> anyhow::Result<()> {
    let stats = SessionStats::new();
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<SenderCommand>(512);
    let (_sack_tx, sack_rx) = tokio::sync::mpsc::channel::<SackPayload>(512);

    let sender = Sender::new(1, Arc::clone(&socket), cmd_rx, sack_rx, Arc::clone(&stats));
    let io = IoEngine::new(std::env::temp_dir()).context("IoEngine::new")?;

    let sender_handle = tokio::spawn(sender.run());

    let t0 = std::time::Instant::now();
    tracing::info!("Sending {:?}", path);
    io.send_directory(path, cmd_tx)
        .await
        .context("send_directory")?;

    // Without live SACK feedback the in_flight window never drains; give the
    // sender 1 s to flush any last retransmits then abort.
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        sender_handle,
    )
    .await;

    // Fire FIN a few times to survive light packet loss.
    let fin = RudpHeader::new_fin(1, 0, now_us());
    let mut fin_buf = [0u8; RudpHeader::SIZE];
    fin.write_to(&mut fin_buf);
    for _ in 0..5 {
        let _ = socket.send(&fin_buf);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let elapsed = t0.elapsed();
    let bytes = stats.bytes_sent.load(Ordering::Relaxed);
    let mbps = bytes as f64 / elapsed.as_secs_f64() / 1_000_000.0;
    tracing::info!(
        "Transfer complete — {bytes} bytes in {:.2}s ({mbps:.1} MB/s), retransmits={}",
        elapsed.as_secs_f64(),
        stats.retransmits.load(Ordering::Relaxed),
    );
    Ok(())
}
