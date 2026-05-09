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

    let ReceiveArgs { output_dir, peer, direct, local_port, stun, .. } = args;

    let proto_socket: Arc<std::net::UdpSocket> = if direct {
        let bind_addr = format!("0.0.0.0:{local_port}");
        let sock = std::net::UdpSocket::bind(&bind_addr)
            .with_context(|| format!("bind UDP on {bind_addr}"))?;
        sock.connect(peer)
            .with_context(|| format!("connect to sender {peer}"))?;
        // Maximize the kernel receive buffer to avoid burst packet loss.
        // Try 16 MB first; the kernel clips to net.core.rmem_max silently.
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let buf: libc::c_int = 16 * 1024 * 1024;
            unsafe {
                libc::setsockopt(
                    sock.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    &buf as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
        }
        tracing::info!(
            "Direct mode: {} ← {}",
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
        std_sock.connect(peer_addr).context("connect to sender")?;
        Arc::new(std_sock)
    };

    if direct {
        run_direct(output_dir, proto_socket).await
    } else {
        run_rudp(output_dir, proto_socket).await
    }
}

// ── Direct-mode receive (framing reassembly) ──────────────────────────────────
//
// The sender (direct mode) prefixes each PackBlock with an 8-byte LE length.
// The RUDP Receiver delivers individual ~1443-byte payloads; this function
// reassembles them into complete PackBlocks before handing off to the IoEngine.
async fn run_direct(
    output_dir: PathBuf,
    socket: Arc<std::net::UdpSocket>,
) -> anyhow::Result<()> {
    let stats = SessionStats::new();

    // raw_data_tx  → RUDP Receiver pushes individual datagram payloads here
    // block_tx     → reassembly task pushes complete length-prefixed blocks here
    let (raw_data_tx, mut raw_data_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(512);
    let (block_tx, block_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let (sack_tx, _sack_rx) = tokio::sync::mpsc::channel::<SackPayload>(512);

    // Reassembly task: accumulates RUDP payloads and extracts complete PackBlocks.
    // The 8-byte LE prefix written by the sender tells us the block's byte length.
    tokio::spawn(async move {
        let mut accum: Vec<u8> = Vec::new();
        while let Some(chunk) = raw_data_rx.recv().await {
            accum.extend_from_slice(&chunk);
            loop {
                if accum.len() < 8 {
                    break;
                }
                let block_len =
                    u64::from_le_bytes(accum[..8].try_into().unwrap()) as usize;
                if accum.len() < 8 + block_len {
                    break;
                }
                let block = accum[8..8 + block_len].to_vec();
                accum.drain(..8 + block_len);
                if block_tx.send(block).await.is_err() {
                    return; // IoEngine dropped block_rx early
                }
            }
        }
        // raw_data_tx closed (RUDP Receiver exited on FIN) → block_tx dropped → IoEngine EOF
    });

    let receiver = Receiver::new(
        1,
        Arc::clone(&socket),
        0,
        raw_data_tx,
        sack_tx,
        Arc::clone(&stats),
    );
    let io = IoEngine::new(output_dir.clone()).context("IoEngine::new")?;

    let recv_handle = tokio::spawn(receiver.run());

    let t0 = std::time::Instant::now();
    tracing::info!("Receiving into {:?}", output_dir);
    io.receive_files(block_rx).await.context("receive_files")?;
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

// ── RUDP receive (NAT traversal mode) ────────────────────────────────────────
async fn run_rudp(
    output_dir: PathBuf,
    socket: Arc<std::net::UdpSocket>,
) -> anyhow::Result<()> {
    let stats = SessionStats::new();
    let (data_tx, data_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(512);
    let (sack_tx, _sack_rx) = tokio::sync::mpsc::channel::<SackPayload>(512);

    let receiver = Receiver::new(
        1,
        Arc::clone(&socket),
        0,
        data_tx,
        sack_tx,
        Arc::clone(&stats),
    );
    let io = IoEngine::new(output_dir.clone()).context("IoEngine::new")?;

    let recv_handle = tokio::spawn(receiver.run());

    let t0 = std::time::Instant::now();
    tracing::info!("Receiving into {:?}", output_dir);
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
