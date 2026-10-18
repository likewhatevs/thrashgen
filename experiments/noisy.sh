#!/usr/bin/env bash
# Replay several profiles at once, each in its own scope, each measured
# only on its own scope, the way several containers on one host would
# run the tool: they share the caches and the memory bus and see each
# other only through them. Usage: experiments/noisy.sh NAME... with the
# profiles taken from a finished user-mode set under SRC_DIR. COPIES
# replays each profile that many times at once, as copies of one
# container would, each under NAME-1, NAME-2 and so on.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
bin=${THRASHGEN:-$root/target/release/thrashgen}
src=${SRC_DIR:-$root/experiments/out/user}
out=${OUT_DIR:-$root/experiments/out/noisy}
scratch=${SCRATCH_DIR:-/var/tmp}
interval=${INTERVAL:-5}
fit_secs=${FIT_SECS:-10} hold_secs=${HOLD_SECS:-150}
copies=${COPIES:-1}
names=() srcs=()
for n in "$@"; do
  if [ "$copies" -gt 1 ]; then
    for i in $(seq 1 "$copies"); do names+=("$n-$i"); srcs+=("$n"); done
  else
    names+=("$n"); srcs+=("$n")
  fi
done

user_svc=user.slice/user-$(id -u).slice/user@$(id -u).service
run_in_scope() { local unit=$1; shift; systemd-run --user --scope --quiet --unit="$unit" --collect "$@"; }
scope_path() { echo "$user_svc/app.slice/$1.scope"; }
wait_for_scope() {
  local path=/sys/fs/cgroup/$1 _
  for _ in $(seq 1 50); do [ -d "$path" ] && return 0; sleep 0.1; done
  echo "scope $1 did not appear" >&2; return 1
}

for i in "${!names[@]}"; do
  mkdir -p "$out/${names[$i]}"
  cp "$src/${srcs[$i]}/orig.json" "$src/${srcs[$i]}/orig.jsonl" "$out/${names[$i]}/"
done

echo "== noisy: fitting ${names[*]} at once for ${fit_secs}s"
pids=()
for n in "${names[@]}"; do
  run_in_scope "thrashgen-noisy-$n-fit-$$" "$bin" run "$out/$n/orig.json" --duration "$fit_secs" --no-cache --scratch-dir "$scratch" --report "$out/$n/report.json" > "$out/$n/run.log" 2>&1 &
  pids+=($!)
done
wait "${pids[@]}"
for n in "${names[@]}"; do jq .knobs "$out/$n/report.json" > "$out/$n/knobs.json"; done

echo "== noisy: holding ${names[*]} at once for ${hold_secs}s"
pids=()
for n in "${names[@]}"; do
  run_in_scope "thrashgen-noisy-$n-replay-$$" "$bin" run "$out/$n/orig.json" --knobs "$out/$n/knobs.json" --no-counters --duration "$hold_secs" --scratch-dir "$scratch" --report "$out/$n/hold-report.json" > "$out/$n/hold.log" 2>&1 &
  pids+=($!)
done
for n in "${names[@]}"; do wait_for_scope "$(scope_path "thrashgen-noisy-$n-replay-$$")"; done
sleep 10
rec=()
for n in "${names[@]}"; do
  "$bin" record --cgroup "$(scope_path "thrashgen-noisy-$n-replay-$$")" --interval "$interval" --duration "$((hold_secs - 15))" -o "$out/$n/replay.jsonl" 2> "$out/$n/record-replay.log" &
  rec+=($!)
done
wait "${rec[@]}" "${pids[@]}"
for n in "${names[@]}"; do
  "$bin" aggregate "$out/$n/replay.jsonl" -o "$out/$n/replay.json"
  "$bin" compare "$out/$n/orig.json" "$out/$n/replay.json" --json "$out/$n/compare.json" | tee "$out/$n/compare.txt"
done
