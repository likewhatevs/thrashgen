//! Per-scope readers for the non-counter observables: busy time,
//! memory composition and limits. Each reader says which method it
//! used so the sampler can record provenance.

use std::fs::File;
use std::io::{self, BufReader};
use std::path::Path;
use std::time::Duration;

use procfs::{FromBufRead, FromRead};

use crate::caps::Root;
use crate::cgroup::Cgroup;
use crate::profile::{self, Method};
use crate::sys::cachestat;

/// What is being measured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    /// This process and its threads.
    SelfProcess,
    /// A cgroup v2 directory.
    Cgroup(Cgroup),
    /// Another process and its threads.
    Process(i32),
    /// The whole machine.
    Host,
}

impl Scope {
    /// Identifier used in samples.
    pub fn name(&self) -> String {
        match self {
            Scope::SelfProcess => "self".into(),
            Scope::Cgroup(cg) => {
                if cg.rel().is_empty() {
                    "/".into()
                } else {
                    cg.rel().to_string()
                }
            }
            Scope::Process(pid) => format!("pid {pid}"),
            Scope::Host => "host".into(),
        }
    }

    /// The profile's notion of scope.
    pub fn kind(&self) -> profile::Scope {
        match self {
            Scope::SelfProcess | Scope::Process(_) => profile::Scope::Process,
            Scope::Cgroup(_) => profile::Scope::Cgroup,
            Scope::Host => profile::Scope::Host,
        }
    }
}

/// Kernel clock ticks per second.
pub fn clock_ticks() -> f64 {
    // SAFETY: sysconf has no preconditions.
    let t = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if t > 0 { t as f64 } else { 100.0 }
}

/// Page size in bytes.
pub fn page_size() -> u64 {
    // SAFETY: sysconf has no preconditions.
    let p = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if p > 0 { p as u64 } else { 4096 }
}

/// Cumulative CPU time a scope has consumed, and how it was read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BusyTime {
    /// User plus system time.
    pub total: Duration,
    /// Source.
    pub method: Method,
}

fn rusage_self() -> io::Result<Duration> {
    // SAFETY: rusage is plain data; all-zero is a valid value.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `ru` is a valid, writable rusage.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, (t.tv_usec as u32) * 1000);
    Ok(tv(ru.ru_utime) + tv(ru.ru_stime))
}

/// Busy time of a process from `/proc/<pid>/stat`, all threads.
fn proc_stat_busy(root: &Root, pid: i32) -> io::Result<Duration> {
    let path = root.path(format!("/proc/{pid}/stat"));
    let stat = procfs::process::Stat::from_read(File::open(&path)?).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: {e}", path.display()),
        )
    })?;
    Ok(Duration::from_secs_f64(
        (stat.utime + stat.stime) as f64 / clock_ticks(),
    ))
}

/// Host busy time from the `cpu` line of `/proc/stat`: everything
/// except idle, with iowait counted as busy. Guest time is already in
/// user and nice, so it is not added again.
pub fn host_busy_from_stat(text: &str) -> io::Result<Duration> {
    let line = text
        .lines()
        .find(|l| l.starts_with("cpu "))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "/proc/stat has no cpu line"))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .map(|f| f.parse().unwrap_or(0))
        .collect();
    if fields.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "/proc/stat cpu line is short",
        ));
    }
    let idle = fields[3];
    let total: u64 = fields.iter().take(8).sum();
    Ok(Duration::from_secs_f64(
        (total - idle) as f64 / clock_ticks(),
    ))
}

/// Read a scope's cumulative busy time.
pub fn busy_time(scope: &Scope, root: &Root) -> io::Result<BusyTime> {
    Ok(match scope {
        Scope::SelfProcess => BusyTime {
            total: rusage_self()?,
            method: Method::Rusage,
        },
        Scope::Cgroup(cg) => BusyTime {
            total: Duration::from_micros(cg.cpu_stat()?.usage_usec),
            method: Method::Cgroup,
        },
        Scope::Process(pid) => BusyTime {
            total: proc_stat_busy(root, *pid)?,
            method: Method::Proc,
        },
        Scope::Host => BusyTime {
            total: host_busy_from_stat(&std::fs::read_to_string(root.path("/proc/stat"))?)?,
            method: Method::Proc,
        },
    })
}

/// Memory composition of a scope, bytes. Fields a scope cannot report
/// are `None`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Memory {
    /// Everything charged, including kernel memory where known.
    pub current: Option<u64>,
    /// Anonymous.
    pub anon: Option<u64>,
    /// Page cache excluding shmem.
    pub pagecache: Option<u64>,
    /// Shared memory and tmpfs.
    pub shmem: Option<u64>,
    /// Source for `current`, `anon` and `shmem`.
    pub method: Method,
    /// Source for `pagecache`, which may differ.
    pub pagecache_method: Method,
}

/// The `Rss*` lines of `/proc/<pid>/status`, in KiB.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Rss {
    anon: Option<u64>,
    file: Option<u64>,
    shmem: Option<u64>,
}

/// Parse the `Rss*` lines; missing lines stay `None`.
fn parse_rss(text: &str) -> Rss {
    let mut rss = Rss::default();
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let value = rest
            .trim()
            .trim_end_matches("kB")
            .trim()
            .parse::<u64>()
            .ok();
        match key {
            "RssAnon" => rss.anon = value,
            "RssFile" => rss.file = value,
            "RssShmem" => rss.shmem = value,
            _ => {}
        }
    }
    rss
}

fn status_rss(root: &Root, pid: &str) -> io::Result<Rss> {
    Ok(parse_rss(&std::fs::read_to_string(
        root.path(format!("/proc/{pid}/status")),
    )?))
}

/// Resident page cache across files, from `cachestat(2)`.
pub fn cached_bytes(files: &[&File]) -> io::Result<u64> {
    let page = page_size();
    files.iter().try_fold(
        0u64,
        |acc, f| Ok(acc + cachestat(*f, 0, 0)?.nr_cache * page),
    )
}

/// Read a scope's memory. For the process itself, `cache_files` are
/// the files whose residency counts as its page cache.
pub fn memory(scope: &Scope, root: &Root, cache_files: &[&File]) -> io::Result<Memory> {
    let kib = |v: Option<u64>| v.map(|k| k * 1024);
    match scope {
        Scope::SelfProcess => {
            let st = status_rss(root, "self")?;
            let anon = kib(st.anon);
            let shmem = kib(st.shmem);
            let pagecache = cached_bytes(cache_files)?;
            Ok(Memory {
                current: anon.zip(shmem).map(|(a, s)| a + s + pagecache),
                anon,
                pagecache: Some(pagecache),
                shmem,
                method: Method::Proc,
                pagecache_method: Method::Cachestat,
            })
        }
        Scope::Process(pid) => {
            let st = status_rss(root, &pid.to_string())?;
            let (anon, file, shmem) = (kib(st.anon), kib(st.file), kib(st.shmem));
            Ok(Memory {
                current: anon.zip(file).zip(shmem).map(|((a, f), s)| a + f + s),
                anon,
                pagecache: file,
                shmem,
                method: Method::Proc,
                pagecache_method: Method::Proc,
            })
        }
        Scope::Cgroup(cg) => {
            let stat = cg.memory_stat()?;
            let get = |k: &str| stat.get(k).copied();
            let shmem = get("shmem");
            Ok(Memory {
                current: Some(cg.memory_current()?),
                anon: get("anon"),
                pagecache: get("file").zip(shmem).map(|(f, s)| f.saturating_sub(s)),
                shmem,
                method: Method::Cgroup,
                pagecache_method: Method::Cgroup,
            })
        }
        Scope::Host => {
            let path = root.path("/proc/meminfo");
            let mi = procfs::Meminfo::from_buf_read(BufReader::new(File::open(&path)?)).map_err(
                |e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{}: {e}", path.display()),
                    )
                },
            )?;
            Ok(Memory {
                current: Some(mi.mem_total.saturating_sub(mi.mem_free)),
                anon: mi.anon_pages,
                pagecache: mi.shmem.map(|s| mi.cached.saturating_sub(s)),
                shmem: mi.shmem,
                method: Method::Proc,
                pagecache_method: Method::Proc,
            })
        }
    }
}

/// Limits of a scope, when it has any.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Limits {
    /// CPU quota as cores.
    pub cpu_cores: Option<f64>,
    /// Memory limit, bytes.
    pub memory_bytes: Option<u64>,
}

/// Read a scope's limits. Only cgroups have them; a missing file means
/// the controller is not enabled, which is no limit.
pub fn limits(scope: &Scope) -> Limits {
    match scope {
        Scope::Cgroup(cg) => Limits {
            cpu_cores: cg.cpu_max().ok().and_then(|m| m.cores()),
            memory_bytes: cg.memory_max().ok().flatten(),
        },
        _ => Limits::default(),
    }
}

/// Open a cgroup directory for `perf_event_open` with `PERF_FLAG_PID_CGROUP`.
pub fn open_cgroup_dir(path: &Path) -> io::Result<File> {
    File::open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const MOUNT: &str = "/sys/fs/cgroup";
    const SESSION: &str = "user.slice/user-1000.slice/session-9.scope";

    fn fixture() -> Root {
        Root::at(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/root"))
    }

    fn session(root: &Root) -> Scope {
        Scope::Cgroup(Cgroup::new(root, Path::new(MOUNT), SESSION))
    }

    #[test]
    fn scope_names_and_kinds() {
        let root = fixture();
        assert_eq!(Scope::SelfProcess.name(), "self");
        assert_eq!(Scope::Host.name(), "host");
        assert_eq!(Scope::Process(7).name(), "pid 7");
        assert_eq!(session(&root).name(), SESSION);
        assert_eq!(
            Scope::Cgroup(Cgroup::new(&root, Path::new(MOUNT), "/")).name(),
            "/"
        );
        assert_eq!(Scope::SelfProcess.kind(), profile::Scope::Process);
        assert_eq!(Scope::Process(1).kind(), profile::Scope::Process);
        assert_eq!(session(&root).kind(), profile::Scope::Cgroup);
        assert_eq!(Scope::Host.kind(), profile::Scope::Host);
    }

    #[test]
    fn busy_time_per_scope_from_fixtures() {
        let root = fixture();
        let ticks = clock_ticks();
        let cg = busy_time(&session(&root), &root).unwrap();
        assert_eq!(cg.total, Duration::from_micros(728_955_499));
        assert_eq!(cg.method, Method::Cgroup);

        let p = busy_time(&Scope::Process(4242), &root).unwrap();
        assert!(
            (p.total.as_secs_f64() - 1801.0 / ticks).abs() < 1e-9,
            "{p:?}"
        );
        assert_eq!(p.method, Method::Proc);

        let h = busy_time(&Scope::Host, &root).unwrap();
        let expect =
            (17_061_365 + 12_145 + 6_165_105 + 265_740 + 1_248_759 + 248_499) as f64 / ticks;
        assert!((h.total.as_secs_f64() - expect).abs() < 1e-6, "{h:?}");

        assert!(
            busy_time(&Scope::Process(1), &root).is_err(),
            "no such fixture pid"
        );
        assert!(host_busy_from_stat("intr 1 2 3\n").is_err());
        assert!(host_busy_from_stat("cpu 1 2\n").is_err());
        assert_eq!(
            host_busy_from_stat("cpu 10 0 20 70 0 0 0 0\n").unwrap(),
            Duration::from_secs_f64(30.0 / ticks)
        );
    }

    #[test]
    fn busy_time_of_self_advances_with_work() {
        let root = Root::default();
        let a = busy_time(&Scope::SelfProcess, &root).unwrap();
        assert_eq!(a.method, Method::Rusage);
        let mut x = 0u64;
        for i in 0..20_000_000u64 {
            x = std::hint::black_box(x.wrapping_add(i * i));
        }
        let b = busy_time(&Scope::SelfProcess, &root).unwrap();
        assert!(b.total > a.total, "{a:?} -> {b:?}");
        let me = std::process::id() as i32;
        let via_proc = busy_time(&Scope::Process(me), &root).unwrap();
        assert!(
            via_proc.total.as_secs_f64() >= b.total.as_secs_f64() - 0.05,
            "{via_proc:?} vs {b:?}"
        );
    }

    #[test]
    fn memory_per_scope_from_fixtures() {
        let root = fixture();
        let cg = memory(&session(&root), &root, &[]).unwrap();
        assert_eq!(cg.current, Some(3_449_266_176));
        assert_eq!(cg.anon, Some(933_859_328));
        assert_eq!(cg.pagecache, Some(2_248_900_608 - 561_152));
        assert_eq!(cg.shmem, Some(561_152));
        assert_eq!(
            (cg.method, cg.pagecache_method),
            (Method::Cgroup, Method::Cgroup)
        );

        let p = memory(&Scope::Process(4242), &root, &[]).unwrap();
        assert_eq!(p.anon, Some(452 * 1024));
        assert_eq!(p.pagecache, Some(3612 * 1024));
        assert_eq!(p.shmem, Some(0));
        assert_eq!(p.current, Some((452 + 3612) * 1024));
        assert_eq!((p.method, p.pagecache_method), (Method::Proc, Method::Proc));

        let h = memory(&Scope::Host, &root, &[]).unwrap();
        assert_eq!(h.current, Some((131_204_216 - 39_081_088) * 1024));
        assert_eq!(h.anon, Some(3_617_428 * 1024));
        assert_eq!(h.pagecache, Some((84_631_032 - 128_916) * 1024));
        assert_eq!(h.shmem, Some(128_916 * 1024));

        assert!(memory(&Scope::Process(1), &root, &[]).is_err());
    }

    #[test]
    fn memory_of_self_counts_our_cache_files() {
        let root = Root::default();
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(&vec![7u8; 8 * 4096]).unwrap();
        f.flush().unwrap();
        let m = memory(&Scope::SelfProcess, &root, &[&f]).unwrap();
        assert_eq!(
            (m.method, m.pagecache_method),
            (Method::Proc, Method::Cachestat)
        );
        assert!(m.anon.unwrap() > 0);
        match cachestat(&f, 0, 0) {
            Ok(_) => assert_eq!(m.pagecache, Some(8 * page_size())),
            Err(_) => unreachable!("memory() would have failed too"),
        }
        assert_eq!(
            m.current,
            m.anon
                .zip(m.shmem)
                .map(|(a, s)| a + s + m.pagecache.unwrap())
        );
        let none = memory(&Scope::SelfProcess, &root, &[]).unwrap();
        assert_eq!(none.pagecache, Some(0));
    }

    #[test]
    fn limits_only_for_cgroups() {
        let root = fixture();
        assert_eq!(
            limits(&session(&root)),
            Limits {
                cpu_cores: None,
                memory_bytes: None
            }
        );
        assert_eq!(limits(&Scope::Host), Limits::default());
        assert_eq!(limits(&Scope::SelfProcess), Limits::default());
        let dir = tempfile::tempdir().unwrap();
        let r = Root::at(dir.path());
        let cg = Cgroup::new(&r, Path::new(MOUNT), "limited");
        std::fs::create_dir_all(cg.dir()).unwrap();
        std::fs::write(cg.dir().join("cpu.max"), "150000 100000\n").unwrap();
        std::fs::write(cg.dir().join("memory.max"), "2147483648\n").unwrap();
        assert_eq!(
            limits(&Scope::Cgroup(cg)),
            Limits {
                cpu_cores: Some(1.5),
                memory_bytes: Some(2 << 30)
            }
        );
    }

    #[test]
    fn rss_parsing() {
        assert_eq!(
            parse_rss("Name:\tx\nRssAnon:\t  452 kB\nRssFile:\t 3612 kB\nRssShmem:\t 0 kB\n"),
            Rss {
                anon: Some(452),
                file: Some(3612),
                shmem: Some(0)
            }
        );
        assert_eq!(parse_rss("RssAnon: lots kB\n"), Rss::default());
        assert_eq!(parse_rss(""), Rss::default());
    }

    #[test]
    fn constants_are_sane() {
        assert!(clock_ticks() >= 1.0);
        assert!(page_size() >= 4096);
        assert!(open_cgroup_dir(Path::new("/")).is_ok());
    }
}
