use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Context;
use clap::Args;

use udpix_ioengine::IoEngine;
use udpix_protocol::receiver::Receiver;
use udpix_protocol::sack::SackPayload;
use udpix_protocol::session::SessionStats;
use udpix_traversal::{TraversalEngine, holepunch::HolePunchConfig};

#[derive(Args)]
pub struct ReceiveArgs {
    /// Directory to write received files into
    pub output_dir: PathBuf,

    /// Sender address (host:port)
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

pub async fn run(args: ReceiveArgs) -> anyhow::Result<()> {
    std::fs::create_dir_all(&args.output_dir)
        .with_context(|| format!("create output dir {}", args.output_dir.display()))?;

    let proto_socket: Arc<std::net::UdpSocket> = if args.direct {
        // ── Direct / LAN mode ─────────────────────────────────────────────────
        let bind_addr = format!("0.0.0.0:{}", args.local_port);
        let sock = std::net::UdpSocket::bind(&bind_addr)
            .with_context(|| format!("bind UDP on {bind_addr}"))?;
        sock.connect(args.peer)
            .with_context(|| format!("connect to sender {}", args.peer))?;
        tracing::info!(
            "Direct mode: {} ← {}",
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
        std_sock.connect(peer_addr).context("connect to sender")?;
        Arc::new(std_sock)
    };

    let stats = SessionStats::new();
    let (data_tx, data_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(512);
    let (sack_tx, _sack_rx) = tokio::sync::mpsc::channel::<SackPayload>(512);

    let receiver = Receiver::new(
        1,
        Arc::clone(&proto_socket),
        0,
        data_tx,
        sack_tx,
        Arc::clone(&stats),
    );
    let io = IoEngine::new(args.output_dir.clone()).context("IoEngine::new")?;

    let recv_handle = tokio::spawn(receiver.run());

    let t0 = std::time::Instant::now();
    tracing::info!("Receiving into {:?}", args.output_dir);
    io.receive_files(data_rx).await.context("receive_files")?;
    recv_handle.await.context("receiver task panicked")?;

    let elapsed = t0.elapsed();
    let bytes = stats.bytes_acked.load(Ordering::Relaxed);
    let mbps = bytes as f64 / elapsed.as_secs_f64() / 1_000_000.0;
    tracing::info!(
        "Transfer complete — {bytes} bytes in {:.2}s ({mbps:.1} MB/s)",
        elapsed.as_secs_f64(),
    );
    Ok(())
}
