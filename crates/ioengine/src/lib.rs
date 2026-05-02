/// # udpix-ioengine
///
/// The storage engine — reads files from disk and feeds them to the protocol
/// layer at the speeds required to saturate a 10 Gbps network link.
///
/// ## The disk I/O problem
/// Once the UDP protocol unlocks multi-gigabit WAN speeds, the bottleneck
/// immediately shifts to the local disk. A single large file is easy — modern
/// NVMe drives handle sequential reads trivially. But transferring 10 million
/// 1 KB files is catastrophic for a naive implementation:
///   - Each `open()` / `read()` / `close()` triplet costs inode lookups
///   - Traditional blocking `read()` stalls the thread on every call
///   - The OS context-switches thousands of times per second doing nothing useful
///
/// ## What this crate implements
///
/// ### `reader` / `writer`  — io_uring async disk I/O
///   `io_uring` is a Linux kernel interface that uses two shared ring buffers
///   (Submission Queue and Completion Queue) mapped between user space and kernel.
///   We submit up to 1024 read/write operations in one batch and poll completions
///   without ever blocking. With `SQPOLL` mode a kernel thread polls the queue,
///   eliminating all syscall overhead for sustained I/O.
///
/// ### `packer`  — Small-file bulk multiplexer (the "RayFile" pattern)
///   When transferring thousands of small files, we do NOT send them individually.
///   Instead the packer groups them into contiguous 16 MB virtual blocks, each
///   with an internal manifest header listing the files it contains (path, size,
///   checksum). The network layer only ever sees large continuous byte streams.
///   On the receiving end the unpacker reads the manifest and writes each file
///   to its correct path. This eliminates per-file handshake overhead entirely.
///
/// ### `zerocopy`  — splice / MSG_ZEROCOPY wrappers
///   Conventionally, reading a file and sending it over a socket copies data:
///     disk → kernel buffer → user buffer → socket buffer → NIC
///   Zero-copy techniques (`splice`, `sendfile`, `MSG_ZEROCOPY`) let the DMA
///   engine move data directly from the filesystem page cache to the NIC,
///   freeing the CPU entirely for encryption and rate-control math.

pub mod packer;
pub mod reader;
pub mod writer;
pub mod zerocopy;
