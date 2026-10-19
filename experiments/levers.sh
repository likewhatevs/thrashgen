#!/usr/bin/env bash
# Measure what each lever of the generator costs per unit, instructions
# and the events it brings, at fixed knobs on this host: the figures
# `docs/status.md` quotes for the reference host, so a change to the
# access path, or a compiler, is checked by measurement. Each lever is
# run at two settings with everything else at one op and no other
# lever, and the slope between them is the cost per unit; page-cache
# reads need a profile whose page cache outgrows the L3 (KREAD_PROFILE).
# Usage: experiments/levers.sh [REPS] with THRASHGEN, PROFILE,
# KREAD_PROFILE, SECS and OUT_DIR to override the defaults.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
bin=${THRASHGEN:-$root/target/release/thrashgen}
profile=${PROFILE:-$root/docs/experiments/user/stress-ng/orig.json}
kread_profile=${KREAD_PROFILE:-$root/docs/experiments/user/fio/orig.json}
out=${OUT_DIR:-$root/experiments/out/levers}
scratch=${SCRATCH_DIR:-/var/tmp}
secs=${SECS:-4}
reps=${1:-2}
mkdir -p "$out"

# A short cold fit gives a knobs file with this host's set sizes and
# thread count; every lever is then set outright.
base() {
  local prof=$1 name=$2
  "$bin" run "$prof" --duration 2 --no-cache --scratch-dir "$scratch" --report "$out/base-$name.json" > "$out/base-$name.log" 2>&1
  jq '.knobs | .duty=1 | .cv=0 | .ops=1 | .chains=1 | .lines=0.001 | .seq=0 | .dep=0 | .wset=1 | .wfrac=0.25 | .ntlines=0 | .tlbvisits=0 | .codevisits=0 | .kreads=0' "$out/base-$name.json"
}
main_base=$(base "$profile" main)
kread_base=$(base "$kread_profile" kread)

# name, base, jq edits
declare -A CONF=(
  [O1]="main ."                 [O256]="main .ops=256"
  [L1]="main .lines=1"          [L8]="main .lines=8"        [L64]="main .lines=64"
  [T1]="main .tlbvisits=1"      [T16]="main .tlbvisits=16"
  [C1]="main .codevisits=1"     [C16]="main .codevisits=16"
  [NT0]="main .lines=8"         [NT64]="main .lines=8|.ntlines=64"
  [K0]="kread ."                [K4]="kread .kreads=4"
)
order=(O1 O256 L1 L8 L64 T1 T16 C1 C16 NT0 NT64 K0 K4)
for name in "${order[@]}"; do
  read -r which edits <<< "${CONF[$name]}"
  [ "$which" = main ] && b=$main_base || b=$kread_base
  jq "$edits" <<< "$b" > "$out/k-$name.json"
done
for rep in $(seq 1 "$reps"); do
  for name in "${order[@]}"; do
    read -r which _ <<< "${CONF[$name]}"
    [ "$which" = main ] && prof=$profile || prof=$kread_profile
    "$bin" run "$prof" --knobs "$out/k-$name.json" --duration "$secs" --no-cache --scratch-dir "$scratch" \
      --report "$out/r-$name-$rep.json" > "$out/r-$name-$rep.log" 2>&1 || echo "$name run $rep failed: $(tail -1 "$out/r-$name-$rep.log")"
  done
done

python3 - "$out" "$secs" <<'PY'
import glob, json, statistics, sys
out, secs = sys.argv[1], float(sys.argv[2])
def windows(name):
    ws = []
    for f in sorted(glob.glob(f"{out}/r-{name}-[0-9]*.json")):
        ws += [w for w in json.load(open(f))["windows"] if w["end_s"] > 1.0]
    return ws
def mean(ws, key):
    if key == "ipa":
        return statistics.mean(w["instructions_per_access"] for w in ws)
    xs = [v["measured"] for w in ws for v in w["values"] if v["key"] == key and v["measured"] is not None]
    return statistics.mean(xs) if xs else 0.0
def per_access(ws, key):
    return mean(ws, key) * mean(ws, "ipa") / 1000
levers = [("op", "O1", "O256", 255, "l3_mpki", "misses"),
          ("line, 1 to 8", "L1", "L8", 7, "l3_mpki", "misses"),
          ("line, 8 to 64", "L8", "L64", 56, "l3_mpki", "misses"),
          ("sparse visit", "T1", "T16", 15, "dtlb_pki", "dTLB"),
          ("stub call", "C1", "C16", 15, "itlb_pki", "iTLB"),
          ("non-temporal line", "NT0", "NT64", 64, "l3_mpki", "misses"),
          ("page-cache read", "K0", "K4", 4, "l3_mpki", "misses")]
print(f"{'lever':20} {'instructions':>12} {'events':>8}  event")
for label, a, b, n, key, what in levers:
    wa, wb = windows(a), windows(b)
    if not wa or not wb:
        print(f"{label:20} {'-':>12}")
        continue
    di = (mean(wb, "ipa") - mean(wa, "ipa")) / n
    de = (per_access(wb, key) - per_access(wa, key)) / n
    print(f"{label:20} {di:12.2f} {de:8.3f}  {what}")
w = windows("O1")
if w:
    print(f"{'one op, no lever':20} {mean(w, 'ipa'):12.1f} {'':8}  instructions an access, IPC {mean(w, 'ipc'):.2f}")
PY
