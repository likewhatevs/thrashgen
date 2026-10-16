#!/usr/bin/env bash
# The experiment set from docs/experiments.md, then the plots.
# MODE picks the privilege the set runs with, each to its own output
# directory under experiments/out:
#   user     (default) unprivileged, as a container would run it
#   kernel   perf_event_paranoid lowered to 1 for the run, so both
#            recordings count kernel mode too
#   resctrl  resctrl mounted with two monitoring groups chowned to the
#            user, so bandwidth is measured, not modelled
# The kernel and resctrl modes use sudo for those settings and restore
# them on exit.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
mode=${MODE:-user}
record=${RECORD_SECS:-120} replay=${REPLAY_SECS:-480}
export OUT_DIR=$root/experiments/out/$mode
mkdir -p "$OUT_DIR"
cargo build --release -q --manifest-path "$root/Cargo.toml"

case $mode in
  user) ;;
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
  *) echo "MODE must be user, kernel or resctrl" >&2; exit 2 ;;
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
