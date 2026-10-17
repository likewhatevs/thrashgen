//! DRAM bandwidth through resctrl monitoring groups. A group counts
//! the bytes its tasks move through memory, per L3 domain; the kernel
//! keeps the totals, and tasks forked by a member inherit membership.

use std::io;
use std::path::{Path, PathBuf};

use nix::unistd::AccessFlags;
use resctrlfs::{ResctrlReader, RmidBytes};

use crate::caps::{Facts, Root};
use crate::sampler::BandwidthSource;

/// A mounted resctrl filesystem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resctrl {
    mount: PathBuf,
}

/// A monitoring group: the root group, or one made under `mon_groups`
/// that is removed when dropped.
#[derive(Debug)]
pub struct MonGroup {
    dir: PathBuf,
    owned: bool,
}

impl Resctrl {
    /// The filesystem mounted at `mount`, resolved under `root`.
    pub fn new(root: &Root, mount: &Path) -> Resctrl {
        Resctrl {
            mount: root.path(mount),
        }
    }

    /// From detected facts, if resctrl is mounted with bandwidth monitoring.
    pub fn from_facts(root: &Root, facts: &Facts) -> Option<Resctrl> {
        let r = facts.resctrl.as_ref()?;
        r.mon_features
            .iter()
            .any(|m| m == "mbm_total_bytes")
            .then(|| Resctrl::new(root, &r.mount))
    }

    /// The root group, which counts every task not in another group.
    pub fn root_group(&self) -> MonGroup {
        MonGroup {
            dir: self.mount.clone(),
            owned: false,
        }
    }

    /// Make a monitoring group under the root control group.
    pub fn create_mon_group(&self, name: &str) -> io::Result<MonGroup> {
        let dir = self.mount.join("mon_groups").join(name);
        std::fs::create_dir(&dir)?;
        Ok(MonGroup { dir, owned: true })
    }

    /// An existing group under `mon_groups` whose `tasks` file this
    /// process can write and that holds no tasks: what root makes and
    /// chowns for a tool that cannot make groups itself. The first by
    /// name; not removed on drop.
    pub fn find_idle_group(&self) -> io::Result<Option<MonGroup>> {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(self.mount.join("mon_groups"))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            let tasks = dir.join("tasks");
            if nix::unistd::access(&tasks, AccessFlags::W_OK).is_err() {
                continue;
            }
            if std::fs::read_to_string(&tasks).is_ok_and(|t| t.trim().is_empty()) {
                return Ok(Some(MonGroup { dir, owned: false }));
            }
        }
        Ok(None)
    }

    /// A group to put tasks in: one made under `name`, or failing that
    /// an idle group someone made for us.
    pub fn acquire_group(&self, name: &str) -> io::Result<MonGroup> {
        let err = match self.create_mon_group(name) {
            Ok(g) => return Ok(g),
            Err(e) => e,
        };
        match self.find_idle_group() {
            Ok(Some(g)) => Ok(g),
            Ok(None) => Err(io::Error::new(
                err.kind(),
                format!("{err}; no idle writable group under mon_groups either"),
            )),
            Err(e) => Err(io::Error::new(err.kind(), format!("{err}; {e}"))),
        }
    }
}

impl MonGroup {
    /// The group directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether this process made the group and removes it on drop.
    pub fn is_owned(&self) -> bool {
        self.owned
    }

    /// Add tasks by thread id. Tasks that no longer exist are skipped;
    /// the count added is returned.
    pub fn add_tasks(&self, tids: &[i32]) -> io::Result<usize> {
        let mut added = 0;
        for tid in tids {
            match std::fs::write(self.dir.join("tasks"), format!("{tid}\n")) {
                Ok(()) => added += 1,
                Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(added)
    }

    /// Add the calling thread; threads it creates afterwards inherit.
    pub fn add_self(&self) -> io::Result<()> {
        self.add_tasks(&[nix::unistd::gettid().as_raw()])
            .map(|_| ())
    }

    /// `mbm_total_bytes` summed over L3 domains, or `None` while any
    /// domain reports `Unavailable`, which the kernel does until the
    /// first hardware sample after a group is made or reused. Counted
    /// as zero, that reading made a baseline against which the next
    /// one charged the group's whole history to one interval.
    pub fn total_bytes(&self) -> io::Result<Option<u64>> {
        let sample = ResctrlReader::new(self.dir.clone(), false)
            .and_then(|r| r.read_all())
            .map_err(|e| io::Error::other(format!("{}: {e}", self.dir.display())))?;
        let stats = sample
            .mon_stat
            .and_then(|m| m.l3_mon_stat)
            .unwrap_or_default();
        if stats.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("{}: no L3 monitoring domains", self.dir.display()),
            ));
        }
        stats
            .values()
            .map(|s| match s.mbm_total_bytes {
                Some(RmidBytes::Bytes(b)) => Some(b),
                _ => None,
            })
            .try_fold(0u64, |acc, b| b.map(|b| acc + b))
            .map_or(Ok(None), |sum| Ok(Some(sum)))
    }
}

impl Drop for MonGroup {
    fn drop(&mut self) {
        if self.owned {
            let _ = std::fs::remove_dir(&self.dir);
        }
    }
}

impl BandwidthSource for MonGroup {
    fn read_bytes(&mut self) -> io::Result<Option<u64>> {
        self.total_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::ResctrlFacts;
    use std::fs;

    /// A resctrl tree the way the kernel lays it out, minus the kernel.
    fn fake_mount(dir: &Path, domains: &[(&str, &str)]) {
        fs::create_dir_all(dir).unwrap();
        for f in ["cpus_list", "tasks"] {
            fs::write(dir.join(f), "").unwrap();
        }
        fs::write(dir.join("mode"), "shareable\n").unwrap();
        fs::create_dir_all(dir.join("mon_groups")).unwrap();
        for (id, bytes) in domains {
            let d = dir.join("mon_data").join(format!("mon_L3_{id}"));
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("mbm_total_bytes"), format!("{bytes}\n")).unwrap();
            fs::write(d.join("mbm_local_bytes"), "0\n").unwrap();
            fs::write(d.join("llc_occupancy"), "0\n").unwrap();
        }
    }

    #[test]
    fn root_group_sums_domains_and_skips_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Root::at(tmp.path());
        let mount = Path::new("/sys/fs/resctrl");
        fake_mount(
            &root.path(mount),
            &[("00", "1000"), ("01", "Unavailable"), ("02", "24")],
        );
        let rc = Resctrl::new(&root, mount);
        let g = rc.root_group();
        assert_eq!(
            g.total_bytes().unwrap(),
            None,
            "one domain unavailable: no reading yet"
        );
        assert!(g.dir().ends_with("resctrl"));
        drop(g);
        assert!(root.path(mount).is_dir(), "the root group is never removed");

        let tmp = tempfile::tempdir().unwrap();
        let root = Root::at(tmp.path());
        fake_mount(&root.path(mount), &[("00", "Unavailable")]);
        assert_eq!(
            Resctrl::new(&root, mount)
                .root_group()
                .total_bytes()
                .unwrap(),
            None,
            "unavailable is no reading, not zero"
        );
        let tmp = tempfile::tempdir().unwrap();
        let root = Root::at(tmp.path());
        fake_mount(&root.path(mount), &[("00", "1000"), ("01", "24")]);
        assert_eq!(
            Resctrl::new(&root, mount)
                .root_group()
                .total_bytes()
                .unwrap(),
            Some(1024)
        );
        let tmp = tempfile::tempdir().unwrap();
        let root = Root::at(tmp.path());
        fake_mount(&root.path(mount), &[]);
        let err = Resctrl::new(&root, mount)
            .root_group()
            .total_bytes()
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported, "no domains at all");
    }

    #[test]
    fn mon_group_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Root::at(tmp.path());
        let mount = Path::new("/sys/fs/resctrl");
        fake_mount(&root.path(mount), &[("00", "0")]);
        let rc = Resctrl::new(&root, mount);
        let g = rc.create_mon_group("thrashgen-1").unwrap();
        let dir = g.dir().to_path_buf();
        assert!(dir.is_dir());
        // The kernel would populate the group; the test does it instead.
        fake_mount(&dir, &[("00", "5"), ("01", "7")]);
        assert_eq!(g.add_tasks(&[11, 22]).unwrap(), 2);
        assert_eq!(
            fs::read_to_string(dir.join("tasks")).unwrap(),
            "22\n",
            "each write replaces the fake file; the kernel appends"
        );
        g.add_self().unwrap();
        assert_eq!(g.total_bytes().unwrap(), Some(12));
        assert!(
            rc.create_mon_group("thrashgen-1").is_err(),
            "already exists"
        );
        let mut bw: Box<dyn BandwidthSource> = Box::new(g);
        assert_eq!(bw.read_bytes().unwrap(), Some(12));
        // The kernel empties a group directory itself; the fake has to.
        for e in fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                fs::remove_dir_all(p).unwrap()
            } else {
                fs::remove_file(p).unwrap()
            }
        }
        drop(bw);
        assert!(!dir.exists(), "owned groups are removed on drop");
        assert!(rc.create_mon_group("a/b").is_err());
    }

    #[test]
    fn idle_groups_are_found_and_used_when_none_can_be_made() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = Root::at(tmp.path());
        let mount = Path::new("/sys/fs/resctrl");
        fake_mount(&root.path(mount), &[("00", "1")]);
        let rc = Resctrl::new(&root, mount);
        assert!(rc.find_idle_group().unwrap().is_none(), "no groups yet");
        let groups = root.path(mount).join("mon_groups");
        for (name, tasks) in [
            (
                "busy", "123
",
            ),
            ("ours", ""),
            ("theirs", ""),
        ] {
            fake_mount(&groups.join(name), &[("00", "2")]);
            fs::write(groups.join(name).join("tasks"), tasks).unwrap();
        }
        let chmod =
            |p: &Path, m: u32| fs::set_permissions(p, fs::Permissions::from_mode(m)).unwrap();
        chmod(&groups.join("theirs/tasks"), 0o444);
        let g = rc.find_idle_group().unwrap().unwrap();
        assert!(g.dir().ends_with("ours"), "{}", g.dir().display());
        assert!(!g.is_owned());
        let dir = g.dir().to_path_buf();
        drop(g);
        assert!(dir.is_dir(), "a found group is not removed");

        assert!(
            rc.acquire_group("made").unwrap().is_owned(),
            "made when possible"
        );
        chmod(&groups, 0o555);
        let g = rc.acquire_group("made-2").unwrap();
        assert!(!g.is_owned() && g.dir().ends_with("ours"));
        fs::write(
            groups.join("ours/tasks"),
            "5
",
        )
        .unwrap();
        let err = rc.acquire_group("made-3").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("no idle writable group"), "{err}");
        chmod(&groups, 0o755);
        chmod(&groups.join("theirs/tasks"), 0o644);
        let tmp = tempfile::tempdir().unwrap();
        let rc = Resctrl::new(&Root::at(tmp.path()), mount);
        assert!(rc.find_idle_group().is_err(), "no mon_groups directory");
        assert!(
            rc.acquire_group("x")
                .unwrap_err()
                .to_string()
                .contains("No such file")
        );
    }

    #[test]
    fn from_facts_needs_bandwidth_monitoring() {
        let root = Root::default();
        let mut facts = Facts::detect(&Root::at(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/root"
        )));
        assert_eq!(Resctrl::from_facts(&root, &facts), None);
        facts.resctrl = Some(ResctrlFacts {
            mount: "/sys/fs/resctrl".into(),
            mon_features: vec!["llc_occupancy".into()],
            can_make_groups: true,
            usable_groups: Vec::new(),
        });
        assert_eq!(Resctrl::from_facts(&root, &facts), None);
        facts
            .resctrl
            .as_mut()
            .unwrap()
            .mon_features
            .push("mbm_total_bytes".into());
        assert_eq!(
            Resctrl::from_facts(&root, &facts),
            Some(Resctrl {
                mount: "/sys/fs/resctrl".into()
            })
        );
    }

    #[test]
    fn missing_tree_is_an_error_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let rc = Resctrl::new(&Root::at(tmp.path()), Path::new("/sys/fs/resctrl"));
        assert!(rc.root_group().total_bytes().is_err());
        assert!(rc.create_mon_group("x").is_err(), "no mon_groups directory");
        let g = MonGroup {
            dir: tmp.path().join("nowhere"),
            owned: true,
        };
        assert!(g.add_tasks(&[1]).is_err());
    }
}

#[cfg(all(test, feature = "root-tests"))]
mod root_tests {
    use super::*;

    #[test]
    fn a_group_counts_our_memory_traffic() {
        let root = Root::default();
        let facts = Facts::detect(&root);
        let rc = Resctrl::from_facts(&root, &facts).expect(
            "resctrl mounted with mbm_total_bytes; mount -t resctrl resctrl /sys/fs/resctrl",
        );
        let g = rc
            .create_mon_group(&format!("thrashgen-test-{}", std::process::id()))
            .unwrap();
        g.add_self().unwrap();
        let mut before = g.total_bytes().unwrap();
        for _ in 0..50 {
            if before.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            before = g.total_bytes().unwrap();
        }
        let before = before.expect("a reading within a second of the group's making");
        std::thread::sleep(std::time::Duration::from_millis(100));
        let mut buf = vec![0u8; 512 << 20];
        for round in 0..3u8 {
            for (i, b) in buf.iter_mut().enumerate().step_by(64) {
                *b = (i as u8).wrapping_add(round);
            }
        }
        std::hint::black_box(&buf);
        let after = g.total_bytes().unwrap().expect("a reading");
        assert!(after - before > 256 << 20, "{before} -> {after}");
    }
}
