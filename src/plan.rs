//! Decide which counters to open for a scope, given what the process is
//! allowed to do: `docs/design.md` section 4. The decision is pure so
//! every fallback is testable without the privilege it falls back from.

use std::collections::BTreeMap;

use crate::caps::Expectations;
use crate::events::{CORE, EventDef, EventTable, FormatError, Static, Sysfs};
use crate::perf::{Options, Probe, Spec};
use crate::profile::{self, Method, Mode, Provenance};
use crate::topology::Topology;

/// The scope to plan for, with the facts the plan needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeInfo {
    /// This process. `pinned_cpus` is set when every worker thread is
    /// pinned, which makes per-core uncore attribution meaningful.
    SelfThreads {
        /// CPUs the workers are pinned to, if they are.
        pinned_cpus: Option<Vec<u32>>,
    },
    /// Another process: its current thread ids.
    Process {
        /// Thread ids to attach to; new threads are inherited.
        tids: Vec<i32>,
    },
    /// A cgroup: the CPUs it may run on and its current thread ids.
    Cgroup {
        /// Effective cpuset, or every online CPU.
        cpus: Vec<u32>,
        /// Thread ids, for the attach fallback.
        tids: Vec<i32>,
    },
    /// The whole machine.
    Host {
        /// Online CPUs.
        cpus: Vec<u32>,
    },
}

/// What is actually permitted, combining the static rules with the
/// live probe. The probe wins where it ran.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Access {
    /// Counters on our own threads, user mode.
    pub self_user: bool,
    /// Counters on our own threads, kernel mode too.
    pub self_kernel: bool,
    /// Per-CPU, per-cgroup and uncore events.
    pub per_cpu: bool,
    /// Attaching to other same-uid processes.
    pub attach_same_uid: bool,
}

impl Access {
    /// Combine the rules with the probe.
    pub fn new(rules: &Expectations, probe: &Probe) -> Access {
        let self_user = probe.self_user.is_ok();
        Access {
            self_user,
            self_kernel: probe.self_kernel.is_ok(),
            per_cpu: probe.per_cpu.is_ok(),
            attach_same_uid: self_user && (rules.attach_same_uid.ok || rules.attach_any.ok),
        }
    }
}

/// Where a group of counters is opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetPlan {
    /// Own threads, inherited.
    SelfThreads,
    /// One task, inherited.
    Task(i32),
    /// One CPU, every task.
    Cpu(u32),
    /// The cgroup on one CPU.
    CgroupOnCpu(u32),
}

/// Counters that share a target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    /// Target.
    pub target: TargetPlan,
    /// Counters to open there.
    pub specs: Vec<Spec>,
    /// Open options.
    pub opts: Options,
}

/// How a sample field is derived from counters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldSource {
    /// Counter names whose readings are summed, across all groups.
    pub counters: Vec<String>,
    /// Scope, mode and method of the result.
    pub provenance: Provenance,
}

/// The counters to open and the fields they feed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Groups to open.
    pub groups: Vec<Group>,
    /// Sample field to its source, in field order.
    pub fields: BTreeMap<String, FieldSource>,
    /// Why things are the way they are, for the report.
    pub notes: Vec<String>,
}

impl Plan {
    /// Whether no counter will be opened.
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Total counters across groups.
    pub fn counter_count(&self) -> usize {
        self.groups.iter().map(|g| g.specs.len()).sum()
    }
}

/// Diagnostic key for the AMD DRAM-fill proxy when the L3 PMU is the
/// primary source; aggregate ignores keys outside the field table.
pub const L3_FILLS_KEY: &str = "l3_dram_fills_per_sec";

/// Core-PMU counters for one target: instructions, cycles, TLBs and,
/// where the core PMU has one, L3 misses or their proxy. On a host
/// with several classes of core each class has the set on its own
/// PMU, under the same names, so a task's readings fold across the
/// classes it ran on (`perf::fold_by_name`) and a CPU's come from the
/// class it belongs to.
struct CoreSet {
    /// One entry per class, or one for the single core PMU: the CPUs
    /// the PMU covers, `None` for all, and the specs on it.
    classes: Vec<(Option<Vec<u32>>, Vec<Spec>)>,
    fields: Vec<(&'static str, Vec<String>, Method)>,
    notes: Vec<String>,
}

impl CoreSet {
    /// Every class's specs, for a target that runs anywhere.
    fn all_specs(&self) -> Vec<Spec> {
        self.classes.iter().flat_map(|(_, s)| s.clone()).collect()
    }

    /// The specs of the class a CPU belongs to, or of every class
    /// whose coverage is unknown.
    fn specs_for_cpu(&self, cpu: u32) -> Vec<Spec> {
        self.classes
            .iter()
            .filter(|(cpus, _)| cpus.as_ref().is_none_or(|c| c.contains(&cpu)))
            .flat_map(|(_, s)| s.clone())
            .collect()
    }
}

fn core_set(table: &EventTable, sysfs: &Sysfs) -> Result<CoreSet, FormatError> {
    let mut fields = vec![
        (
            "instructions_per_sec",
            vec!["instructions".to_string()],
            Method::Perf,
        ),
        ("cycles_per_sec", vec!["cycles".to_string()], Method::Perf),
    ];
    let dtlb_names: Vec<String> = table
        .dtlb
        .iter()
        .map(|s| format!("dtlb:{}", s.def().name))
        .collect();
    fields.push(("dtlb_misses_per_sec", dtlb_names.clone(), Method::Perf));
    let itlb_name = format!("itlb:{}", table.itlb.def().name);
    fields.push(("itlb_misses_per_sec", vec![itlb_name.clone()], Method::Perf));

    // The single core PMU, or one class per PMU, each with the L3
    // miss event it has.
    let pmus: Vec<(Option<&'static str>, Option<Static>)> = if table.classes.is_empty() {
        vec![(None, table.l3_miss)]
    } else {
        table
            .classes
            .iter()
            .map(|c| (Some(c.pmu), c.l3_miss))
            .collect()
    };
    let mut classes = Vec::with_capacity(pmus.len());
    let mut l3_names: Vec<String> = Vec::new();
    let mut l3_method = Method::Perf;
    let mut without_l3: Vec<&str> = Vec::new();
    for (pmu, l3) in &pmus {
        let on = |def: EventDef| match pmu {
            Some(p) => def.on_pmu(p),
            None => def,
        };
        let (cpus, mut specs) = match pmu {
            Some(p) => {
                let t = sysfs.pmu_type(p)?;
                (
                    sysfs.pmu_cpus(p),
                    vec![Spec::instructions_on(t), Spec::cycles_on(t)],
                )
            }
            None => (None, vec![Spec::instructions(), Spec::cycles()]),
        };
        for (s, name) in table.dtlb.iter().zip(&dtlb_names) {
            specs.push(Spec::on_core(
                name,
                sysfs.encode(table.vendor, &on(s.def()))?,
            ));
        }
        specs.push(Spec::on_core(
            &itlb_name,
            sysfs.encode(table.vendor, &on(table.itlb.def()))?,
        ));
        match l3 {
            Some(l3) if l3.def().pmu == CORE => {
                let def = on(l3.def());
                let name = format!("l3:{}", def.name);
                specs.push(Spec::on_core(&name, sysfs.encode(table.vendor, &def)?));
                if !l3_names.contains(&name) {
                    l3_names.push(name);
                }
            }
            Some(_) => {
                if let Some(proxy) = table.l3_miss_proxy {
                    let def = on(proxy.def());
                    let name = format!("l3proxy:{}", def.name);
                    specs.push(Spec::on_core(&name, sysfs.encode(table.vendor, &def)?));
                    if !l3_names.contains(&name) {
                        l3_names.push(name);
                    }
                    l3_method = Method::Proxy;
                }
            }
            None => without_l3.push(pmu.unwrap_or(CORE)),
        }
        classes.push((cpus, specs));
    }
    let mut notes = Vec::new();
    if l3_names.is_empty() {
        notes.push("no L3 miss event on this CPU: L3 misses are not measured".into());
    } else {
        fields.push(("l3_misses_per_sec", l3_names, l3_method));
        if !without_l3.is_empty() {
            let with: Vec<&str> = pmus
                .iter()
                .filter(|(_, l3)| l3.is_some())
                .filter_map(|(p, _)| *p)
                .collect();
            notes.push(format!(
                "L3 misses are counted on {} only: {} has no event for them, so work on its CPUs adds instructions and cycles but no misses",
                with.join(" and "),
                without_l3.join(" and ")
            ));
        }
    }
    Ok(CoreSet {
        classes,
        fields,
        notes,
    })
}

/// AMD L3 PMU groups for a set of CPUs: one group per L3 domain, with
/// one filtered counter per CPU, or a single all-cores counter when
/// the set covers the whole domain.
fn amd_l3_groups(
    table: &EventTable,
    sysfs: &Sysfs,
    topo: &Topology,
    cpus: &[u32],
) -> Result<(Vec<Group>, Vec<String>, bool), FormatError> {
    let mut groups = Vec::new();
    let mut names = Vec::new();
    let mut whole = true;
    for domain in topo.l3_domains() {
        let covered: Vec<u32> = domain
            .iter()
            .copied()
            .filter(|c| cpus.contains(c))
            .collect();
        if covered.is_empty() {
            continue;
        }
        let base = table.l3_miss.expect("an uncore L3 event").def();
        let mut specs = Vec::new();
        if covered.len() == domain.len() {
            let def = base.with_fields(&[("enallcores", 1), ("enallslices", 1)]);
            let name = format!("l3:{}@domain{}", def.name, domain[0]);
            specs.push(Spec::new(&name, sysfs.encode(table.vendor, &def)?));
            names.push(name);
        } else {
            whole = false;
            for cpu in &covered {
                let Some(filter) = topo.amd_l3_filter(*cpu) else {
                    continue;
                };
                let def = base.with_fields(&[
                    ("coreid", u64::from(filter.coreid)),
                    ("threadmask", u64::from(filter.threadmask)),
                    ("enallslices", 1),
                ]);
                let name = format!("l3:{}@cpu{cpu}", def.name);
                specs.push(Spec::new(&name, sysfs.encode(table.vendor, &def)?));
                names.push(name);
            }
        }
        if !specs.is_empty() {
            groups.push(Group {
                target: TargetPlan::Cpu(domain[0]),
                specs,
                opts: Options {
                    kernel: true,
                    inherit: false,
                    pinned: false,
                },
            });
        }
    }
    Ok((groups, names, whole))
}

/// Plan the counters for a scope.
pub fn plan(
    scope: &ScopeInfo,
    access: &Access,
    table: Option<&EventTable>,
    sysfs: &Sysfs,
    topo: &Topology,
) -> Result<Plan, FormatError> {
    let mut plan = Plan::default();
    let Some(table) = table else {
        plan.notes
            .push("no event table for this CPU model: no counters".into());
        return Ok(plan);
    };
    let core = core_set(table, sysfs)?;
    plan.notes.extend(core.notes.iter().cloned());
    let amd = table.l3_miss.is_some_and(|l3| l3.def().pmu != CORE);

    let task_opts = |access: &Access| Options {
        kernel: access.self_kernel,
        inherit: true,
        pinned: false,
    };
    let task_mode = |access: &Access| {
        if access.self_kernel {
            Mode::All
        } else {
            Mode::User
        }
    };
    let add_fields = |plan: &mut Plan, scope: profile::Scope, mode: Mode, core: &CoreSet| {
        for (key, counters, method) in &core.fields {
            plan.fields.insert(
                key.to_string(),
                FieldSource {
                    counters: counters.clone(),
                    provenance: Provenance {
                        scope,
                        mode,
                        method: *method,
                    },
                },
            );
        }
    };

    match scope {
        ScopeInfo::SelfThreads { pinned_cpus } => {
            if !access.self_user {
                plan.notes
                    .push("counters on own threads are not permitted: no counters".into());
                return Ok(plan);
            }
            plan.groups.push(Group {
                target: TargetPlan::SelfThreads,
                specs: core.all_specs(),
                opts: task_opts(access),
            });
            add_fields(&mut plan, profile::Scope::Process, task_mode(access), &core);
            if !access.self_kernel {
                plan.notes
                    .push("kernel mode is not permitted: counters cover user mode only".into());
            }
            if amd {
                match (pinned_cpus, access.per_cpu) {
                    (Some(cpus), true) => {
                        let (groups, names, _) = amd_l3_groups(table, sysfs, topo, cpus)?;
                        plan.groups.extend(groups);
                        let proxy = plan.fields.remove("l3_misses_per_sec").expect("proxy field");
                        plan.fields.insert(L3_FILLS_KEY.to_string(), proxy);
                        plan.fields.insert(
                            "l3_misses_per_sec".to_string(),
                            FieldSource {
                                counters: names,
                                provenance: Provenance { scope: profile::Scope::Process, mode: Mode::All, method: Method::Perf },
                            },
                        );
                        plan.notes.push("L3 misses from the L3 PMU filtered to the pinned CPUs; DRAM fills kept as a diagnostic".into());
                    }
                    (None, true) => plan.notes.push("L3 misses from the DRAM-fill proxy: workers are not pinned, so the L3 PMU cannot be filtered to them".into()),
                    (_, false) => plan.notes.push("L3 misses from the DRAM-fill proxy: the L3 PMU needs CAP_PERFMON".into()),
                }
            }
        }
        ScopeInfo::Process { tids } => {
            if !access.attach_same_uid {
                plan.notes
                    .push("attaching to another process is not permitted: no counters".into());
                return Ok(plan);
            }
            for tid in tids {
                plan.groups.push(Group {
                    target: TargetPlan::Task(*tid),
                    specs: core.all_specs(),
                    opts: task_opts(access),
                });
            }
            add_fields(&mut plan, profile::Scope::Process, task_mode(access), &core);
            if !access.self_kernel {
                plan.notes
                    .push("kernel mode is not permitted: counters cover user mode only".into());
            }
            if amd {
                plan.notes.push(
                    "L3 misses from the DRAM-fill proxy: a process is not tied to cores".into(),
                );
            }
        }
        ScopeInfo::Cgroup { cpus, tids } => {
            if access.per_cpu {
                for cpu in cpus {
                    plan.groups.push(Group {
                        target: TargetPlan::CgroupOnCpu(*cpu),
                        specs: core.specs_for_cpu(*cpu),
                        opts: Options {
                            kernel: true,
                            inherit: false,
                            pinned: false,
                        },
                    });
                }
                add_fields(&mut plan, profile::Scope::Cgroup, Mode::All, &core);
                if amd {
                    let (groups, names, whole) = amd_l3_groups(table, sysfs, topo, cpus)?;
                    plan.groups.extend(groups);
                    let proxy = plan
                        .fields
                        .remove("l3_misses_per_sec")
                        .expect("proxy field");
                    plan.fields.insert(L3_FILLS_KEY.to_string(), proxy);
                    let scope = if whole {
                        profile::Scope::Host
                    } else {
                        profile::Scope::Cgroup
                    };
                    plan.fields.insert(
                        "l3_misses_per_sec".to_string(),
                        FieldSource {
                            counters: names,
                            provenance: Provenance {
                                scope,
                                mode: Mode::All,
                                method: Method::Perf,
                            },
                        },
                    );
                    plan.notes.push(if whole {
                        "L3 misses from the L3 PMU over whole domains: the cgroup may run anywhere, so this is host-wide".into()
                    } else {
                        "L3 misses from the L3 PMU filtered to the cgroup's CPUs; other tenants on those CPUs are included".into()
                    });
                }
            } else if access.attach_same_uid {
                for tid in tids {
                    plan.groups.push(Group {
                        target: TargetPlan::Task(*tid),
                        specs: core.all_specs(),
                        opts: task_opts(access),
                    });
                }
                add_fields(&mut plan, profile::Scope::Cgroup, task_mode(access), &core);
                plan.notes.push(format!(
                    "per-cgroup counters need CAP_PERFMON: attached to the {} threads present now; tasks that join later are not counted",
                    tids.len()
                ));
                if !access.self_kernel {
                    plan.notes
                        .push("kernel mode is not permitted: counters cover user mode only".into());
                }
                if amd {
                    plan.notes.push(
                        "L3 misses from the DRAM-fill proxy: the L3 PMU needs CAP_PERFMON".into(),
                    );
                }
            } else {
                plan.notes.push(
                    "neither per-cgroup counters nor attaching are permitted: no counters".into(),
                );
            }
        }
        ScopeInfo::Host { cpus } => {
            if !access.per_cpu {
                plan.notes
                    .push("host-wide counters need CAP_PERFMON: no counters".into());
                return Ok(plan);
            }
            for cpu in cpus {
                plan.groups.push(Group {
                    target: TargetPlan::Cpu(*cpu),
                    specs: core.specs_for_cpu(*cpu),
                    opts: Options {
                        kernel: true,
                        inherit: false,
                        pinned: false,
                    },
                });
            }
            add_fields(&mut plan, profile::Scope::Host, Mode::All, &core);
            if amd {
                let (groups, names, _) = amd_l3_groups(table, sysfs, topo, cpus)?;
                plan.groups.extend(groups);
                let proxy = plan
                    .fields
                    .remove("l3_misses_per_sec")
                    .expect("proxy field");
                plan.fields.insert(L3_FILLS_KEY.to_string(), proxy);
                plan.fields.insert(
                    "l3_misses_per_sec".to_string(),
                    FieldSource {
                        counters: names,
                        provenance: Provenance {
                            scope: profile::Scope::Host,
                            mode: Mode::All,
                            method: Method::Perf,
                        },
                    },
                );
            }
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::table_named;

    fn sysfs() -> Sysfs {
        Sysfs::at(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/root"))
    }

    fn topo() -> Topology {
        Topology::read(&sysfs()).unwrap()
    }

    const ALL: Access = Access {
        self_user: true,
        self_kernel: true,
        per_cpu: true,
        attach_same_uid: true,
    };
    const USER: Access = Access {
        self_user: true,
        self_kernel: false,
        per_cpu: false,
        attach_same_uid: true,
    };
    const NONE: Access = Access {
        self_user: false,
        self_kernel: false,
        per_cpu: false,
        attach_same_uid: false,
    };

    fn zen4() -> &'static EventTable {
        table_named("amdzen4").unwrap()
    }

    fn apple_sysfs() -> Sysfs {
        Sysfs::at(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/apple"))
    }

    fn m2() -> &'static EventTable {
        table_named("applem2").unwrap()
    }

    fn names(p: &Plan) -> Vec<String> {
        p.groups
            .iter()
            .flat_map(|g| g.specs.iter().map(|s| s.name.clone()))
            .collect()
    }

    #[test]
    fn self_unprivileged_on_amd_uses_the_proxy() {
        let p = plan(
            &ScopeInfo::SelfThreads { pinned_cpus: None },
            &USER,
            Some(zen4()),
            &sysfs(),
            &topo(),
        )
        .unwrap();
        assert_eq!(p.groups.len(), 1);
        assert_eq!(p.groups[0].target, TargetPlan::SelfThreads);
        assert_eq!(
            p.groups[0].opts,
            Options {
                kernel: false,
                inherit: true,
                pinned: false
            }
        );
        assert_eq!(
            names(&p),
            [
                "instructions",
                "cycles",
                "dtlb:ls_l1_d_tlb_miss.tlb_reload_l2_hit",
                "itlb:bp_l1_tlb_miss_l2_tlb_miss.all",
                "l3proxy:ls_any_fills_from_sys.dram_io_all"
            ]
        );
        let l3 = &p.fields["l3_misses_per_sec"];
        assert_eq!(
            l3.provenance,
            Provenance {
                scope: profile::Scope::Process,
                mode: Mode::User,
                method: Method::Proxy
            }
        );
        assert_eq!(
            p.fields["dtlb_misses_per_sec"].counters,
            ["dtlb:ls_l1_d_tlb_miss.tlb_reload_l2_hit"]
        );
        assert_eq!(p.fields.len(), 5);
        assert!(p.notes.iter().any(|n| n.contains("user mode only")));
        assert!(p.notes.iter().any(|n| n.contains("needs CAP_PERFMON")));
        assert_eq!(p.counter_count(), 5);
    }

    #[test]
    fn self_pinned_and_privileged_on_amd_filters_the_l3_pmu() {
        let p = plan(
            &ScopeInfo::SelfThreads {
                pinned_cpus: Some(vec![0, 33, 8]),
            },
            &ALL,
            Some(zen4()),
            &sysfs(),
            &topo(),
        )
        .unwrap();
        assert_eq!(
            p.groups.len(),
            3,
            "core group plus one uncore group per touched domain"
        );
        assert!(p.groups[0].opts.kernel);
        let dom0 = &p.groups[1];
        assert_eq!(dom0.target, TargetPlan::Cpu(0));
        assert_eq!(
            dom0.specs
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            [
                "l3:l3_lookup_state.l3_miss@cpu0",
                "l3:l3_lookup_state.l3_miss@cpu33"
            ]
        );
        let cpu33 = &dom0.specs[1].enc;
        assert_eq!(cpu33.type_id, 13);
        assert_eq!(
            cpu33.config,
            0x04 | (0x01 << 8) | (1u64 << 42) | (0b10u64 << 56) | (1u64 << 46)
        );
        assert_eq!(p.groups[2].target, TargetPlan::Cpu(8));
        let l3 = &p.fields["l3_misses_per_sec"];
        assert_eq!(l3.counters.len(), 3);
        assert_eq!(l3.provenance.method, Method::Perf);
        assert_eq!(p.fields[L3_FILLS_KEY].provenance.method, Method::Proxy);
        assert_eq!(p.fields["instructions_per_sec"].provenance.mode, Mode::All);
    }

    #[test]
    fn self_privileged_but_unpinned_keeps_the_proxy() {
        let p = plan(
            &ScopeInfo::SelfThreads { pinned_cpus: None },
            &ALL,
            Some(zen4()),
            &sysfs(),
            &topo(),
        )
        .unwrap();
        assert_eq!(p.groups.len(), 1);
        assert_eq!(
            p.fields["l3_misses_per_sec"].provenance.method,
            Method::Proxy
        );
        assert!(p.notes.iter().any(|n| n.contains("not pinned")));
        assert!(!p.fields.contains_key(L3_FILLS_KEY));
    }

    #[test]
    fn intel_core_pmu_has_l3_misses_without_uncore() {
        let skx = table_named("skylakex").unwrap();
        let p = plan(
            &ScopeInfo::SelfThreads {
                pinned_cpus: Some(vec![0]),
            },
            &ALL,
            Some(skx),
            &sysfs(),
            &topo(),
        )
        .unwrap();
        assert_eq!(p.groups.len(), 1);
        assert_eq!(
            p.fields["l3_misses_per_sec"].counters,
            ["l3:LONGEST_LAT_CACHE.MISS"]
        );
        assert_eq!(
            p.fields["l3_misses_per_sec"].provenance.method,
            Method::Perf
        );
        assert_eq!(
            p.fields["dtlb_misses_per_sec"].counters.len(),
            2,
            "load and store walks are summed"
        );
        assert!(p.notes.is_empty(), "{:?}", p.notes);
    }

    #[test]
    fn two_classes_of_core_open_the_set_on_each_pmu() {
        let sysfs = apple_sysfs();
        let topo = Topology::read(&sysfs).unwrap();
        let p = plan(
            &ScopeInfo::SelfThreads { pinned_cpus: None },
            &USER,
            Some(m2()),
            &sysfs,
            &topo,
        )
        .unwrap();
        assert_eq!(p.groups.len(), 1, "one target, both classes' events in it");
        let specs = &p.groups[0].specs;
        assert_eq!(
            specs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            [
                "instructions",
                "cycles",
                "dtlb:L2_TLB_MISS_DATA",
                "itlb:L1I_TLB_MISS_DEMAND",
                "l3:unlisted_0x23",
                "instructions",
                "cycles",
                "dtlb:L2_TLB_MISS_DATA",
                "itlb:L1I_TLB_MISS_DEMAND",
            ],
            "the same names on each PMU, so a task's readings fold; the E cores have no L3 event"
        );
        assert_eq!(
            specs[0].enc.config,
            (11 << 32) | 1,
            "instructions on the P PMU"
        );
        assert_eq!(specs[6].enc.config, 10 << 32, "cycles on the E PMU");
        assert_eq!((specs[2].enc.type_id, specs[2].enc.config), (11, 0x0b));
        assert_eq!((specs[7].enc.type_id, specs[7].enc.config), (10, 0x0b));
        assert_eq!((specs[4].enc.type_id, specs[4].enc.config), (11, 0x23));
        assert!(
            specs.iter().all(|s| s.core),
            "class PMUs take the mode asked for"
        );
        assert_eq!(p.fields["l3_misses_per_sec"].counters, ["l3:unlisted_0x23"]);
        assert_eq!(
            p.fields["l3_misses_per_sec"].provenance.method,
            Method::Perf
        );
        assert_eq!(p.fields["instructions_per_sec"].counters, ["instructions"]);
        assert_eq!(p.fields.len(), 5);
        assert!(
            p.notes
                .iter()
                .any(|n| n.contains("apple_avalanche_pmu only")
                    && n.contains("apple_blizzard_pmu has no event")),
            "{:?}",
            p.notes
        );

        // Per-CPU targets take the class the CPU belongs to.
        let host = plan(
            &ScopeInfo::Host { cpus: vec![0, 4] },
            &ALL,
            Some(m2()),
            &sysfs,
            &topo,
        )
        .unwrap();
        assert_eq!(host.groups.len(), 2);
        assert_eq!(host.groups[0].target, TargetPlan::Cpu(0));
        assert_eq!(
            host.groups[0]
                .specs
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            [
                "instructions",
                "cycles",
                "dtlb:L2_TLB_MISS_DATA",
                "itlb:L1I_TLB_MISS_DEMAND"
            ],
            "an E core: no L3 event"
        );
        assert!(
            host.groups[0]
                .specs
                .iter()
                .all(|s| s.enc.type_id == 10 || s.enc.config >> 32 == 10)
        );
        assert_eq!(host.groups[1].specs.len(), 5, "a P core: with the L3 event");
        assert!(
            host.groups[1]
                .specs
                .iter()
                .all(|s| s.enc.type_id == 11 || s.enc.config >> 32 == 11)
        );
        let cg = plan(
            &ScopeInfo::Cgroup {
                cpus: vec![5],
                tids: vec![],
            },
            &ALL,
            Some(m2()),
            &sysfs,
            &topo,
        )
        .unwrap();
        assert_eq!(cg.groups[0].target, TargetPlan::CgroupOnCpu(5));
        assert_eq!(cg.groups[0].specs.len(), 5);
    }

    #[test]
    fn process_scope_attaches_per_thread() {
        let p = plan(
            &ScopeInfo::Process { tids: vec![10, 11] },
            &USER,
            Some(zen4()),
            &sysfs(),
            &topo(),
        )
        .unwrap();
        assert_eq!(
            p.groups
                .iter()
                .map(|g| g.target.clone())
                .collect::<Vec<_>>(),
            [TargetPlan::Task(10), TargetPlan::Task(11)]
        );
        assert!(p.groups.iter().all(|g| g.opts.inherit));
        assert_eq!(
            p.fields["l3_misses_per_sec"].provenance.method,
            Method::Proxy
        );
        let denied = plan(
            &ScopeInfo::Process { tids: vec![10] },
            &NONE,
            Some(zen4()),
            &sysfs(),
            &topo(),
        )
        .unwrap();
        assert!(denied.is_empty());
        assert!(denied.notes[0].contains("not permitted"));
    }

    #[test]
    fn cgroup_scope_prefers_per_cpu_then_attach_then_nothing() {
        let cpus = vec![0, 33];
        let privileged = plan(
            &ScopeInfo::Cgroup {
                cpus: cpus.clone(),
                tids: vec![5],
            },
            &ALL,
            Some(zen4()),
            &sysfs(),
            &topo(),
        )
        .unwrap();
        let targets: Vec<_> = privileged.groups.iter().map(|g| g.target.clone()).collect();
        assert_eq!(
            targets,
            [
                TargetPlan::CgroupOnCpu(0),
                TargetPlan::CgroupOnCpu(33),
                TargetPlan::Cpu(0)
            ],
            "one uncore group, domain 0 only"
        );
        assert_eq!(
            privileged.groups[2].specs.len(),
            2,
            "one filtered counter per CPU"
        );
        assert_eq!(
            privileged.fields["l3_misses_per_sec"].provenance.scope,
            profile::Scope::Cgroup
        );
        assert_eq!(
            privileged.fields["instructions_per_sec"].provenance,
            Provenance {
                scope: profile::Scope::Cgroup,
                mode: Mode::All,
                method: Method::Perf
            }
        );
        assert!(privileged.notes.iter().any(|n| n.contains("other tenants")));

        let whole: Vec<u32> = topo().cpus.iter().map(|c| c.id).collect();
        let host_wide = plan(
            &ScopeInfo::Cgroup {
                cpus: whole,
                tids: vec![],
            },
            &ALL,
            Some(zen4()),
            &sysfs(),
            &topo(),
        )
        .unwrap();
        assert_eq!(
            host_wide.fields["l3_misses_per_sec"].provenance.scope,
            profile::Scope::Host,
            "every online CPU: whole domains"
        );
        assert!(host_wide.notes.iter().any(|n| n.contains("host-wide")));

        let attached = plan(
            &ScopeInfo::Cgroup {
                cpus: cpus.clone(),
                tids: vec![5, 6],
            },
            &USER,
            Some(zen4()),
            &sysfs(),
            &topo(),
        )
        .unwrap();
        assert_eq!(attached.groups.len(), 2);
        assert!(matches!(attached.groups[0].target, TargetPlan::Task(5)));
        assert_eq!(
            attached.fields["instructions_per_sec"].provenance,
            Provenance {
                scope: profile::Scope::Cgroup,
                mode: Mode::User,
                method: Method::Perf
            }
        );
        assert!(attached.notes.iter().any(|n| n.contains("join later")));

        let nothing = plan(
            &ScopeInfo::Cgroup {
                cpus,
                tids: vec![5],
            },
            &NONE,
            Some(zen4()),
            &sysfs(),
            &topo(),
        )
        .unwrap();
        assert!(nothing.is_empty());
        assert!(nothing.fields.is_empty());
    }

    #[test]
    fn host_scope_needs_per_cpu() {
        let cpus: Vec<u32> = topo().cpus.iter().map(|c| c.id).collect();
        let p = plan(
            &ScopeInfo::Host { cpus: cpus.clone() },
            &ALL,
            Some(zen4()),
            &sysfs(),
            &topo(),
        )
        .unwrap();
        assert_eq!(p.groups.len(), cpus.len() + 2);
        assert_eq!(
            p.fields["l3_misses_per_sec"].counters,
            [
                "l3:l3_lookup_state.l3_miss@domain0",
                "l3:l3_lookup_state.l3_miss@domain8"
            ]
        );
        let all_cores = &p.groups[cpus.len()].specs[0].enc;
        assert_eq!(
            all_cores.config,
            0x04 | (0x01 << 8) | (1u64 << 47) | (1u64 << 46)
        );
        assert_eq!(
            p.fields["l3_misses_per_sec"].provenance.scope,
            profile::Scope::Host
        );
        let denied = plan(
            &ScopeInfo::Host { cpus },
            &USER,
            Some(zen4()),
            &sysfs(),
            &topo(),
        )
        .unwrap();
        assert!(denied.is_empty());
    }

    #[test]
    fn no_table_or_no_access_means_no_counters() {
        let p = plan(
            &ScopeInfo::SelfThreads { pinned_cpus: None },
            &ALL,
            None,
            &sysfs(),
            &topo(),
        )
        .unwrap();
        assert!(p.is_empty() && p.fields.is_empty());
        assert!(p.notes[0].contains("no event table"));
        let p = plan(
            &ScopeInfo::SelfThreads { pinned_cpus: None },
            &NONE,
            Some(zen4()),
            &sysfs(),
            &topo(),
        )
        .unwrap();
        assert!(p.is_empty());
    }

    #[test]
    fn access_combines_rules_and_probe() {
        use crate::caps::{Facts, Root};
        let facts = Facts::detect(&Root::at(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/root"
        )));
        let rules = Expectations::from_facts(&facts);
        let probe = Probe {
            self_user: Ok(()),
            self_kernel: Err("EACCES".into()),
            per_cpu: Err("EACCES".into()),
        };
        assert_eq!(
            Access::new(&rules, &probe),
            Access {
                self_user: true,
                self_kernel: false,
                per_cpu: false,
                attach_same_uid: true
            }
        );
        let no_pmu = Probe {
            self_user: Err("ENOENT".into()),
            self_kernel: Err("ENOENT".into()),
            per_cpu: Err("ENOENT".into()),
        };
        assert_eq!(
            Access::new(&rules, &no_pmu),
            Access::default(),
            "no PMU: nothing, whatever the rules say"
        );
    }
}
