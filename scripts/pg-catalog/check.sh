#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/pg-catalog/check.sh <eggdb binary> <work directory>

Fills a database of a running PostgreSQL server with scripts/pg-catalog/fixture.sql, restores
its pg_dump into EggDB with eggdb restore, serves it with eggdb serve, runs every psql command of
scripts/pg-catalog/commands.txt against both servers and fails unless the two outputs are byte
for byte the same. A second run with -E keeps the queries psql sent to each server in the work
directory, and postgres.oids keeps the relation OIDs of PostgreSQL's side
(scripts/pg-catalog/record.sh reads both through QUERIES).

PostgreSQL's tools run inside the docker container named by PG_CONTAINER, or from PG_BIN (a
directory), or from PATH; PGHOST, PGPORT, PGUSER and PGPASSWORD reach the server. PSQL is the
psql command that runs the compared commands (default: psql from PG_BIN or PATH), for example
\"docker run --rm -i --network host -e PGPASSWORD postgres:17 psql\". DATABASE names the database
on both servers (default eggdb_catalog); it is dropped and created again."

[ $# -eq 2 ] || { echo "$usage" >&2; exit 2; }
eggdb=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
work=$2
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$work"
work=$(cd "$work" && pwd)
database=${DATABASE:-eggdb_catalog}
export PGTZ=UTC PGCLIENTENCODING=UTF8
psql_command=${PSQL:-${PG_BIN:+$PG_BIN/}psql}
read -r -a psql_words <<< "$psql_command"

pg() {
  if [ -n "${PG_CONTAINER:-}" ]; then
    docker exec -i -e PGHOST -e PGPORT -e PGUSER -e PGPASSWORD -e PGTZ -e PGCLIENTENCODING -e PGDATABASE "$PG_CONTAINER" "$@"
  else
    local tool=$1
    shift
    "${PG_BIN:+$PG_BIN/}$tool" "$@"
  fi
}

fail() {
  echo "pg-catalog check FAILED: $*" >&2
  exit 1
}

PGDATABASE=postgres pg psql -X -v ON_ERROR_STOP=1 -q -c "DROP DATABASE IF EXISTS $database" \
  -c "CREATE DATABASE $database TEMPLATE template0 ENCODING 'UTF8' LC_COLLATE 'C' LC_CTYPE 'C'"
PGDATABASE=$database pg psql -X -v ON_ERROR_STOP=1 -q -f - < "$here/fixture.sql" > /dev/null
PGDATABASE=$database pg pg_dump > "$work/dump.sql"
PGDATABASE=$database pg psql -X -At -F ' ' -c "SELECT c.oid, quote_ident(n.nspname) || '.' || quote_ident(c.relname)
  FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.oid >= 16384" > "$work/postgres.oids"
version=$(PGDATABASE=$database pg psql -X -At -c 'SHOW server_version')
client=$("${psql_words[@]}" --version)
echo "PostgreSQL $version; $client"

rm -f "$work/catalog.db"
"$eggdb" restore --create "$work/catalog.db" "$work/dump.sql" > "$work/restore.out" ||
  fail "eggdb restore failed: $(cat "$work/restore.out")"

printf 'catalog-secret\n' > "$work/password.txt"
"$eggdb" serve "$work/catalog.db" --listen 127.0.0.1:0 --password-file "$work/password.txt" \
  --database "$database" > "$work/serve.out" 2> "$work/serve.err" &
server=$!
trap 'kill "$server" 2> /dev/null || true' EXIT
for _ in $(seq 1 100); do
  [ -s "$work/serve.out" ] && break
  sleep 0.1
done
port=$(sed -n 's/.*"port":\([0-9]*\).*/\1/p' "$work/serve.out" | head -n 1)
[ -n "$port" ] || fail "eggdb serve printed no port: $(cat "$work/serve.out") $(cat "$work/serve.err")"

run_postgres() {
  "${psql_words[@]}" -X -v database="$database" "$@" -d "$database" -f - < "$here/commands.txt" 2>&1
}

run_eggdb() {
  PGPASSWORD=catalog-secret "${psql_words[@]}" -X -v database="$database" "$@" \
    "host=127.0.0.1 port=$port user=eggdb dbname=$database" -f - < "$here/commands.txt" 2>&1
}

run_postgres > "$work/postgres.out" || true
run_eggdb > "$work/eggdb.out" || true
run_postgres -E > "$work/postgres.queries.log" || true
run_eggdb -E > "$work/eggdb.queries.log" || true

kill -TERM "$server"
wait "$server" || fail "eggdb serve exited badly: $(cat "$work/serve.err")"
trap - EXIT

if ! cmp -s "$work/postgres.out" "$work/eggdb.out"; then
  diff -u "$work/postgres.out" "$work/eggdb.out" > "$work/output.diff" || true
  head -n 80 "$work/output.diff" >&2
  fail "psql shows EggDB's catalog differently from PostgreSQL's (whole diff in $work/output.diff)"
fi
echo "pg-catalog check passed: $(grep -c . "$here/commands.txt") psql commands show the same bytes on PostgreSQL $version and EggDB"
