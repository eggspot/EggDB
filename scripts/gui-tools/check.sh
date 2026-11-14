#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/gui-tools/check.sh <eggdb binary> <work directory>

Runs the GUI database tools themselves against eggdb serve, through the same metadata code their
desktop windows use (D163, D1172):
- pgAdmin 4, from its official server-mode container listening on PGADMIN_URL: scripts/gui-tools/pgadmin.py
  signs in, registers EggDB, connects, opens the object browser down to columns, the table's
  properties and SQL tab, views, sequences and functions, the dashboards, and runs a query in the
  query tool;
- DBeaver's PostgreSQL support, through CloudBeaver (DBeaver's web edition, the same DBeaver
  PostgreSQL plugin) listening on CLOUDBEAVER_URL: scripts/gui-tools/cloudbeaver.py signs in over its
  GraphQL API, creates and opens a connection, walks the navigator and runs a query in the SQL editor;
- SQL Server Management Studio's Object Explorer and Generate Scripts, through SQL Server
  Management Objects (SMO, MIT), which they are built on: crates/eggdb/tests/clients/dotnet/SmoCheck
  lists databases, schemas, tables with columns, indexes, foreign keys and checks, procedures and
  views of the sqlpackage fixture and scripts the tables.
pgAdmin and CloudBeaver read scripts/gui-tools/library.sql, served as the PostgreSQL database
library; SMO reads crates/eggdb/tests/data/sqlserver/shop-sqlpackage.sql, served over TDS as shop.
Each tool fails the check when an object is missing or a request fails.

PGADMIN_URL, PGADMIN_EMAIL and PGADMIN_PASSWORD name pgAdmin and its sign-in; CLOUDBEAVER_URL,
CLOUDBEAVER_ADMIN and CLOUDBEAVER_PASSWORD name CloudBeaver and its administrator; PYTHON names
python3 and DOTNET the dotnet command (default dotnet), whose SDK restores SMO from NuGet."

[ $# -eq 2 ] || { echo "$usage" >&2; exit 2; }
eggdb=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
work=$2
here=$(cd "$(dirname "$0")" && pwd)
clients=$(cd "$here/../../crates/eggdb/tests/clients" && pwd)
fixture=$(cd "$here/../../crates/eggdb/tests/data/sqlserver" && pwd)/shop-sqlpackage.sql
mkdir -p "$work"
work=$(cd "$work" && pwd)
python=${PYTHON:-python3}
dotnet=${DOTNET:-dotnet}
: "${PGADMIN_URL:?PGADMIN_URL must name pgAdmin}"
: "${PGADMIN_EMAIL:?PGADMIN_EMAIL must name pgAdmin's sign-in}"
: "${PGADMIN_PASSWORD:?PGADMIN_PASSWORD must name pgAdmin's password}"
: "${CLOUDBEAVER_URL:?CLOUDBEAVER_URL must name CloudBeaver}"
: "${CLOUDBEAVER_ADMIN:?CLOUDBEAVER_ADMIN must name CloudBeaver's administrator}"
: "${CLOUDBEAVER_PASSWORD:?CLOUDBEAVER_PASSWORD must name CloudBeaver's administrator password}"
export DOTNET_NOLOGO=1 DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_SKIP_FIRST_TIME_EXPERIENCE=1

fail() {
  echo "gui-tools check FAILED: $*" >&2
  exit 1
}

"$dotnet" build "$clients/dotnet/SmoCheck" -c Release --artifacts-path "$work/dotnet" > "$work/dotnet-build.log" 2>&1 ||
  fail "the SMO program did not build: $(tail -n 40 "$work/dotnet-build.log")"

password=gui-secret
printf '%s\n' "$password" > "$work/password.txt"
: > "$work/serve.err"

serve() {
  local file=$1 name=$2
  shift 2
  "$eggdb" serve "$file" --listen 127.0.0.1:0 "$@" --password-file "$work/password.txt" \
    --database "$name" > "$work/serve-$name.out" 2>> "$work/serve.err" &
  server=$!
  trap 'kill "$server" 2> /dev/null || true' EXIT
  for _ in $(seq 1 100); do
    [ -s "$work/serve-$name.out" ] && break
    sleep 0.1
  done
}

stop() {
  kill -TERM "$server"
  wait "$server" || fail "eggdb serve exited badly: $(cat "$work/serve.err")"
  trap - EXIT
}

failures=0

rm -f "$work/library.db" "$work/shop.db"
"$eggdb" sql --create --dialect postgresql "$work/library.db" "$here/library.sql" > "$work/library-setup.out" 2>&1 ||
  fail "the library fixture was not created: $(cat "$work/library-setup.out")"
serve "$work/library.db" library
port=$(sed -n 's/.*"port":\([0-9]*\).*/\1/p' "$work/serve-library.out" | head -n 1)
[ -n "$port" ] || fail "eggdb serve printed no port: $(cat "$work/serve-library.out") $(cat "$work/serve.err")"
if "$python" "$here/pgadmin.py" "$PGADMIN_URL" "$PGADMIN_EMAIL" "$PGADMIN_PASSWORD" "$port" "$password" > "$work/pgadmin.out" 2>&1; then
  echo "pgAdmin 4: passed"
else
  echo "pgAdmin 4: FAILED"
  failures=$((failures + 1))
fi
cat "$work/pgadmin.out"
if "$python" "$here/cloudbeaver.py" "$CLOUDBEAVER_URL" "$CLOUDBEAVER_ADMIN" "$CLOUDBEAVER_PASSWORD" "$port" "$password" > "$work/cloudbeaver.out" 2>&1; then
  echo "DBeaver (CloudBeaver): passed"
else
  echo "DBeaver (CloudBeaver): FAILED"
  failures=$((failures + 1))
fi
cat "$work/cloudbeaver.out"
stop

"$eggdb" restore --dialect sqlserver --create "$work/shop.db" "$fixture" > "$work/restore.out" 2>&1 ||
  fail "the sqlpackage fixture did not restore: $(cat "$work/restore.out")"
serve "$work/shop.db" shop --tds-listen 127.0.0.1:0
tds_port=$(sed -n 's/.*"tds_port":\([0-9]*\).*/\1/p' "$work/serve-shop.out" | head -n 1)
[ -n "$tds_port" ] || fail "eggdb serve printed no TDS port: $(cat "$work/serve-shop.out") $(cat "$work/serve.err")"
if "$dotnet" run --no-build --project "$clients/dotnet/SmoCheck" -c Release --artifacts-path "$work/dotnet" -- \
  "Server=127.0.0.1,$tds_port;Database=shop;User ID=eggdb;Password=$password;Encrypt=false;Connect Timeout=10" shop \
  > "$work/smo.out" 2>&1; then
  echo "SSMS Object Explorer and Generate Scripts (SMO): passed"
else
  echo "SSMS Object Explorer and Generate Scripts (SMO): FAILED"
  failures=$((failures + 1))
fi
cat "$work/smo.out"
stop

[ "$failures" -eq 0 ] || fail "$failures of the 3 tools failed; their outputs are above and in $work"
echo "gui-tools check passed"
