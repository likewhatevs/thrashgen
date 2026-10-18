# Privileges

What each level of privilege unlocks, as checked against the kernel
source at v7.1 (`kernel/events/core.c`, `include/linux/perf_event.h`,
`arch/x86/events/amd/uncore.c`, `fs/resctrl/rdtgroup.c`).

| capability | unlocks |
| --- | --- |
| none, `perf_event_paranoid` at the default 2 | counters on the tool's own threads, user mode only, any event the core PMU offers, including raw vendor events |
| `perf_event_paranoid` at most 1 | kernel-mode counting on the tool's own threads |
| `perf_event_paranoid` at most 0, or `CAP_PERFMON` | per-CPU events, per-cgroup events (which are per-CPU by construction), uncore PMUs such as AMD's L3 counters, which reject task-scoped events |
| same uid as the target, ptrace allowed by yama | attaching counters to another process's existing threads |
| `CAP_PERFMON` | attaching to any process or cgroup, all modes |
| resctrl mounted and a monitoring group owned by the tool's uid | per-scope DRAM bandwidth; the kernel lets a task's owner or root write that task into a group, so the tool uses an idle group it can write when it cannot make one |
| root | mounting resctrl and creating groups |
| Yama `ptrace_scope` at most 1 | holding a recorded command at its exec with `PTRACE_TRACEME` so counters attach before it runs; otherwise the command starts first and the note says so |

`CAP_PERFMON` can be given to the binary alone as a file capability,
`setcap cap_perfmon+ep thrashgen`, which an unprivileged user then
exercises without root; the kernel ignores file capabilities on
`nosuid` mounts, which `/tmp` usually is, so the binary must sit
elsewhere. Inside a container the default seccomp profile of podman and
docker returns an error for `perf_event_open` unless the container
holds `CAP_PERFMON`, `CAP_SYS_ADMIN` or `CAP_BPF`. `--cap-add=PERFMON` on a
rootful container gives everything in the `CAP_PERFMON` rows. In a
rootless container the capability is not effective in the initial user
namespace, so the tool is back to the first row.

## Degradation

| observable | no privileges | plus `CAP_PERFMON` | plus a resctrl group |
| --- | --- | --- | --- |
| busy cores, burstiness | verified from rusage or cgroup `cpu.stat` | same | same |
| anon, page cache, shmem | verified from `/proc` and `cachestat` | same | same |
| IPC, dTLB, iTLB | verified, user mode only | verified, all modes | same |
| L3 misses on Intel and ARM | verified from core events | same | same |
| L3 misses on AMD | proxy from the core-side count of line fills sourced from DRAM; calibrated against the L3 PMU once | verified from the L3 PMU with core filters | same |
| DRAM bandwidth | modelled from counted fills and the bytes the generator wrote non-temporally or dirtied; reported, not fitted | same | verified, and fitted |

The report names the tier each target was checked at. "verified" means
measured with the same definition the recorder uses; "proxy" means a
different counter that tracks it, with the calibration ratio printed;
"modelled" means computed from what the generator knows it emitted;
"unverified" means run open-loop from a calibration or built-in table.
