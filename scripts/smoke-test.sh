#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/smoke-test.sh <path to the eggdb binary>"
[ $# -eq 1 ] || { echo "$usage" >&2; exit 2; }
eggdb=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
samples=$(cd "$(dirname "$0")/../crates/eggdb/tests/data" && pwd)
[ -x "$eggdb" ] || { echo "smoke test: $eggdb is not an executable" >&2; exit 2; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"
export TERM=xterm-256color
unset NO_COLOR

passed=0
fail() {
  echo "smoke test FAILED: $*" >&2
  exit 1
}
pass() {
  passed=$((passed + 1))
  echo "ok   $*"
}
expect_exit() {
  local expected=$1 status=0
  shift
  "$@" > last.out 2> last.err || status=$?
  [ "$status" -eq "$expected" ] ||
    fail "$* exited $status, expected $expected; stderr: $(cat last.err)"
}
expect_output() {
  local expected=$1
  shift
  expect_exit 0 "$@"
  [ "$(cat last.out)" = "$expected" ] ||
    fail "$* printed '$(cat last.out)', expected '$expected'"
}

version=$("$eggdb" version)
case "$version" in
  "eggdb "[0-9]*.[0-9]*.[0-9]*) pass "version: $version" ;;
  *) fail "eggdb version printed '$version'" ;;
esac
expect_output "$version" "$eggdb" --version
expect_exit 0 "$eggdb" help
grep -q '^Usage: eggdb <command> <file>' last.out || fail "eggdb help has no usage line"
expect_exit 2 "$eggdb" frobnicate
pass "help, --version and an unknown command"

expect_exit 0 "$eggdb" create app.db
expect_exit 3 "$eggdb" create app.db
expect_exit 3 "$eggdb" get missing.db user:1
pass "create, and no file made by a wrong path"

expect_exit 0 "$eggdb" put app.db user:1 Ada
expect_exit 0 "$eggdb" put app.db user:2 Grace
expect_exit 0 "$eggdb" put app.db user:3 Margaret
expect_exit 0 "$eggdb" put app.db 0x00ff 0xdeadbeef
printf 'from stdin' | "$eggdb" put app.db piped > /dev/null
{ yes 'a large value on overflow pages' || true; } | head -c 3000000 > large.bin
expect_exit 0 "$eggdb" put app.db large @large.bin
expect_output Ada "$eggdb" get app.db user:1
expect_output 'from stdin' "$eggdb" get app.db piped
expect_output deadbeef "$eggdb" get app.db 0x00ff --format hex
expect_exit 0 "$eggdb" get app.db large
cmp -s last.out large.bin || fail "the large value came back different"
expect_exit 1 "$eggdb" get app.db user:9
pass "put and get: text, hex, stdin, a 3 MB value, a missing key"

expect_exit 0 "$eggdb" del app.db user:3
expect_exit 1 "$eggdb" del app.db user:3
expect_exit 1 "$eggdb" get app.db user:3
pass "del"

expect_exit 0 "$eggdb" scan app.db --prefix user: --format json
[ "$(cat last.out)" = '{"key":"user:1","value":"Ada"}
{"key":"user:2","value":"Grace"}' ] || fail "scan --prefix printed $(cat last.out)"
expect_output user:2 "$eggdb" scan app.db --prefix user: --reverse --limit 1 --keys-only --format raw
expect_exit 0 "$eggdb" scan app.db --keys-only --format json
[ "$(wc -l < last.out | tr -d ' ')" = 5 ] || fail "scan listed $(cat last.out)"
pass "scan: prefix, reverse, limit, keys only"

expect_exit 0 "$eggdb" check app.db --format json
grep -q '^{"ok":true,' last.out || fail "check reported $(cat last.out)"
grep -q '"entries":5,' last.out || fail "check counted $(cat last.out)"
grep -q '"leaked_pages":0}' last.out || fail "check found leaked pages: $(cat last.out)"
expect_exit 0 "$eggdb" stats app.db --format json
grep -q '"page_size":4096,' last.out || fail "stats reported $(cat last.out)"
pass "check and stats"

expect_exit 0 "$eggdb" export app.db app.jsonl
expect_exit 0 "$eggdb" import --create copy.db app.jsonl
expect_exit 0 "$eggdb" export copy.db copy.jsonl
tail -n +2 app.jsonl > app.entries
tail -n +2 copy.jsonl > copy.entries
cmp -s app.entries copy.entries || fail "the import differs from what was exported"
expect_exit 0 "$eggdb" get copy.db large
cmp -s last.out large.bin || fail "the imported large value differs"
expect_exit 3 "$eggdb" import copy.db app.jsonl
expect_exit 0 "$eggdb" import copy.db app.jsonl --on-conflict skip
pass "export and import round trip"

cp "$samples/v0.2.0.db" "$samples/v0.2.0-log-to-replay.db" .
expect_exit 0 "$eggdb" check v0.2.0.db
expect_output 'Grace Hopper' "$eggdb" get v0.2.0.db user:2
expect_exit 0 "$eggdb" scan v0.2.0.db --keys-only --format json
[ "$(wc -l < last.out | tr -d ' ')" = 286 ] || fail "the v0.2.0 sample lists $(wc -l < last.out) keys"
expect_exit 0 "$eggdb" check v0.2.0-log-to-replay.db
expect_output 'the last commit' "$eggdb" get v0.2.0-log-to-replay.db log:2
expect_exit 1 "$eggdb" get v0.2.0-log-to-replay.db large
pass "the v0.2.0 sample files: check, get, scan, a log to replay"

expect_exit 0 "$eggdb" ui --help
grep -q '^Usage: eggdb ui <file>' last.out || fail "eggdb ui --help printed $(cat last.out)"
expect_exit 2 "$eggdb" ui app.db < /dev/null
grep -q 'eggdb ui needs a terminal' last.err || fail "eggdb ui without a terminal said $(cat last.err)"
pass "ui --help, and ui refusing to run without a terminal"

run_in_terminal() {
  local command="stty cols 80 rows 24; exec $1"
  if script --version 2> /dev/null | grep -q util-linux; then
    script -q -e -c "$command" screen.log
  else
    script -q screen.log sh -c "$command"
  fi
}

if ! command -v script > /dev/null 2>&1 || [ "${OS:-}" = Windows_NT ]; then
  echo "skip ui in a terminal: no pseudo-terminal through script(1) on this system"
else
  expect_exit 0 "$eggdb" create ui.db
  for key in a:1 a:2 a:3; do expect_exit 0 "$eggdb" put ui.db "$key" "value of $key"; done
  status=0
  {
    sleep 1
    printf j
    sleep 1
    printf d
    sleep 1
    printf d
    sleep 1
    printf q
    sleep 1
  } | run_in_terminal "'$eggdb' ui ui.db" > /dev/null 2>&1 || status=$?
  [ "$status" -eq 0 ] || fail "eggdb ui in a terminal exited $status; screen: $(cat screen.log)"
  grep -q 'EggDB' screen.log || fail "eggdb ui drew no header: $(cat screen.log)"
  grep -q 'value of a:1' screen.log || fail "eggdb ui showed no value of a:1: $(cat screen.log)"
  grep -q 'Deleted a:2' screen.log || fail "eggdb ui reported no delete of a:2: $(cat screen.log)"
  expect_exit 1 "$eggdb" get ui.db a:2
  expect_output 'value of a:1' "$eggdb" get ui.db a:1
  expect_output 'value of a:3' "$eggdb" get ui.db a:3
  expect_exit 0 "$eggdb" check ui.db
  pass "ui in a terminal: open, move, delete with a confirmation, quit"
fi

echo "smoke test passed: $passed groups against $eggdb"
