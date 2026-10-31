#!/usr/bin/env bash
set -euo pipefail

budgets="benchmarks/footprint.txt"
update=0
if [ "${1:-}" = "--update" ]; then
  update=1
fi

cargo build --release -p eggdb -p eggdb-capi
work="$(mktemp -d)"
system="$(uname -s | tr 'A-Z' 'a-z')"
case "$system" in
  darwin) library="libeggdb_capi.dylib" ;;
  *) library="libeggdb_capi.so" ;;
esac
cp target/release/eggdb "$work/eggdb"
cp "target/release/$library" "$work/$library"
strip "$work/eggdb" 2> /dev/null || true
strip -x "$work/$library" 2> /dev/null || true

measure() {
  wc -c < "$1" | tr -d ' '
}

lines=()
lines+=("$system embedded c_api_library_bytes $(measure "$work/$library")")
lines+=("$system full eggdb_binary_bytes $(measure "$work/eggdb")")
if [ -f target/wasm32-wasip1/release/eggdb_wasm.wasm ]; then
  lines+=("any embedded wasm_module_bytes $(measure target/wasm32-wasip1/release/eggdb_wasm.wasm)")
fi

failed=0
for line in "${lines[@]}"; do
  read -r scope profile name value <<< "$line"
  budget=$(awk -v scope="$scope" -v profile="$profile" -v name="$name" '$1 == scope && $2 == profile && $3 == name { print $4 }' "$budgets" 2> /dev/null || true)
  if [ -z "$budget" ]; then
    echo "$line (no budget yet)"
  elif [ "$value" -gt "$budget" ]; then
    echo "$line OVER its budget of $budget: growth needs a stated reason (D6)"
    failed=1
  else
    echo "$line (budget $budget)"
  fi
done

if [ "$update" = 1 ]; then
  {
    grep -v "^$system " "$budgets" 2> /dev/null | grep -v "^any embedded wasm_module_bytes" || true
    printf '%s\n' "${lines[@]}"
  } | sort > "$work/budgets"
  cp "$work/budgets" "$budgets"
  echo "wrote $budgets"
  exit 0
fi
exit "$failed"
