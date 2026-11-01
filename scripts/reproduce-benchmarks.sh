#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: scripts/reproduce-benchmarks.sh <empty output directory>" >&2
  exit 2
fi
out="$1"
mkdir -p "$out"
cd "$(dirname "$0")/.."

{
  echo "commit: $(git rev-parse HEAD 2> /dev/null || echo unknown)"
  echo "system: $(uname -srm)"
  echo "toolchain: $(rustc --version)"
  case "$(uname -s)" in
    Darwin)
      echo "cpu: $(sysctl -n machdep.cpu.brand_string)"
      echo "cores: $(sysctl -n hw.ncpu)"
      echo "memory bytes: $(sysctl -n hw.memsize)"
      ;;
    *)
      echo "cpu: $(sed -n 's/^model name[[:space:]]*: //p' /proc/cpuinfo | head -n 1)"
      echo "cores: $(getconf _NPROCESSORS_ONLN)"
      echo "memory bytes: $(($(sed -n 's/^MemTotal:[[:space:]]*\([0-9]*\) kB/\1/p' /proc/meminfo) * 1024))"
      ;;
  esac
} > "$out/hardware.txt"

energy_counter=/sys/class/powercap/intel-rapl:0/energy_uj
read_energy() { if [ -r "$energy_counter" ]; then cat "$energy_counter"; else echo ""; fi; }

cargo build --release -p eggdb-bench
before="$(read_energy)"
cargo run --release -p eggdb-bench -- --results "$out/eggdb.txt" | tee "$out/eggdb-table.txt"
after="$(read_energy)"
if [ -n "$before" ] && [ -n "$after" ] && [ "$after" -ge "$before" ]; then
  echo "package energy during eggdb-bench: $(((after - before) / 1000)) millijoules (RAPL, package 0)" > "$out/energy.txt"
else
  echo "package energy: not measured (no readable RAPL counter at $energy_counter)" > "$out/energy.txt"
fi
cargo run --release --manifest-path crates/eggdb-sqlite-compare/Cargo.toml -- --results "$out/sqlite-compare.txt" | tee "$out/sqlite-compare-table.txt"
echo "results in $out: hardware.txt, eggdb.txt, sqlite-compare.txt, energy.txt"
