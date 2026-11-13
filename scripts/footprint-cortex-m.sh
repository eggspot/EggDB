#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/footprint-cortex-m.sh [results file]

Builds crates/eggdb-cortex-m for the Cortex-M3 in release, runs it under QEMU's mps2-an385 board,
and prints the microcontroller profile's footprint lines in benchmarks/footprint.txt's layout:
code_bytes (.text, .rodata and .data's image in flash, by llvm-size from rustup's llvm-tools),
and the run's own report of heap_peak_bytes, frame_bytes, static_bytes and ram_floor_bytes (the
heap it took, the page cache's frames and the engine's statics, the test's flash array and
heap reserve left out), and flash_write_amplification_milli and flash_erased_sectors (bytes
programmed into flash per byte committed, in thousandths, and sectors erased). Each line is
compared with its budget in benchmarks/footprint.txt and the script fails when one is over (D41,
D230, D1085, D1087). Needs qemu-system-arm and the
thumbv7m-none-eabi target."

[ $# -le 1 ] || { echo "$usage" >&2; exit 2; }
results=${1:-}
root=$(cd "$(dirname "$0")/.." && pwd)
budgets="$root/benchmarks/footprint.txt"
cd "$root/crates/eggdb-cortex-m"
cargo build --release --locked
elf=target/thumbv7m-none-eabi/release/eggdb-cortex-m
size_tool=$(find "$(rustc --print sysroot)/lib/rustlib" -maxdepth 3 -name llvm-size -type f | head -n 1)
[ -n "$size_tool" ] || { echo "llvm-size is missing: rustup component add llvm-tools" >&2; exit 1; }
code_bytes=$("$size_tool" -A "$elf" | awk '$1 == ".vector_table" || $1 == ".text" || $1 == ".rodata" || $1 == ".data" { total += $2 } END { print total + 0 }')
run=$(timeout 300 cargo run --release --locked 2>&1)
lines=("cortex-m3 engine code_bytes $code_bytes")
while IFS= read -r line; do
  case "$line" in
    "cortex-m3 engine "*) lines+=("$line") ;;
  esac
done <<< "$run"
over=0
for line in "${lines[@]}"; do
  echo "$line"
  read -r system profile metric value <<< "$line"
  budget=$(awk -v s="$system" -v p="$profile" -v m="$metric" '$1 == s && $2 == p && $3 == m { print $4 }' "$budgets")
  if [ -n "$budget" ] && [ "$value" -gt "$budget" ]; then
    echo "over budget: $metric is $value, the budget is $budget" >&2
    over=1
  fi
done
[ -z "$results" ] || printf '%s\n' "${lines[@]}" > "$results"
exit "$over"
