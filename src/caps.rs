//! What this process is allowed to measure, and why.
//!
//! [`Facts`] are read from `/proc` and `/sys`; [`Expectations`] apply
//! the kernel's rules from `docs/privileges.md` to them. Live probes
//! that actually open counters live in the measurement modules and
//! refine these expectations; this module never fails, it only reports.
//! Capabilities granted inside a user namespace show up here but do not
//! satisfy the kernel's checks, which is one reason the live probes
//! have the final word.

use std::fmt;
use std::path::{Path, PathBuf};

use nix::unistd::{AccessFlags, access};

use crate::sys::cachestat;

const CAP_SYS_PTRACE: u32 = 19;
const CAP_SYS_ADMIN: u32 = 21;
const CAP_SYS_NICE: u32 = 23;
const CAP_PERFMON: u32 = 38;

/// Where `/proc` and `/sys` live. Tests point this at a fixture tree.
#[derive(Clone, Debug)]
pub struct Root(PathBuf);

impl Default for Root {
    fn default() -> Self {
        Root(PathBuf::from("/"))
    }
}

impl Root {
    /// A root somewhere other than `/`.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Root(path.into())
    }

    /// Whether this is the live system.
    pub fn is_live(&self) -> bool {
        self.0 == Path::new("/")
    }

    /// Resolve an absolute path under this root.
    pub fn path(&self, abs: impl AsRef<Path>) -> PathBuf {
        let rel = abs.as_ref().strip_prefix("/").unwrap_or(abs.as_ref());
        self.0.join(rel)
    }

    fn read(&self, abs: &str) -> Option<String> {
        std::fs::read_to_string(self.path(abs)).ok()
    }
}

/// The process's own cgroup and whether its files can be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CgroupFacts {
    /// Mount point of the cgroup v2 hierarchy.
    pub mount: PathBuf,
    /// This process's cgroup, relative to the mount, without a leading slash.
    pub path: String,
    /// Controllers enabled for it.
    pub controllers: Vec<String>,
    /// `cpu.stat` is readable.
    pub cpu_stat: bool,
    /// `memory.stat` is readable.
    pub memory_stat: bool,
}

/// The resctrl filesystem, when mounted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResctrlFacts {
    /// Mount point.
    pub mount: PathBuf,
    /// Events listed in `info/L3_MON/mon_features`.
    pub mon_features: Vec<String>,
    /// `mon_groups` under the root group is writable, so groups can be made.
    pub can_make_groups: bool,
    /// Groups under `mon_groups` whose `tasks` file is writable, so this
    /// process can put tasks into them without making a group.
    pub usable_groups: Vec<String>,
}

/// Facts about the process and the host, with no interpretation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Facts {
    /// Effective uid.
    pub euid: Option<u32>,
    /// `CAP_PERFMON` is effective.
    pub cap_perfmon: bool,
    /// `CAP_SYS_ADMIN` is effective.
    pub cap_sys_admin: bool,
    /// `CAP_SYS_PTRACE` is effective.
    pub cap_sys_ptrace: bool,
    /// `CAP_SYS_NICE` is effective.
    pub cap_sys_nice: bool,
    /// `kernel.perf_event_paranoid`, `None` when perf is absent.
    pub perf_event_paranoid: Option<i64>,
    /// `kernel.yama.ptrace_scope`, `None` without Yama.
    pub yama_ptrace_scope: Option<u32>,
    /// Transparent hugepage mode: `always`, `madvise` or `never`.
    pub thp: Option<String>,
    /// Own cgroup.
    pub cgroup: Option<CgroupFacts>,
    /// resctrl, when mounted.
    pub resctrl: Option<ResctrlFacts>,
    /// PMUs under `/sys/bus/event_source/devices`.
    pub pmus: Vec<String>,
    /// `cachestat(2)` works; `None` when not probed.
    pub cachestat: Option<bool>,
}

fn parse_status(text: &str) -> (Option<u32>, u64) {
    let mut euid = None;
    let mut cap_eff = 0;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            euid = rest.split_whitespace().nth(1).and_then(|s| s.parse().ok());
        } else if let Some(rest) = line.strip_prefix("CapEff:") {
            cap_eff = u64::from_str_radix(rest.trim(), 16).unwrap_or(0);
        }
    }
    (euid, cap_eff)
}

fn has_cap(mask: u64, bit: u32) -> bool {
    mask & (1u64 << bit) != 0
}

/// Mount point of the first mount with this filesystem type.
fn mount_point(mounts: &str, fstype: &str) -> Option<PathBuf> {
    mounts
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let _dev = it.next()?;
            let point = it.next()?;
            let ty = it.next()?;
            (ty == fstype).then(|| PathBuf::from(point))
        })
        .next()
}

/// Own cgroup v2 path from `/proc/self/cgroup`, without the leading slash.
fn own_cgroup(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| p.trim().trim_start_matches('/').to_string())
}

fn thp_mode(text: &str) -> Option<String> {
    let start = text.find('[')?;
    let end = text[start..].find(']')? + start;
    Some(text[start + 1..end].to_string())
}

impl Facts {
    /// Read the facts under a root. On the live root, `cachestat` is
    /// probed; elsewhere it stays `None`.
    pub fn detect(root: &Root) -> Facts {
        let (euid, caps) = root
            .read("/proc/self/status")
            .map(|t| parse_status(&t))
            .unwrap_or((None, 0));
        let mounts = root.read("/proc/self/mounts").unwrap_or_default();

        let cgroup = mount_point(&mounts, "cgroup2").and_then(|mount| {
            let path = own_cgroup(&root.read("/proc/self/cgroup")?)?;
            let dir = root.path(&mount).join(&path);
            let readable = |f: &str| std::fs::read_to_string(dir.join(f)).is_ok();
            let controllers = std::fs::read_to_string(dir.join("cgroup.controllers"))
                .map(|t| t.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default();
            Some(CgroupFacts {
                mount,
                path,
                controllers,
                cpu_stat: readable("cpu.stat"),
                memory_stat: readable("memory.stat"),
            })
        });

        let resctrl = mount_point(&mounts, "resctrl").map(|mount| {
            let dir = root.path(&mount);
            let mon_features = std::fs::read_to_string(dir.join("info/L3_MON/mon_features"))
                .map(|t| {
                    t.lines()
                        .map(|l| l.trim().to_string())
                        .filter(|l| !l.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let can_make_groups = access(&dir.join("mon_groups"), AccessFlags::W_OK).is_ok();
            let mut usable_groups: Vec<String> = std::fs::read_dir(dir.join("mon_groups"))
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter(|e| access(&e.path().join("tasks"), AccessFlags::W_OK).is_ok())
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            usable_groups.sort();
            ResctrlFacts {
                mount,
                mon_features,
                can_make_groups,
                usable_groups,
            }
        });

        let mut pmus: Vec<String> = std::fs::read_dir(root.path("/sys/bus/event_source/devices"))
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        pmus.sort();

        let cachestat_ok = root.is_live().then(|| {
            std::fs::File::open("/proc/self/exe")
                .map(|f| cachestat(&f, 0, 0).is_ok())
                .unwrap_or(false)
        });

        Facts {
            euid,
            cap_perfmon: has_cap(caps, CAP_PERFMON),
            cap_sys_admin: has_cap(caps, CAP_SYS_ADMIN),
            cap_sys_ptrace: has_cap(caps, CAP_SYS_PTRACE),
            cap_sys_nice: has_cap(caps, CAP_SYS_NICE),
            perf_event_paranoid: root
                .read("/proc/sys/kernel/perf_event_paranoid")
                .and_then(|t| t.trim().parse().ok()),
            yama_ptrace_scope: root
                .read("/proc/sys/kernel/yama/ptrace_scope")
                .and_then(|t| t.trim().parse().ok()),
            thp: root
                .read("/sys/kernel/mm/transparent_hugepage/enabled")
                .and_then(|t| thp_mode(&t)),
            cgroup,
            resctrl,
            pmus,
            cachestat: cachestat_ok,
        }
    }

    /// `perfmon_capable()` as the kernel defines it.
    pub fn perfmon_capable(&self) -> bool {
        self.cap_perfmon || self.cap_sys_admin
    }
}

/// One permission, with the reason it is granted or not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    /// Whether the kernel's rules allow it.
    pub ok: bool,
    /// Which fact decided it.
    pub why: String,
}

impl Rule {
    fn new(ok: bool, why: impl Into<String>) -> Rule {
        Rule {
            ok,
            why: why.into(),
        }
    }
}

/// What the facts predict, before any counter is opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expectations {
    /// Counters on our own threads, user mode.
    pub self_counters_user: Rule,
    /// Counters on our own threads including kernel mode.
    pub self_counters_kernel: Rule,
    /// Per-CPU events, hence per-cgroup and uncore events.
    pub per_cpu_counters: Rule,
    /// Counters on another process's threads, same uid.
    pub attach_same_uid: Rule,
    /// Counters on any process.
    pub attach_any: Rule,
    /// Reading resctrl monitoring data.
    pub resctrl_read: Rule,
    /// Making a resctrl monitoring group for ourselves.
    pub resctrl_group: Rule,
    /// Reading our own cgroup's statistics.
    pub cgroup_stats: Rule,
    /// Page-cache residency through `cachestat(2)`.
    pub cachestat: Rule,
}

impl Expectations {
    /// Apply the kernel's rules.
    pub fn from_facts(f: &Facts) -> Expectations {
        let capable = f.perfmon_capable();
        let cap_why = if f.cap_perfmon {
            "CAP_PERFMON"
        } else {
            "CAP_SYS_ADMIN"
        };
        let paranoid = |limit: i64, what: &str| -> Rule {
            match f.perf_event_paranoid {
                None => Rule::new(false, "perf_event_paranoid is absent: no perf support"),
                Some(_) if capable => Rule::new(true, cap_why),
                Some(p) if p <= limit => {
                    Rule::new(true, format!("perf_event_paranoid {p} allows {what}"))
                }
                Some(p) => Rule::new(
                    false,
                    format!("perf_event_paranoid {p} forbids {what} without CAP_PERFMON"),
                ),
            }
        };
        let attach_same_uid = if f.perf_event_paranoid.is_none() {
            Rule::new(false, "no perf support")
        } else if capable {
            Rule::new(true, cap_why)
        } else {
            match f.yama_ptrace_scope {
                None | Some(0) => Rule::new(true, "ptrace access to same-uid processes"),
                Some(1) if f.cap_sys_ptrace => Rule::new(true, "CAP_SYS_PTRACE"),
                Some(1) => Rule::new(false, "yama ptrace_scope 1: descendants only"),
                Some(s) => Rule::new(false, format!("yama ptrace_scope {s}")),
            }
        };
        let attach_any = if f.perf_event_paranoid.is_none() {
            Rule::new(false, "no perf support")
        } else if capable {
            Rule::new(true, cap_why)
        } else {
            Rule::new(false, "needs CAP_PERFMON")
        };
        let resctrl_read = match &f.resctrl {
            None => Rule::new(false, "resctrl is not mounted"),
            Some(r) if r.mon_features.iter().any(|m| m == "mbm_total_bytes") => Rule::new(
                true,
                format!("mounted at {} with mbm_total_bytes", r.mount.display()),
            ),
            Some(r) => Rule::new(
                false,
                format!(
                    "mounted at {} but without mbm_total_bytes",
                    r.mount.display()
                ),
            ),
        };
        let resctrl_group = match &f.resctrl {
            None => Rule::new(false, "resctrl is not mounted"),
            Some(_) if !resctrl_read.ok => Rule::new(false, resctrl_read.why.clone()),
            Some(r) if r.can_make_groups => Rule::new(true, "mon_groups is writable"),
            Some(r) if !r.usable_groups.is_empty() => Rule::new(
                true,
                format!(
                    "writable groups under mon_groups: {}",
                    r.usable_groups.join(" ")
                ),
            ),
            Some(_) => Rule::new(
                false,
                "neither mon_groups nor any group under it is writable; needs root or a chowned group",
            ),
        };
        let cgroup_stats = match &f.cgroup {
            None => Rule::new(false, "no cgroup v2 mount or no own cgroup"),
            Some(c) if c.cpu_stat && c.memory_stat => Rule::new(true, format!("cgroup {}", c.path)),
            Some(c) => Rule::new(false, format!("cgroup {} files are not readable", c.path)),
        };
        let cachestat_rule = match f.cachestat {
            Some(true) => Rule::new(true, "cachestat(2) works"),
            Some(false) => Rule::new(false, "cachestat(2) failed; kernel before 6.5"),
            None => Rule::new(false, "not probed"),
        };
        Expectations {
            self_counters_user: paranoid(2, "self-monitoring"),
            self_counters_kernel: paranoid(1, "kernel-mode counting"),
            per_cpu_counters: paranoid(0, "per-CPU events"),
            attach_same_uid,
            attach_any,
            resctrl_read,
            resctrl_group,
            cgroup_stats,
            cachestat: cachestat_rule,
        }
    }

    /// Rows for a report, in display order.
    pub fn rows(&self) -> [(&'static str, &Rule); 9] {
        [
            ("own threads, user mode", &self.self_counters_user),
            ("own threads, kernel mode", &self.self_counters_kernel),
            ("per-CPU, per-cgroup, uncore", &self.per_cpu_counters),
            ("attach to same-uid process", &self.attach_same_uid),
            ("attach to any process", &self.attach_any),
            ("resctrl bandwidth read", &self.resctrl_read),
            ("resctrl monitoring group", &self.resctrl_group),
            ("own cgroup statistics", &self.cgroup_stats),
            ("page cache residency", &self.cachestat),
        ]
    }
}

impl fmt::Display for Facts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let opt = |v: Option<&dyn fmt::Display>| {
            v.map(|v| v.to_string()).unwrap_or_else(|| "absent".into())
        };
        writeln!(
            f,
            "euid                 {}",
            opt(self.euid.as_ref().map(|v| v as &dyn fmt::Display))
        )?;
        let mut caps = Vec::new();
        for (on, name) in [
            (self.cap_perfmon, "PERFMON"),
            (self.cap_sys_admin, "SYS_ADMIN"),
            (self.cap_sys_ptrace, "SYS_PTRACE"),
            (self.cap_sys_nice, "SYS_NICE"),
        ] {
            if on {
                caps.push(name);
            }
        }
        writeln!(
            f,
            "capabilities         {}",
            if caps.is_empty() {
                "none".to_string()
            } else {
                caps.join(" ")
            }
        )?;
        writeln!(
            f,
            "perf_event_paranoid  {}",
            opt(self
                .perf_event_paranoid
                .as_ref()
                .map(|v| v as &dyn fmt::Display))
        )?;
        writeln!(
            f,
            "yama ptrace_scope    {}",
            opt(self
                .yama_ptrace_scope
                .as_ref()
                .map(|v| v as &dyn fmt::Display))
        )?;
        writeln!(
            f,
            "transparent hugepage {}",
            self.thp.as_deref().unwrap_or("absent")
        )?;
        match &self.cgroup {
            Some(c) => writeln!(
                f,
                "cgroup               {} [{}]",
                if c.path.is_empty() { "/" } else { &c.path },
                c.controllers.join(" ")
            )?,
            None => writeln!(f, "cgroup               none")?,
        }
        match &self.resctrl {
            Some(r) => writeln!(
                f,
                "resctrl              {} [{}]",
                r.mount.display(),
                r.mon_features.join(" ")
            )?,
            None => writeln!(f, "resctrl              not mounted")?,
        }
        writeln!(f, "pmus                 {}", self.pmus.join(" "))
    }
}

impl fmt::Display for Expectations {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (what, rule) in self.rows() {
            writeln!(
                f,
                "{:30} {:3}  {}",
                what,
                if rule.ok { "yes" } else { "no" },
                rule.why
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> Root {
        Root::at(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/root"))
    }

    /// A writable copy of the fixture, for variants.
    fn scratch() -> (tempfile::TempDir, Root) {
        let dir = tempfile::tempdir().unwrap();
        copy_tree(&fixture().0, dir.path());
        let root = Root::at(dir.path());
        (dir, root)
    }

    fn copy_tree(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for e in fs::read_dir(from).unwrap() {
            let e = e.unwrap();
            let dest = to.join(e.file_name());
            if e.file_type().unwrap().is_dir() {
                copy_tree(&e.path(), &dest);
            } else {
                fs::copy(e.path(), dest).unwrap();
            }
        }
    }

    fn write(root: &Root, abs: &str, text: &str) {
        let p = root.path(abs);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    #[test]
    fn host_fixture_is_an_unprivileged_user_at_paranoid_2() {
        let f = Facts::detect(&fixture());
        assert_eq!(f.euid, Some(1000));
        assert!(!f.cap_perfmon && !f.cap_sys_admin && !f.cap_sys_ptrace && !f.cap_sys_nice);
        assert_eq!(f.perf_event_paranoid, Some(2));
        assert_eq!(f.yama_ptrace_scope, Some(0));
        assert_eq!(f.thp.as_deref(), Some("always"));
        let c = f.cgroup.as_ref().unwrap();
        assert_eq!(c.mount, Path::new("/sys/fs/cgroup"));
        assert_eq!(c.path, "user.slice/user-1000.slice/session-9.scope");
        assert_eq!(c.controllers, ["cpu", "io", "memory", "pids"]);
        assert!(c.cpu_stat && c.memory_stat);
        assert_eq!(f.resctrl, None);
        assert_eq!(f.pmus, ["amd_l3", "cpu"]);
        assert_eq!(f.cachestat, None, "fixtures are not probed");

        let e = Expectations::from_facts(&f);
        assert!(e.self_counters_user.ok);
        assert!(!e.self_counters_kernel.ok);
        assert!(!e.per_cpu_counters.ok);
        assert!(e.attach_same_uid.ok, "{}", e.attach_same_uid.why);
        assert!(!e.attach_any.ok);
        assert!(!e.resctrl_read.ok && !e.resctrl_group.ok);
        assert!(e.cgroup_stats.ok);
        assert!(!e.cachestat.ok);
        assert_eq!(e.rows().len(), 9);
        let text = format!("{f}{e}");
        assert!(text.contains("perf_event_paranoid  2"));
        assert!(text.contains("own threads, user mode         yes"));
        assert!(text.contains("per-CPU, per-cgroup, uncore    no "));
    }

    #[test]
    fn cap_perfmon_unlocks_everything_perf() {
        let (_d, root) = scratch();
        write(
            &root,
            "/proc/self/status",
            &format!(
                "Uid:\t1000\t1000\t1000\t1000\nCapEff:\t{:016x}\n",
                1u64 << CAP_PERFMON
            ),
        );
        let f = Facts::detect(&root);
        assert!(f.cap_perfmon && f.perfmon_capable());
        let e = Expectations::from_facts(&f);
        assert!(
            e.self_counters_kernel.ok
                && e.per_cpu_counters.ok
                && e.attach_any.ok
                && e.attach_same_uid.ok
        );
        assert_eq!(e.per_cpu_counters.why, "CAP_PERFMON");

        write(
            &root,
            "/proc/self/status",
            &format!(
                "Uid:\t0\t0\t0\t0\nCapEff:\t{:016x}\n",
                1u64 << CAP_SYS_ADMIN
            ),
        );
        let e = Expectations::from_facts(&Facts::detect(&root));
        assert!(e.per_cpu_counters.ok);
        assert_eq!(e.per_cpu_counters.why, "CAP_SYS_ADMIN");
    }

    #[test]
    fn paranoid_levels() {
        let (_d, root) = scratch();
        for (level, user, kernel, cpu) in [
            (-1, true, true, true),
            (0, true, true, true),
            (1, true, true, false),
            (2, true, false, false),
            (3, false, false, false),
        ] {
            write(
                &root,
                "/proc/sys/kernel/perf_event_paranoid",
                &format!("{level}\n"),
            );
            let e = Expectations::from_facts(&Facts::detect(&root));
            assert_eq!(
                (
                    e.self_counters_user.ok,
                    e.self_counters_kernel.ok,
                    e.per_cpu_counters.ok
                ),
                (user, kernel, cpu),
                "level {level}"
            );
        }
        fs::remove_file(root.path("/proc/sys/kernel/perf_event_paranoid")).unwrap();
        let e = Expectations::from_facts(&Facts::detect(&root));
        assert!(!e.self_counters_user.ok && !e.attach_same_uid.ok && !e.attach_any.ok);
        assert!(e.self_counters_user.why.contains("absent"));
    }

    #[test]
    fn yama_scopes_gate_same_uid_attach() {
        let (_d, root) = scratch();
        for (scope, ok) in [(0, true), (1, false), (2, false), (3, false)] {
            write(
                &root,
                "/proc/sys/kernel/yama/ptrace_scope",
                &format!("{scope}\n"),
            );
            let e = Expectations::from_facts(&Facts::detect(&root));
            assert_eq!(e.attach_same_uid.ok, ok, "scope {scope}");
        }
        write(
            &root,
            "/proc/self/status",
            &format!(
                "Uid:\t1000\t1000\t1000\t1000\nCapEff:\t{:016x}\n",
                1u64 << CAP_SYS_PTRACE
            ),
        );
        write(&root, "/proc/sys/kernel/yama/ptrace_scope", "1\n");
        let f = Facts::detect(&root);
        assert!(f.cap_sys_ptrace);
        assert!(Expectations::from_facts(&f).attach_same_uid.ok);
        fs::remove_file(root.path("/proc/sys/kernel/yama/ptrace_scope")).unwrap();
        let f = Facts::detect(&root);
        assert_eq!(f.yama_ptrace_scope, None);
        assert!(
            Expectations::from_facts(&f).attach_same_uid.ok,
            "no Yama means classic ptrace rules"
        );
    }

    #[test]
    fn resctrl_mounted_with_and_without_bandwidth() {
        let (_d, root) = scratch();
        let mounts = fs::read_to_string(root.path("/proc/self/mounts")).unwrap()
            + "resctrl /sys/fs/resctrl resctrl rw,relatime 0 0\n";
        write(&root, "/proc/self/mounts", &mounts);
        write(
            &root,
            "/sys/fs/resctrl/info/L3_MON/mon_features",
            "llc_occupancy\nmbm_total_bytes\nmbm_local_bytes\n",
        );
        fs::create_dir_all(root.path("/sys/fs/resctrl/mon_groups")).unwrap();
        let f = Facts::detect(&root);
        let r = f.resctrl.as_ref().unwrap();
        assert_eq!(r.mount, Path::new("/sys/fs/resctrl"));
        assert_eq!(
            r.mon_features,
            ["llc_occupancy", "mbm_total_bytes", "mbm_local_bytes"]
        );
        assert!(r.can_make_groups, "the scratch copy is ours");
        let e = Expectations::from_facts(&f);
        assert!(e.resctrl_read.ok && e.resctrl_group.ok);
        assert!(format!("{f}").contains(
            "resctrl              /sys/fs/resctrl [llc_occupancy mbm_total_bytes mbm_local_bytes]"
        ));

        write(
            &root,
            "/sys/fs/resctrl/info/L3_MON/mon_features",
            "llc_occupancy\n",
        );
        let e = Expectations::from_facts(&Facts::detect(&root));
        assert!(!e.resctrl_read.ok && !e.resctrl_group.ok);
        assert!(e.resctrl_read.why.contains("without mbm_total_bytes"));

        write(
            &root,
            "/sys/fs/resctrl/info/L3_MON/mon_features",
            "mbm_total_bytes\n",
        );
        fs::remove_dir(root.path("/sys/fs/resctrl/mon_groups")).unwrap();
        let e = Expectations::from_facts(&Facts::detect(&root));
        assert!(e.resctrl_read.ok && !e.resctrl_group.ok);

        // A group someone made and chowned to us, under a mon_groups we
        // cannot write: usable, and the rule says which.
        let groups = root.path("/sys/fs/resctrl/mon_groups");
        fs::create_dir_all(groups.join("theirs")).unwrap();
        fs::create_dir_all(groups.join("ours")).unwrap();
        fs::write(groups.join("ours/tasks"), "").unwrap();
        fs::write(groups.join("theirs/tasks"), "").unwrap();
        let ro = |p: &Path, mode: u32| {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap()
        };
        ro(&groups.join("theirs/tasks"), 0o444);
        ro(&groups, 0o555);
        let f = Facts::detect(&root);
        let r = f.resctrl.as_ref().unwrap();
        assert!(!r.can_make_groups);
        assert_eq!(r.usable_groups, ["ours"]);
        let e = Expectations::from_facts(&f);
        assert!(e.resctrl_group.ok, "{}", e.resctrl_group.why);
        assert!(e.resctrl_group.why.contains("ours"));
        ro(&groups, 0o755);
        ro(&groups.join("theirs/tasks"), 0o644);
    }

    #[test]
    fn cgroup_absent_or_unreadable() {
        let (_d, root) = scratch();
        fs::remove_file(
            root.path("/sys/fs/cgroup/user.slice/user-1000.slice/session-9.scope/memory.stat"),
        )
        .unwrap();
        let f = Facts::detect(&root);
        let c = f.cgroup.as_ref().unwrap();
        assert!(c.cpu_stat && !c.memory_stat);
        assert!(!Expectations::from_facts(&f).cgroup_stats.ok);

        write(&root, "/proc/self/cgroup", "0::/\n");
        let f = Facts::detect(&root);
        assert_eq!(f.cgroup.as_ref().unwrap().path, "");
        assert!(format!("{f}").contains("cgroup               / ["));

        write(&root, "/proc/self/mounts", "proc /proc proc rw 0 0\n");
        let f = Facts::detect(&root);
        assert_eq!(f.cgroup, None);
        assert!(!Expectations::from_facts(&f).cgroup_stats.ok);
    }

    #[test]
    fn missing_everything_is_reported_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let f = Facts::detect(&Root::at(dir.path()));
        assert_eq!(f.euid, None);
        assert_eq!(f.perf_event_paranoid, None);
        assert_eq!(f.thp, None);
        assert!(f.pmus.is_empty());
        let e = Expectations::from_facts(&f);
        assert!(e.rows().iter().all(|(_, r)| !r.ok));
        let text = format!("{f}{e}");
        assert!(text.contains("euid                 absent"));
        assert!(text.contains("capabilities         none"));
        assert!(text.contains("transparent hugepage absent"));
        assert!(text.contains("cgroup               none"));
    }

    #[test]
    fn parsers() {
        assert_eq!(
            parse_status("Uid:\t5\t7\t5\t5\nCapEff:\t0000000000200000\n"),
            (Some(7), 1 << 21)
        );
        assert_eq!(parse_status("CapEff:\tzz\n"), (None, 0));
        assert_eq!(
            mount_point("a /x cgroup2 rw 0 0\nb /y resctrl rw 0 0\n", "resctrl"),
            Some(PathBuf::from("/y"))
        );
        assert_eq!(mount_point("short line\n", "resctrl"), None);
        assert_eq!(
            own_cgroup("1:name=systemd:/x\n0::/a/b\n"),
            Some("a/b".into())
        );
        assert_eq!(own_cgroup("1:cpu:/x\n"), None);
        assert_eq!(thp_mode("always [madvise] never\n"), Some("madvise".into()));
        assert_eq!(thp_mode("always madvise never\n"), None);
        assert_eq!(
            Root::default().path("/proc/self/status"),
            PathBuf::from("/proc/self/status")
        );
        assert_eq!(
            Root::at("/tmp/x").path("/proc/self/status"),
            PathBuf::from("/tmp/x/proc/self/status")
        );
        assert!(Root::default().is_live());
    }

    #[test]
    fn live_detection_runs() {
        let f = Facts::detect(&Root::default());
        assert!(f.euid.is_some());
        assert!(f.cachestat.is_some());
        let _ = Expectations::from_facts(&f);
    }
}
