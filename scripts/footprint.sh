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

now_nanoseconds() {
  python3 -c 'import time; print(time.time_ns())'
}

serve_first_line_milliseconds() {
  local output="$work/first-line.out"
  local started
  started=$(now_nanoseconds)
  EGGDB_PASSWORD=footprint "$work/eggdb" serve "$work/first.db" --create --listen 127.0.0.1:0 > "$output" 2>&1 &
  local pid=$!
  local waited=0
  until [ -s "$output" ] || [ "$waited" -ge "$((ready_timeout_tenths * 10))" ]; do
    sleep 0.01
    waited=$((waited + 1))
  done
  local finished
  finished=$(now_nanoseconds)
  kill "$pid" 2> /dev/null || true
  wait "$pid" 2> /dev/null || true
  if [ -s "$output" ]; then
    echo $(((finished - started) / 1000000))
  fi
}

switches_of() {
  awk '/^voluntary_ctxt_switches|^nonvoluntary_ctxt_switches/ { total += $2 } END { print total + 0 }' "/proc/$1/status"
}

serve_idle_wakeups_per_minute() {
  [ -r /proc/self/status ] || return 0
  local output="$work/wakeups.out"
  EGGDB_PASSWORD=footprint "$work/eggdb" serve "$work/wakeups.db" --create --listen 127.0.0.1:0 > "$output" 2>&1 &
  local pid=$!
  sleep "$idle_settle_seconds"
  local before after
  before=$(switches_of "$pid")
  sleep 20
  after=$(switches_of "$pid")
  kill "$pid" 2> /dev/null || true
  wait "$pid" 2> /dev/null || true
  echo $(((after - before) * 3))
}

energy_counter=/sys/class/powercap/intel-rapl:0/energy_uj

millijoules_per_thousand() {
  [ -r "$energy_counter" ] || return 0
  local script=$1 queries=$2
  local before after
  before=$(cat "$energy_counter")
  "$work/eggdb" sql "$work/energy.db" --create < "$script" > /dev/null
  after=$(cat "$energy_counter")
  [ "$after" -ge "$before" ] || return 0
  echo $(((after - before) / queries))
}

energy_workloads() {
  [ -r "$energy_counter" ] || return 0
  local inserts="$work/inserts.sql" selects="$work/selects.sql" count=20000
  {
    echo "CREATE TABLE IF NOT EXISTS kv (id integer PRIMARY KEY, v text);"
    echo "BEGIN;"
    for id in $(seq 1 "$count"); do echo "INSERT INTO kv VALUES ($id, 'value $id');"; done
    echo "COMMIT;"
  } > "$inserts"
  for id in $(seq 1 "$count"); do echo "SELECT v FROM kv WHERE id = $id;"; done > "$selects"
  lines+=("$system full insert_millijoules_per_1000 $(millijoules_per_thousand "$inserts" "$count")")
  lines+=("$system full point_select_millijoules_per_1000 $(millijoules_per_thousand "$selects" "$count")")
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
lines+=("$system full serve_first_line_milliseconds $(serve_first_line_milliseconds)")
if [ -r /proc/self/status ]; then
  lines+=("$system full serve_idle_wakeups_per_minute $(serve_idle_wakeups_per_minute)")
fi
energy_workloads

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
lines+=("$system embedded disk_bytes_per_row $(bench_value footprint.disk bytes_per_row)")
lines+=("$system embedded disk_bytes_per_index_entry $(bench_value footprint.disk bytes_per_index_entry)")

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
