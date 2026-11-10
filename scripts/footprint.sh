#!/usr/bin/env bash
set -euo pipefail

targets="benchmarks/footprint.txt"
near_percent=80
idle_settle_seconds=2
ready_timeout_tenths=100

case "$(uname -s)" in
  Darwin) system="macos"; library="libeggdb_capi.dylib" ;;
  *) system="linux"; library="libeggdb_capi.so" ;;
esac

cargo build --release -p eggdb -p eggdb-capi
work="$(mktemp -d)"
cp target/release/eggdb "$work/eggdb"
cp "target/release/$library" "$work/$library"
strip "$work/eggdb" 2> /dev/null || true
strip -x "$work/$library" 2> /dev/null || true

measure_bytes() {
  wc -c < "$1" | tr -d ' '
}

serve_idle_bytes() {
  local output="$work/serve.out"
  EGGDB_PASSWORD=footprint "$work/eggdb" serve "$work/idle.db" --create --listen 127.0.0.1:0 > "$output" 2>&1 &
  local pid=$!
  local waited=0
  until [ -s "$output" ] || [ "$waited" -ge "$ready_timeout_tenths" ]; do
    sleep 0.1
    waited=$((waited + 1))
  done
  sleep "$idle_settle_seconds"
  local kib
  kib=$( (ps -o rss= -p "$pid" || true) | tr -d ' ')
  kill "$pid" 2> /dev/null || true
  wait "$pid" 2> /dev/null || true
  if [ -n "$kib" ]; then
    echo $((kib * 1024))
  fi
}

bench_value() {
  awk -v benchmark="$1" -v metric="$2" '$2 == benchmark && $3 == metric { print $4 }' "$work/bench.txt"
}

lines=()
lines+=("$system embedded c_api_library_bytes $(measure_bytes "$work/$library")")
lines+=("$system full eggdb_binary_bytes $(measure_bytes "$work/eggdb")")
if [ -f target/wasm32-wasip1/release/eggdb_wasm.wasm ]; then
  lines+=("any embedded wasm_module_bytes $(measure_bytes target/wasm32-wasip1/release/eggdb_wasm.wasm)")
fi
lines+=("$system full serve_idle_resident_bytes $(serve_idle_bytes)")

cargo run --release -p eggdb-bench -- --only footprint --results "$work/bench.txt" > "$work/bench.log" 2>&1 || true
if [ ! -s "$work/bench.txt" ]; then
  cat "$work/bench.log"
  echo "eggdb-bench --only footprint wrote no results"
  exit 2
fi
lines+=("$system full start_peak_resident_bytes $(bench_value footprint.start peak_resident_bytes)")
lines+=("$system full start_ratio $(bench_value footprint.start ratio_to_a_program_that_exits_at_once)")
lines+=("$system full open_and_get_peak_resident_bytes $(bench_value footprint.open_and_get peak_resident_bytes)")
lines+=("$system full open_and_get_ratio $(bench_value footprint.open_and_get ratio_to_a_program_that_exits_at_once)")

summary="${GITHUB_STEP_SUMMARY:-/dev/null}"
{
  echo "| System | Profile | Metric | Measured | Target | Share |"
  echo "|--------|---------|--------|----------|--------|-------|"
} >> "$summary"

failed=0
for line in "${lines[@]}"; do
  read -r scope profile name value <<< "$line"
  target=$(awk -v scope="$scope" -v profile="$profile" -v name="$name" '$1 == scope && $2 == profile && $3 == name { print $4 }' "$targets" 2> /dev/null || true)
  if [ -z "$value" ]; then
    echo "$scope $profile $name was not measured"
    failed=1
    continue
  fi
  if [ -z "$target" ]; then
    echo "$line (no target yet)"
    continue
  fi
  share=$(awk -v value="$value" -v target="$target" 'BEGIN { printf "%d", value * 100 / target }')
  if awk -v value="$value" -v target="$target" 'BEGIN { exit !(value > target) }'; then
    verdict="OVER its target of $target: trim it in this pass (D354)"
    failed=1
  elif [ "$share" -ge "$near_percent" ]; then
    verdict="NEAR its target of $target ($share%): trim it in this pass (D354)"
  else
    verdict="(target $target, $share%)"
  fi
  echo "$line $verdict"
  echo "| $scope | $profile | $name | $value | $target | $share% |" >> "$summary"
done
exit "$failed"
