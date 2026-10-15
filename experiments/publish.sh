#!/usr/bin/env bash
# Copy the small, reviewable outputs of experiments/out into docs/experiments
# so the results travel with the repository. Recordings stay out.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/experiments/out
dest=$root/docs/experiments
mkdir -p "$dest"
for dir in "$out"/*/; do
  name=$(basename "$dir")
  [ -f "$dir/compare.json" ] || continue
  mkdir -p "$dest/$name"
  for f in ratio.svg cores.svg compare.txt compare.json orig.json replay.json report.json; do
    [ -f "$dir/$f" ] && cp "$dir/$f" "$dest/$name/$f"
  done
done
[ -f "$out/summary.svg" ] && cp "$out/summary.svg" "$dest/summary.svg"
echo "published to $dest"
