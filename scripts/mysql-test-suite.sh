#!/usr/bin/env bash
set -uo pipefail

if [ "$#" -ne 4 ]; then
  echo "usage: scripts/mysql-test-suite.sh <eggdb binary> <mysql-test directory> <work directory> <rates file>" >&2
  exit 2
fi
eggdb="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
suite="$2"
work="$3"
rates="$4"
minimum="${MYSQL_TEST_MINIMUM:-0}"
export EGGDB_PASSWORD="${EGGDB_PASSWORD:-mysql-test}"
mysqltest="$(command -v mysqltest || true)"
if [ -z "$mysqltest" ]; then
  for candidate in /usr/lib/mysql-test/mysqltest /usr/bin/mysqltest /usr/lib/mysql/bin/mysqltest; do
    [ -x "$candidate" ] && mysqltest="$candidate" && break
  done
fi
if [ -z "$mysqltest" ]; then
  echo "no mysqltest binary: install MySQL's test suite package" >&2
  exit 2
fi

"$eggdb" serve "$work/mysql-test.eggdb" --create --listen 127.0.0.1:55436 --mysql-listen 127.0.0.1:53306 --database test &
server=$!
trap 'kill "$server" 2> /dev/null || true' EXIT
for _ in $(seq 1 50); do
  (exec 3<> /dev/tcp/127.0.0.1/53306) 2> /dev/null && break
  sleep 0.2
done

passed=0
total=0
: > "$rates"
for test_file in "$suite"/t/*.test; do
  name="$(basename "$test_file" .test)"
  result_file="$suite/r/$name.result"
  [ -f "$result_file" ] || continue
  total=$((total + 1))
  if timeout 60 "$mysqltest" --host=127.0.0.1 --port=53306 --user=eggdb --password="$EGGDB_PASSWORD" \
    --database=test --basedir="$suite" --tmpdir="$work" --logdir="$work" \
    --test-file="$test_file" --result-file="$result_file" > "$work/mysqltest-$name.log" 2>&1; then
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
echo "MySQL test suite: $passed of $total tests pass ($rate%)" | tee -a "$rates"
if [ "$rate" -lt "$minimum" ]; then
  echo "below the minimum of $minimum%" >&2
  exit 1
fi
