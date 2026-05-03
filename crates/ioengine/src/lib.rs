/// # udpix-ioengine
///
/// The storage engine that bridges the file system and the RUDP protocol layer.
///
/// ## The problem it solves
///
/// Transferring 10 million 1 KB files naively produces:
///   - One `open()` + `close()` syscall pair per file (≈ 20 million syscalls)
///   - Kernel inode-cache thrashing on both sender and receiver
///   - Per-file protocol handshake overhead (one connection setup per file)
///   - Disk seek storms as the OS chases inodes across the file system
///
/// This engine removes all of that overhead by abstracting the file system away
/// entirely before data reaches the network layer.
///
/// ## Architecture
///
/// ```text
///  File system                  Network layer (Phase 1)
///  ───────────                  ───────────────────────
///  dir/
///   ├── file_00001.bin          SenderCommand::SendChunk(Vec<u8>)
///   ├── file_00002.bin    ──►   ──► Sender → sendmmsg → UDP wire
///   └── ...
///
///  reader.rs   — io_uring async reads (256-deep SQ, 64KB per op)
///  packer.rs   — groups files into 16 MB PackBlocks with binary manifest
///  writer.rs   — io_uring async writes, re-assembles split large files
///  zerocopy.rs — splice(2) / sendfile(2) helpers for zero-copy paths
/// ```
///
/// ## Module responsibilities
///
/// ### `packer`  — Bulk file multiplexer
///   Packs `(path, bytes)` pairs into ≤16 MB `PackBlock`s with an embedded
///   binary manifest (magic, entry count, per-file path/offset/size/part info).
///   The receiver calls `Packer::unpack` to reconstruct the original file tree.
///
/// ### `reader`  — Async disk reader
///   Uses `io_uring` on Linux (QUEUE_DEPTH=256, READ_CHUNK=64KB) and falls
///   back to `std::fs::read` on other platforms.  Runs on a dedicated OS
///   thread bridged to Tokio via crossbeam channels.
///
/// ### `writer`  — Async disk writer
///   Same threading model as `reader`.  Accepts packed block bytes from the
///   network receiver, unpacks them, creates intermediate directories, and
///   writes each file via `io_uring` write operations.
///
/// ### `zerocopy`  — Zero-copy helpers
///   `splice(2)` (file → pipe → fd) and `sendfile(2)` (file → socket) wrappers.
///   Linux-only; return `Unsupported` on other platforms.

pub mod packer;
pub mod reader;
pub mod writer;
pub mod zerocopy;

use std::path::PathBuf;

use anyhow::Context;
use tokio::sync::mpsc;

use udpix_protocol::sender::SenderCommand;

// ── IoEngine coordinator ──────────────────────────────────────────────────────

/// Top-level coordinator that wires reader → packer → sender (send path)
/// and receiver → writer (receive path).
///
/// Construct one `IoEngine` per transfer session.
pub struct IoEngine {
    reader: reader::AsyncReader,
    writer: writer::AsyncWriter,
}

impl IoEngine {
    /// Create a new engine.  All files received will be written under `output_dir`.
    pub fn new(output_dir: PathBuf) -> anyhow::Result<Self> {
        Ok(Self {
            reader: reader::AsyncReader::new().context("failed to start io_uring reader")?,
            writer: writer::AsyncWriter::new(output_dir).context("failed to start io_uring writer")?,
        })
    }

    /// **Send path**: walk `root`, pack all files, and feed the packed blocks
    /// into the RUDP sender via `sender_tx`.
    pub async fn send_directory(
        &self,
        root: PathBuf,
        sender_tx: mpsc::Sender<SenderCommand>,
    ) -> anyhow::Result<()> {
        let total = self
            .reader
            .stream_directory(root, sender_tx.clone())
            .await
            .context("IoEngine::send_directory failed")?;

        // Signal end of transfer.
        sender_tx
            .send(SenderCommand::Shutdown)
            .await
            .context("sender channel closed before Shutdown could be sent")?;

        tracing::info!("IoEngine: sent {total} bytes");
        Ok(())
    }

    /// **Receive path**: drain `data_rx` and write all files to disk.
    pub async fn receive_files(
        &self,
        data_rx: mpsc::Receiver<Vec<u8>>,
    ) -> anyhow::Result<()> {
        let total = self
            .writer
            .receive_and_write(data_rx)
            .await
            .context("IoEngine::receive_files failed")?;
        tracing::info!("IoEngine: received and wrote {total} bytes");
        Ok(())
    }
}

// ── Integration tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    struct TmpDir(std::path::PathBuf);
    impl TmpDir {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir()
                .join(format!("udpix_test_{name}_{}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &std::path::Path { &self.0 }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn engine_send_receive_loopback() {
        let src_dir = TmpDir::new("engine_src");
        let dst_dir = TmpDir::new("engine_dst");

        // Write test files in the source directory.
        let test_files: Vec<(&str, Vec<u8>)> = vec![
            ("alpha.txt",       b"hello from alpha".to_vec()),
            ("sub/beta.bin",    vec![0xBEu8; 64]),
            ("sub/gamma.bin",   vec![0xCAu8; 128]),
        ];
        for (name, data) in &test_files {
            let path = src_dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, data).unwrap();
        }

        // Wire: reader → mpsc channel → writer (simulating the network in-process).
        let (sender_tx, mut sender_rx) = mpsc::channel::<SenderCommand>(64);
        let (data_tx,   data_rx)       = mpsc::channel::<Vec<u8>>(64);

        let dst_path_send = dst_dir.path().to_path_buf();
        let dst_path_recv = dst_dir.path().to_path_buf();
        let src_root      = src_dir.path().to_path_buf();

        // Drive the sender side.
        let send_handle = tokio::spawn(async move {
            let engine = IoEngine::new(dst_path_send).unwrap();
            engine.send_directory(src_root, sender_tx).await.unwrap();
        });

        // Bridge: forward SendChunk payloads to the receiver channel.
        let bridge_handle = tokio::spawn(async move {
            while let Some(cmd) = sender_rx.recv().await {
                match cmd {
                    SenderCommand::SendChunk(bytes) => {
                        data_tx.send(bytes).await.unwrap();
                    }
                    SenderCommand::Shutdown => break,
                }
            }
        });

        // Drive the receiver side.
        let recv_handle = tokio::spawn(async move {
            let engine = IoEngine::new(dst_path_recv).unwrap();
            engine.receive_files(data_rx).await.unwrap();
        });

        send_handle.await.unwrap();
        bridge_handle.await.unwrap();
        recv_handle.await.unwrap();

        // Verify all files landed correctly.
        for (name, expected) in &test_files {
            let path = dst_dir.path().join(name);
            let got  = std::fs::read(&path)
                .unwrap_or_else(|_| panic!("missing output file: {path:?}"));
            assert_eq!(&got, expected, "content mismatch for {name}");
        }
    }
}
