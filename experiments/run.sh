#!/usr/bin/env bash
# Record a benchmark, replay its profile, record the replay the same way,
# and compare. Usage: experiments/run.sh NAME RECORD_SECS FIT_SECS -- command...
# Both the benchmark and the replay run in a transient user scope so the
# recorder sees a cgroup with the same accounting for each. The replay
# is two runs: one fits the knobs for FIT_SECS, then the knobs it ended
# on run again for HOLD_SECS with no counters of their own, and that run
# is what is recorded, so the recorder never shares the PMU with the
# replay's own fitter. OUT_DIR chooses where NAME's outputs go;
# BENCH_WARMUP_SECS delays the recording for a benchmark that needs to
# fill up first, and the command must then run that much longer than
# RECORD_SECS. The fit starts cold: the experiments measure the fit,
# and the cache of earlier fits is the product's, not the experiment's.
set -euo pipefail
name=$1 record_secs=$2 fit_secs=$3; shift 3
hold_secs=${HOLD_SECS:-150}
[ "$1" = "--" ] && shift
root=$(cd "$(dirname "$0")/.." && pwd)
out=${OUT_DIR:-$root/experiments/out}/$name
bin=${THRASHGEN:-$root/target/release/thrashgen}
scratch=${SCRATCH_DIR:-/var/tmp}
interval=${INTERVAL:-5}
mkdir -p "$out"

user_svc=user.slice/user-$(id -u).slice/user@$(id -u).service
run_in_scope() {        # unit-name, then the command
  local unit=$1; shift
  systemd-run --user --scope --quiet --unit="$unit" --collect "$@"
}
scope_path() { echo "$user_svc/app.slice/$1.scope"; }
wait_for_scope() {      # the scope appears a moment after systemd-run starts
  local path=/sys/fs/cgroup/$1 i
  for i in $(seq 1 50); do [ -d "$path" ] && return 0; sleep 0.1; done
  echo "scope $1 did not appear" >&2; return 1
}

unit=thrashgen-$name-$$
echo "== $name: recording the benchmark for ${record_secs}s"
run_in_scope "$unit" "$@" > "$out/benchmark.log" 2>&1 &
bench_pid=$!
wait_for_scope "$(scope_path "$unit")"
sleep "${BENCH_WARMUP_SECS:-0}"
"$bin" record --cgroup "$(scope_path "$unit")" --interval "$interval" --duration "$record_secs" -o "$out/orig.jsonl" 2> "$out/record-orig.log"
wait $bench_pid || echo "benchmark exited with $? (see benchmark.log)" >&2
"$bin" aggregate "$out/orig.jsonl" -o "$out/orig.json"

echo "== $name: fitting for ${fit_secs}s"
unit=thrashgen-$name-fit-$$
run_in_scope "$unit" "$bin" run "$out/orig.json" --duration "$fit_secs" --no-cache --scratch-dir "$scratch" --report "$out/report.json" > "$out/run.log" 2>&1
jq .knobs "$out/report.json" > "$out/knobs.json"

echo "== $name: holding the fitted knobs for ${hold_secs}s"
unit=thrashgen-$name-replay-$$
run_in_scope "$unit" "$bin" run "$out/orig.json" --knobs "$out/knobs.json" --no-counters --duration "$hold_secs" --scratch-dir "$scratch" --report "$out/hold-report.json" > "$out/hold.log" 2>&1 &
replay_pid=$!
wait_for_scope "$(scope_path "$unit")"
# The footprint settles in the first seconds; the scope vanishes the
# moment the replay exits, so the recording stops a little earlier.
sleep 10
"$bin" record --cgroup "$(scope_path "$unit")" --interval "$interval" --duration "$((hold_secs - 15))" -o "$out/replay.jsonl" 2> "$out/record-replay.log"
wait $replay_pid
"$bin" aggregate "$out/replay.jsonl" -o "$out/replay.json"
"$bin" compare "$out/orig.json" "$out/replay.json" --json "$out/compare.json" | tee "$out/compare.txt"
