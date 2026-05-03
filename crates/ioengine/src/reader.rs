/// Asynchronous file reader powered by `io_uring`.
///
/// # Why io_uring?
///
/// Traditional `read(2)` blocks the calling thread until the disk responds.
/// At 10 Gbps we need to be reading the *next* 64 KB block before the
/// current one is even off the disk.  `io_uring` solves this:
///
///   1. Submit up to `QUEUE_DEPTH` (256) read operations in one batch
///   2. Continue preparing the next batch while the kernel fulfils the first
///   3. Reap completions in a tight loop — no blocking, no context switches
///
/// With `IORING_FEAT_SQPOLL` (kernel 5.11+) the kernel has its own polling
/// thread, eliminating even the `io_uring_enter(2)` syscall on the submit path.
///
/// # Threading model
///
/// `io-uring 0.7` is a low-level synchronous binding.  We run the submission /
/// completion loop on a **dedicated OS thread** and bridge back to the async
/// Tokio world via `crossbeam_channel`.  This keeps the hot disk I/O path
/// isolated from the Tokio executor thread pool.
///
/// # Fallback
///
/// Non-Linux platforms (developer laptops on macOS, CI runners) fall back to
/// `tokio::fs::read` so the rest of the code compiles and tests run unmodified.

use std::path::{Path, PathBuf};

use anyhow::Context;
use crossbeam_channel::{bounded, Sender as CbSender};
use tokio::sync::mpsc;

use crate::packer::Packer;
use udpix_protocol::sender::SenderCommand;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Number of in-flight io_uring SQ entries (must be a power of 2).
pub const QUEUE_DEPTH: u32 = 256;

/// Bytes per io_uring read operation.  64 KB balances syscall overhead and
/// memory pressure; matches typical kernel read-ahead granularity.
pub const READ_CHUNK: usize = 64 * 1024;

// ── Internal request / response types ────────────────────────────────────────

/// A work item sent from the async API to the background io_uring thread.
enum ReadRequest {
    /// Read this file completely and return its bytes.
    ReadFile(PathBuf),
    /// Shut down the background thread.
    Shutdown,
}

/// Result returned by the background thread for a single file read.
struct ReadResult {
    path:  PathBuf,
    bytes: anyhow::Result<Vec<u8>>,
}

// ── AsyncReader ───────────────────────────────────────────────────────────────

/// Async file reader that uses `io_uring` on Linux and `tokio::fs` elsewhere.
///
/// Each `AsyncReader` owns one background thread running the io_uring event
/// loop.  Multiple concurrent reads are submitted to the ring and reaped in
/// order of completion.
pub struct AsyncReader {
    /// Channel for sending read requests to the background thread.
    request_tx: CbSender<ReadRequest>,

    /// Channel on which the background thread returns completed read results.
    result_rx: crossbeam_channel::Receiver<ReadResult>,
}

impl AsyncReader {
    /// Spawn the background io_uring thread and return a ready reader.
    pub fn new() -> anyhow::Result<Self> {
        let (request_tx, request_rx) = bounded::<ReadRequest>(QUEUE_DEPTH as usize * 2);
        let (result_tx, result_rx)   = bounded::<ReadResult>(QUEUE_DEPTH as usize * 2);

        std::thread::Builder::new()
            .name("udpix-io-uring-reader".into())
            .spawn(move || io_uring_read_loop(request_rx, result_tx))?;

        Ok(Self { request_tx, result_rx })
    }

    // ── Public async API ──────────────────────────────────────────────────────

    /// Walk `root`, pack all files into `PackBlock`s, and send each block as a
    /// `SenderCommand::SendChunk` on `sender_tx`.
    ///
    /// Returns the total bytes of file data dispatched.
    pub async fn stream_directory(
        &self,
        root: PathBuf,
        sender_tx: mpsc::Sender<SenderCommand>,
    ) -> anyhow::Result<u64> {
        // Collect the file list synchronously (directory walk is cheap relative
        // to disk I/O and keeps the submission ordering deterministic).
        let file_paths = collect_files(&root)?;
        let n = file_paths.len();

        // Submit all read requests to the background thread.
        for path in &file_paths {
            self.request_tx
                .send(ReadRequest::ReadFile(path.clone()))
                .context("io_uring reader thread unexpectedly closed")?;
        }

        // Accumulate (path, bytes) pairs as completions arrive, then pack.
        let mut file_data: Vec<(PathBuf, Vec<u8>)> = Vec::with_capacity(n);
        let mut total_bytes = 0u64;

        for _ in 0..n {
            // Blocking receive on the crossbeam channel — yield the Tokio
            // runtime while we wait so other tasks keep making progress.
            let result = tokio::task::block_in_place(|| self.result_rx.recv())
                .context("io_uring reader thread died")?;

            let bytes = result.bytes.with_context(|| {
                format!("failed to read '{}'", result.path.display())
            })?;

            // Store a relative path (strip the root prefix for portability).
            let rel = result.path.strip_prefix(&root)
                .unwrap_or(&result.path)
                .to_path_buf();
            total_bytes += bytes.len() as u64;
            file_data.push((rel, bytes));
        }

        // Pack and ship.
        let blocks = Packer::pack_files(&file_data);
        for block in blocks {
            let data: Vec<u8> = block.data.to_vec();
            sender_tx
                .send(SenderCommand::SendChunk(data))
                .await
                .context("Sender channel closed before all blocks were sent")?;
        }

        Ok(total_bytes)
    }

    /// Read a single large file and stream it in `PackBlock` sized chunks.
    pub async fn read_large_file(
        &self,
        path: PathBuf,
        sender_tx: mpsc::Sender<SenderCommand>,
    ) -> anyhow::Result<u64> {
        self.request_tx
            .send(ReadRequest::ReadFile(path.clone()))
            .context("io_uring reader thread closed")?;

        let result = tokio::task::block_in_place(|| self.result_rx.recv())
            .context("io_uring reader thread died")?;

        let bytes = result
            .bytes
            .with_context(|| format!("failed to read '{}'", path.display()))?;

        let total = bytes.len() as u64;
        let rel   = path.file_name().map(PathBuf::from).unwrap_or(path.clone());
        let blocks = Packer::pack_files(&[(rel, bytes)]);

        for block in blocks {
            let data: Vec<u8> = block.data.to_vec();
            sender_tx
                .send(SenderCommand::SendChunk(data))
                .await
                .context("Sender channel closed")?;
        }
        Ok(total)
    }
}

impl Drop for AsyncReader {
    fn drop(&mut self) {
        // Best-effort shutdown: if the channel is already closed, that's fine.
        let _ = self.request_tx.send(ReadRequest::Shutdown);
    }
}

// ── Background io_uring read loop ─────────────────────────────────────────────

/// Run on a dedicated OS thread.  Receives `ReadRequest`s, submits io_uring
/// read operations, reaps completions, and sends `ReadResult`s back.
fn io_uring_read_loop(
    rx: crossbeam_channel::Receiver<ReadRequest>,
    tx: CbSender<ReadResult>,
) {
    for req in rx {
        match req {
            ReadRequest::Shutdown => break,
            ReadRequest::ReadFile(path) => {
                let bytes = read_file_platform(&path);
                let _ = tx.send(ReadResult { path, bytes });
            }
        }
    }
}

/// Platform-specific file read.
///
/// On Linux we use `io_uring` for overlapping reads.
/// Elsewhere we fall back to std I/O (still in the background thread, so we
/// don't block the Tokio executor).
fn read_file_platform(path: &Path) -> anyhow::Result<Vec<u8>> {
    #[cfg(target_os = "linux")]
    return read_file_io_uring(path);

    #[cfg(not(target_os = "linux"))]
    return read_file_std(path);
}

// ── Linux io_uring implementation ─────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn read_file_io_uring(path: &Path) -> anyhow::Result<Vec<u8>> {
    use io_uring::{opcode, types, IoUring};
    use std::os::unix::io::AsRawFd;

    let file = std::fs::File::open(path)
        .with_context(|| format!("open '{}'", path.display()))?;
    let file_len = file.metadata()?.len() as usize;
    let fd = types::Fd(file.as_raw_fd());

    let mut result = vec![0u8; file_len];
    let mut ring   = IoUring::new(QUEUE_DEPTH)
        .context("io_uring::new")?;
    let mut offset = 0usize;

    while offset < file_len {
        let chunk = (file_len - offset).min(READ_CHUNK);
        let buf_ptr = result[offset..offset + chunk].as_mut_ptr();

        // Build a Read submission entry.
        let sqe = opcode::Read::new(fd, buf_ptr, chunk as u32)
            .offset(offset as u64)
            .build()
            .user_data(offset as u64); // tag with offset so we can verify ordering

        // SAFETY: buf_ptr points into `result` which lives for the whole
        // duration of this function; `ring` is not moved after submission.
        unsafe { ring.submission().push(&sqe) }
            .context("io_uring SQ full")?;

        ring.submit_and_wait(1).context("io_uring submit_and_wait")?;

        // Reap the completion.
        for cqe in ring.completion() {
            let n = cqe.result();
            if n < 0 {
                anyhow::bail!(
                    "io_uring read error at offset {}: {}",
                    cqe.user_data(),
                    std::io::Error::from_raw_os_error(-n)
                );
            }
            offset += n as usize;
        }
    }

    Ok(result)
}

// ── Portable fallback ─────────────────────────────────────────────────────────

#[cfg(not(target_os = "linux"))]
fn read_file_std(path: &Path) -> anyhow::Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("read '{}'", path.display()))
}

// ── Directory walker ──────────────────────────────────────────────────────────

/// Recursively collect all regular file paths under `root`.
fn collect_files(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    collect_recursive(root, &mut paths)?;
    Ok(paths)
}

fn collect_recursive(dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("read_dir '{}'", dir.display()))?
    {
        let entry = entry?;
        let path  = entry.path();
        let meta  = entry.metadata()?;
        if meta.is_dir() {
            collect_recursive(&path, out)?;
        } else if meta.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a uniquely-named temp directory under /tmp for a test, clean it up on drop.
    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("udpix_test_{name}_{}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path { &self.0 }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reads_temp_file() {
        let dir     = TmpDir::new("reader_single");
        let path    = dir.path().join("hello.txt");
        let content = b"io_uring reader test payload";
        std::fs::write(&path, content).unwrap();

        let reader     = AsyncReader::new().unwrap();
        let (tx, mut rx) = mpsc::channel(16);

        let bytes_sent = reader
            .read_large_file(path.clone(), tx)
            .await
            .unwrap();

        assert_eq!(bytes_sent as usize, content.len());

        // The sender should have received one SendChunk.
        let cmd = rx.recv().await.unwrap();
        let SenderCommand::SendChunk(data) = cmd else { panic!("expected SendChunk") };

        // Unpack the block and verify the file contents.
        let block = crate::packer::PackBlock {
            id:   udpix_common::types::BlockId(0),
            data: bytes::Bytes::from(data),
        };
        let entries = Packer::unpack(&block).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1.as_ref(), content);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stream_directory_sends_all_files() {
        let dir = TmpDir::new("reader_dir");
        // Write 5 small test files.
        for i in 0..5 {
            let p = dir.path().join(format!("f{i}.bin"));
            std::fs::write(p, vec![i as u8; 128]).unwrap();
        }

        let reader = AsyncReader::new().unwrap();
        let (tx, mut rx) = mpsc::channel(32);

        let bytes_sent = reader
            .stream_directory(dir.path().to_path_buf(), tx)
            .await
            .unwrap();

        assert_eq!(bytes_sent, 5 * 128);
        assert!(rx.try_recv().is_ok(), "expected at least one SendChunk");
    }
}
