//! CPU identification from `/proc/cpuinfo`.
//!
//! The kernel already combines the extended family and model fields on
//! x86, so the display values it prints are the ones the perf event
//! tables are keyed by. On ARM the implementer and part numbers play
//! the same role.

use std::collections::BTreeMap;

use crate::profile::CpuIdent;

/// Identify the CPU this process runs on.
pub fn detect() -> Option<CpuIdent> {
    let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    from_cpuinfo(&text)
}

/// Parse the first processor block of a `/proc/cpuinfo` text.
pub fn from_cpuinfo(text: &str) -> Option<CpuIdent> {
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
                .map(|s| s.to_string())
                .unwrap_or_default(),
        });
    }
    if let (Some(imp), Some(part)) = (kv.get("CPU implementer"), kv.get("CPU part")) {
        let family = parse_hex(imp)?;
        let model = parse_hex(part)?;
        let name = kv
            .get("model name")
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("ARM implementer 0x{family:x} part 0x{model:x}"));
        return Some(CpuIdent {
            vendor: "arm".to_string(),
            family,
            model,
            name,
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
        let c = from_cpuinfo(include_str!("../tests/fixtures/cpuinfo-arm.txt")).unwrap();
        assert_eq!(c.vendor, "arm");
        assert_eq!((c.family, c.model), (0x41, 0xd49));
        assert_eq!(c.name, "ARM implementer 0x41 part 0xd49");
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
