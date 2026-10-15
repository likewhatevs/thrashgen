#!/usr/bin/env bash
# The experiment set from docs/experiments.md, then the plots.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
record=${RECORD_SECS:-120} replay=${REPLAY_SECS:-420}
cargo build --release -q --manifest-path "$root/Cargo.toml"
fio_dir=${SCRATCH_DIR:-/var/tmp}/thrashgen-fio
mkdir -p "$fio_dir"
"$root/experiments/run.sh" schbench  "$record" "$replay" -- schbench -m 2 -t 4 -r "$record" -F 4096 -n 20
"$root/experiments/run.sh" stress-ng "$record" "$replay" -- stress-ng --vm 4 --vm-bytes 1G --vm-keep --vm-method write64 -t "$record"
"$root/experiments/run.sh" sysbench  "$record" "$replay" -- sysbench cpu --threads=4 --time="$record" run
"$root/experiments/run.sh" fio       "$record" "$replay" -- fio --name=pagecache --directory="$fio_dir" --rw=randread --bs=4k --size=1G --numjobs=2 --ioengine=psync --direct=0 --time_based --runtime="$record" --group_reporting
rm -rf "$fio_dir"
cargo run --release -q --manifest-path "$root/Cargo.toml" --example experiment_plot -- "$root/experiments/out"
