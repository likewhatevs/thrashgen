#!/usr/bin/env bash
# A replay fits and keeps fitting while a neighbour starts beside it
# and stops again: the live fit is recorded in three phases, alone,
# beside the neighbour, and alone again, and each phase is compared
# with the profile, so the comparison says whether the fit follows a
# real change in its surroundings and the report's windows say how
# soon. Usage: experiments/disturb.sh NAME... -- neighbour command...
# with the profiles taken from a finished user-mode set under SRC_DIR.
# The neighbour runs in a scope of its own and is stopped by the
# harness, so its command may run as long as it likes. Unlike run.sh,
# the recorder here shares the PMU with the replay's own fitter, since
# a fit that must follow its neighbours cannot give its counters up;
# the two agreed within one percent when that was measured.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
bin=${THRASHGEN:-$root/target/release/thrashgen}
src=${SRC_DIR:-$root/experiments/out/user}
out=${OUT_DIR:-$root/experiments/out/disturbed}
scratch=${SCRATCH_DIR:-/var/tmp}
settle=${SETTLE_SECS:-10} phase=${PHASE_SECS:-25} gap=${GAP_SECS:-5}
names=()
while [ $# -gt 0 ] && [ "$1" != "--" ]; do names+=("$1"); shift; done
[ "${1:-}" = "--" ] && shift
if [ ${#names[@]} -eq 0 ] || [ $# -eq 0 ]; then
  echo "usage: experiments/disturb.sh NAME... -- neighbour command..." >&2; exit 2
fi
neighbour=("$@")

user_svc=user.slice/user-$(id -u).slice/user@$(id -u).service
run_in_scope() { local unit=$1; shift; systemd-run --user --scope --quiet --unit="$unit" --collect "$@"; }
scope_path() { echo "$user_svc/app.slice/$1.scope"; }
wait_for_scope() {
  local path=/sys/fs/cgroup/$1 _
  for _ in $(seq 1 50); do [ -d "$path" ] && return 0; sleep 0.1; done
  echo "scope $1 did not appear" >&2; return 1
}
since() { awk "BEGIN { printf \"%.2f\", $2 - $1 }"; }

# The recorder stops before the replay's scope vanishes.
total=$((settle + 3 * phase + 2 * gap + 5))
for n in "${names[@]}"; do
  d=$out/$n
  mkdir -p "$d"
  cp "$src/$n/orig.json" "$d/orig.json"
  unit=thrashgen-disturb-$n-$$
  echo "== $n: a live fit for ${total}s, the neighbour from $((settle + phase)) s to $((settle + 2 * phase + gap)) s"
  t0=$(date +%s.%N)
  run_in_scope "$unit" "$bin" run "$d/orig.json" --duration "$total" --no-cache --scratch-dir "$scratch" --report "$d/report.json" > "$d/run.log" 2>&1 &
  run_pid=$!
  wait_for_scope "$(scope_path "$unit")"
  sleep "$settle"
  "$bin" record --cgroup "$(scope_path "$unit")" --interval 1 --duration "$phase" -o "$d/alone.jsonl" 2> "$d/record-alone.log"
  nunit=thrashgen-neighbour-$n-$$
  t1=$(date +%s.%N)
  run_in_scope "$nunit" "${neighbour[@]}" > "$d/neighbour.log" 2>&1 &
  neighbour_pid=$!
  sleep "$gap"
  "$bin" record --cgroup "$(scope_path "$unit")" --interval 1 --duration "$phase" -o "$d/beside.jsonl" 2> "$d/record-beside.log"
  systemctl --user stop "$nunit.scope" || true
  wait $neighbour_pid || true
  t2=$(date +%s.%N)
  sleep "$gap"
  "$bin" record --cgroup "$(scope_path "$unit")" --interval 1 --duration "$phase" -o "$d/after.jsonl" 2> "$d/record-after.log"
  wait $run_pid
  jq -n --argjson start "$(since "$t0" "$t1")" --argjson stop "$(since "$t0" "$t2")" \
    --argjson settle "$settle" --argjson phase "$phase" --argjson gap "$gap" \
    '{neighbour: $ARGS.named | {start_s: .start, stop_s: .stop},
      phases: {alone: [$settle, $settle + $phase],
               beside: [$settle + $phase + $gap, $settle + 2 * $phase + $gap],
               after: [$settle + 2 * $phase + 2 * $gap, $settle + 3 * $phase + 2 * $gap]}}' \
    > "$d/phases.json"
  for ph in alone beside after; do
    "$bin" aggregate "$d/$ph.jsonl" -o "$d/$ph.json"
    echo "-- $n $ph:"
    "$bin" compare "$d/orig.json" "$d/$ph.json" --json "$d/compare-$ph.json" | tee "$d/compare-$ph.txt"
  done
done
