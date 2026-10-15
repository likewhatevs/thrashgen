//! Reading one cgroup v2 directory. Paths are resolved under a
//! [`Root`] so fixture trees can stand in for `/sys/fs/cgroup`.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use crate::caps::Root;
use crate::events::parse_cpulist;

/// `cpu.stat`, microseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuStat {
    /// Total CPU time consumed.
    pub usage_usec: u64,
    /// User-mode part.
    pub user_usec: u64,
    /// Kernel-mode part.
    pub system_usec: u64,
    /// Periods the quota throttled the group.
    pub nr_throttled: u64,
    /// Time spent throttled.
    pub throttled_usec: u64,
}

/// `cpu.max`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuMax {
    /// Quota per period, `None` for `max`.
    pub quota_usec: Option<u64>,
    /// Period.
    pub period_usec: u64,
}

impl CpuMax {
    /// The quota as a number of cores.
    pub fn cores(&self) -> Option<f64> {
        self.quota_usec.map(|q| q as f64 / self.period_usec as f64)
    }
}

/// A cgroup v2 directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cgroup {
    dir: PathBuf,
    rel: String,
}

fn invalid(what: &str, text: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{what}: cannot parse `{}`", text.trim()),
    )
}

/// Parse `key value` lines into a map; lines that do not fit are skipped.
pub fn parse_kv(text: &str) -> BTreeMap<String, u64> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let k = it.next()?;
            let v = it.next()?.parse().ok()?;
            Some((k.to_string(), v))
        })
        .collect()
}

impl Cgroup {
    /// The cgroup at `rel` under the hierarchy mounted at `mount`,
    /// resolved under `root`.
    pub fn new(root: &Root, mount: &Path, rel: &str) -> Cgroup {
        let rel = rel.trim_matches('/').to_string();
        Cgroup {
            dir: root.path(mount).join(&rel),
            rel,
        }
    }

    /// The cgroup directory itself.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path relative to the hierarchy root, without slashes at the ends.
    pub fn rel(&self) -> &str {
        &self.rel
    }

    /// Whether the directory exists.
    pub fn exists(&self) -> bool {
        self.dir.is_dir()
    }

    fn read(&self, file: &str) -> io::Result<String> {
        std::fs::read_to_string(self.dir.join(file))
    }

    /// `cpu.stat`.
    pub fn cpu_stat(&self) -> io::Result<CpuStat> {
        let kv = parse_kv(&self.read("cpu.stat")?);
        let get = |k: &str| kv.get(k).copied().unwrap_or(0);
        if !kv.contains_key("usage_usec") {
            return Err(invalid("cpu.stat", "no usage_usec"));
        }
        Ok(CpuStat {
            usage_usec: get("usage_usec"),
            user_usec: get("user_usec"),
            system_usec: get("system_usec"),
            nr_throttled: get("nr_throttled"),
            throttled_usec: get("throttled_usec"),
        })
    }

    /// `memory.current`, bytes.
    pub fn memory_current(&self) -> io::Result<u64> {
        let text = self.read("memory.current")?;
        text.trim()
            .parse()
            .map_err(|_| invalid("memory.current", &text))
    }

    /// `memory.stat` as a map, bytes.
    pub fn memory_stat(&self) -> io::Result<BTreeMap<String, u64>> {
        Ok(parse_kv(&self.read("memory.stat")?))
    }

    /// `cpu.max`.
    pub fn cpu_max(&self) -> io::Result<CpuMax> {
        let text = self.read("cpu.max")?;
        let mut it = text.split_whitespace();
        let quota = it.next().ok_or_else(|| invalid("cpu.max", &text))?;
        let period: u64 = it
            .next()
            .and_then(|p| p.parse().ok())
            .ok_or_else(|| invalid("cpu.max", &text))?;
        let quota_usec = if quota == "max" {
            None
        } else {
            Some(quota.parse().map_err(|_| invalid("cpu.max", &text))?)
        };
        if period == 0 {
            return Err(invalid("cpu.max", &text));
        }
        Ok(CpuMax {
            quota_usec,
            period_usec: period,
        })
    }

    /// `memory.max`, `None` for `max`.
    pub fn memory_max(&self) -> io::Result<Option<u64>> {
        let text = self.read("memory.max")?;
        let t = text.trim();
        if t == "max" {
            Ok(None)
        } else {
            t.parse()
                .map(Some)
                .map_err(|_| invalid("memory.max", &text))
        }
    }

    fn id_list(&self, file: &str) -> io::Result<Vec<u32>> {
        let text = self.read(file)?;
        text.split_whitespace()
            .map(|t| t.parse().map_err(|_| invalid(file, &text)))
            .collect()
    }

    /// `cgroup.threads`: every thread id in the cgroup.
    pub fn threads(&self) -> io::Result<Vec<u32>> {
        self.id_list("cgroup.threads")
    }

    /// `cgroup.procs`: every process id in the cgroup.
    pub fn procs(&self) -> io::Result<Vec<u32>> {
        self.id_list("cgroup.procs")
    }

    /// `cgroup.controllers`.
    pub fn controllers(&self) -> io::Result<Vec<String>> {
        Ok(self
            .read("cgroup.controllers")?
            .split_whitespace()
            .map(str::to_string)
            .collect())
    }

    /// `cpuset.cpus.effective`, `None` when the cpuset controller is
    /// not enabled here.
    pub fn cpuset_effective(&self) -> io::Result<Option<Vec<u32>>> {
        match self.read("cpuset.cpus.effective") {
            Ok(text) => parse_cpulist(&text)
                .map(Some)
                .ok_or_else(|| invalid("cpuset.cpus.effective", &text)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNT: &str = "/sys/fs/cgroup";
    const SESSION: &str = "user.slice/user-1000.slice/session-9.scope";

    fn fixture() -> Root {
        Root::at(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/root"))
    }

    #[test]
    fn reads_the_session_fixture() {
        let cg = Cgroup::new(&fixture(), Path::new(MOUNT), &format!("/{SESSION}/"));
        assert_eq!(cg.rel(), SESSION);
        assert!(cg.exists());
        assert!(cg.dir().ends_with(SESSION));
        let cpu = cg.cpu_stat().unwrap();
        assert_eq!(cpu.usage_usec, 728_955_499);
        assert_eq!(cpu.user_usec, 635_309_550);
        assert_eq!(cpu.system_usec, 93_645_948);
        assert_eq!((cpu.nr_throttled, cpu.throttled_usec), (0, 0));
        assert_eq!(cg.memory_current().unwrap(), 3_449_266_176);
        let m = cg.memory_stat().unwrap();
        assert_eq!(m["anon"], 933_859_328);
        assert_eq!(m["file"], 2_248_900_608);
        assert_eq!(m["shmem"], 561_152);
        assert_eq!(
            cg.cpu_max().unwrap(),
            CpuMax {
                quota_usec: None,
                period_usec: 100_000
            }
        );
        assert_eq!(cg.cpu_max().unwrap().cores(), None);
        assert_eq!(cg.memory_max().unwrap(), None);
        assert_eq!(cg.threads().unwrap(), [4242, 4243, 4300]);
        assert_eq!(cg.procs().unwrap(), [4242, 4300]);
        assert_eq!(cg.controllers().unwrap(), ["cpu", "io", "memory", "pids"]);
        assert_eq!(cg.cpuset_effective().unwrap().unwrap().len(), 64);
    }

    #[test]
    fn limits_and_malformed_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::at(dir.path());
        let cg = Cgroup::new(&root, Path::new(MOUNT), "x");
        assert!(!cg.exists());
        assert!(cg.cpu_stat().is_err());
        assert_eq!(cg.cpuset_effective().unwrap(), None, "no cpuset controller");
        std::fs::create_dir_all(cg.dir()).unwrap();
        let w = |f: &str, t: &str| std::fs::write(cg.dir().join(f), t).unwrap();
        w("cpu.max", "200000 100000\n");
        assert_eq!(cg.cpu_max().unwrap().cores(), Some(2.0));
        w("cpu.max", "50000 100000\n");
        assert_eq!(cg.cpu_max().unwrap().cores(), Some(0.5));
        w("memory.max", "1073741824\n");
        assert_eq!(cg.memory_max().unwrap(), Some(1 << 30));
        for bad in ["max\n", "100000\n", "x 100000\n", "100000 0\n"] {
            w("cpu.max", bad);
            assert!(cg.cpu_max().is_err(), "{bad:?}");
        }
        w("memory.max", "lots\n");
        assert!(cg.memory_max().is_err());
        w("memory.current", "-1\n");
        assert!(cg.memory_current().is_err());
        w("cpu.stat", "user_usec 5\n");
        assert!(cg.cpu_stat().is_err(), "usage_usec is required");
        w("cpu.stat", "usage_usec 5\nnonsense\n");
        assert_eq!(cg.cpu_stat().unwrap().usage_usec, 5);
        w("cgroup.threads", "1\ntwo\n");
        assert!(cg.threads().is_err());
        w("cgroup.threads", "");
        assert!(cg.threads().unwrap().is_empty());
        w("cpuset.cpus.effective", "3-1\n");
        assert!(cg.cpuset_effective().is_err());
        assert_eq!(
            parse_kv("a 1\nb x\nc\n d 4 extra\n"),
            BTreeMap::from([("a".into(), 1), ("d".into(), 4)])
        );
    }
}
