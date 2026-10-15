//! CPU topology from sysfs: which CPUs share a core, a package and an
//! L3, and the filter values AMD's L3 PMU needs to attribute misses to
//! one CPU.

use std::path::Path;

use thiserror::Error;

use crate::events::{Sysfs, parse_cpulist};

/// One logical CPU.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cpu {
    /// CPU number.
    pub id: u32,
    /// `topology/core_id`: on AMD, the physical core slot derived from
    /// the APIC id, so it can have gaps on parts with disabled cores.
    pub core_id: u32,
    /// `topology/physical_package_id`.
    pub package: u32,
    /// All CPUs on the same core, including this one, ascending.
    pub siblings: Vec<u32>,
    /// All CPUs sharing this CPU's L3, ascending; `None` without an L3.
    pub l3: Option<Vec<u32>>,
}

/// The online CPUs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Topology {
    /// Online CPUs, ascending.
    pub cpus: Vec<Cpu>,
}

/// Errors reading topology.
#[derive(Debug, Error)]
pub enum TopologyError {
    /// A sysfs file was missing or unreadable.
    #[error("{0}: {1}")]
    Read(String, std::io::Error),
    /// A sysfs file did not parse.
    #[error("{0}: cannot parse `{1}`")]
    Parse(String, String),
}

/// Filter values for one CPU on AMD's L3 PMU, family 19h and later:
/// `coreid` is the core's slot within its L3 complex, `threadmask` the
/// bit of its SMT thread. Validated on Zen 4 by the root tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AmdL3Filter {
    /// Value for the `coreid` field.
    pub coreid: u8,
    /// Value for the `threadmask` field.
    pub threadmask: u8,
    /// A CPU in the same L3 domain to open the uncore event on.
    pub domain_cpu: u32,
}

/// Cores per L3 complex on Zen 3, 4 and 5; `coreid` is `core_id`
/// modulo this.
const AMD_CORES_PER_CCX: u32 = 8;

fn read_file(path: &Path) -> Result<String, TopologyError> {
    std::fs::read_to_string(path).map_err(|e| TopologyError::Read(path.display().to_string(), e))
}

fn read_u32(path: &Path) -> Result<u32, TopologyError> {
    let text = read_file(path)?;
    text.trim()
        .parse()
        .map_err(|_| TopologyError::Parse(path.display().to_string(), text.trim().into()))
}

fn read_cpulist(path: &Path) -> Result<Vec<u32>, TopologyError> {
    let text = read_file(path)?;
    let mut list = parse_cpulist(&text)
        .ok_or_else(|| TopologyError::Parse(path.display().to_string(), text.trim().into()))?;
    list.sort_unstable();
    Ok(list)
}

impl Topology {
    /// Read the online CPUs.
    pub fn read(sysfs: &Sysfs) -> Result<Topology, TopologyError> {
        let base = sysfs.path("/sys/devices/system/cpu");
        let online = read_cpulist(&base.join("online"))?;
        let mut cpus = Vec::with_capacity(online.len());
        for id in online {
            let dir = base.join(format!("cpu{id}"));
            let l3 = l3_dir(&dir)?
                .map(|d| read_cpulist(&d.join("shared_cpu_list")))
                .transpose()?;
            cpus.push(Cpu {
                id,
                core_id: read_u32(&dir.join("topology/core_id"))?,
                package: read_u32(&dir.join("topology/physical_package_id"))?,
                siblings: read_cpulist(&dir.join("topology/thread_siblings_list"))?,
                l3,
            });
        }
        Ok(Topology { cpus })
    }

    /// Look a CPU up.
    pub fn cpu(&self, id: u32) -> Option<&Cpu> {
        self.cpus.iter().find(|c| c.id == id)
    }

    /// Distinct L3 domains, each the ascending list of online CPUs
    /// sharing one L3, in order of their first CPU.
    pub fn l3_domains(&self) -> Vec<Vec<u32>> {
        let mut out: Vec<Vec<u32>> = Vec::new();
        for l3 in self.cpus.iter().filter_map(|c| c.l3.as_ref()) {
            let online: Vec<u32> = l3
                .iter()
                .copied()
                .filter(|id| self.cpu(*id).is_some())
                .collect();
            if !out.contains(&online) {
                out.push(online);
            }
        }
        out.sort_by_key(|d| d.first().copied());
        out
    }

    /// The L3 domain containing a CPU.
    pub fn l3_domain_of(&self, id: u32) -> Option<&[u32]> {
        self.cpu(id)?.l3.as_deref()
    }

    /// AMD L3 PMU filter for one CPU.
    pub fn amd_l3_filter(&self, id: u32) -> Option<AmdL3Filter> {
        let cpu = self.cpu(id)?;
        let domain = cpu.l3.as_ref()?;
        let thread = cpu.siblings.iter().position(|&s| s == id)?;
        Some(AmdL3Filter {
            coreid: (cpu.core_id % AMD_CORES_PER_CCX) as u8,
            threadmask: 1u8 << thread,
            domain_cpu: *domain.first()?,
        })
    }
}

/// The `cache/indexN` directory with level 3, if any.
fn l3_dir(cpu_dir: &Path) -> Result<Option<std::path::PathBuf>, TopologyError> {
    let cache = cpu_dir.join("cache");
    let Ok(entries) = std::fs::read_dir(&cache) else {
        return Ok(None);
    };
    let mut dirs: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    dirs.sort();
    for d in dirs {
        let level = d.join("level");
        if level.is_file() && read_u32(&level)? == 3 {
            return Ok(Some(d));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Sysfs {
        Sysfs::at(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/root"))
    }

    #[test]
    fn reads_the_host_fixture() {
        let t = Topology::read(&fixture()).unwrap();
        assert_eq!(
            t.cpus.iter().map(|c| c.id).collect::<Vec<_>>(),
            [0, 1, 8, 32, 33]
        );
        let c33 = t.cpu(33).unwrap();
        assert_eq!(c33.core_id, 1);
        assert_eq!(c33.siblings, [1, 33]);
        assert_eq!(c33.package, 0);
        assert_eq!(c33.l3.as_deref().unwrap().len(), 16);
        assert_eq!(
            t.l3_domains(),
            [vec![0, 1, 32, 33], vec![8]],
            "online members only"
        );
        assert_eq!(t.l3_domain_of(32).unwrap()[..3], [0, 1, 2]);
        assert_eq!(t.l3_domain_of(99), None);
        assert_eq!(t.cpu(2), None);
    }

    #[test]
    fn amd_filters() {
        let t = Topology::read(&fixture()).unwrap();
        assert_eq!(
            t.amd_l3_filter(0),
            Some(AmdL3Filter {
                coreid: 0,
                threadmask: 0b01,
                domain_cpu: 0
            })
        );
        assert_eq!(
            t.amd_l3_filter(33),
            Some(AmdL3Filter {
                coreid: 1,
                threadmask: 0b10,
                domain_cpu: 0
            })
        );
        assert_eq!(
            t.amd_l3_filter(8),
            Some(AmdL3Filter {
                coreid: 0,
                threadmask: 0b01,
                domain_cpu: 8
            })
        );
        assert_eq!(t.amd_l3_filter(7), None);
    }

    #[test]
    fn missing_and_malformed_files() {
        let dir = tempfile::tempdir().unwrap();
        let sysfs = Sysfs::at(dir.path());
        assert!(matches!(
            Topology::read(&sysfs),
            Err(TopologyError::Read(..))
        ));
        let base = dir.path().join("sys/devices/system/cpu");
        std::fs::create_dir_all(base.join("cpu0/topology")).unwrap();
        std::fs::write(base.join("online"), "0\n").unwrap();
        std::fs::write(base.join("cpu0/topology/core_id"), "zero\n").unwrap();
        assert!(matches!(
            Topology::read(&sysfs),
            Err(TopologyError::Parse(..))
        ));
        std::fs::write(base.join("cpu0/topology/core_id"), "0\n").unwrap();
        std::fs::write(base.join("cpu0/topology/physical_package_id"), "0\n").unwrap();
        std::fs::write(base.join("cpu0/topology/thread_siblings_list"), "0\n").unwrap();
        let t = Topology::read(&sysfs).unwrap();
        assert_eq!(t.cpus[0].l3, None, "no cache directory means no L3");
        assert!(t.l3_domains().is_empty());
        assert_eq!(t.amd_l3_filter(0), None);
        std::fs::create_dir_all(base.join("cpu0/cache/index0")).unwrap();
        std::fs::write(base.join("cpu0/cache/index0/level"), "1\n").unwrap();
        assert_eq!(
            Topology::read(&sysfs).unwrap().cpus[0].l3,
            None,
            "only an L1"
        );
        std::fs::write(base.join("online"), "0-x\n").unwrap();
        assert!(matches!(
            Topology::read(&sysfs),
            Err(TopologyError::Parse(..))
        ));
    }

    #[test]
    fn live_host_reads() {
        let t = Topology::read(&Sysfs::default()).unwrap();
        assert!(!t.cpus.is_empty());
        assert!(t.cpus.iter().all(|c| c.siblings.contains(&c.id)));
    }
}
