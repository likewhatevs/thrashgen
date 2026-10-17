#!/usr/bin/env bash
# The experiment set from docs/experiments.md, then the plots.
# MODE picks the privilege the set runs with, each to its own output
# directory under experiments/out:
#   user     (default) unprivileged, as a container would run it
#   kernel   perf_event_paranoid lowered to 1 for the run, so both
#            recordings count kernel mode too
#   resctrl  resctrl mounted with two monitoring groups chowned to the
#            user, so bandwidth is measured, not modelled
#   noisy    three profiles from a finished user-mode set replayed at
#            once, each in its own scope, as several containers would
#   perfmon  a copy of the binary carrying the CAP_PERFMON file
#            capability, so both sides count kernel mode, the recorder
#            attaches per cgroup, and L3 misses come from the L3 PMU
#            instead of the DRAM-fill proxy
# The kernel, resctrl and perfmon modes use sudo for those settings and
# restore them on exit.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
mode=${MODE:-user}
record=${RECORD_SECS:-120} replay=${FIT_SECS:-10}
export OUT_DIR=$root/experiments/out/$mode
mkdir -p "$OUT_DIR"
cargo build --release -q --manifest-path "$root/Cargo.toml"

case $mode in
  user) ;;
  noisy)
    "$root/experiments/noisy.sh" schbench stress-ng taobench
    cargo run --release -q --manifest-path "$root/Cargo.toml" --example experiment_plot -- "$OUT_DIR"
    exit 0
    ;;
  kernel)
    prev=$(sysctl -n kernel.perf_event_paranoid)
    sudo sysctl -q "kernel.perf_event_paranoid=1"
    trap 'sudo sysctl -q "kernel.perf_event_paranoid=$prev"' EXIT
    ;;
  resctrl)
    mounted_by_us=0
    if ! mountpoint -q /sys/fs/resctrl; then
      sudo mount -t resctrl resctrl /sys/fs/resctrl
      mounted_by_us=1
    fi
    groups=(/sys/fs/resctrl/mon_groups/thrashgen-a /sys/fs/resctrl/mon_groups/thrashgen-b)
    sudo mkdir -p "${groups[@]}"
    sudo chown -R "$(id -u)" "${groups[@]}"
    # shellcheck disable=SC2064
    trap "sudo rmdir ${groups[*]}; [ $mounted_by_us = 1 ] && sudo umount /sys/fs/resctrl" EXIT
    ;;
  perfmon)
    # File capabilities are ignored on nosuid mounts, which /tmp is on
    # most hosts; the copy lives next to the build.
    cap_bin=$root/target/perfmon/thrashgen
    mkdir -p "$(dirname "$cap_bin")"
    cp "$root/target/release/thrashgen" "$cap_bin"
    sudo setcap cap_perfmon+ep "$cap_bin"
    export THRASHGEN=$cap_bin
    # shellcheck disable=SC2064
    trap "rm -f $cap_bin" EXIT
    ;;
  *) echo "MODE must be user, kernel, resctrl, noisy or perfmon" >&2; exit 2 ;;
esac

fio_dir=${SCRATCH_DIR:-/var/tmp}/thrashgen-fio
mkdir -p "$fio_dir"
"$root/experiments/run.sh" schbench  "$record" "$replay" -- schbench -m 2 -t 4 -r "$record" -F 4096 -n 20
"$root/experiments/run.sh" stress-ng "$record" "$replay" -- stress-ng --vm 4 --vm-bytes 1G --vm-keep --vm-method write64 -t "$record"
"$root/experiments/run.sh" sysbench  "$record" "$replay" -- sysbench cpu --threads=4 --time="$record" run
"$root/experiments/run.sh" fio       "$record" "$replay" -- fio --name=pagecache --directory="$fio_dir" --rw=randread --bs=4k --size=1G --numjobs=2 --ioengine=psync --direct=0 --time_based --runtime="$record" --group_reporting
rm -rf "$fio_dir"
# TaoBench's cache starts empty and fills at its slow-path rate, about three
# minutes for 2 GiB; record it once the hit rate has settled.
BENCH_WARMUP_SECS=${TAOBENCH_WARMUP_SECS:-180} \
  "$root/experiments/run.sh" taobench "$record" "$replay" -- "$root/experiments/taobench.sh" "$((record + ${TAOBENCH_WARMUP_SECS:-180}))"
cargo run --release -q --manifest-path "$root/Cargo.toml" --example experiment_plot -- "$OUT_DIR"
