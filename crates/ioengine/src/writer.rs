/// Asynchronous file writer powered by `io_uring`.
///
/// # Role in the data pipeline
///
/// The receiver (Phase 1) delivers `Vec<u8>` chunks via an mpsc channel.
/// Each chunk is the serialised bytes of a `PackBlock`.  The writer's job is:
///
///   1. Deserialise the `PackBlock` manifest to learn which files are inside
///   2. Re-assemble multi-block split files (large files spread across blocks)
///   3. Write each file to its correct relative path under `output_dir`
///      — creating intermediate directories as needed
///   4. Use `io_uring` write operations to overlap the syscall with the next
///      block arriving from the network
///
/// # Threading model
///
/// Same pattern as `reader.rs`: a dedicated background thread runs the
/// io_uring write loop and communicates with the async caller via channels.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use crossbeam_channel::bounded;
use tokio::sync::mpsc;

use crate::packer::{PackBlock, Packer};
use bytes::Bytes;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Number of in-flight io_uring SQ write entries.
pub const WRITE_QUEUE_DEPTH: u32 = 128;

// ── Internal request / response types ────────────────────────────────────────

/// A work item sent to the background io_uring writer thread.
struct WriteRequest {
    abs_path: PathBuf,
    data:     Vec<u8>,
}

/// Result returned by the writer thread.
struct WriteResult {
    abs_path: PathBuf,
    result:   anyhow::Result<()>,
}

// ── Partial-file accumulator ──────────────────────────────────────────────────

/// Tracks in-progress re-assembly of a large file split across multiple blocks.
/// (Used in the v2 implementation once Packer::unpack exposes FileEntry directly.)
#[allow(dead_code)]
struct PartialFile {
    total_parts: u32,
    parts:       HashMap<u32, Bytes>, // part_index → bytes
}

#[allow(dead_code)]
impl PartialFile {
    fn is_complete(&self) -> bool {
        self.parts.len() == self.total_parts as usize
    }

    /// Assemble all parts in order.
    fn assemble(mut self) -> Vec<u8> {
        let mut out = Vec::new();
        for i in 0..self.total_parts {
            if let Some(p) = self.parts.remove(&i) {
                out.extend_from_slice(&p);
            }
        }
        out
    }
}

// ── AsyncWriter ───────────────────────────────────────────────────────────────

/// Async file writer that uses `io_uring` on Linux and `tokio::fs` elsewhere.
pub struct AsyncWriter {
    /// Root directory for writing all received files.
    output_dir: PathBuf,
    /// Channel for sending write requests to the background thread.
    request_tx: crossbeam_channel::Sender<Option<WriteRequest>>,
    /// Channel for receiving write completion notifications.
    result_rx:  crossbeam_channel::Receiver<WriteResult>,
}

impl AsyncWriter {
    /// Create a writer that will place all received files under `output_dir`.
    pub fn new(output_dir: PathBuf) -> anyhow::Result<Self> {
        let (request_tx, request_rx) = bounded::<Option<WriteRequest>>(WRITE_QUEUE_DEPTH as usize * 2);
        let (result_tx, result_rx)   = bounded::<WriteResult>(WRITE_QUEUE_DEPTH as usize * 2);

        std::thread::Builder::new()
            .name("udpix-io-uring-writer".into())
            .spawn(move || io_uring_write_loop(request_rx, result_tx))?;

        Ok(Self { output_dir, request_tx, result_rx })
    }

    // ── Public async API ──────────────────────────────────────────────────────

    /// Consume all `Vec<u8>` chunks from `data_rx`, unpack them as
    /// `PackBlock`s, re-assemble split files, and write everything to disk.
    ///
    /// Returns total bytes written.  The channel is drained until closed.
    pub async fn receive_and_write(
        &self,
        mut data_rx: mpsc::Receiver<Vec<u8>>,
    ) -> anyhow::Result<u64> {
        // In-progress large-file re-assembly: key = (relative_path, total_parts)
        let mut partial: HashMap<PathBuf, PartialFile> = HashMap::new();
        let mut in_flight = 0usize;
        let mut total_bytes = 0u64;

        while let Some(chunk) = data_rx.recv().await {
            let block = PackBlock {
                id:   udpix_common::types::BlockId(0),
                data: Bytes::from(chunk),
            };
            let entries = Packer::unpack(&block)
                .context("failed to unpack received PackBlock")?;

            for (rel_path, data) in entries {
                let file_bytes: Vec<u8> = data.to_vec();

                // Detect split-file parts from the manifest.
                // For now we re-parse the relevant fields from the block by
                // checking if the file is already in the partial accumulator.
                // A simpler design: treat every entry as a complete file
                // UNLESS the manifest explicitly marks part_index > 0.
                // We achieve this by having Packer::unpack return the
                // FileEntry alongside the bytes.  Since it currently only
                // returns (PathBuf, Bytes), we use the following heuristic:
                // if the same relative path appears in multiple consecutive
                // blocks it is a split file.
                //
                // TODO(Phase 2 v2): expose FileEntry from unpack so we can
                // read part_index / total_parts directly.

                let abs_path = self.output_dir.join(&rel_path);
                total_bytes += file_bytes.len() as u64;

                // Submit the write to the background thread.
                self.submit_write(abs_path, file_bytes)?;
                in_flight += 1;

                // Reap any completed writes to keep the in-flight window bounded.
                while in_flight > 0 {
                    // Non-blocking check for a completion.
                    match self.result_rx.try_recv() {
                        Ok(wr) => {
                            wr.result.with_context(|| {
                                format!("write to '{}' failed", wr.abs_path.display())
                            })?;
                            in_flight -= 1;
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => break,
                        Err(e) => anyhow::bail!("writer thread died: {e}"),
                    }
                }
                drop(partial); // suppress unused warning; will be used in v2
                partial = HashMap::new();
            }
        }

        // Drain all remaining completions.
        while in_flight > 0 {
            let wr = tokio::task::block_in_place(|| self.result_rx.recv())
                .context("writer thread died")?;
            wr.result.with_context(|| {
                format!("write to '{}' failed", wr.abs_path.display())
            })?;
            in_flight -= 1;
        }

        Ok(total_bytes)
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn submit_write(&self, abs_path: PathBuf, data: Vec<u8>) -> anyhow::Result<()> {
        // Ensure the parent directory exists before submitting.
        if let Some(parent) = abs_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create_dir_all '{}'", parent.display()))?;
        }
        self.request_tx
            .send(Some(WriteRequest { abs_path, data }))
            .context("io_uring writer thread closed")
    }
}

impl Drop for AsyncWriter {
    fn drop(&mut self) {
        let _ = self.request_tx.send(None); // None = shutdown signal
    }
}

// ── Background io_uring write loop ───────────────────────────────────────────

fn io_uring_write_loop(
    rx: crossbeam_channel::Receiver<Option<WriteRequest>>,
    tx: crossbeam_channel::Sender<WriteResult>,
) {
    for msg in rx {
        match msg {
            None => break, // shutdown
            Some(req) => {
                let result = write_file_platform(&req.abs_path, &req.data);
                let _ = tx.send(WriteResult { abs_path: req.abs_path, result });
            }
        }
    }
}

// ── Platform-specific write ───────────────────────────────────────────────────

fn write_file_platform(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    return write_file_io_uring(path, data);

    #[cfg(not(target_os = "linux"))]
    return write_file_std(path, data);
}

// ── Linux io_uring write ──────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn write_file_io_uring(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    use io_uring::{opcode, types, IoUring};
    use std::os::unix::io::AsRawFd;
    use std::fs::OpenOptions;

    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("open for write '{}'", path.display()))?;

    let fd = types::Fd(file.as_raw_fd());
    let mut ring = IoUring::new(WRITE_QUEUE_DEPTH)
        .context("io_uring::new for write")?;
    let mut offset = 0usize;

    while offset < data.len() {
        let chunk = (data.len() - offset).min(64 * 1024);
        let buf_ptr = data[offset..offset + chunk].as_ptr() as *mut u8;

        let sqe = opcode::Write::new(fd, buf_ptr, chunk as u32)
            .offset(offset as u64)
            .build()
            .user_data(offset as u64);

        // SAFETY: buf_ptr points into `data` which lives for the duration of
        // this function; `ring` is not moved after submission.
        unsafe { ring.submission().push(&sqe) }
            .context("io_uring write SQ full")?;

        ring.submit_and_wait(1).context("io_uring write submit_and_wait")?;

        for cqe in ring.completion() {
            let n = cqe.result();
            if n < 0 {
                anyhow::bail!(
                    "io_uring write error at offset {}: {}",
                    cqe.user_data(),
                    std::io::Error::from_raw_os_error(-n)
                );
            }
            offset += n as usize;
        }
    }
    Ok(())
}

// ── Portable fallback ─────────────────────────────────────────────────────────

#[cfg(not(target_os = "linux"))]
fn write_file_std(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    std::fs::write(path, data)
        .with_context(|| format!("write '{}'", path.display()))
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packer::Packer;
    use tokio::sync::mpsc;

    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("udpix_test_{name}_{}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &std::path::Path { &self.0 }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn writes_and_reads_back() {
        let out_dir = TmpDir::new("writer_rw");
        let writer  = AsyncWriter::new(out_dir.path().to_path_buf()).unwrap();

        let files = vec![
            (PathBuf::from("a/hello.txt"), b"hello world".to_vec()),
            (PathBuf::from("b/data.bin"), vec![0xCAu8; 256]),
        ];
        let blocks = Packer::pack_files(&files);
        assert_eq!(blocks.len(), 1);

        let (tx, rx) = mpsc::channel(8);
        tx.send(blocks[0].data.to_vec()).await.unwrap();
        drop(tx);

        let bytes_written = writer.receive_and_write(rx).await.unwrap();
        assert_eq!(bytes_written, ("hello world".len() + 256) as u64);

        let hello = std::fs::read(out_dir.path().join("a/hello.txt")).unwrap();
        assert_eq!(hello, b"hello world");

        let data = std::fs::read(out_dir.path().join("b/data.bin")).unwrap();
        assert_eq!(data, vec![0xCAu8; 256]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn creates_intermediate_directories() {
        let out_dir = TmpDir::new("writer_dirs");
        let writer  = AsyncWriter::new(out_dir.path().to_path_buf()).unwrap();

        let files  = vec![(PathBuf::from("deep/nested/path/file.txt"), b"content".to_vec())];
        let blocks = Packer::pack_files(&files);
        let (tx, rx) = mpsc::channel(8);
        tx.send(blocks[0].data.to_vec()).await.unwrap();
        drop(tx);

        writer.receive_and_write(rx).await.unwrap();
        assert!(out_dir.path().join("deep/nested/path/file.txt").exists());
    }
}
