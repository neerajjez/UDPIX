use std::path::PathBuf;
use std::net::SocketAddr;
use std::sync::Arc;

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

    /// Peer address to receive from (host:port)
    pub peer: SocketAddr,

    /// STUN server for NAT traversal (host:port)
    #[arg(long)]
    pub stun: Option<SocketAddr>,

    /// Local UDP port (0 = OS-assigned)
    #[arg(long, default_value = "0")]
    pub local_port: u16,

    /// Username for authentication
    #[arg(long, default_value = "admin")]
    pub username: String,

    /// Password for authentication
    #[arg(long, default_value = "changeme")]
    pub password: String,
}

pub async fn run(args: ReceiveArgs) -> anyhow::Result<()> {
    std::fs::create_dir_all(&args.output_dir)
        .with_context(|| format!("create output dir {}", args.output_dir.display()))?;

    let stun_servers = args.stun.into_iter().collect::<Vec<_>>();
    let engine = TraversalEngine::new(HolePunchConfig {
        stun_servers,
        ..Default::default()
    });

    tracing::info!("Establishing UDP path to {}", args.peer);
    let result = engine.connect(args.peer, args.local_port).await
        .context("NAT traversal failed")?;
    let peer_addr = result.peer_addr;
    tracing::info!("Connected to {} (relay={})", peer_addr, result.relay_used);

    // Convert tokio socket → std socket (protocol crate uses std)
    let tokio_sock = Arc::try_unwrap(result.socket)
        .map_err(|_| anyhow::anyhow!("unexpected shared socket Arc after punch"))?;
    let std_sock = tokio_sock.into_std().context("into_std")?;
    std_sock.connect(peer_addr).context("connect to peer")?;
    let proto_socket = Arc::new(std_sock);

    // Wire channels
    let stats = SessionStats::new();
    let (data_tx, data_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    let (sack_tx, _sack_rx) = tokio::sync::mpsc::channel::<SackPayload>(256);

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

    tracing::info!("Receiving files into {:?}", args.output_dir);
    io.receive_files(data_rx).await
        .context("receive_files")?;

    recv_handle.await.context("receiver task panicked")?;

    tracing::info!(
        "Transfer complete — received {} bytes",
        stats.bytes_acked.load(std::sync::atomic::Ordering::Relaxed),
    );
    Ok(())
}
