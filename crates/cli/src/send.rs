use std::path::PathBuf;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use clap::Args;

use udpix_ioengine::IoEngine;
use udpix_protocol::sender::{Sender, SenderCommand};
use udpix_protocol::sack::SackPayload;
use udpix_protocol::session::SessionStats;
use udpix_traversal::{TraversalEngine, holepunch::HolePunchConfig};

#[derive(Args)]
pub struct SendArgs {
    /// File or directory to send
    pub path: PathBuf,

    /// Server address (host:port)
    pub server: SocketAddr,

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

pub async fn run(args: SendArgs) -> anyhow::Result<()> {
    let stun_servers = args.stun.into_iter().collect::<Vec<_>>();
    let engine = TraversalEngine::new(HolePunchConfig {
        stun_servers,
        ..Default::default()
    });

    tracing::info!("Establishing UDP path to {}", args.server);
    let result = engine.connect(args.server, args.local_port).await
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
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<SenderCommand>(256);
    let (_sack_tx, sack_rx) = tokio::sync::mpsc::channel::<SackPayload>(256);

    let sender = Sender::new(1, Arc::clone(&proto_socket), cmd_rx, sack_rx, Arc::clone(&stats));

    // IoEngine: output_dir unused on send side
    let io = IoEngine::new(std::env::temp_dir()).context("IoEngine::new")?;

    let sender_handle = tokio::spawn(sender.run());

    tracing::info!("Sending {:?}", args.path);
    io.send_directory(args.path, cmd_tx).await
        .context("send_directory")?;

    sender_handle.await.context("sender task panicked")?;

    tracing::info!(
        "Transfer complete — sent {} bytes, retransmits={}",
        stats.bytes_sent.load(std::sync::atomic::Ordering::Relaxed),
        stats.retransmits.load(std::sync::atomic::Ordering::Relaxed),
    );
    Ok(())
}
