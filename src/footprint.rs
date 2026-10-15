//! The generator's memory footprint: anonymous, page cache, shared and
//! kernel memory, each as an owned object that is released on drop.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::num::NonZeroUsize;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use nix::sys::mman::{MapFlags, MmapAdvise, ProtFlags, madvise, mmap, mmap_anonymous, munmap};
use nix::sys::statfs::{TMPFS_MAGIC, statfs};
use procfs::process::Process;

use crate::measure::page_size;
use crate::sys::cachestat;

/// Rough slab cost of one empty file: a dentry and an inode, as
/// measured on a 6.x kernel. The fitter corrects it where it can
/// measure kernel memory.
pub const BYTES_PER_EMPTY_FILE: u64 = 1100;

/// Whether a directory is on tmpfs, where a file is shared memory
/// rather than page cache.
pub fn is_tmpfs(path: &Path) -> io::Result<bool> {
    Ok(statfs(path)?.filesystem_type() == TMPFS_MAGIC)
}

/// Touch one byte per page so the mapping is resident.
fn touch(ptr: NonNull<libc::c_void>, len: usize) {
    let page = page_size() as usize;
    let base = ptr.as_ptr().cast::<u8>();
    for off in (0..len).step_by(page) {
        // SAFETY: `off` is within the `len` bytes mapped at `base`.
        unsafe { std::ptr::write_volatile(base.add(off), 1) };
    }
}

/// One smaps statistic of the mapping that contains `ptr`, in bytes.
fn mapping_field(ptr: NonNull<libc::c_void>, key: &str) -> io::Result<u64> {
    let addr = ptr.as_ptr() as u64;
    let maps = Process::myself()
        .and_then(|p| p.smaps())
        .map_err(io::Error::other)?;
    Ok(maps
        .iter()
        .filter(|m| m.address.0 <= addr && addr < m.address.1)
        .map(|m| m.extension.map.get(key).copied().unwrap_or(0))
        .sum())
}

/// A private anonymous mapping, touched once.
#[derive(Debug)]
pub struct AnonBuffer {
    ptr: NonNull<libc::c_void>,
    len: usize,
    huge_advised: bool,
}

// SAFETY: the buffer is a plain region of memory; workers access it
// through volatile reads and writes at disjoint or benignly racing
// addresses and never form references into it.
unsafe impl Send for AnonBuffer {}
// SAFETY: as above; shared access is by raw pointer only.
unsafe impl Sync for AnonBuffer {}

impl AnonBuffer {
    /// Map and touch `len` bytes. With `huge`, ask for transparent
    /// hugepages; whether the kernel granted them is reported by
    /// [`huge_bytes`](Self::huge_bytes).
    pub fn new(len: usize, huge: bool) -> io::Result<AnonBuffer> {
        let buf = Self::new_untouched(len, huge)?;
        touch(buf.ptr, len);
        Ok(buf)
    }

    /// Map `len` bytes without touching them; pages fault in on first
    /// use, so untouched ones never count as resident.
    pub fn new_untouched(len: usize, huge: bool) -> io::Result<AnonBuffer> {
        let size = NonZeroUsize::new(len)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty buffer"))?;
        // SAFETY: anonymous mapping with no address hint.
        let ptr = unsafe {
            mmap_anonymous(
                None,
                size,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS,
            )
        }?;
        let mut buf = AnonBuffer {
            ptr,
            len,
            huge_advised: false,
        };
        if huge {
            // SAFETY: the range is exactly what was just mapped.
            buf.huge_advised = unsafe { madvise(ptr, len, MmapAdvise::MADV_HUGEPAGE) }.is_ok();
        }
        Ok(buf)
    }

    /// Length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the mapping is empty; it never is.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Base address.
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr().cast()
    }

    /// Whether `MADV_HUGEPAGE` was accepted.
    pub fn huge_advised(&self) -> bool {
        self.huge_advised
    }

    /// Bytes of this mapping backed by transparent hugepages, from
    /// `/proc/self/smaps`.
    pub fn huge_bytes(&self) -> io::Result<u64> {
        mapping_field(self.ptr, "AnonHugePages")
    }

    /// Resident bytes of this mapping, from `/proc/self/smaps`. The
    /// kernel merges adjacent mappings with equal flags, so a merged
    /// neighbour's pages are included.
    pub fn resident_bytes(&self) -> io::Result<u64> {
        mapping_field(self.ptr, "Rss")
    }
}

impl Drop for AnonBuffer {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly what `new` mapped.
        let _ = unsafe { munmap(self.ptr, self.len) };
    }
}

/// A file written to a disk-backed directory so its pages sit in the
/// page cache. Removed on drop.
#[derive(Debug)]
pub struct CacheFile {
    file: File,
    path: PathBuf,
    len: u64,
}

impl CacheFile {
    /// Create `len` bytes of file under `dir`.
    pub fn create(dir: &Path, len: u64) -> io::Result<CacheFile> {
        let path = dir.join(format!(
            "thrashgen-cache-{}-{}",
            std::process::id(),
            nix::unistd::gettid()
        ));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        let chunk = vec![0x5au8; 1 << 20];
        let mut written = 0u64;
        while written < len {
            let n = (len - written).min(chunk.len() as u64) as usize;
            file.write_all(&chunk[..n])?;
            written += n as u64;
        }
        file.flush()?;
        Ok(CacheFile { file, path, len })
    }

    /// Length in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the file is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The open file, for residency checks.
    pub fn file(&self) -> &File {
        &self.file
    }

    /// Bytes currently resident, from `cachestat(2)`.
    pub fn resident_bytes(&self) -> io::Result<u64> {
        Ok(cachestat(&self.file, 0, 0)?.nr_cache * page_size())
    }

    /// Read the whole file to pull evicted pages back in.
    pub fn reread(&mut self) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = self.file.read(&mut buf)?;
            if n == 0 {
                break;
            }
        }
        Ok(())
    }
}

impl Drop for CacheFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Shared memory: a `memfd` mapped and touched, so it counts as
/// resident shmem for the process and as shmem for its cgroup.
#[derive(Debug)]
pub struct ShmemBuffer {
    _fd: OwnedFd,
    ptr: NonNull<libc::c_void>,
    len: usize,
}

// SAFETY: as for `AnonBuffer`.
unsafe impl Send for ShmemBuffer {}
// SAFETY: as for `AnonBuffer`.
unsafe impl Sync for ShmemBuffer {}

impl ShmemBuffer {
    /// Create and touch `len` bytes.
    pub fn new(len: usize) -> io::Result<ShmemBuffer> {
        let size = NonZeroUsize::new(len)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty buffer"))?;
        let fd = nix::sys::memfd::memfd_create(
            "thrashgen-shmem",
            nix::sys::memfd::MFdFlags::MFD_CLOEXEC,
        )?;
        nix::unistd::ftruncate(fd.as_fd(), len as libc::off_t)?;
        // SAFETY: shared mapping of the whole memfd.
        let ptr = unsafe {
            mmap(
                None,
                size,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                fd.as_fd(),
                0,
            )
        }?;
        touch(ptr, len);
        Ok(ShmemBuffer { _fd: fd, ptr, len })
    }

    /// Length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the buffer is empty; it never is.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Base address.
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr().cast()
    }

    /// Resident bytes of this mapping, from `/proc/self/smaps`.
    pub fn resident_bytes(&self) -> io::Result<u64> {
        mapping_field(self.ptr, "Rss")
    }
}

impl Drop for ShmemBuffer {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly what `new` mapped.
        let _ = unsafe { munmap(self.ptr, self.len) };
    }
}

/// Empty files whose dentries and inodes occupy kernel slab charged to
/// the creating cgroup. Removed on drop.
#[derive(Debug)]
pub struct KernelFiles {
    dir: PathBuf,
    count: usize,
}

impl KernelFiles {
    /// An empty set in a fresh subdirectory of `dir`.
    pub fn new(dir: &Path) -> io::Result<KernelFiles> {
        let dir = dir.join(format!("thrashgen-slab-{}", std::process::id()));
        std::fs::create_dir(&dir)?;
        Ok(KernelFiles { dir, count: 0 })
    }

    /// Files currently present.
    pub fn count(&self) -> usize {
        self.count
    }

    /// Create or remove files to reach `n`.
    pub fn set_count(&mut self, n: usize) -> io::Result<()> {
        while self.count < n {
            File::create(self.dir.join(self.count.to_string()))?;
            self.count += 1;
        }
        while self.count > n {
            self.count -= 1;
            std::fs::remove_file(self.dir.join(self.count.to_string()))?;
        }
        Ok(())
    }

    /// Files needed for roughly `bytes` of slab.
    pub fn files_for(bytes: u64) -> usize {
        bytes.div_ceil(BYTES_PER_EMPTY_FILE) as usize
    }
}

impl Drop for KernelFiles {
    fn drop(&mut self) {
        let _ = self.set_count(0);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::Root;
    use crate::measure::{Scope, memory};

    fn rss() -> (u64, u64) {
        let m = memory(&Scope::SelfProcess, &Root::default(), &[]).unwrap();
        (m.anon.unwrap(), m.shmem.unwrap())
    }

    #[test]
    fn anon_buffer_is_resident_and_lazy_when_untouched() {
        let buf = AnonBuffer::new(16 << 20, true).unwrap();
        assert_eq!(buf.len(), 16 << 20);
        assert!(!buf.is_empty());
        assert!(!buf.as_ptr().is_null());
        let resident = buf.resident_bytes().unwrap();
        assert!(resident >= 15 << 20, "{resident}");
        let huge = buf.huge_bytes().unwrap();
        assert!(huge <= 16 << 20, "{huge}");
        if huge > 0 {
            assert!(buf.huge_advised());
        }
        // The kernel merges adjacent mappings with equal flags into one
        // VMA, so a neighbour's pages can show up here; half the size is
        // still far below what touching would give.
        let lazy = AnonBuffer::new_untouched(64 << 20, false).unwrap();
        let lazy_resident = lazy.resident_bytes().unwrap();
        assert!(
            lazy_resident < 32 << 20,
            "untouched pages are not resident: {lazy_resident}"
        );
        assert!(AnonBuffer::new(0, false).is_err());
    }

    #[test]
    fn cache_file_is_resident_rereadable_and_removed() {
        let dir = std::env::temp_dir();
        let mut f = CacheFile::create(&dir, 4 << 20).unwrap();
        assert_eq!(f.len(), 4 << 20);
        assert!(!f.is_empty());
        let path = f.path.clone();
        assert!(path.exists());
        let resident = f.resident_bytes().unwrap();
        assert!(resident > 0 && resident <= 4 << 20, "{resident}");
        f.reread().unwrap();
        assert!(f.resident_bytes().unwrap() > 0);
        assert_eq!(f.file().metadata().unwrap().len(), 4 << 20);
        drop(f);
        assert!(!path.exists());
        assert!(CacheFile::create(Path::new("/nonexistent"), 1).is_err());
        let _ = is_tmpfs(&dir).unwrap();
        assert!(is_tmpfs(Path::new("/nonexistent")).is_err());
    }

    #[test]
    fn shmem_buffer_is_resident_and_counts_as_shared() {
        let before = rss().1;
        let s = ShmemBuffer::new(4 << 20).unwrap();
        assert_eq!(s.len(), 4 << 20);
        assert!(!s.is_empty() && !s.as_ptr().is_null());
        assert!(s.resident_bytes().unwrap() >= (4 << 20) - 4096);
        let during = rss().1;
        assert!(
            during >= before + (4 << 20) - 4096,
            "RssShmem {before} -> {during}"
        );
        assert!(ShmemBuffer::new(0).is_err());
    }

    #[test]
    fn kernel_files_scale_and_clean_up() {
        let tmp = tempfile::tempdir().unwrap();
        let mut k = KernelFiles::new(tmp.path()).unwrap();
        let dir = k.dir.clone();
        k.set_count(50).unwrap();
        assert_eq!(k.count(), 50);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 50);
        k.set_count(20).unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 20);
        assert_eq!(KernelFiles::files_for(0), 0);
        assert_eq!(KernelFiles::files_for(BYTES_PER_EMPTY_FILE * 3 + 1), 4);
        drop(k);
        assert!(!dir.exists());
        assert!(KernelFiles::new(Path::new("/nonexistent")).is_err());
    }
}
