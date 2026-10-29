#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/pg-apps/check.sh <eggdb binary> <work directory>

Fills a database of a running PostgreSQL server with scripts/pg-catalog/fixture.sql, restores its
pg_dump into EggDB with eggdb restore and serves it with eggdb serve. Then runs the same client
programs against both servers: crates/eggdb/tests/clients/java/AppCheck.java through pgjdbc
(CRUD, prepared statements, a transaction and DatabaseMetaData),
crates/eggdb/tests/clients/python/app_check.py through psycopg 3 and SQLAlchemy Core (CRUD and
MetaData.reflect()) and crates/eggdb/tests/clients/dotnet/NpgsqlAppCheck through Npgsql (type
loading, CRUD over pooled connections, transactions, a batch, a batch that fails half way and a
cancelled query). Fails unless each program prints the same bytes on both servers.

PostgreSQL's tools run inside the docker container named by PG_CONTAINER, or from PG_BIN (a
directory), or from PATH; PGHOST, PGPORT, PGUSER and PGPASSWORD reach the server. PGJDBC_JAR names
the pgjdbc jar, JAVA the java launcher (default java) and PYTHON a python with psycopg and
SQLAlchemy installed (default python3) and DOTNET the dotnet command (default dotnet), whose SDK
restores Npgsql from NuGet. DATABASE names the database on both servers (default
eggdb_apps); it is dropped and created again."

[ $# -eq 2 ] || { echo "$usage" >&2; exit 2; }
eggdb=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
work=$2
here=$(cd "$(dirname "$0")" && pwd)
clients=$(cd "$here/../../crates/eggdb/tests/clients" && pwd)
mkdir -p "$work"
work=$(cd "$work" && pwd)
database=${DATABASE:-eggdb_apps}
java=${JAVA:-java}
python=${PYTHON:-python3}
dotnet=${DOTNET:-dotnet}
: "${PGJDBC_JAR:?PGJDBC_JAR must name the pgjdbc jar}"
: "${PGHOST:?PGHOST must reach PostgreSQL}"
: "${PGUSER:?PGUSER must name a PostgreSQL superuser}"
export PGTZ=UTC PGCLIENTENCODING=UTF8
export DOTNET_NOLOGO=1 DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_SKIP_FIRST_TIME_EXPERIENCE=1
pg_port=${PGPORT:-5432}
pg_password=${PGPASSWORD:-}

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
  echo "pg-apps check FAILED: $*" >&2
  exit 1
}

PGDATABASE=postgres pg psql -X -v ON_ERROR_STOP=1 -q -c "DROP DATABASE IF EXISTS $database" \
  -c "CREATE DATABASE $database TEMPLATE template0 ENCODING 'UTF8' LC_COLLATE 'C' LC_CTYPE 'C'"
PGDATABASE=$database pg psql -X -v ON_ERROR_STOP=1 -q -f - < "$here/../pg-catalog/fixture.sql" > /dev/null
PGDATABASE=$database pg pg_dump > "$work/dump.sql"
version=$(PGDATABASE=$database pg psql -X -At -c 'SHOW server_version')
echo "PostgreSQL $version; $("$java" -version 2>&1 | head -n 1); $("$python" --version 2>&1); .NET SDK $("$dotnet" --version)"
"$dotnet" build "$clients/dotnet/NpgsqlAppCheck" -c Release --artifacts-path "$work/dotnet" > "$work/dotnet-build.log" 2>&1 ||
  fail "the Npgsql program did not build: $(tail -n 40 "$work/dotnet-build.log")"

rm -f "$work/apps.db"
"$eggdb" restore --create "$work/apps.db" "$work/dump.sql" > "$work/restore.out" ||
  fail "eggdb restore failed: $(cat "$work/restore.out")"

eggdb_password=apps-secret
printf '%s\n' "$eggdb_password" > "$work/password.txt"
"$eggdb" serve "$work/apps.db" --listen 127.0.0.1:0 --password-file "$work/password.txt" \
  --database "$database" > "$work/serve.out" 2> "$work/serve.err" &
server=$!
trap 'kill "$server" 2> /dev/null || true' EXIT
for _ in $(seq 1 100); do
  [ -s "$work/serve.out" ] && break
  sleep 0.1
done
eggdb_port=$(sed -n 's/.*"port":\([0-9]*\).*/\1/p' "$work/serve.out" | head -n 1)
[ -n "$eggdb_port" ] || fail "eggdb serve printed no port: $(cat "$work/serve.out") $(cat "$work/serve.err")"

run_java() {
  local port=$1 user=$2 password=$3
  "$java" -Duser.timezone=UTC -cp "$PGJDBC_JAR" "$clients/java/AppCheck.java" \
    "jdbc:postgresql://$PGHOST:$port/$database" "$user" "$password" 2>&1
}

run_python() {
  local port=$1 user=$2 password=$3
  "$python" "$clients/python/app_check.py" \
    "host=$PGHOST port=$port user=$user password=$password dbname=$database" \
    "postgresql+psycopg://$user:$password@$PGHOST:$port/$database" 2>&1
}

run_dotnet() {
  local port=$1 user=$2 password=$3
  "$dotnet" run --no-build --project "$clients/dotnet/NpgsqlAppCheck" -c Release --artifacts-path "$work/dotnet" -- \
    "Host=$PGHOST;Port=$port;Username=$user;Password=$password;Database=$database;Timeout=10;Command Timeout=60" 2>&1
}

run_java "$pg_port" "$PGUSER" "$pg_password" > "$work/postgres.java.out" || true
run_java "$eggdb_port" eggdb "$eggdb_password" > "$work/eggdb.java.out" || true
run_python "$pg_port" "$PGUSER" "$pg_password" > "$work/postgres.python.out" || true
run_python "$eggdb_port" eggdb "$eggdb_password" > "$work/eggdb.python.out" || true
run_dotnet "$pg_port" "$PGUSER" "$pg_password" > "$work/postgres.dotnet.out" || true
run_dotnet "$eggdb_port" eggdb "$eggdb_password" > "$work/eggdb.dotnet.out" || true

kill -TERM "$server"
wait "$server" || fail "eggdb serve exited badly: $(cat "$work/serve.err")"
trap - EXIT

: > "$work/output.diff"
different=""
for program in java python dotnet; do
  if ! cmp -s "$work/postgres.$program.out" "$work/eggdb.$program.out"; then
    diff -u "$work/postgres.$program.out" "$work/eggdb.$program.out" >> "$work/output.diff" || true
    different="$different $program"
  fi
done
if [ -n "$different" ]; then
  head -n 120 "$work/output.diff" >&2
  fail "the$different client programs printed different bytes on EggDB (whole diff in $work/output.diff)"
fi
echo "pg-apps check passed: pgjdbc, psycopg 3, SQLAlchemy Core and Npgsql print the same bytes on PostgreSQL $version and EggDB"
