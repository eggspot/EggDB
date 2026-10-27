#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/smoke-test.sh <path to the eggdb binary>"
[ $# -eq 1 ] || { echo "$usage" >&2; exit 2; }
eggdb=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
samples=$(cd "$(dirname "$0")/../crates/eggdb/tests/data" && pwd)
[ -x "$eggdb" ] || { echo "smoke test: $eggdb is not an executable" >&2; exit 2; }

work=$(mktemp -d)
server_pid=""
trap '[ -z "$server_pid" ] || kill "$server_pid" 2> /dev/null || true; rm -rf "$work"' EXIT
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
[ ! -e missing.db ] || fail "get on a missing path created a file"
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

expect_exit 0 "$eggdb" sql --create shop.db -c "CREATE TABLE items (id integer PRIMARY KEY, name text NOT NULL, price double precision)"
printf '%s\n' "INSERT INTO items VALUES (1, 'egg', 0.25), (2, 'bread', 2.5);" "UPDATE items SET price = price * 2 WHERE id = 1;" > shop.sql
expect_exit 0 "$eggdb" sql shop.db shop.sql
expect_output 'egg|0.5' sh -c "\"$eggdb\" sql shop.db -c 'SELECT name, price FROM items WHERE id = 1' | sed -n 3p | tr -d ' '"
printf 'SELECT name FROM items ORDER BY price DESC LIMIT 1;\n' > top.sql
expect_exit 0 "$eggdb" sql shop.db - < top.sql
grep -q 'bread' last.out || fail "eggdb sql from stdin printed $(cat last.out)"
expect_exit 3 "$eggdb" sql shop.db -c "SELECT * FROM nope"
grep -q 'relation "nope" does not exist' last.err || fail "eggdb sql error said $(cat last.err)"
expect_exit 3 "$eggdb" sql shop.db -c "BEGIN; DELETE FROM items WHERE id = 1; INSERT INTO items VALUES (2, 'dup', 1)"
expect_output '2' sh -c "\"$eggdb\" sql shop.db -c 'SELECT id FROM items' | grep -c '^ *[0-9]'"
expect_exit 0 "$eggdb" sql shop.db -c "CREATE TABLE sales (id integer PRIMARY KEY, item_id integer, quantity integer); INSERT INTO sales VALUES (1, 1, 3), (2, 1, 4), (3, 2, 1), (4, 9, 5)"
expect_output 'egg|2|7' sh -c "\"$eggdb\" sql shop.db -c 'SELECT i.name, count(*), sum(s.quantity) FROM items i JOIN sales s ON s.item_id = i.id GROUP BY i.id ORDER BY 3 DESC' | sed -n 3p | tr -d ' '"
expect_output '2' sh -c "\"$eggdb\" sql shop.db -c 'SELECT i.name, s.quantity FROM items i LEFT JOIN sales s ON s.item_id = i.id AND s.quantity > 3' | sed -n '3,\$p' | grep -c '^ *[a-z]'"
expect_exit 3 "$eggdb" sql shop.db -c "SELECT name, count(*) FROM items GROUP BY price"
grep -q '42803' last.err || fail "eggdb sql grouping error said $(cat last.err)"
expect_exit 2 "$eggdb" sql shop.db -c "SELECT 1" extra.sql
expect_exit 0 "$eggdb" check shop.db
pass "sql: create, script, stdin, a join with GROUP BY, errors, a rolled-back block, usage"

expect_exit 2 env -u EGGDB_PASSWORD "$eggdb" serve shop.db
grep -q 'serve needs a password' last.err || fail "eggdb serve without a password said $(cat last.err)"
expect_exit 2 env EGGDB_PASSWORD=secret "$eggdb" serve shop.db --listen 0.0.0.0:5432
grep -q 'not a loopback address' last.err || fail "eggdb serve on 0.0.0.0 said $(cat last.err)"
if [ "${OS:-}" = Windows_NT ]; then
  echo "skip serve with clients: stopping a server gracefully needs SIGTERM, which Windows lacks"
else
  printf 'pg-secret\n' > password.txt
  "$eggdb" sql --create pg.db -c "CREATE TABLE items (id integer PRIMARY KEY, name text NOT NULL)" > /dev/null
  "$eggdb" serve pg.db --listen 127.0.0.1:0 --mysql-listen 127.0.0.1:0 --password-file password.txt > serve.out 2> serve.err &
  server_pid=$!
  for _ in $(seq 1 50); do
    [ -s serve.out ] && break
    sleep 0.1
  done
  port=$(sed -n 's/.*"port":\([0-9]*\).*/\1/p' serve.out | head -n 1)
  [ -n "$port" ] || fail "eggdb serve printed no port: $(cat serve.out) $(cat serve.err)"
  if command -v psql > /dev/null 2>&1; then
    export PGPASSWORD=pg-secret PGCONNECT_TIMEOUT=10
    connection="host=127.0.0.1 port=$port user=eggdb dbname=pg"
    psql "$connection" -v ON_ERROR_STOP=1 -At \
      -c "INSERT INTO items VALUES (1, 'egg'), (2, 'bread')" \
      -c "CREATE TABLE sales (id integer PRIMARY KEY, item_id integer, quantity integer)" \
      -c "INSERT INTO sales VALUES (1, 1, 3), (2, 2, 1), (3, 1, 4)" \
      -c "SELECT i.name, sum(s.quantity) FROM items i JOIN sales s ON s.item_id = i.id GROUP BY i.name ORDER BY i.name" \
      > psql.out 2> psql.err || fail "psql failed: $(cat psql.err)"
    [ "$(tail -n 2 psql.out | tr '\n' ' ')" = "bread|1 egg|7 " ] || fail "psql printed $(cat psql.out)"
    psql "$connection" -At -c 'SELECT * FROM missing' > psql.out 2> psql.err && fail "psql ran a query on a missing table"
    grep -q 'relation "missing" does not exist' psql.err || fail "psql error said $(cat psql.err)"
    PGPASSWORD=wrong psql "$connection" -At -c 'SELECT 1' > psql.out 2> psql.err && fail "psql connected with a wrong password"
    grep -q 'password authentication failed' psql.err || fail "a wrong password said $(cat psql.err)"
    pass "serve: psql creates, inserts and joins; errors and a wrong password are refused"
  else
    echo "skip psql: psql is not on PATH (CI's Linux job installs postgresql-client)"
  fi
  python="${EGGDB_SMOKE_PYTHON:-python3}"
  if command -v "$python" > /dev/null 2>&1 && "$python" -c 'import pg8000' > /dev/null 2>&1; then
    "$python" - "$port" > driver.out 2> driver.err << 'PYTHON' || fail "pg8000 failed: $(cat driver.err)"
import sys
import pg8000.native

connection = pg8000.native.Connection(
    "eggdb", host="127.0.0.1", port=int(sys.argv[1]), database="pg", password="pg-secret"
)
connection.run("INSERT INTO items VALUES (:id, :name)", id=3, name="jam")
print(connection.run("SELECT name FROM items WHERE id = :id", id=3))
print(connection.run("SELECT count(*) FROM items WHERE id > :low", low=0))
connection.close()
PYTHON
    [ "$(tr -d '\n' < driver.out)" = "[['jam']][[3]]" ] || fail "pg8000 printed $(cat driver.out)"
    pass "serve: the pg8000 driver runs parameterized queries"
  else
    echo "skip driver: pg8000 is not installed (CI's Linux job installs it)"
  fi
  mysql_port=$(sed -n 's/.*"mysql_port":\([0-9]*\).*/\1/p' serve.out | head -n 1)
  [ -n "$mysql_port" ] || fail "eggdb serve printed no MySQL port: $(cat serve.out)"
  if command -v mysql > /dev/null 2>&1; then
    mysql_client() {
      MYSQL_PWD="$1" mysql -h 127.0.0.1 -P "$mysql_port" -u eggdb -D pg --batch --skip-column-names --connect-timeout=10 -e "$2"
    }
    mysql_client pg-secret "INSERT INTO items VALUES (4, 'tea'); SELECT \`name\` FROM items WHERE id = 4; SELECT @@version_comment" \
      > mysql.out 2> mysql.err || fail "mysql failed: $(cat mysql.err)"
    [ "$(tr '\n' ' ' < mysql.out)" = "tea EggDB " ] || fail "mysql printed $(cat mysql.out)"
    mysql_client pg-secret "SELECT * FROM missing" > mysql.out 2> mysql.err && fail "mysql ran a query on a missing table"
    grep -q "ERROR 1146 (42S02).*Table 'pg.missing' doesn't exist" mysql.err || fail "mysql error said $(cat mysql.err)"
    mysql_client wrong "SELECT 1" > mysql.out 2> mysql.err && fail "mysql connected with a wrong password"
    grep -q 'ERROR 1045' mysql.err || fail "a wrong MySQL password said $(cat mysql.err)"
    pass "serve: mysql inserts and selects; errors and a wrong password are refused"
  else
    echo "skip mysql: the mysql client is not on PATH (CI's Linux job installs mysql-client)"
  fi
  if command -v "$python" > /dev/null 2>&1 && "$python" -c 'import pymysql' > /dev/null 2>&1; then
    "$python" - "$mysql_port" > driver.out 2> driver.err << 'PYTHON' || fail "PyMySQL failed: $(cat driver.err)"
import sys
import pymysql

connection = pymysql.connect(
    host="127.0.0.1", port=int(sys.argv[1]), user="eggdb", password="pg-secret", database="pg"
)
with connection.cursor() as cursor:
    cursor.execute("INSERT INTO items VALUES (%s, %s)", (5, "honey"))
    connection.commit()
    cursor.execute("SELECT name FROM items WHERE id = %s", (5,))
    print(cursor.fetchall())
connection.close()
PYTHON
    [ "$(tr -d '\n' < driver.out)" = "(('honey',),)" ] || fail "PyMySQL printed $(cat driver.out)"
    pass "serve: the PyMySQL driver runs parameterized queries"
  else
    echo "skip driver: PyMySQL is not installed (CI's Linux job installs it)"
  fi
  kill -TERM "$server_pid"
  status=0
  wait "$server_pid" || status=$?
  server_pid=""
  [ "$status" -eq 0 ] || fail "eggdb serve exited $status after SIGTERM: $(cat serve.err)"
  grep -q '"stopped":true' serve.out || fail "eggdb serve printed $(cat serve.out)"
  expect_exit 0 "$eggdb" check pg.db
  pass "serve: listens on loopback, stops on SIGTERM with exit 0, leaves the file healthy"
fi

cp "$samples/v0.2.0.db" "$samples/v0.2.0-log-to-replay.db" .
expect_exit 0 "$eggdb" check v0.2.0.db
expect_output 'Grace Hopper' "$eggdb" get v0.2.0.db user:2
expect_exit 0 "$eggdb" scan v0.2.0.db --keys-only --format json
[ "$(wc -l < last.out | tr -d ' ')" = 286 ] || fail "the v0.2.0 sample lists $(wc -l < last.out) keys"
expect_exit 0 "$eggdb" check v0.2.0-log-to-replay.db
expect_output 'the last commit' "$eggdb" get v0.2.0-log-to-replay.db log:2
expect_exit 1 "$eggdb" get v0.2.0-log-to-replay.db large
pass "the v0.2.0 sample files: check, get, scan, a log to replay"

expect_exit 0 "$eggdb" create shell.db
printf '%s\n' 'put user:1 Ada' 'create table t (id integer primary key, name text);' \
  "insert into t values (1, 'x')," "  (2, 'y');" 'select name from t order by id;' '.tables' 'get user:1' > shell.in
expect_exit 0 "$eggdb" shell shell.db < shell.in
[ "$(tr '\n' '|' < last.out)" = "CREATE TABLE|INSERT 0 2| name|------| x| y|(2 rows)||t|Ada|" ] ||
  fail "eggdb shell with piped lines printed $(cat last.out)"
grep -q "$(printf '\033')" last.out && fail "eggdb shell wrote escape sequences into a pipe"
expect_exit 3 "$eggdb" shell shell.db -c 'select 1/0;' -c 'get user:1'
grep -q 'SQLSTATE:  22012' last.err || fail "eggdb shell -c said $(cat last.err)"
[ ! -s last.out ] || fail "eggdb shell ran on after the first failure: $(cat last.out)"
expect_exit 1 "$eggdb" shell shell.db --keep-going -c 'get nobody' -c 'select count(*) from t'
grep -q '     2' last.out || fail "eggdb shell --keep-going printed $(cat last.out)"
pass "shell: piped lines and -c, key-value commands, SQL and meta-commands, stopping at the first failure"

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
  status=0
  {
    sleep 1
    printf s
    sleep 1
    printf 'SELECT 6 * 7 AS answer;'
    sleep 1
    printf '\033[15~'
    sleep 1
    printf '\003'
    sleep 1
  } | run_in_terminal "'$eggdb' ui ui.db" > /dev/null 2>&1 || status=$?
  [ "$status" -eq 0 ] || fail "eggdb ui's query screen exited $status; screen: $(cat screen.log)"
  grep -q 'answer' screen.log || fail "the query screen showed no column: $(cat screen.log)"
  grep -q '42' screen.log || fail "the query screen showed no result: $(cat screen.log)"
  grep -q '(1 row)' screen.log || fail "the query screen showed no row count: $(cat screen.log)"
  pass "ui in a terminal: the query screen runs SQL with F5 and shows the result"
  status=0
  {
    sleep 1
    printf 'select 40 + 2 as answer;\r'
    sleep 1
    printf '.quit\r'
    sleep 1
  } | EGGDB_HISTORY="$work/history" run_in_terminal "'$eggdb' shell ui.db" > /dev/null 2>&1 || status=$?
  [ "$status" -eq 0 ] || fail "eggdb shell in a terminal exited $status; screen: $(cat screen.log)"
  grep -q 'Type .help for help' screen.log || fail "eggdb shell showed no banner: $(cat screen.log)"
  grep -q 'ui=> ' screen.log || fail "eggdb shell showed no prompt: $(cat screen.log)"
  grep -q '42' screen.log || fail "eggdb shell showed no result: $(cat screen.log)"
  grep -qx 'select 40 + 2 as answer;' "$work/history" || fail "eggdb shell kept no history: $(cat "$work/history")"
  pass "shell in a terminal: banner, prompt, a statement, history, .quit"
fi

echo "smoke test passed: $passed groups against $eggdb"
