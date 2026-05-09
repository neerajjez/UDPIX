use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Context;
use clap::Args;

use udpix_ioengine::IoEngine;
use udpix_protocol::sack::SackPayload;
use udpix_protocol::sender::{Sender, SenderCommand};
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
    let proto_socket: Arc<std::net::UdpSocket> = if args.direct {
        // ── Direct / LAN mode ─────────────────────────────────────────────────
        // Bind to a known local port, connect straight to the peer.
        // No STUN, no ICE — works on any subnet where the peer is reachable.
        let bind_addr = format!("0.0.0.0:{}", args.local_port);
        let sock = std::net::UdpSocket::bind(&bind_addr)
            .with_context(|| format!("bind UDP on {bind_addr}"))?;
        sock.connect(args.peer)
            .with_context(|| format!("connect to {}", args.peer))?;
        tracing::info!(
            "Direct mode: {} → {}",
            sock.local_addr().unwrap(),
            args.peer
        );
        Arc::new(sock)
    } else {
        // ── NAT traversal mode ────────────────────────────────────────────────
        let stun_servers = args.stun.into_iter().collect::<Vec<_>>();
        let engine = TraversalEngine::new(HolePunchConfig {
            stun_servers,
            ..Default::default()
        });
        tracing::info!("NAT traversal: connecting to {}", args.peer);
        let result = engine
            .connect(args.peer, args.local_port)
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

    let stats = SessionStats::new();
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<SenderCommand>(512);
    let (_sack_tx, sack_rx) = tokio::sync::mpsc::channel::<SackPayload>(512);

    let sender = Sender::new(1, Arc::clone(&proto_socket), cmd_rx, sack_rx, Arc::clone(&stats));
    let io = IoEngine::new(std::env::temp_dir()).context("IoEngine::new")?;

    let sender_handle = tokio::spawn(sender.run());

    let t0 = std::time::Instant::now();
    tracing::info!("Sending {:?}", args.path);
    io.send_directory(args.path, cmd_tx)
        .await
        .context("send_directory")?;
    sender_handle.await.context("sender task panicked")?;

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
