#!/usr/bin/env bash
# Copy the small, reviewable outputs of experiments/out into docs/experiments
# so the results travel with the repository, one directory per mode.
# Recordings stay out.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/experiments/out
dest=$root/docs/experiments
mkdir -p "$dest"
for mode_dir in "$out"/*/; do
  mode=$(basename "$mode_dir")
  for dir in "$mode_dir"*/; do
    name=$(basename "$dir")
    [ -f "$dir/compare.json" ] || [ -f "$dir/compare-beside.json" ] || continue
    mkdir -p "$dest/$mode/$name"
    for f in ratio.svg cores.svg compare.txt compare.json orig.json replay.json report.json \
             phases.json alone.json beside.json after.json \
             compare-alone.json compare-beside.json compare-after.json \
             compare-alone.txt compare-beside.txt compare-after.txt; do
      [ -f "$dir/$f" ] && cp "$dir/$f" "$dest/$mode/$name/$f"
    done
  done
  [ -f "$mode_dir/summary.svg" ] && cp "$mode_dir/summary.svg" "$dest/$mode/summary.svg"
done
echo "published to $dest"
