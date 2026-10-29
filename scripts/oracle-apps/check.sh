#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/oracle-apps/check.sh <eggdb binary> <work directory>

Restores Oracle's HR sample schema (crates/eggdb/tests/data/oracle/hr/hr_install.sql, unchanged)
with eggdb restore --dialect oracle, serves it with eggdb serve as the user hr, and runs Oracle
SQL through PostgreSQL's own clients with the dialect chosen per connection
(options=-c eggdb.dialect=oracle): psql runs scripts/oracle-apps/commands.sql (HR's queries,
the data dictionary, SYS_CONTEXT and NLS_SESSION_PARAMETERS) and
crates/eggdb/tests/clients/java/OracleAppCheck.java runs prepared statements and a transaction
through pgjdbc. Fails unless psql prints exactly scripts/oracle-apps/expected-psql.out and the
Java program exactly scripts/oracle-apps/expected-java.out, written from HR's documented data.

PSQL names the psql command (default: psql from PG_BIN or PATH), for example
\"docker run --rm -i --network host -e PGPASSWORD -e PGOPTIONS postgres:17 psql\"; PGJDBC_JAR
names the pgjdbc jar and JAVA the java launcher (default java)."

[ $# -eq 2 ] || { echo "$usage" >&2; exit 2; }
eggdb=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
work=$2
here=$(cd "$(dirname "$0")" && pwd)
clients=$(cd "$here/../../crates/eggdb/tests/clients" && pwd)
installer=$(cd "$here/../../crates/eggdb/tests/data/oracle/hr" && pwd)/hr_install.sql
mkdir -p "$work"
work=$(cd "$work" && pwd)
java=${JAVA:-java}
psql_command=${PSQL:-${PG_BIN:+$PG_BIN/}psql}
read -r -a psql_words <<< "$psql_command"
: "${PGJDBC_JAR:?PGJDBC_JAR must name the pgjdbc jar}"
database=oracle_apps

fail() {
  echo "oracle-apps check FAILED: $*" >&2
  exit 1
}

echo "$("${psql_words[@]}" --version); $("$java" -version 2>&1 | head -n 1)"

rm -f "$work/hr.db"
"$eggdb" restore --dialect oracle --create "$work/hr.db" "$installer" > "$work/restore.out" ||
  fail "eggdb restore failed: $(cat "$work/restore.out")"

password=hr-secret
printf '%s\n' "$password" > "$work/password.txt"
"$eggdb" serve "$work/hr.db" --listen 127.0.0.1:0 --user hr --password-file "$work/password.txt" \
  --database "$database" > "$work/serve.out" 2> "$work/serve.err" &
server=$!
trap 'kill "$server" 2> /dev/null || true' EXIT
for _ in $(seq 1 100); do
  [ -s "$work/serve.out" ] && break
  sleep 0.1
done
port=$(sed -n 's/.*"port":\([0-9]*\).*/\1/p' "$work/serve.out" | head -n 1)
[ -n "$port" ] || fail "eggdb serve printed no port: $(cat "$work/serve.out") $(cat "$work/serve.err")"

PGPASSWORD=$password PGOPTIONS='-c eggdb.dialect=oracle' "${psql_words[@]}" -X -A -t -F '|' \
  -v ON_ERROR_STOP=1 -h 127.0.0.1 -p "$port" -U hr -d "$database" -f - \
  < "$here/commands.sql" > "$work/psql.out" 2>&1 || true
"$java" -Duser.timezone=UTC -cp "$PGJDBC_JAR" "$clients/java/OracleAppCheck.java" \
  "jdbc:postgresql://127.0.0.1:$port/$database" hr "$password" > "$work/java.out" 2>&1 || true

kill -TERM "$server"
wait "$server" || fail "eggdb serve exited badly: $(cat "$work/serve.err")"
trap - EXIT

: > "$work/output.diff"
different=""
for program in psql java; do
  if ! cmp -s "$here/expected-$program.out" "$work/$program.out"; then
    diff -u "$here/expected-$program.out" "$work/$program.out" >> "$work/output.diff" || true
    different="$different $program"
  fi
done
if [ -n "$different" ]; then
  head -n 120 "$work/output.diff" >&2
  fail "the$different output differs from the expected output (whole diff in $work/output.diff)"
fi
echo "oracle-apps check passed: psql and pgjdbc run HR's queries and Oracle's data dictionary in Oracle's dialect"
