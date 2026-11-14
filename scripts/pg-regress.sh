#!/usr/bin/env bash
set -uo pipefail

if [ "$#" -ne 4 ]; then
  echo "usage: scripts/pg-regress.sh <eggdb binary> <src/test/regress directory> <work directory> <rates file>" >&2
  exit 2
fi
eggdb="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
regress="$(cd "$2" && pwd)"
work="$3"
rates="$4"
root="$(cd "$(dirname "$0")/.." && pwd)"
minimum="${PG_REGRESS_MINIMUM:-$(awk '$1 == "pg_regress" && $2 == "minimum_pass_percent" { print $3 }' "$root/benchmarks/conformance.txt")}"
minimum="${minimum:-0}"
export EGGDB_PASSWORD="${EGGDB_PASSWORD:-pg-regress}"
export PGPASSWORD="$EGGDB_PASSWORD"
export PG_ABS_SRCDIR="$regress"
export PG_ABS_BUILDDIR="$work"
export PG_LIBDIR="$work"
export PG_DLSUFFIX=".so"
port=55437

"$eggdb" serve "$work/regression.eggdb" --create --database regression --listen "127.0.0.1:$port" > "$work/serve.log" 2>&1 &
server=$!
trap 'kill "$server" 2> /dev/null || true' EXIT
for _ in $(seq 1 50); do
  (exec 3<> "/dev/tcp/127.0.0.1/$port") 2> /dev/null && break
  sleep 0.2
done

tests=$(awk '$1 == "test:" { for (i = 2; i <= NF; i++) print $i }' "$regress/parallel_schedule")
mkdir -p "$work/results"
passed=0
total=0
: > "$rates"
for name in $tests; do
  sql="$regress/sql/$name.sql"
  [ -f "$sql" ] || continue
  total=$((total + 1))
  out="$work/results/$name.out"
  (cd "$regress" && timeout 120 psql -X -a -q -h 127.0.0.1 -p "$port" -U eggdb -d regression \
    -v HIDE_TABLEAM=on -v HIDE_TOAST_COMPRESSION=on -f "$sql" > "$out" 2>&1)
  matched=0
  for expected in "$regress/expected/$name.out" "$regress"/expected/"$name"_[0-9].out; do
    [ -f "$expected" ] || continue
    if diff -q "$expected" "$out" > /dev/null; then
      matched=1
      break
    fi
  done
  if [ "$matched" -eq 1 ]; then
    passed=$((passed + 1))
    echo "pass $name" >> "$rates"
  else
    echo "fail $name" >> "$rates"
  fi
done

rate=0
if [ "$total" -gt 0 ]; then
  rate=$((passed * 100 / total))
fi
echo "PostgreSQL regression suite: $passed of $total tests match their expected output ($rate%)" | tee -a "$rates"
if [ "$rate" -lt "$minimum" ]; then
  echo "below the minimum of $minimum%" >&2
  exit 1
fi
