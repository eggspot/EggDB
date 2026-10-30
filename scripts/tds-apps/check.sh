#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/tds-apps/check.sh <eggdb binary> <work directory>

Serves a new EggDB database over SQL Server's TDS protocol with eggdb serve --tds-listen and runs
two client programs against it through Microsoft's own drivers, without encryption:
crates/eggdb/tests/clients/java/TdsAppCheck.java through mssql-jdbc and
crates/eggdb/tests/clients/dotnet/TdsAppCheck through Microsoft.Data.SqlClient. Each runs
parameterized CRUD (sp_executesql, sp_prepexec and sp_execute), a committed and a rolled-back
transaction and a failing query; the .NET program also reuses a pooled connection, which resets
it, and runs scripts/tds-apps/batch-logic.sql batch by batch as sqlcmd does (GO separators):
Transact-SQL batch logic with a loop, TRY/CATCH, THROW, RAISERROR, PRINT and sp_executesql with
an OUTPUT parameter, a stored procedure run with EXEC @status = ... OUTPUT, a scalar and an inline
table-valued function, and then calls a stored procedure as CommandType.StoredProcedure with an
output parameter and a return value, also after SqlCommandBuilder.DeriveParameters. Then restores the sqlpackage fixture crates/eggdb/tests/data/sqlserver/shop-sqlpackage.sql
into a second database and lists it through each driver's metadata calls:
crates/eggdb/tests/clients/java/TdsMetadataCheck.java (DatabaseMetaData) and the .NET program's
metadata mode (SqlConnection.GetSchema). Last, loads the committed bcp character-format file
crates/eggdb/tests/data/sqlserver/bcp/items.dat (bcp ... out -c; bcp itself is not run here,
its license keeps it out of CI) into a third database twice: with eggdb import --dialect
sqlserver, and with SqlBulkCopy in the .NET program's bulk mode, which compares both tables'
rows with the file. Then serves a fourth, new database to the EF Core app
crates/eggdb/tests/clients/dotnet/EfCoreAppCheck: its migrations applied with
Database.Migrate(), SaveChanges batches (MERGE ... OUTPUT for several inserts, UPDATE and DELETE
with OUTPUT 1), LINQ queries (OFFSET ... FETCH, joins, EXISTS, GROUP BY, OPENJSON for Contains)
and a rolled-back transaction with EF's save point. Fails unless each program prints exactly
scripts/tds-apps/expected-java.out, expected-dotnet.out, expected-logic-dotnet.out,
expected-metadata-java.out, expected-metadata-dotnet.out, expected-bulk-dotnet.out and
expected-efcore-dotnet.out, written from Microsoft's documentation of SQL Server's behavior.

MSSQL_JDBC_JAR names the mssql-jdbc jar, JAVA the java launcher (default java) and DOTNET the
dotnet command (default dotnet), whose SDK restores Microsoft.Data.SqlClient and EF Core from
NuGet."

[ $# -eq 2 ] || { echo "$usage" >&2; exit 2; }
eggdb=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
work=$2
here=$(cd "$(dirname "$0")" && pwd)
clients=$(cd "$here/../../crates/eggdb/tests/clients" && pwd)
fixture=$(cd "$here/../../crates/eggdb/tests/data/sqlserver" && pwd)/shop-sqlpackage.sql
bcp=$(cd "$here/../../crates/eggdb/tests/data/sqlserver/bcp" && pwd)
mkdir -p "$work"
work=$(cd "$work" && pwd)
java=${JAVA:-java}
dotnet=${DOTNET:-dotnet}
: "${MSSQL_JDBC_JAR:?MSSQL_JDBC_JAR must name the mssql-jdbc jar}"
export DOTNET_NOLOGO=1 DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_SKIP_FIRST_TIME_EXPERIENCE=1

fail() {
  echo "tds-apps check FAILED: $*" >&2
  exit 1
}

echo "$("$java" -version 2>&1 | head -n 1); .NET SDK $("$dotnet" --version)"
"$dotnet" build "$clients/dotnet/TdsAppCheck" -c Release --artifacts-path "$work/dotnet" > "$work/dotnet-build.log" 2>&1 ||
  fail "the .NET program did not build: $(tail -n 40 "$work/dotnet-build.log")"
"$dotnet" build "$clients/dotnet/EfCoreAppCheck" -c Release --artifacts-path "$work/dotnet" >> "$work/dotnet-build.log" 2>&1 ||
  fail "the EF Core program did not build: $(tail -n 40 "$work/dotnet-build.log")"

password=apps-secret
printf '%s\n' "$password" > "$work/password.txt"

serve() {
  local database=$1 name=$2
  "$eggdb" serve "$work/$name.db" --listen 127.0.0.1:0 --tds-listen 127.0.0.1:0 \
    --password-file "$work/password.txt" --database "$database" --log-batches \
    > "$work/serve-$name.out" 2>> "$work/serve.err" &
  server=$!
  trap 'kill "$server" 2> /dev/null || true' EXIT
  for _ in $(seq 1 100); do
    [ -s "$work/serve-$name.out" ] && break
    sleep 0.1
  done
  port=$(sed -n 's/.*"tds_port":\([0-9]*\).*/\1/p' "$work/serve-$name.out" | head -n 1)
  [ -n "$port" ] || fail "eggdb serve printed no TDS port: $(cat "$work/serve-$name.out") $(cat "$work/serve.err")"
}

stop() {
  kill -TERM "$server"
  wait "$server" || fail "eggdb serve exited badly: $(cat "$work/serve.err")"
  trap - EXIT
}

: > "$work/serve.err"
rm -f "$work/apps.db" "$work/shop.db" "$work/bulk.db" "$work/efcore.db"
"$eggdb" create "$work/apps.db" > /dev/null || fail "eggdb create failed"
serve apps apps
"$java" -cp "$MSSQL_JDBC_JAR" "$clients/java/TdsAppCheck.java" \
  "jdbc:sqlserver://127.0.0.1:$port;databaseName=apps;encrypt=false;loginTimeout=10" eggdb "$password" \
  > "$work/java.out" 2>&1 || true
"$dotnet" run --no-build --project "$clients/dotnet/TdsAppCheck" -c Release --artifacts-path "$work/dotnet" -- \
  "Server=127.0.0.1,$port;Database=apps;User ID=eggdb;Password=$password;Encrypt=false;Connect Timeout=10" \
  > "$work/dotnet.out" 2>&1 || true
"$dotnet" run --no-build --project "$clients/dotnet/TdsAppCheck" -c Release --artifacts-path "$work/dotnet" -- \
  "Server=127.0.0.1,$port;Database=apps;User ID=eggdb;Password=$password;Encrypt=false;Connect Timeout=10" logic "$here/batch-logic.sql" \
  > "$work/logic-dotnet.out" 2>&1 || true
stop

"$eggdb" restore --dialect sqlserver --create "$work/shop.db" "$fixture" > "$work/restore.out" 2>&1 ||
  fail "the sqlpackage fixture did not restore: $(cat "$work/restore.out")"
serve shop shop
"$java" -cp "$MSSQL_JDBC_JAR" "$clients/java/TdsMetadataCheck.java" \
  "jdbc:sqlserver://127.0.0.1:$port;databaseName=shop;encrypt=false;loginTimeout=10" eggdb "$password" \
  > "$work/metadata-java.out" 2>&1 || true
"$dotnet" run --no-build --project "$clients/dotnet/TdsAppCheck" -c Release --artifacts-path "$work/dotnet" -- \
  "Server=127.0.0.1,$port;Database=shop;User ID=eggdb;Password=$password;Encrypt=false;Connect Timeout=10" metadata \
  > "$work/metadata-dotnet.out" 2>&1 || true
stop

"$eggdb" sql --create --dialect sqlserver "$work/bulk.db" "$bcp/items.sql" > "$work/bulk-setup.out" 2>&1 ||
  fail "the bulk load table was not created: $(cat "$work/bulk-setup.out")"
"$eggdb" import --dialect sqlserver "$work/bulk.db" --csv "$bcp/items.dat" --table dbo.items_imported \
  > "$work/bulk-import.out" 2>&1 || fail "eggdb import of the bcp file failed: $(cat "$work/bulk-import.out")"
serve bulk bulk
"$dotnet" run --no-build --project "$clients/dotnet/TdsAppCheck" -c Release --artifacts-path "$work/dotnet" -- \
  "Server=127.0.0.1,$port;Database=bulk;User ID=eggdb;Password=$password;Encrypt=false;Connect Timeout=10" bulk "$bcp/items.dat" \
  > "$work/bulk-dotnet.out" 2>&1 || true
stop

"$eggdb" create "$work/efcore.db" > /dev/null || fail "eggdb create failed"
serve efcore efcore
"$dotnet" run --no-build --project "$clients/dotnet/EfCoreAppCheck" -c Release --artifacts-path "$work/dotnet" -- \
  "Server=127.0.0.1,$port;Database=efcore;User ID=eggdb;Password=$password;Encrypt=false;Connect Timeout=10" \
  > "$work/efcore-dotnet.out" 2>&1 || true
stop

: > "$work/output.diff"
different=""
for program in java dotnet logic-dotnet metadata-java metadata-dotnet bulk-dotnet efcore-dotnet; do
  if ! cmp -s "$here/expected-$program.out" "$work/$program.out"; then
    diff -u "$here/expected-$program.out" "$work/$program.out" >> "$work/output.diff" || true
    different="$different $program"
  fi
done
if [ -n "$different" ]; then
  head -n 120 "$work/output.diff" >&2
  tail -n 40 "$work/serve.err" >&2
  fail "the$different client programs printed something else on EggDB (whole diff in $work/output.diff)"
fi
echo "tds-apps check passed: mssql-jdbc and Microsoft.Data.SqlClient run parameterized CRUD, transactions, Transact-SQL batch logic and stored procedures with output parameters, list the restored fixture's metadata and load a bcp file with SqlBulkCopy and eggdb import, and an EF Core app migrates, saves and queries on EggDB"
