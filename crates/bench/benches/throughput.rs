use std::path::PathBuf;

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};

use udpix_protocol::packet::{now_us, RudpHeader};
use udpix_protocol::sack::{SackManager, SACK_WINDOW};

// ── Benchmark 1: RUDP packet encode / decode round-trip ──────────────────────

fn bench_rudp_encode_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("rudp_packet");
    let payload = vec![0xABu8; 1400];
    let header_size = RudpHeader::SIZE;
    group.throughput(Throughput::Bytes((header_size + payload.len()) as u64));

    group.bench_function("encode_1400b", |b| {
        b.iter(|| {
            let hdr = RudpHeader::new_data(
                black_box(1),
                black_box(42),
                black_box(now_us()),
                black_box(1400),
            );
            let mut buf = vec![0u8; header_size + payload.len()];
            hdr.write_to(&mut buf[..header_size]);
            buf[header_size..].copy_from_slice(&payload);
            black_box(buf)
        })
    });

    group.bench_function("decode_1400b", |b| {
        let hdr = RudpHeader::new_data(1, 42, now_us(), 1400);
        let mut buf = vec![0u8; header_size + 1400];
        hdr.write_to(&mut buf[..header_size]);
        b.iter(|| {
            let parsed = RudpHeader::read_from(black_box(&buf[..header_size])).unwrap();
            black_box(parsed)
        })
    });

    group.finish();
}

// ── Benchmark 2: SACK bitmap operations ─────────────────────────────────────

fn bench_sack_bitmap(c: &mut Criterion) {
    let mut group = c.benchmark_group("sack_bitmap");

    group.bench_function("mark_1024_sequential", |b| {
        b.iter(|| {
            let mut mgr = SackManager::new(0);
            for seq in 0..black_box(1024u64) {
                mgr.on_packet_received(seq);
            }
            black_box(mgr.payload().clone())
        })
    });

    group.bench_function("missing_seqs_sparse", |b| {
        // Pre-build a manager with every other seq missing
        let mut mgr = SackManager::new(0);
        let window = SACK_WINDOW.min(128);
        for seq in (0..window).step_by(2) {
            mgr.on_packet_received(seq);
        }
        b.iter(|| {
            let missing: Vec<u64> = mgr.payload().missing_seqs().collect();
            black_box(missing)
        })
    });

    group.finish();
}

// ── Benchmark 3: IoEngine small-file packing ─────────────────────────────────

fn bench_io_engine_pack(c: &mut Criterion) {
    use udpix_ioengine::IoEngine;
    use udpix_protocol::sender::SenderCommand;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    // Create a tmpdir with 100 × 4 KB files once
    let tmp = tempdir_with_files(100, 4096);
    let root = tmp.0.clone();
    let file_bytes = 100 * 4096u64;

    let mut group = c.benchmark_group("io_engine");
    group.throughput(Throughput::Bytes(file_bytes));
    group.sample_size(10); // disk I/O is slow; limit samples

    group.bench_function("pack_100x4kb", |b| {
        b.iter(|| {
            rt.block_on(async {
                let (tx, mut rx) = tokio::sync::mpsc::channel::<SenderCommand>(1024);
                let engine = IoEngine::new(std::env::temp_dir()).unwrap();
                let send_fut = engine.send_directory(root.clone(), tx);
                let drain_fut = async move {
                    while let Some(cmd) = rx.recv().await {
                        match cmd {
                            SenderCommand::Shutdown => break,
                            SenderCommand::SendChunk(data) => { black_box(data); }
                        }
                    }
                };
                tokio::join!(send_fut, drain_fut).0.unwrap();
            })
        })
    });

    group.finish();
    // tmp is dropped here, deleting the temp directory
    drop(tmp);
}

/// Create `n` files of `size` bytes each in a new temp directory.
/// Returns (dir_path, TempKeeper) — drop TempKeeper to delete the dir.
fn tempdir_with_files(n: usize, size: usize) -> (PathBuf, TempKeeper) {
    let dir = std::env::temp_dir().join(format!("udpix-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let payload = vec![0xCDu8; size];
    for i in 0..n {
        std::fs::write(dir.join(format!("file_{i:04}.bin")), &payload).unwrap();
    }
    (dir.clone(), TempKeeper(dir))
}

struct TempKeeper(PathBuf);
impl Drop for TempKeeper {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}

criterion_group!(
    benches,
    bench_rudp_encode_decode,
    bench_sack_bitmap,
    bench_io_engine_pack,
);
criterion_main!(benches);
