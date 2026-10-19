//! CPU identification from `/proc/cpuinfo`.
//!
//! The kernel already combines the extended family and model fields on
//! x86, so the display values it prints are the ones the perf event
//! tables are keyed by. On ARM the implementer and part numbers play
//! the same role.

use std::collections::BTreeMap;

use crate::profile::CpuIdent;

/// Identify the CPU this process runs on. Arm hosts print no model
/// name in `/proc/cpuinfo`; there the device tree's `model`, the
/// machine and its chip, stands in for reports.
pub fn detect() -> Option<CpuIdent> {
    let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    let dt_model = std::fs::read_to_string("/sys/firmware/devicetree/base/model").ok();
    from_cpuinfo_and_dt(&text, dt_model.as_deref())
}

/// Parse the first processor block of a `/proc/cpuinfo` text.
pub fn from_cpuinfo(text: &str) -> Option<CpuIdent> {
    from_cpuinfo_and_dt(text, None)
}

/// As [`from_cpuinfo`], naming a CPU that `/proc/cpuinfo` does not by
/// the device tree's `model` when there is one. The string is
/// NUL-terminated in sysfs.
pub fn from_cpuinfo_and_dt(text: &str, dt_model: Option<&str>) -> Option<CpuIdent> {
    let mut cpu = parse_cpuinfo(text)?;
    if cpu.name.is_empty() {
        if let Some(model) = dt_model {
            let model = model.trim_end_matches('\0').trim();
            if !model.is_empty() {
                cpu.name = model.to_string();
            }
        }
    }
    Some(cpu)
}

fn parse_cpuinfo(text: &str) -> Option<CpuIdent> {
    let block = text.split("\n\n").find(|b| !b.trim().is_empty())?;
    let kv: BTreeMap<&str, &str> = block
        .lines()
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim(), v.trim()))
        .collect();
    if let Some(vendor_id) = kv.get("vendor_id") {
        let vendor = match *vendor_id {
            "AuthenticAMD" => "amd",
            "GenuineIntel" => "intel",
            other => other,
        };
        return Some(CpuIdent {
            vendor: vendor.to_string(),
            family: kv.get("cpu family")?.parse().ok()?,
            model: kv.get("model")?.parse().ok()?,
            name: kv
                .get("model name")
                .map(std::string::ToString::to_string)
                .unwrap_or_default(),
        });
    }
    if let (Some(imp), Some(part)) = (kv.get("CPU implementer"), kv.get("CPU part")) {
        let family = parse_hex(imp)?;
        let model = parse_hex(part)?;
        return Some(CpuIdent {
            vendor: "arm".to_string(),
            family,
            model,
            name: kv
                .get("model name")
                .map(std::string::ToString::to_string)
                .unwrap_or_default(),
        });
    }
    None
}

fn parse_hex(s: &str) -> Option<u32> {
    u32::from_str_radix(s.trim_start_matches("0x"), 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amd() {
        let c = from_cpuinfo(include_str!("../tests/fixtures/cpuinfo-amd.txt")).unwrap();
        assert_eq!(c.vendor, "amd");
        assert_eq!((c.family, c.model), (25, 24));
        assert_eq!(c.name, "AMD Ryzen Threadripper PRO 7975WX 32-Cores");
    }

    #[test]
    fn intel() {
        let c = from_cpuinfo(include_str!("../tests/fixtures/cpuinfo-intel.txt")).unwrap();
        assert_eq!(c.vendor, "intel");
        assert_eq!((c.family, c.model), (6, 143));
        assert_eq!(c.name, "Intel(R) Xeon(R) Platinum 8480+");
    }

    #[test]
    fn arm() {
        let text = include_str!("../tests/fixtures/cpuinfo-arm.txt");
        let c = from_cpuinfo(text).unwrap();
        assert_eq!(c.vendor, "arm");
        assert_eq!((c.family, c.model), (0x41, 0xd49));
        assert_eq!(c.name, "", "no model name line");
        let named = from_cpuinfo_and_dt(text, Some("Some Board\0")).unwrap();
        assert_eq!(named.name, "Some Board", "the device tree names it");
        assert_eq!(from_cpuinfo_and_dt(text, Some("\0")).unwrap().name, "");
        let x86 = include_str!("../tests/fixtures/cpuinfo-amd.txt");
        assert_eq!(
            from_cpuinfo_and_dt(x86, Some("Board")).unwrap().name,
            "AMD Ryzen Threadripper PRO 7975WX 32-Cores",
            "a cpuinfo name wins"
        );
    }

    #[test]
    fn apple() {
        let c = from_cpuinfo(include_str!("../tests/fixtures/cpuinfo-apple.txt")).unwrap();
        assert_eq!(c.vendor, "arm");
        assert_eq!(
            (c.family, c.model),
            (0x61, 0x038),
            "implementer Apple, a Blizzard core first"
        );
    }

    #[test]
    fn only_the_first_block_counts() {
        let two = format!(
            "{}\n\n{}",
            include_str!("../tests/fixtures/cpuinfo-intel.txt"),
            include_str!("../tests/fixtures/cpuinfo-amd.txt")
        );
        assert_eq!(from_cpuinfo(&two).unwrap().vendor, "intel");
        let blank_first = format!("\n\n{}", include_str!("../tests/fixtures/cpuinfo-amd.txt"));
        assert_eq!(from_cpuinfo(&blank_first).unwrap().vendor, "amd");
    }

    #[test]
    fn garbage() {
        assert_eq!(from_cpuinfo(""), None);
        assert_eq!(from_cpuinfo("processor : 0\nflags : fpu\n"), None);
        assert_eq!(
            from_cpuinfo("vendor_id : GenuineIntel\ncpu family : six\nmodel : 1\n"),
            None
        );
        assert_eq!(
            from_cpuinfo("CPU implementer : 0x41\nCPU part : zz\n"),
            None
        );
        let other = from_cpuinfo("vendor_id : CentaurHauls\ncpu family : 7\nmodel : 59\n").unwrap();
        assert_eq!(other.vendor, "CentaurHauls");
        assert_eq!(other.name, "");
    }

    #[test]
    fn this_host_is_identified() {
        let c = detect().expect("/proc/cpuinfo parses");
        assert!(!c.vendor.is_empty());
    }
}
