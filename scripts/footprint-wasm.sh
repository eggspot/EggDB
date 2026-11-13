#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/footprint-wasm.sh [results file]

Builds the eggdb-wasm module in release for wasm32-wasip1 (the browser build web/eggdb.js loads)
and for wasm32-unknown-unknown (the module without WASI web/eggdb-bare.js loads), and prints
their sizes in benchmarks/footprint.txt's layout: module_bytes and module_gzip_bytes, the
download a browser makes with gzip. Each line is compared with its budget in
benchmarks/footprint.txt and the script fails when one is over (D6, D221, D1090). Needs both
targets (rustup target add wasm32-wasip1 wasm32-unknown-unknown) and gzip."

[ $# -le 1 ] || { echo "$usage" >&2; exit 2; }
results=${1:-}
root=$(cd "$(dirname "$0")/.." && pwd)
budgets="$root/benchmarks/footprint.txt"
cd "$root"
lines=()
for pair in "wasm32-wasip1 wasi" "wasm32-unknown-unknown bare"; do
  read -r target profile <<< "$pair"
  cargo build --release --locked -p eggdb-wasm --target "$target"
  module="target/$target/release/eggdb_wasm.wasm"
  bytes=$(wc -c < "$module" | tr -d ' ')
  gzip_bytes=$(gzip -9 -c "$module" | wc -c | tr -d ' ')
  lines+=("wasm32 $profile module_bytes $bytes" "wasm32 $profile module_gzip_bytes $gzip_bytes")
done
over=0
for line in "${lines[@]}"; do
  echo "$line"
  read -r system profile metric value <<< "$line"
  budget=$(awk -v s="$system" -v p="$profile" -v m="$metric" '$1 == s && $2 == p && $3 == m { print $4 }' "$budgets")
  if [ -n "$budget" ] && [ "$value" -gt "$budget" ]; then
    echo "over budget: $profile $metric is $value, the budget is $budget" >&2
    over=1
  fi
done
[ -z "$results" ] || printf '%s\n' "${lines[@]}" > "$results"
exit "$over"
