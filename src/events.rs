//! Vendor event definitions and their encoding into `perf_event_attr`
//! config words: `docs/design.md` section 2.1.
//!
//! Every event is a PMU name plus named fields, the way `perf stat -e
//! pmu/field=value,.../` spells it. Encoding needs the PMU's format,
//! which the kernel exposes under `/sys/bus/event_source/devices/<pmu>/
//! format/` as bit ranges. The core PMU's format is also built in,
//! verified against the kernel source, so core events encode without
//! sysfs; uncore PMUs such as `amd_l3` need sysfs for their type id
//! anyway.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::profile::CpuIdent;

/// `PERF_TYPE_RAW`: the core PMU, on every architecture.
pub const PERF_TYPE_RAW: u32 = 4;

/// Name of the core PMU in event definitions.
pub const CORE: &str = "cpu";

/// CPU vendor, as far as event tables are concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vendor {
    /// AMD Zen.
    Amd,
    /// Intel Core-derived server parts.
    Intel,
    /// Arm Ltd. cores with `PMUv3`.
    Arm,
    /// Apple silicon, implementer 0x61: its own PMU, one per class of
    /// core, driven by `drivers/perf/apple_m1_cpu_pmu.c`.
    Apple,
}

impl Vendor {
    /// Vendor of an identified CPU, if it is one the tables cover. The
    /// Arm implementer, which `/proc/cpuinfo` reports as the family,
    /// tells Apple from Arm Ltd.
    pub fn of(cpu: &CpuIdent) -> Option<Vendor> {
        match (cpu.vendor.as_str(), cpu.family) {
            ("amd", _) => Some(Vendor::Amd),
            ("intel", _) => Some(Vendor::Intel),
            ("arm", 0x61) => Some(Vendor::Apple),
            ("arm", _) => Some(Vendor::Arm),
            _ => None,
        }
    }

    /// The core PMU's format, from the kernel driver at v7.1:
    /// `arch/x86/events/amd/core.c`, `arch/x86/events/intel/core.c`,
    /// `drivers/perf/arm_pmuv3.c`, `drivers/perf/apple_m1_cpu_pmu.c`.
    pub fn core_format(self) -> Format {
        let text = match self {
            Vendor::Amd => "event=config:0-7,32-35\numask=config:8-15",
            Vendor::Intel => "event=config:0-7\numask=config:8-15",
            Vendor::Arm => "event=config:0-15",
            Vendor::Apple => "event=config:0-7",
        };
        Format::parse(text).expect("built-in format parses")
    }
}

/// One event: a PMU and the fields to set on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventDef {
    /// The kernel's name for it, for reports.
    pub name: &'static str,
    /// PMU directory name, or [`CORE`].
    pub pmu: &'static str,
    /// Field values, in format-file names.
    pub fields: Vec<(&'static str, u64)>,
}

impl EventDef {
    const fn core(name: &'static str, event: u64, umask: u64) -> Static {
        Static {
            name,
            pmu: CORE,
            event,
            umask: Some(umask),
        }
    }

    /// A core event on a PMU whose format has only `event`.
    const fn core_event(name: &'static str, event: u64) -> Static {
        Static {
            name,
            pmu: CORE,
            event,
            umask: None,
        }
    }

    /// A copy with extra fields appended, for uncore filters.
    #[must_use]
    pub fn with_fields(&self, extra: &[(&'static str, u64)]) -> EventDef {
        let mut fields = self.fields.clone();
        fields.extend_from_slice(extra);
        EventDef {
            name: self.name,
            pmu: self.pmu,
            fields,
        }
    }

    /// A copy on another PMU: a core event on one class's PMU of a
    /// host with several classes of core.
    #[must_use]
    pub fn on_pmu(&self, pmu: &'static str) -> EventDef {
        EventDef {
            name: self.name,
            pmu,
            fields: self.fields.clone(),
        }
    }
}

impl fmt::Display for EventDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/", self.pmu)?;
        for (i, (k, v)) in self.fields.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            write!(f, "{k}=0x{v:x}")?;
        }
        f.write_str("/")
    }
}

/// Table entry: an event with `event` and `umask` fields.
#[derive(Clone, Copy, Debug)]
pub struct Static {
    name: &'static str,
    pmu: &'static str,
    event: u64,
    umask: Option<u64>,
}

impl Static {
    /// Expand into an [`EventDef`].
    pub fn def(&self) -> EventDef {
        let mut fields = vec![("event", self.event)];
        if let Some(umask) = self.umask {
            fields.push(("umask", umask));
        }
        EventDef {
            name: self.name,
            pmu: self.pmu,
            fields,
        }
    }
}

/// The events behind the profile's counter fields for one CPU model
/// group. Encodings come from the kernel's `tools/perf/pmu-events`
/// JSON at v7.1, or from the kernel driver where the kernel has no
/// table; `table` is the directory they were taken from, or the
/// driver's name, and is also the calibration key.
#[derive(Clone, Copy, Debug)]
pub struct EventTable {
    /// Kernel pmu-events directory, also the calibration key.
    pub table: &'static str,
    /// Vendor.
    pub vendor: Vendor,
    /// Classes of core with a PMU each, where the host has several;
    /// empty where the one core PMU is [`CORE`]. Each class opens the
    /// table's core events on its own PMU, and the L3 miss event is
    /// the class's.
    pub classes: &'static [CoreClass],
    /// The L3 miss definition. On AMD this is an uncore event that
    /// needs per-CPU access. `None` where no core PMU counts it, as
    /// with the classes below, which then say which do.
    pub l3_miss: Option<Static>,
    /// A per-task core event that tracks `l3_miss` for memory-bound
    /// load: data cache fills from DRAM. AMD only.
    pub l3_miss_proxy: Option<Static>,
    /// dTLB events, summed.
    pub dtlb: &'static [Static],
    /// iTLB event.
    pub itlb: Static,
    /// Bytes a miss fetches from memory, where that is not the line
    /// size sysfs reports: the generator's line.
    pub memory_line: Option<usize>,
    /// Whether the L3 miss event counts demand fetches only, none the
    /// prefetchers made, and a visit's further lines in a page are
    /// fetched by the region prefetcher: the generator then lays a
    /// run's lines out in pages of their own, and the fitter spares
    /// counted misses as it does under AMD's proxy.
    pub demand_misses_only: bool,
}

/// One class of core on a host with several, as `cpu_core` and
/// `cpu_atom` or Apple's P and E cores: its PMU, whose `cpus` file in
/// sysfs says which CPUs it covers, and what it counts that differs.
#[derive(Clone, Copy, Debug)]
pub struct CoreClass {
    /// PMU directory name.
    pub pmu: &'static str,
    /// The L3 miss event on this class, if it has one.
    pub l3_miss: Option<Static>,
}

const AMD_L3_ZEN4: Static = Static {
    name: "l3_lookup_state.l3_miss",
    pmu: "amd_l3",
    event: 0x04,
    umask: Some(0x01),
};
const AMD_L3_ZEN3: Static = Static {
    name: "l3_comb_clstr_state.request_miss",
    pmu: "amd_l3",
    event: 0x06,
    umask: Some(0x01),
};
/// `ls_any_fills_from_sys.dram_io_all` on Zen 4 and 5; on Zen 3 the
/// same bits are `mem_io_local` (0x08) plus `mem_io_remote` (0x40).
const AMD_DRAM_FILLS: Static = EventDef::core("ls_any_fills_from_sys.dram_io_all", 0x44, 0x48);
/// L1 dTLB misses that hit the L2 TLB, all page sizes.
const AMD_DTLB: &[Static] = &[EventDef::core(
    "ls_l1_d_tlb_miss.tlb_reload_l2_hit",
    0x45,
    0x0f,
)];
const AMD_ITLB: Static = EventDef::core("bp_l1_tlb_miss_l2_tlb_miss.all", 0x85, 0x0f);

const INTEL_LLC: Static = EventDef::core("LONGEST_LAT_CACHE.MISS", 0x2e, 0x41);
const INTEL_DTLB_SKX: &[Static] = &[
    EventDef::core("DTLB_LOAD_MISSES.WALK_COMPLETED", 0x08, 0x0e),
    EventDef::core("DTLB_STORE_MISSES.WALK_COMPLETED", 0x49, 0x0e),
];
const INTEL_ITLB_SKX: Static = EventDef::core("ITLB_MISSES.WALK_COMPLETED", 0x85, 0x0e);
const INTEL_DTLB_SPR: &[Static] = &[
    EventDef::core("DTLB_LOAD_MISSES.WALK_COMPLETED", 0x12, 0x0e),
    EventDef::core("DTLB_STORE_MISSES.WALK_COMPLETED", 0x13, 0x0e),
];
const INTEL_ITLB_SPR: Static = EventDef::core("ITLB_MISSES.WALK_COMPLETED", 0x11, 0x0e);

const ARM_LLC: Static = EventDef::core_event("LL_CACHE_MISS_RD", 0x37);
const ARM_DTLB: &[Static] = &[EventDef::core_event("DTLB_WALK", 0x34)];
const ARM_ITLB: Static = EventDef::core_event("ITLB_WALK", 0x35);

/// Apple's PMU driver lists its events by name but exposes only cycles
/// and instructions through sysfs; any 8-bit event number passes
/// through as raw. The TLB events are the driver's: the data one an
/// access that missed the last TLB level, the instruction one a
/// demand fetch that missed the first, since the instruction-side L2
/// miss and walk events count speculative fetches too and at a low
/// stub rate came in bursts of hundreds per call where this one held
/// at 1.2. The L3 miss event is in no list and was found by sweeping
/// every event number on an M2 Ultra (`docs/design.md` section 2.1):
/// on the P cores event 0x23 counted one per 128-byte line a demand
/// load or store fetched from memory, none for a set the system cache
/// holds, none for a prefetched sequential read, and the E cores have
/// no event that does the same.
const APPLE_DTLB: &[Static] = &[EventDef::core_event("L2_TLB_MISS_DATA", 0x0b)];
const APPLE_ITLB: Static = EventDef::core_event("L1I_TLB_MISS_DEMAND", 0xd4);
const APPLE_P_LLC: Static = EventDef::core_event("unlisted_0x23", 0x23);
const APPLE_CLASSES: &[CoreClass] = &[
    CoreClass {
        pmu: "apple_avalanche_pmu",
        l3_miss: Some(APPLE_P_LLC),
    },
    CoreClass {
        pmu: "apple_blizzard_pmu",
        l3_miss: None,
    },
];

/// Every table, in lookup order.
pub const TABLES: &[EventTable] = &[
    EventTable {
        table: "amdzen3",
        vendor: Vendor::Amd,
        classes: &[],
        l3_miss: Some(AMD_L3_ZEN3),
        l3_miss_proxy: Some(AMD_DRAM_FILLS),
        dtlb: AMD_DTLB,
        itlb: AMD_ITLB,
        memory_line: None,
        demand_misses_only: false,
    },
    EventTable {
        table: "amdzen4",
        vendor: Vendor::Amd,
        classes: &[],
        l3_miss: Some(AMD_L3_ZEN4),
        l3_miss_proxy: Some(AMD_DRAM_FILLS),
        dtlb: AMD_DTLB,
        itlb: AMD_ITLB,
        memory_line: None,
        demand_misses_only: false,
    },
    EventTable {
        table: "amdzen5",
        vendor: Vendor::Amd,
        classes: &[],
        l3_miss: Some(AMD_L3_ZEN4),
        l3_miss_proxy: Some(AMD_DRAM_FILLS),
        dtlb: AMD_DTLB,
        itlb: AMD_ITLB,
        memory_line: None,
        demand_misses_only: false,
    },
    EventTable {
        table: "skylakex",
        vendor: Vendor::Intel,
        classes: &[],
        l3_miss: Some(INTEL_LLC),
        l3_miss_proxy: None,
        dtlb: INTEL_DTLB_SKX,
        itlb: INTEL_ITLB_SKX,
        memory_line: None,
        demand_misses_only: false,
    },
    EventTable {
        table: "sapphirerapids",
        vendor: Vendor::Intel,
        classes: &[],
        l3_miss: Some(INTEL_LLC),
        l3_miss_proxy: None,
        dtlb: INTEL_DTLB_SPR,
        itlb: INTEL_ITLB_SPR,
        memory_line: None,
        demand_misses_only: false,
    },
    EventTable {
        table: "arm64",
        vendor: Vendor::Arm,
        classes: &[],
        l3_miss: Some(ARM_LLC),
        l3_miss_proxy: None,
        dtlb: ARM_DTLB,
        itlb: ARM_ITLB,
        memory_line: None,
        demand_misses_only: false,
    },
    EventTable {
        table: "applem2",
        vendor: Vendor::Apple,
        classes: APPLE_CLASSES,
        l3_miss: None,
        l3_miss_proxy: None,
        dtlb: APPLE_DTLB,
        itlb: APPLE_ITLB,
        memory_line: Some(128),
        demand_misses_only: true,
    },
];

/// Find a table by its name.
pub fn table_named(name: &str) -> Option<&'static EventTable> {
    TABLES.iter().find(|t| t.table == name)
}

/// The table for a CPU, following the kernel's `mapfile.csv` at v7.1.
/// Models the kernel has no table for, or that this crate has not
/// verified, return `None`; callers then run counters as unavailable.
/// Apple's M2 parts, Blizzard and Avalanche cores in the M2, M2 Pro,
/// Max and Ultra, are the ones the sweep behind the table ran on; the
/// M1's cores share the driver's event list but not that measurement.
pub fn table_for(cpu: &CpuIdent) -> Option<&'static EventTable> {
    let name = match (Vendor::of(cpu)?, cpu.family, cpu.model) {
        (Vendor::Amd, 25, 0x00..=0x0f | 0x20..=0x2f | 0x40..=0x5f) => "amdzen3",
        (Vendor::Amd, 25, _) => "amdzen4",
        (Vendor::Amd, 26, 0x00..=0x2f | 0x40..=0x4f | 0x60..=0x7f) => "amdzen5",
        (Vendor::Intel, 6, 0x55 | 0x6a | 0x6c | 0xaf) => "skylakex",
        (Vendor::Intel, 6, 0x8f | 0xcf | 0xad | 0xae) => "sapphirerapids",
        (Vendor::Arm, 0x41, _) => "arm64",
        (Vendor::Apple, 0x61, 0x032..=0x039) => "applem2",
        _ => return None,
    };
    table_named(name)
}

/// One contiguous run of bits in a config word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitRange {
    /// `config`, `config1` or `config2`.
    pub word: u8,
    /// Lowest bit.
    pub lo: u8,
    /// Highest bit, inclusive.
    pub hi: u8,
}

/// A PMU's format: field name to bit ranges, lowest value bits first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Format {
    fields: BTreeMap<String, Vec<BitRange>>,
}

/// Errors parsing a format or encoding an event against it.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum FormatError {
    /// A format line that is not `name=configN:lo-hi,...`.
    #[error("bad format line `{0}`")]
    BadLine(String),
    /// A field the PMU does not define.
    #[error("pmu has no field `{0}`")]
    NoField(String),
    /// A value wider than the field.
    #[error("value 0x{value:x} does not fit field `{field}` ({bits} bits)")]
    TooWide {
        /// Field name.
        field: String,
        /// Value given.
        value: u64,
        /// Width of the field.
        bits: u32,
    },
    /// Sysfs did not have what was needed.
    #[error("{0}")]
    Sysfs(String),
}

impl Format {
    /// Parse `name=config:0-7,32-35` lines.
    pub fn parse(text: &str) -> Result<Format, FormatError> {
        let mut fields = BTreeMap::new();
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let (name, spec) = line
                .split_once('=')
                .ok_or_else(|| FormatError::BadLine(line.into()))?;
            let (word, ranges) = spec
                .split_once(':')
                .ok_or_else(|| FormatError::BadLine(line.into()))?;
            let word = match word {
                "config" => 0,
                "config1" => 1,
                "config2" => 2,
                _ => return Err(FormatError::BadLine(line.into())),
            };
            let mut parsed = Vec::new();
            for r in ranges.split(',') {
                let (lo, hi) = match r.split_once('-') {
                    Some((lo, hi)) => (lo, hi),
                    None => (r, r),
                };
                let lo: u8 = lo.parse().map_err(|_| FormatError::BadLine(line.into()))?;
                let hi: u8 = hi.parse().map_err(|_| FormatError::BadLine(line.into()))?;
                if lo > hi || hi > 63 {
                    return Err(FormatError::BadLine(line.into()));
                }
                parsed.push(BitRange { word, lo, hi });
            }
            fields.insert(name.to_string(), parsed);
        }
        Ok(Format { fields })
    }

    /// Read a PMU's `format/` directory, one file per field.
    pub fn from_dir(dir: &Path) -> Result<Format, FormatError> {
        let mut text = String::new();
        let entries = std::fs::read_dir(dir)
            .map_err(|e| FormatError::Sysfs(format!("{}: {e}", dir.display())))?;
        for entry in entries {
            let entry = entry.map_err(|e| FormatError::Sysfs(format!("{}: {e}", dir.display())))?;
            let path = entry.path();
            let spec = std::fs::read_to_string(&path)
                .map_err(|e| FormatError::Sysfs(format!("{}: {e}", path.display())))?;
            text.push_str(&entry.file_name().to_string_lossy());
            text.push('=');
            text.push_str(spec.trim());
            text.push('\n');
        }
        Format::parse(&text)
    }

    /// Width of a field in bits.
    pub fn width(&self, field: &str) -> Option<u32> {
        self.fields
            .get(field)
            .map(|rs| rs.iter().map(|r| u32::from(r.hi - r.lo + 1)).sum())
    }

    /// Place field values into config words.
    pub fn encode(&self, fields: &[(&str, u64)]) -> Result<[u64; 3], FormatError> {
        let mut words = [0u64; 3];
        for (name, value) in fields {
            let ranges = self
                .fields
                .get(*name)
                .ok_or_else(|| FormatError::NoField((*name).into()))?;
            let bits = self.width(name).unwrap_or(0);
            if bits < 64 && *value >> bits != 0 {
                return Err(FormatError::TooWide {
                    field: (*name).into(),
                    value: *value,
                    bits,
                });
            }
            let mut remaining = *value;
            for r in ranges {
                let n = u32::from(r.hi - r.lo + 1);
                let mask = if n == 64 { u64::MAX } else { (1u64 << n) - 1 };
                words[usize::from(r.word)] |= (remaining & mask) << r.lo;
                remaining = if n == 64 { 0 } else { remaining >> n };
            }
        }
        Ok(words)
    }
}

/// An event ready for `perf_event_attr`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Encoded {
    /// `attr.type`.
    pub type_id: u32,
    /// `attr.config`.
    pub config: u64,
    /// `attr.config1`.
    pub config1: u64,
    /// `attr.config2`.
    pub config2: u64,
}

/// Where PMUs are described. Real systems use `/`; tests point at a
/// fixture tree.
#[derive(Clone, Debug)]
pub struct Sysfs {
    root: PathBuf,
}

impl Default for Sysfs {
    fn default() -> Self {
        Sysfs {
            root: PathBuf::from("/"),
        }
    }
}

impl Sysfs {
    /// A sysfs rooted somewhere else.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Sysfs { root: root.into() }
    }

    /// Resolve an absolute path under this root.
    pub fn path(&self, abs: impl AsRef<Path>) -> PathBuf {
        let rel = abs.as_ref().strip_prefix("/").unwrap_or(abs.as_ref());
        self.root.join(rel)
    }

    /// `/sys/bus/event_source/devices/<pmu>` under this root.
    pub fn pmu_dir(&self, pmu: &str) -> PathBuf {
        self.path("/sys/bus/event_source/devices").join(pmu)
    }

    /// Whether the PMU exists.
    pub fn has_pmu(&self, pmu: &str) -> bool {
        self.pmu_dir(pmu).join("type").is_file()
    }

    /// The PMU's `type` number.
    pub fn pmu_type(&self, pmu: &str) -> Result<u32, FormatError> {
        let path = self.pmu_dir(pmu).join("type");
        let text = std::fs::read_to_string(&path)
            .map_err(|e| FormatError::Sysfs(format!("{}: {e}", path.display())))?;
        text.trim()
            .parse()
            .map_err(|_| FormatError::Sysfs(format!("{}: not a number", path.display())))
    }

    /// The PMU's format.
    pub fn pmu_format(&self, pmu: &str) -> Result<Format, FormatError> {
        Format::from_dir(&self.pmu_dir(pmu).join("format"))
    }

    /// CPUs listed in the PMU's `cpumask`, one per uncore instance,
    /// or `None` when the file is absent.
    pub fn pmu_cpumask(&self, pmu: &str) -> Option<Vec<u32>> {
        let text = std::fs::read_to_string(self.pmu_dir(pmu).join("cpumask")).ok()?;
        parse_cpulist(&text)
    }

    /// CPUs listed in the PMU's `cpus`, which a core PMU of one class
    /// of core on a host with several has, or `None` when the file is
    /// absent.
    pub fn pmu_cpus(&self, pmu: &str) -> Option<Vec<u32>> {
        let text = std::fs::read_to_string(self.pmu_dir(pmu).join("cpus")).ok()?;
        parse_cpulist(&text)
    }

    /// Encode an event. Core events use the vendor's built-in format
    /// and `PERF_TYPE_RAW`; anything else is read from sysfs.
    pub fn encode(&self, vendor: Vendor, ev: &EventDef) -> Result<Encoded, FormatError> {
        let (type_id, format) = if ev.pmu == CORE {
            (PERF_TYPE_RAW, vendor.core_format())
        } else {
            (self.pmu_type(ev.pmu)?, self.pmu_format(ev.pmu)?)
        };
        let [config, config1, config2] = format.encode(&ev.fields)?;
        Ok(Encoded {
            type_id,
            config,
            config1,
            config2,
        })
    }
}

/// Parse `0-3,8,12-13` into CPU numbers.
pub fn parse_cpulist(text: &str) -> Option<Vec<u32>> {
    let mut out = Vec::new();
    for part in text.trim().split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (u32, u32) = (a.parse().ok()?, b.parse().ok()?);
                if a > b {
                    return None;
                }
                out.extend(a..=b);
            }
            None => out.push(part.parse().ok()?),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Sysfs {
        Sysfs::at(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/root"))
    }

    fn apple_fixture() -> Sysfs {
        Sysfs::at(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/apple"))
    }

    fn ident(vendor: &str, family: u32, model: u32) -> CpuIdent {
        CpuIdent {
            vendor: vendor.into(),
            family,
            model,
            name: String::new(),
        }
    }

    #[test]
    fn model_mapping_follows_the_kernel_mapfile() {
        let t = |v, f, m| table_for(&ident(v, f, m)).map(|t| t.table);
        assert_eq!(t("amd", 25, 1), Some("amdzen3"), "EPYC 7763");
        assert_eq!(t("amd", 25, 0x21), Some("amdzen3"), "Ryzen 5000");
        assert_eq!(t("amd", 25, 0x50), Some("amdzen3"));
        assert_eq!(t("amd", 25, 0x11), Some("amdzen4"), "EPYC 9654");
        assert_eq!(t("amd", 25, 24), Some("amdzen4"), "Threadripper PRO 7975WX");
        assert_eq!(t("amd", 25, 0x61), Some("amdzen4"), "Ryzen 7000");
        assert_eq!(t("amd", 26, 0x02), Some("amdzen5"), "EPYC 9005");
        assert_eq!(t("amd", 26, 0x44), Some("amdzen5"), "Ryzen 9000");
        assert_eq!(t("amd", 26, 0x80), None, "unverified model");
        assert_eq!(t("amd", 23, 0x31), None, "Zen 2 is not tabled");
        assert_eq!(t("intel", 6, 0x55), Some("skylakex"));
        assert_eq!(
            t("intel", 6, 0x6a),
            Some("skylakex"),
            "Ice Lake-SP shares the encodings"
        );
        assert_eq!(
            t("intel", 6, 0xaf),
            Some("skylakex"),
            "Sierra Forest shares the encodings"
        );
        assert_eq!(t("intel", 6, 0x8f), Some("sapphirerapids"));
        assert_eq!(
            t("intel", 6, 0xcf),
            Some("sapphirerapids"),
            "Emerald Rapids"
        );
        assert_eq!(
            t("intel", 6, 0xad),
            Some("sapphirerapids"),
            "Granite Rapids"
        );
        assert_eq!(t("intel", 6, 0x9e), None, "client part");
        assert_eq!(t("arm", 0x41, 0xd49), Some("arm64"), "Neoverse N2");
        assert_eq!(t("arm", 0x41, 0xd0c), Some("arm64"), "Neoverse N1");
        assert_eq!(t("arm", 0x50, 0x000), None, "Ampere implementer");
        assert_eq!(t("arm", 0x61, 0x038), Some("applem2"), "M2 Max Blizzard");
        assert_eq!(t("arm", 0x61, 0x039), Some("applem2"), "M2 Max Avalanche");
        assert_eq!(t("arm", 0x61, 0x033), Some("applem2"), "M2 Avalanche");
        assert_eq!(t("arm", 0x61, 0x023), None, "M1 Firestorm: not measured");
        assert_eq!(t("CentaurHauls", 7, 59), None);
        assert!(table_named("nope").is_none());
    }

    #[test]
    fn tables_are_internally_consistent() {
        for t in TABLES {
            assert_eq!(table_named(t.table).map(|x| x.table), Some(t.table));
            assert!(!t.dtlb.is_empty());
            if t.vendor == Vendor::Amd {
                assert_eq!(t.l3_miss.unwrap().pmu, "amd_l3", "{}", t.table);
                assert!(t.l3_miss_proxy.is_some(), "{}", t.table);
            } else {
                assert!(t.l3_miss_proxy.is_none(), "{}", t.table);
            }
            if t.classes.is_empty() {
                assert!(t.l3_miss.is_some(), "{}", t.table);
            } else {
                assert!(t.l3_miss.is_none(), "{}: classes carry it", t.table);
                assert!(t.classes.iter().any(|c| c.l3_miss.is_some()), "{}", t.table);
                assert!(t.classes.iter().all(|c| !c.pmu.is_empty()), "{}", t.table);
            }
            for s in t.dtlb.iter().chain([&t.itlb]) {
                assert_eq!(s.pmu, CORE, "{}: TLB events are core events", t.table);
            }
        }
    }

    #[test]
    fn every_table_event_encodes() {
        for t in TABLES {
            let sysfs = if t.vendor == Vendor::Apple {
                apple_fixture()
            } else {
                fixture()
            };
            let all = t
                .l3_miss
                .into_iter()
                .chain(t.l3_miss_proxy)
                .chain(t.dtlb.iter().copied())
                .chain([t.itlb]);
            for s in all {
                let def = s.def();
                let enc = sysfs
                    .encode(t.vendor, &def)
                    .unwrap_or_else(|e| panic!("{}: {def}: {e}", t.table));
                if def.pmu == CORE {
                    assert_eq!(enc.type_id, PERF_TYPE_RAW);
                } else {
                    assert_eq!(enc.type_id, 13, "amd_l3 type from the fixture");
                }
                assert_eq!((enc.config1, enc.config2), (0, 0));
            }
            for class in t.classes {
                let on_class = t
                    .dtlb
                    .iter()
                    .copied()
                    .chain([t.itlb])
                    .chain(class.l3_miss)
                    .map(|s| s.def().on_pmu(class.pmu));
                for def in on_class {
                    let enc = sysfs
                        .encode(t.vendor, &def)
                        .unwrap_or_else(|e| panic!("{}: {def}: {e}", t.table));
                    assert_eq!(
                        enc.type_id,
                        sysfs.pmu_type(class.pmu).unwrap(),
                        "{}: {def} on its class's PMU",
                        t.table
                    );
                    assert_eq!(
                        enc.config,
                        sysfs.encode(t.vendor, &def.on_pmu(CORE)).unwrap().config,
                        "{}: {def}: the class PMU's format is the core format",
                        t.table
                    );
                }
                assert!(sysfs.pmu_cpus(class.pmu).is_some_and(|c| !c.is_empty()));
            }
        }
    }

    #[test]
    fn apple_classes_from_the_fixture() {
        let sysfs = apple_fixture();
        assert_eq!(sysfs.pmu_type("apple_avalanche_pmu"), Ok(11));
        assert_eq!(sysfs.pmu_type("apple_blizzard_pmu"), Ok(10));
        assert_eq!(sysfs.pmu_cpus("apple_avalanche_pmu"), Some(vec![4, 5]));
        assert_eq!(sysfs.pmu_cpus("apple_blizzard_pmu"), Some(vec![0, 1]));
        assert_eq!(sysfs.pmu_cpus("nope"), None);
        assert_eq!(fixture().pmu_cpus("cpu"), None, "one class: no cpus file");
        let p = APPLE_P_LLC.def().on_pmu("apple_avalanche_pmu");
        assert_eq!(p.to_string(), "apple_avalanche_pmu/event=0x23/");
        let enc = sysfs.encode(Vendor::Apple, &p).unwrap();
        assert_eq!((enc.type_id, enc.config), (11, 0x23));
        assert_eq!(
            sysfs
                .encode(Vendor::Apple, &APPLE_DTLB[0].def())
                .unwrap()
                .config,
            0x0b
        );
        assert_eq!(
            sysfs.encode(Vendor::Apple, &EventDef::core_event("x", 0x100).def()),
            Err(FormatError::TooWide {
                field: "event".into(),
                value: 0x100,
                bits: 8
            }),
            "eight event bits"
        );
    }

    #[test]
    fn core_encodings_match_perf_stat_spelling() {
        let sysfs = fixture();
        let amd_dtlb = sysfs.encode(Vendor::Amd, &AMD_DTLB[0].def()).unwrap();
        assert_eq!(amd_dtlb.config, 0x0f45, "cpu/event=0x45,umask=0x0f/");
        let amd_wide = sysfs
            .encode(Vendor::Amd, &EventDef::core("x", 0x120, 0x03).def())
            .unwrap();
        assert_eq!(
            amd_wide.config, 0x1_0000_0320,
            "event bits 8-11 land in config 32-35"
        );
        let intel = sysfs.encode(Vendor::Intel, &INTEL_LLC.def()).unwrap();
        assert_eq!(intel.config, 0x412e);
        let arm = sysfs.encode(Vendor::Arm, &ARM_DTLB[0].def()).unwrap();
        assert_eq!(arm.config, 0x34);
        assert_eq!(ARM_DTLB[0].def().to_string(), "cpu/event=0x34/");
        assert_eq!(
            sysfs.encode(Vendor::Intel, &EventDef::core("x", 0x100, 0).def()),
            Err(FormatError::TooWide {
                field: "event".into(),
                value: 0x100,
                bits: 8
            })
        );
    }

    #[test]
    fn amd_l3_filters_land_in_the_right_bits() {
        let sysfs = fixture();
        let filtered = AMD_L3_ZEN4.def().with_fields(&[
            ("coreid", 5),
            ("threadmask", 0b10),
            ("enallslices", 1),
            ("enallcores", 0),
        ]);
        assert_eq!(
            filtered.to_string(),
            "amd_l3/event=0x4,umask=0x1,coreid=0x5,threadmask=0x2,enallslices=0x1,enallcores=0x0/"
        );
        let enc = sysfs.encode(Vendor::Amd, &filtered).unwrap();
        let expect = 0x04 | (0x01 << 8) | (5u64 << 42) | (0b10u64 << 56) | (1u64 << 46);
        assert_eq!(enc.config, expect);
        assert_eq!(enc.type_id, 13);
        assert_eq!(
            sysfs.encode(
                Vendor::Amd,
                &AMD_L3_ZEN4.def().with_fields(&[("coreid", 8)])
            ),
            Err(FormatError::TooWide {
                field: "coreid".into(),
                value: 8,
                bits: 3
            })
        );
        assert_eq!(
            sysfs.encode(
                Vendor::Amd,
                &AMD_L3_ZEN4.def().with_fields(&[("slicemask", 1)])
            ),
            Err(FormatError::NoField("slicemask".into()))
        );
        assert_eq!(sysfs.pmu_cpumask("amd_l3"), Some(vec![0, 8, 16, 24]));
        assert!(sysfs.has_pmu("amd_l3"));
        assert!(!sysfs.has_pmu("intel_imc"));
        assert!(matches!(
            sysfs.encode(
                Vendor::Intel,
                &Static {
                    name: "x",
                    pmu: "intel_imc",
                    event: 1,
                    umask: None
                }
                .def()
            ),
            Err(FormatError::Sysfs(_))
        ));
    }

    #[test]
    fn format_parsing() {
        let f = Format::parse("a=config:0-3\nb=config1:4\nc=config2:60-63,0-1\n").unwrap();
        assert_eq!(f.width("a"), Some(4));
        assert_eq!(f.width("b"), Some(1));
        assert_eq!(f.width("c"), Some(6));
        assert_eq!(f.width("d"), None);
        assert_eq!(f.encode(&[("b", 1)]).unwrap(), [0, 1 << 4, 0]);
        // c: low 4 bits at 60-63, next 2 bits at 0-1
        assert_eq!(
            f.encode(&[("c", 0b11_1010)]).unwrap(),
            [0, 0, (0b1010u64 << 60) | 0b11]
        );
        assert_eq!(
            f.encode(&[("b", 2)]),
            Err(FormatError::TooWide {
                field: "b".into(),
                value: 2,
                bits: 1
            })
        );
        for bad in [
            "noequals",
            "x=config",
            "x=config3:0-1",
            "x=config:5-2",
            "x=config:0-64",
            "x=config:a-b",
        ] {
            assert!(
                matches!(Format::parse(bad), Err(FormatError::BadLine(_))),
                "{bad}"
            );
        }
        assert_eq!(Format::parse("").unwrap(), Format::default());
        let wide = Format::parse("w=config:0-63").unwrap();
        assert_eq!(wide.encode(&[("w", u64::MAX)]).unwrap(), [u64::MAX, 0, 0]);
    }

    #[test]
    fn cpulists() {
        assert_eq!(
            parse_cpulist("0-3,8,12-13\n"),
            Some(vec![0, 1, 2, 3, 8, 12, 13])
        );
        assert_eq!(parse_cpulist(""), Some(vec![]));
        assert_eq!(parse_cpulist("3-1"), None);
        assert_eq!(parse_cpulist("x"), None);
    }

    #[test]
    fn display_and_vendor() {
        assert_eq!(INTEL_LLC.def().to_string(), "cpu/event=0x2e,umask=0x41/");
        assert_eq!(Vendor::of(&ident("amd", 25, 1)), Some(Vendor::Amd));
        assert_eq!(Vendor::of(&ident("arm", 0x41, 0xd49)), Some(Vendor::Arm));
        assert_eq!(Vendor::of(&ident("arm", 0x61, 0x039)), Some(Vendor::Apple));
        assert_eq!(Vendor::of(&ident("via", 1, 1)), None);
    }
}
