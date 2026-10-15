//! Thin wrappers over kernel interfaces the crates we use do not cover.

use std::io;
use std::os::fd::AsRawFd;

/// `__NR_cachestat`, the same on x86_64 and the asm-generic table
/// (arm64, riscv) at v7.1.
pub const NR_CACHESTAT: libc::c_long = 451;

/// Page-cache residency of a file range, from `cachestat(2)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct CacheStat {
    /// Pages resident.
    pub nr_cache: u64,
    /// Resident pages that are dirty.
    pub nr_dirty: u64,
    /// Resident pages under writeback.
    pub nr_writeback: u64,
    /// Pages evicted since last access.
    pub nr_evicted: u64,
    /// Pages evicted recently enough that refaulting them counts as thrashing.
    pub nr_recently_evicted: u64,
}

#[repr(C)]
struct CacheStatRange {
    off: u64,
    len: u64,
}

/// Query `cachestat(2)` for `len` bytes at `off`; `len` 0 means to the
/// end of the file. Returns `ErrorKind::Unsupported` on kernels before
/// 6.5.
pub fn cachestat(fd: &impl AsRawFd, off: u64, len: u64) -> io::Result<CacheStat> {
    let range = CacheStatRange { off, len };
    let mut out = CacheStat::default();
    // SAFETY: the kernel reads `range` and writes `out`, both live
    // `repr(C)` locals matching `include/uapi/linux/mman.h`, and
    // flags are zero as the syscall requires.
    let rc = unsafe {
        libc::syscall(
            NR_CACHESTAT,
            fd.as_raw_fd(),
            std::ptr::from_ref(&range),
            std::ptr::from_mut(&mut out),
            0u32,
        )
    };
    if rc == 0 {
        Ok(out)
    } else {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENOSYS) {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "cachestat(2) needs Linux 6.5",
            ))
        } else {
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn cachestat_counts_a_file_we_just_wrote() {
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(&vec![1u8; 3 * 4096]).unwrap();
        f.flush().unwrap();
        match cachestat(&f, 0, 0) {
            Ok(st) => assert_eq!(st.nr_cache, 3, "{st:?}"),
            Err(e) if e.kind() == io::ErrorKind::Unsupported => {}
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn cachestat_rejects_a_bad_fd() {
        struct Bad;
        impl AsRawFd for Bad {
            fn as_raw_fd(&self) -> i32 {
                -1
            }
        }
        let err = cachestat(&Bad, 0, 0).unwrap_err();
        assert!(
            matches!(err.raw_os_error(), Some(libc::EBADF))
                || err.kind() == io::ErrorKind::Unsupported
        );
    }
}
