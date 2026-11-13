#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/release-compat.sh <this build's eggdb> <the previous release's eggdb>"
[ $# -eq 2 ] || { echo "$usage" >&2; exit 2; }
new=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
old=$(cd "$(dirname "$2")" && pwd)/$(basename "$2")
for binary in "$new" "$old"; do
  [ -x "$binary" ] || { echo "release compat: $binary is not an executable" >&2; exit 2; }
done

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"

old_version=$("$old" version | awk '{print $2}')
old_release=${old_version%.*}
passed=0
fail() {
  echo "release compat FAILED: $*" >&2
  exit 1
}
pass() {
  passed=$((passed + 1))
  echo "ok   $*"
}
expect_output() {
  local expected=$1 got
  shift
  got=$("$@" 2> last.err) || fail "$* failed: $(cat last.err)"
  [ "$got" = "$expected" ] || fail "$* printed '$got', expected '$expected'"
}
count_of() {
  "$1" sql "$2" -c "COPY (SELECT count(*) FROM $3) TO STDOUT"
}

"$old" create upgraded.db
"$old" put upgraded.db greeting hello
"$old" sql upgraded.db -c "CREATE TABLE notes (id integer PRIMARY KEY, body text)"
"$old" sql upgraded.db -c "INSERT INTO notes VALUES (1, 'written by $old_version')"
"$new" check upgraded.db > /dev/null || fail "this build cannot check a file written by $old_version"
expect_output hello "$new" get upgraded.db greeting
expect_output 1 count_of "$new" upgraded.db notes
pass "upgrade: this build reads and checks a file written by $old_version"

"$new" put upgraded.db greeting again
"$new" sql upgraded.db -c "INSERT INTO notes VALUES (2, 'written by this build')"
"$old" check upgraded.db > /dev/null || fail "$old_version cannot check the file after this build wrote to it"
expect_output again "$old" get upgraded.db greeting
expect_output 2 count_of "$old" upgraded.db notes
"$old" sql upgraded.db -c "INSERT INTO notes VALUES (3, 'written by $old_version again')"
expect_output 3 count_of "$new" upgraded.db notes
pass "mixed versions: both write the same file in turn and read each other's writes"

"$new" create current.db
"$new" sql current.db -c "CREATE TABLE events (id integer PRIMARY KEY, body text) WITH (storage = lsm)"
"$new" sql current.db -c "INSERT INTO events SELECT g, 'event ' || g FROM generate_series(1, 500) AS g"
"$new" sql current.db -c "CREATE TABLE plain (id integer PRIMARY KEY)"
"$new" sql current.db -c "INSERT INTO plain VALUES (1), (2)"
"$new" put current.db greeting current
"$new" backup current.db downgraded.db --for-release "$old_release" > /dev/null ||
  fail "this build cannot copy its file for $old_release"
"$old" check downgraded.db > /dev/null || fail "$old_version cannot check the copy made for it"
expect_output 500 count_of "$old" downgraded.db events
expect_output 2 count_of "$old" downgraded.db plain
expect_output current "$old" get downgraded.db greeting
pass "downgrade: $old_version opens and checks the copy this build made for $old_release"

echo "release compat: $passed checks passed against $old_version"
