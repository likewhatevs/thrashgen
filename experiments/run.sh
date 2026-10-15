#!/usr/bin/env bash
# Record a benchmark, replay its profile, record the replay the same way,
# and compare. Usage: experiments/run.sh NAME RECORD_SECS REPLAY_SECS -- command...
# Both the benchmark and the replay run in a transient user scope so the
# recorder sees a cgroup with the same accounting for each.
set -euo pipefail
name=$1 record_secs=$2 replay_secs=$3; shift 3
[ "$1" = "--" ] && shift
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/experiments/out/$name
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
"$bin" record --cgroup "$(scope_path "$unit")" --interval "$interval" --duration "$record_secs" -o "$out/orig.jsonl" 2> "$out/record-orig.log"
wait $bench_pid || echo "benchmark exited with $? (see benchmark.log)" >&2
"$bin" aggregate "$out/orig.jsonl" -o "$out/orig.json"

echo "== $name: replaying for ${replay_secs}s"
unit=thrashgen-$name-replay-$$
run_in_scope "$unit" "$bin" run "$out/orig.json" --duration "$replay_secs" --scratch-dir "$scratch" --report "$out/report.json" > "$out/run.log" 2>&1 &
replay_pid=$!
wait_for_scope "$(scope_path "$unit")"
# The comparison is against the replay as it holds its fit, not as it
# searches for it, so the recording starts after a warm-up of most of
# the replay; and the scope vanishes the moment the replay exits, so it
# stops a little earlier.
warmup=${WARMUP_SECS:-$((replay_secs * 6 / 10))}
sleep "$warmup"
"$bin" record --cgroup "$(scope_path "$unit")" --interval "$interval" --duration "$((replay_secs - warmup - 5))" -o "$out/replay.jsonl" 2> "$out/record-replay.log"
wait $replay_pid
"$bin" aggregate "$out/replay.jsonl" -o "$out/replay.json"
"$bin" compare "$out/orig.json" "$out/replay.json" --json "$out/compare.json" | tee "$out/compare.txt"
