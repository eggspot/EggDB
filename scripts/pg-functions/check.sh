#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/pg-functions/check.sh <eggdb binary> <work directory> [cases file]

Runs every statement of the cases file (scripts/pg-functions/cases.sql when none is given) with
psql against a running PostgreSQL server and against eggdb serve on a new database file, and fails
unless psql prints the same bytes for both. Errors go to separate files and are not compared: a
statement that fails on one side and not on the other changes the printed rows, which are.

psql runs inside the docker container named by PG_CONTAINER (on the host's network), or from
PG_BIN (a directory), or from PATH; PGHOST, PGPORT, PGUSER and PGPASSWORD reach PostgreSQL."

[ $# -eq 2 ] || [ $# -eq 3 ] || { echo "$usage" >&2; exit 2; }
eggdb=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
work=$2
here=$(cd "$(dirname "$0")" && pwd)
cases=${3:-$here/cases.sql}
cases=$(cd "$(dirname "$cases")" && pwd)/$(basename "$cases")
mkdir -p "$work"
work=$(cd "$work" && pwd)
export PGTZ=UTC PGCLIENTENCODING=UTF8 PGDATABASE=postgres

psql_run() {
  if [ -n "${PG_CONTAINER:-}" ]; then
    docker exec -i -e PGHOST -e PGPORT -e PGUSER -e PGPASSWORD -e PGTZ -e PGCLIENTENCODING -e PGDATABASE \
      "$PG_CONTAINER" psql "$@"
  else
    "${PG_BIN:+$PG_BIN/}psql" "$@"
  fi
}

fail() {
  echo "pg-functions check FAILED: $*" >&2
  exit 1
}

version=$(psql_run -X -At -c 'SHOW server_version')
echo "PostgreSQL $version"

rm -f "$work/functions.db"
printf 'functions-secret\n' > "$work/password.txt"
"$eggdb" serve "$work/functions.db" --create --listen 127.0.0.1:0 --password-file "$work/password.txt" \
  --database postgres > "$work/serve.out" 2> "$work/serve.err" &
server=$!
trap 'kill "$server" 2> /dev/null || true' EXIT
for _ in $(seq 1 100); do
  [ -s "$work/serve.out" ] && break
  sleep 0.1
done
port=$(sed -n 's/.*"port":\([0-9]*\).*/\1/p' "$work/serve.out" | head -n 1)
[ -n "$port" ] || fail "eggdb serve printed no port: $(cat "$work/serve.out") $(cat "$work/serve.err")"

psql_run -X -A -v ON_ERROR_STOP=0 -f - < "$cases" > "$work/postgres.out" 2> "$work/postgres.err" || true
(
  export PGHOST=127.0.0.1 PGPORT="$port" PGUSER=eggdb PGPASSWORD=functions-secret
  psql_run -X -A -v ON_ERROR_STOP=0 -f - < "$cases" > "$work/eggdb.out" 2> "$work/eggdb.err"
) || true

kill -TERM "$server"
wait "$server" || fail "eggdb serve exited badly: $(cat "$work/serve.err")"
trap - EXIT

if ! cmp -s "$work/postgres.out" "$work/eggdb.out"; then
  diff -u "$work/postgres.out" "$work/eggdb.out" > "$work/output.diff" || true
  head -n 80 "$work/output.diff" >&2
  echo "EggDB's errors:" >&2
  head -n 40 "$work/eggdb.err" >&2
  fail "psql prints EggDB's answers differently from PostgreSQL's (whole diff in $work/output.diff)"
fi
statements=$(grep -c ';$' "$cases")
echo "pg-functions check passed: $statements statements print the same bytes on PostgreSQL $version and EggDB"
