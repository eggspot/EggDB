#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/pg-restore/check.sh <eggdb binary> <work directory> [--rows <count>] [--fixtures <directory> <label>]

Fills a database of a running PostgreSQL server with scripts/pg-restore/fixture.sql, dumps it
plain, with --inserts, with --column-inserts and as a custom-format archive converted by
pg_restore -f, restores every dump with eggdb restore and the plain one with psql -f against
eggdb serve, and compares every table's sorted COPY output with PostgreSQL's.

PostgreSQL's tools run inside the docker container named by PG_CONTAINER, or from PG_BIN (a
directory), or from PATH; PGHOST, PGPORT, PGUSER and PGPASSWORD reach the server."

[ $# -ge 2 ] || { echo "$usage" >&2; exit 2; }
eggdb=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
work=$2
shift 2
rows=50000
fixtures=""
label=""
while [ $# -gt 0 ]; do
  case "$1" in
    --rows) rows=$2; shift 2 ;;
    --fixtures) fixtures=$2; label=$3; shift 3 ;;
    *) echo "$usage" >&2; exit 2 ;;
  esac
done
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$work"
work=$(cd "$work" && pwd)
export PGTZ=UTC PGCLIENTENCODING=UTF8

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
  echo "pg-restore check FAILED: $*" >&2
  exit 1
}

tables=(
  "public.customers|id"
  "public.products|id"
  "public.orders|id"
  "public.order_items|order_id, line"
  "shop.warehouses|code"
  "shop.stock|warehouse, product_id"
  "public.blobs|id"
  "public.texts|id"
  "public.events|id"
  "public.measurements|sensor, taken"
  "public.log_lines|logged_at, level, message"
  "public.ledger|id"
)

query_of() {
  local table=${1%%|*} order=${1#*|}
  echo "COPY (SELECT * FROM $table ORDER BY $order) TO STDOUT"
}

database=eggdb_fixture
PGDATABASE=postgres pg psql -v ON_ERROR_STOP=1 -q -c "DROP DATABASE IF EXISTS $database" \
  -c "CREATE DATABASE $database TEMPLATE template0 ENCODING 'UTF8' LC_COLLATE 'C' LC_CTYPE 'C'"
export PGDATABASE=$database
pg psql -v ON_ERROR_STOP=1 -q -v rows="$rows" -f - < "$here/fixture.sql" > /dev/null
version=$(pg psql -At -c 'SHOW server_version')
echo "PostgreSQL $version, $rows rows"

: > "$work/expected.txt"
for spec in "${tables[@]}"; do
  query=$(query_of "$spec")
  echo "== $query" >> "$work/expected.txt"
  pg psql -v ON_ERROR_STOP=1 -q -c "$query" >> "$work/expected.txt"
done

pg pg_dump > "$work/plain.sql"
pg pg_dump --inserts > "$work/inserts.sql"
pg pg_dump --column-inserts > "$work/column-inserts.sql"
pg pg_dump -Fc > "$work/archive.dump"
pg pg_restore -f - < "$work/archive.dump" > "$work/custom.sql"

compare() {
  local form=$1 db=$2
  : > "$work/$form.found"
  for spec in "${tables[@]}"; do
    query=$(query_of "$spec")
    echo "== $query" >> "$work/$form.found"
    "$eggdb" sql "$db" -c "$query" >> "$work/$form.found" ||
      fail "$form: eggdb could not read ${spec%%|*}"
  done
  if ! cmp -s "$work/expected.txt" "$work/$form.found"; then
    diff "$work/expected.txt" "$work/$form.found" | head -n 40 >&2 || true
    fail "$form: the restored tables differ from PostgreSQL's"
  fi
  echo "ok   $form: every table matches PostgreSQL's"
}

status=0
"$eggdb" restore --create "$work/refused.db" "$work/archive.dump" > /dev/null 2> "$work/refused.err" || status=$?
[ "$status" -eq 2 ] || fail "a custom-format archive exited $status, expected 2"
grep -q "pg_restore -f dump.sql" "$work/refused.err" || fail "a custom-format archive said $(cat "$work/refused.err")"
[ ! -e "$work/refused.db" ] || fail "a refused archive created a file"
echo "ok   a custom-format archive is refused with the pg_restore command"

for form in plain inserts column-inserts custom; do
  rm -f "$work/$form.db"
  "$eggdb" restore --create "$work/$form.db" "$work/$form.sql" --report "$work/$form.report.json" > "$work/$form.out" ||
    fail "$form: eggdb restore failed: $(cat "$work/$form.out") $(cat "$work/$form.report.json" 2> /dev/null)"
  grep -q '"tables_skipped":0,' "$work/$form.report.json" || fail "$form: a table was skipped: $(cat "$work/$form.report.json")"
  compare "$form" "$work/$form.db"
done

if [ "${OS:-}" != Windows_NT ]; then
  rm -f "$work/served.db"
  printf 'restore-secret\n' > "$work/password.txt"
  "$eggdb" serve --create "$work/served.db" --listen 127.0.0.1:0 --password-file "$work/password.txt" --cache 1g > "$work/serve.out" 2> "$work/serve.err" &
  server=$!
  trap 'kill "$server" 2> /dev/null || true' EXIT
  for _ in $(seq 1 100); do
    [ -s "$work/serve.out" ] && break
    sleep 0.1
  done
  port=$(sed -n 's/.*"port":\([0-9]*\).*/\1/p' "$work/serve.out" | head -n 1)
  [ -n "$port" ] || fail "eggdb serve printed no port: $(cat "$work/serve.out") $(cat "$work/serve.err")"
  PGPASSWORD=restore-secret pg psql "host=127.0.0.1 port=$port user=eggdb dbname=served" -q -f - < "$work/plain.sql" > "$work/psql.out" 2> "$work/psql.err" || true
  kill -TERM "$server"
  wait "$server" || fail "eggdb serve exited badly: $(cat "$work/serve.err")"
  trap - EXIT
  compare psql-f "$work/served.db"
fi

if [ -n "$fixtures" ]; then
  mkdir -p "$fixtures"
  cp "$work/plain.sql" "$fixtures/$label.sql"
  cp "$work/inserts.sql" "$fixtures/$label-inserts.sql"
  cp "$work/column-inserts.sql" "$fixtures/$label-column-inserts.sql"
  cp "$work/custom.sql" "$fixtures/$label-custom.sql"
  cp "$work/expected.txt" "$fixtures/expected.txt"
  echo "fixtures written to $fixtures with the label $label"
fi

echo "pg-restore check passed against PostgreSQL $version"
