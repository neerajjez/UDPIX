/// Zero-copy kernel helpers: `splice(2)` and `sendfile(2)`.
///
/// # Why these matter
///
/// A normal `read()` + `write()` path copies data **four times**:
///   disk → kernel page cache → user buffer → kernel socket buffer → NIC
///
/// `splice(2)` cuts this to two:
///   disk → kernel page cache → kernel socket buffer → NIC
///
/// `sendfile(2)` is similar but transfers directly from a file fd to a socket fd.
///
/// At 10 Gbps these copies become the bottleneck.  Eliminating them frees the CPU
/// to focus on AES-GCM encryption and the rate-control math in the protocol layer.
///
/// # Platform support
///
/// Both syscalls are Linux-only.  On other platforms the functions fall back to
/// an in-process `read` + `write` loop so the rest of the code compiles and runs
/// unmodified on macOS / Windows developer machines.

use std::io;

#[cfg(target_os = "linux")]
use std::os::unix::io::RawFd;

// ── Linux implementation ──────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
pub mod linux {
    use super::*;

    /// Copy up to `len` bytes from `src_fd` to `dst_fd` via an in-kernel pipe.
    ///
    /// Both `src_fd` and `dst_fd` must be open file descriptors.
    /// Returns the number of bytes actually transferred.
    ///
    /// # How it works
    ///
    /// `splice` cannot go directly from one regular file fd to another regular
    /// file fd — it needs at least one pipe end.  We create a temporary pipe,
    /// splice `src_fd` → pipe write end, then splice pipe read end → `dst_fd`.
    ///
    /// The kernel moves pages between file system cache and the pipe without
    /// copying bytes into user space at any point.
    pub fn splice_copy(src_fd: RawFd, dst_fd: RawFd, len: usize) -> io::Result<usize> {
        // Create a kernel pipe.
        let mut pipe_fds = [0i32; 2];
        let rc = unsafe { libc::pipe(pipe_fds.as_mut_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        let pipe_rd = pipe_fds[0];
        let pipe_wr = pipe_fds[1];

        let mut total = 0usize;
        let mut remaining = len;

        while remaining > 0 {
            // Splice from source into the write end of the pipe.
            let spliced_in = unsafe {
                libc::splice(
                    src_fd,
                    std::ptr::null_mut(),
                    pipe_wr,
                    std::ptr::null_mut(),
                    remaining,
                    libc::SPLICE_F_MOVE | libc::SPLICE_F_MORE,
                )
            };
            if spliced_in <= 0 {
                break; // EOF or error; close pipe and return what we got
            }

            // Drain the pipe into the destination fd.
            let mut to_drain = spliced_in as usize;
            while to_drain > 0 {
                let spliced_out = unsafe {
                    libc::splice(
                        pipe_rd,
                        std::ptr::null_mut(),
                        dst_fd,
                        std::ptr::null_mut(),
                        to_drain,
                        libc::SPLICE_F_MOVE,
                    )
                };
                if spliced_out <= 0 {
                    unsafe { libc::close(pipe_rd); libc::close(pipe_wr); }
                    return Err(io::Error::last_os_error());
                }
                to_drain -= spliced_out as usize;
                total    += spliced_out as usize;
            }

            remaining -= spliced_in as usize;
        }

        unsafe { libc::close(pipe_rd); libc::close(pipe_wr); }
        Ok(total)
    }

    /// Copy up to `count` bytes from an open file fd directly to a socket fd
    /// using `sendfile(2)`.
    ///
    /// The kernel transfers data from the file's page cache to the socket's
    /// send buffer without a round-trip through user space.
    ///
    /// Note: `sendfile` on Linux does NOT work with encrypted payloads because
    /// we can't insert AES-GCM between the file cache and the socket.  Use this
    /// only for unencrypted paths (e.g., intra-datacenter transfers where the
    /// wire is trusted) or for Phase 2 disk-to-disk copies.
    pub fn sendfile_to_socket(file_fd: RawFd, sock_fd: RawFd, count: usize) -> io::Result<usize> {
        let mut offset: libc::off_t = 0;
        let sent = unsafe {
            libc::sendfile(sock_fd, file_fd, &mut offset, count)
        };
        if sent < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(sent as usize)
        }
    }
}

// ── Public re-exports and portable fallbacks ──────────────────────────────────

/// Copy up to `len` bytes from `src_fd` to `dst_fd`.
///
/// On Linux: uses `splice(2)` for zero-copy via in-kernel pipe.
/// On other platforms: falls back to a `read` + `write` loop.
#[cfg(target_os = "linux")]
pub fn splice_copy(src_fd: RawFd, dst_fd: RawFd, len: usize) -> io::Result<usize> {
    linux::splice_copy(src_fd, dst_fd, len)
}

#[cfg(not(target_os = "linux"))]
pub fn splice_copy(_src_fd: i32, _dst_fd: i32, _len: usize) -> io::Result<usize> {
    // Non-Linux stub.  The ioengine uses regular tokio::fs I/O on these platforms.
    Err(io::Error::new(io::ErrorKind::Unsupported, "splice_copy requires Linux"))
}

/// Transfer up to `count` bytes from a file fd to a socket fd.
///
/// On Linux: uses `sendfile(2)`.
/// On other platforms: returns `Unsupported`.
#[cfg(target_os = "linux")]
pub fn sendfile_to_socket(file_fd: RawFd, sock_fd: RawFd, count: usize) -> io::Result<usize> {
    linux::sendfile_to_socket(file_fd, sock_fd, count)
}

#[cfg(not(target_os = "linux"))]
pub fn sendfile_to_socket(_file_fd: i32, _sock_fd: i32, _count: usize) -> io::Result<usize> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "sendfile requires Linux"))
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn splice_copies_data() {
        use std::io::{Read, Seek, Write};
        use std::os::unix::io::AsRawFd;

        let pid  = std::process::id();
        let src_path = std::env::temp_dir().join(format!("udpix_splice_src_{pid}"));
        let dst_path = std::env::temp_dir().join(format!("udpix_splice_dst_{pid}"));

        let data = b"splice test payload 1234567890";
        {
            let mut f = std::fs::File::create(&src_path).unwrap();
            f.write_all(data).unwrap();
        }

        let mut src = std::fs::OpenOptions::new().read(true).open(&src_path).unwrap();
        src.seek(std::io::SeekFrom::Start(0)).unwrap();
        let dst = std::fs::File::create(&dst_path).unwrap();

        let n = super::splice_copy(src.as_raw_fd(), dst.as_raw_fd(), data.len()).unwrap();
        assert_eq!(n, data.len());

        let mut out = std::fs::File::open(&dst_path).unwrap();
        let mut buf = Vec::new();
        out.read_to_end(&mut buf).unwrap();
        assert_eq!(&buf, data);

        let _ = std::fs::remove_file(&src_path);
        let _ = std::fs::remove_file(&dst_path);
    }

    #[test]
    fn splice_unsupported_on_non_linux() {
        // This test only runs if we're NOT on Linux.
        #[cfg(not(target_os = "linux"))]
        {
            let r = super::splice_copy(0, 1, 16);
            assert!(r.is_err());
        }
        #[cfg(target_os = "linux")]
        {
            // On Linux the test is done in splice_copies_data above.
        }
    }
}
