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

/// Resident bytes of exactly the range `[ptr, ptr + len)`, from
/// `mincore(2)`, which unlike smaps is not confused by the kernel
/// merging adjacent mappings into one VMA.
fn resident_in_range(ptr: NonNull<libc::c_void>, len: usize) -> io::Result<u64> {
    let page = page_size() as usize;
    let pages = len.div_ceil(page);
    let mut vec = vec![0u8; pages];
    // SAFETY: `ptr` is a page-aligned mapping of at least `len` bytes
    // and `vec` has one byte per page in it.
    if unsafe { libc::mincore(ptr.as_ptr(), len, vec.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(vec.iter().filter(|b| *b & 1 == 1).count() as u64 * page as u64)
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
    /// hugepages, otherwise forbid them, since a host set to `always`
    /// would back the buffer with them regardless; whether the kernel
    /// granted them is reported by [`huge_bytes`](Self::huge_bytes).
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
        } else {
            // Explicit, not merely absent: a THP policy of `always` would
            // otherwise back the buffer with hugepages.
            // SAFETY: as above.
            let _ = unsafe { madvise(ptr, len, MmapAdvise::MADV_NOHUGEPAGE) };
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

    /// Resident bytes of this mapping, from `mincore(2)`.
    pub fn resident_bytes(&self) -> io::Result<u64> {
        resident_in_range(self.ptr, self.len)
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

/// A read-only shared mapping of the start of a [`CacheFile`]: pages
/// the sparse stream can visit that count as page cache, not as
/// anonymous memory, for a profile whose anonymous footprint cannot
/// spare them.
#[derive(Debug)]
pub struct FileMap {
    ptr: NonNull<libc::c_void>,
    len: usize,
}

// SAFETY: a plain read-only region; workers only read it through
// volatile loads and never form references into it.
unsafe impl Send for FileMap {}
// SAFETY: as above; concurrent volatile reads of a read-only mapping
// race with nothing.
unsafe impl Sync for FileMap {}

impl FileMap {
    /// Length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the mapping is empty; it never is.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Start of the mapping.
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr().cast()
    }
}

impl Drop for FileMap {
    fn drop(&mut self) {
        // SAFETY: the mapping was made by `CacheFile::map` with this length.
        let _ = unsafe { munmap(self.ptr, self.len) };
    }
}

impl CacheFile {
    /// Map the first `len` bytes read-only and shared, advised for
    /// random access so no read-ahead follows the stream.
    pub fn map(&self, len: usize) -> io::Result<FileMap> {
        let len = len.min(self.len as usize);
        let size = NonZeroUsize::new(len)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty mapping"))?;
        // SAFETY: a shared read-only mapping of our own file.
        let ptr = unsafe {
            mmap(
                None,
                size,
                ProtFlags::PROT_READ,
                MapFlags::MAP_SHARED,
                self.file.as_fd(),
                0,
            )
        }?;
        // SAFETY: the range is exactly what was just mapped.
        let _ = unsafe { madvise(ptr, len, MmapAdvise::MADV_RANDOM) };
        Ok(FileMap { ptr, len })
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

    /// Resident bytes of this mapping, from `mincore(2)`.
    pub fn resident_bytes(&self) -> io::Result<u64> {
        resident_in_range(self.ptr, self.len)
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

    #[test]
    fn a_cache_file_maps_read_only_and_shows_its_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheFile::create(dir.path(), 3 * 4096).unwrap();
        let map = cache.map(2 * 4096).unwrap();
        assert_eq!(map.len(), 2 * 4096, "capped to what was asked");
        assert!(!map.is_empty());
        let whole = cache.map(1 << 30).unwrap();
        assert_eq!(whole.len(), 3 * 4096, "capped to the file");
        // SAFETY: inside the mapping.
        let first = unsafe { std::ptr::read_volatile(map.as_ptr()) };
        // SAFETY: the last byte of the mapping.
        let last = unsafe { std::ptr::read_volatile(map.as_ptr().add(2 * 4096 - 1)) };
        let _ = (first, last);
        assert!(cache.map(0).is_err());
    }
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
        assert_eq!(buf.resident_bytes().unwrap(), 16 << 20);
        let huge = buf.huge_bytes().unwrap();
        assert!(huge <= 16 << 20, "{huge}");
        if huge > 0 {
            assert!(buf.huge_advised());
        }
        let lazy = AnonBuffer::new_untouched(64 << 20, false).unwrap();
        assert_eq!(
            lazy.resident_bytes().unwrap(),
            0,
            "untouched pages are not resident"
        );
        assert_eq!(resident_in_range(lazy.ptr, 1).unwrap(), 0);
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
