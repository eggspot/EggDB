#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/ss-follow/check.sh <eggdb binary> <work directory>

Starts SQL Server 2022 in Docker with SQL Server Agent on (CDC's capture job needs it), turns on
change tracking and change data capture (CDC), and follows the database with eggdb follow:
- without --changes, customers and orders (CDC capture instances) are read through CDC and notes
  (change tracking only) through change tracking, while inserts, updates, deletes and a primary
  key update arrive during the copy and after it; SIGTERM stops it;
- --changes change-tracking --until caught-up reads every table through change tracking;
- --changes cdc refuses notes, which has no capture instance;
- a copy into an existing file that already has orders fails and drops the tables it created, so
  the same command runs again once orders is gone;
- CDC cleanup past the follow's position ends the follow with a message to copy afresh.
Every copy is compared with SQL Server through go-sqlcmd against eggdb serve --tds-listen.
SQLCMD names go-sqlcmd (default sqlcmd)."

[ $# -eq 2 ] || { echo "$usage" >&2; exit 2; }
eggdb=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
work=$2
mkdir -p "$work"
work=$(cd "$work" && pwd)
sqlcmd=${SQLCMD:-sqlcmd}
container="eggdb-ss-follow"
port=51433
source_password='Follow-check-2026'
served_password='served-secret'
follow_pid=
serve_pid=

fail() {
  echo "ss-follow check FAILED: $*" >&2
  [ -f "$work/follow.err" ] && sed 's/^/  follow: /' "$work/follow.err" >&2
  exit 1
}

cleanup() {
  [ -n "$follow_pid" ] && kill -TERM "$follow_pid" 2> /dev/null || true
  [ -n "$serve_pid" ] && kill -TERM "$serve_pid" 2> /dev/null || true
  docker rm -f "$container" > /dev/null 2>&1 || true
}
trap cleanup EXIT

source_sql() {
  SQLCMDPASSWORD=$source_password "$sqlcmd" -S "127.0.0.1,$port" -U sa -d "${database:-app}" -N true -C \
    -b -h -1 -W "$@"
}

docker rm -f "$container" > /dev/null 2>&1 || true
docker run -d --name "$container" -e ACCEPT_EULA=Y -e MSSQL_SA_PASSWORD="$source_password" \
  -e MSSQL_AGENT_ENABLED=true -p "$port:1433" mcr.microsoft.com/mssql/server:2022-latest > /dev/null
for _ in $(seq 1 120); do
  database=master source_sql -Q "SET NOCOUNT ON; SELECT 1" > /dev/null 2>&1 && break
  sleep 1
done
database=master source_sql -Q "SET NOCOUNT ON; SELECT 1" > /dev/null || fail "SQL Server did not start"
agent_ready_query="SET NOCOUNT ON;
DECLARE @starting int = 1;
EXEC master.dbo.xp_sqlagent_is_starting @starting OUTPUT;
SELECT CASE WHEN @starting = 0 AND EXISTS (SELECT 1 FROM sys.dm_server_services
  WHERE servicename LIKE N'SQL Server Agent%' AND status_desc = N'Running') THEN 1 ELSE 0 END"
agent_ready=
for _ in $(seq 1 180); do
  agent_ready=$(database=master source_sql -Q "$agent_ready_query" 2> /dev/null | tr -d '[:space:]' || true)
  [ "$agent_ready" = "1" ] && break
  sleep 1
done
[ "$agent_ready" = "1" ] || fail "SQL Server Agent did not finish starting, and CDC's capture job needs it"
database=master source_sql -Q "CREATE DATABASE app" || fail "cannot create the database"
source_sql -Q "ALTER DATABASE app SET CHANGE_TRACKING = ON (CHANGE_RETENTION = 2 DAYS, AUTO_CLEANUP = ON)" \
  || fail "cannot turn change tracking on"
source_sql -Q "EXEC sys.sp_cdc_enable_db" || fail "cannot turn CDC on"
source_sql -Q "SET NOCOUNT ON;
CREATE TABLE dbo.customers (id int PRIMARY KEY, name nvarchar(100) NOT NULL, email nvarchar(200) NULL);
CREATE TABLE dbo.orders (id int IDENTITY PRIMARY KEY, customer_id int NOT NULL, amount decimal(10, 2) NOT NULL, note nvarchar(max) NULL);
CREATE TABLE dbo.notes (id int PRIMARY KEY, body nvarchar(100) NULL);
ALTER TABLE dbo.customers ENABLE CHANGE_TRACKING;
ALTER TABLE dbo.orders ENABLE CHANGE_TRACKING;
ALTER TABLE dbo.notes ENABLE CHANGE_TRACKING;
EXEC sys.sp_cdc_enable_table @source_schema = N'dbo', @source_name = N'customers', @role_name = NULL;
EXEC sys.sp_cdc_enable_table @source_schema = N'dbo', @source_name = N'orders', @role_name = NULL;
WITH n AS (SELECT TOP (2000) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS i FROM sys.all_objects a CROSS JOIN sys.all_objects b)
INSERT INTO dbo.customers (id, name, email) SELECT i, CONCAT(N'customer ', i), CONCAT(N'c', i, N'@example.com') FROM n;
WITH n AS (SELECT TOP (20000) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS i FROM sys.all_objects a CROSS JOIN sys.all_objects b)
INSERT INTO dbo.orders (customer_id, amount, note)
SELECT 1 + i % 2000, CAST((i % 997) / 7.0 AS decimal(10, 2)), CASE WHEN i % 3 = 0 THEN NULL ELSE CONCAT(N'it''s order ', i) END FROM n;
WITH n AS (SELECT TOP (300) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS i FROM sys.all_objects)
INSERT INTO dbo.notes (id, body) SELECT i, CONCAT(N'note ', i) FROM n;" || fail "cannot fill the database"

writes_during_copy() {
  source_sql -Q "SET NOCOUNT ON;
INSERT INTO dbo.customers (id, name, email) VALUES (5001, N'late arrival', N'late@example.com');
UPDATE dbo.orders SET amount = amount + 1, note = N'changed' WHERE id % 50 = 0;
DELETE FROM dbo.orders WHERE id % 97 = 0;
BEGIN TRANSACTION;
INSERT INTO dbo.orders (customer_id, amount) SELECT TOP (300) 7, 12.34 FROM sys.all_objects;
UPDATE dbo.customers SET email = NULL WHERE id = 3;
COMMIT;
UPDATE dbo.notes SET body = N'edited' WHERE id % 10 = 0;
DELETE FROM dbo.notes WHERE id % 7 = 0;" || fail "the writes during the copy failed"
}

writes_after_copy() {
  source_sql -Q "SET NOCOUNT ON;
UPDATE dbo.customers SET id = 9004 WHERE id = 4;
UPDATE dbo.customers SET name = N'renamed' WHERE id = 1;
DELETE FROM dbo.customers WHERE id = 2;
INSERT INTO dbo.notes (id, body) VALUES (1001, NULL);
INSERT INTO dbo.customers (id, name) VALUES (7777, N'last write');" || fail "the writes after the copy failed"
}

wait_for_capture() {
  local marker=$1
  for _ in $(seq 1 120); do
    captured=$(source_sql -Q "SET NOCOUNT ON; SELECT COUNT(*) FROM cdc.dbo_customers_CT WHERE name = N'$marker'" 2> /dev/null || true)
    [ "$(echo "$captured" | tr -d '[:space:]')" = "1" ] && return 0
    sleep 1
  done
  fail "CDC's capture job did not capture '$marker' (is SQL Server Agent running?)"
}

compare() {
  local file=$1
  "$eggdb" serve "$file" --listen 127.0.0.1:0 --tds-listen 127.0.0.1:0 --tls-self-signed --database app \
    > "$work/serve.out" 2> "$work/serve.err" < /dev/null &
  serve_pid=$!
  for _ in $(seq 1 100); do
    [ -s "$work/serve.out" ] && break
    sleep 0.1
  done
  local tds_port
  tds_port=$(sed -n 's/.*"tds_port":\([0-9]*\).*/\1/p' "$work/serve.out" | head -n 1)
  [ -n "$tds_port" ] || fail "eggdb serve printed no TDS port: $(cat "$work/serve.out" "$work/serve.err")"
  for query in \
    "SELECT CONCAT(id, N'|', name, N'|', COALESCE(email, N'<null>')) FROM dbo.customers ORDER BY id" \
    "SELECT CONCAT(id, N'|', customer_id, N'|', amount, N'|', COALESCE(note, N'<null>')) FROM dbo.orders ORDER BY id" \
    "SELECT CONCAT(id, N'|', COALESCE(body, N'<null>')) FROM dbo.notes ORDER BY id"; do
    source_sql -Q "SET NOCOUNT ON; $query" > "$work/source.out" || fail "querying SQL Server failed: $query"
    SQLCMDPASSWORD=$served_password "$sqlcmd" -S "127.0.0.1,$tds_port" -U eggdb -d app -N true -C -b -h -1 -W \
      -Q "SET NOCOUNT ON; $query" > "$work/target.out" 2> "$work/eggdb.err" \
      || fail "querying eggdb failed: $(cat "$work/target.out" "$work/eggdb.err")"
    diff -u "$work/source.out" "$work/target.out" > "$work/output.diff" || fail "$query differs (see output.diff)"
  done
  kill -TERM "$serve_pid"
  wait "$serve_pid" || true
  serve_pid=
}

url="sqlserver://sa@127.0.0.1:$port/app?trustServerCertificate=true"
export EGGDB_PASSWORD=$served_password

rm -f "$work/follow.db"
SQLCMDPASSWORD=$source_password "$eggdb" follow --create "$work/follow.db" "$url" --poll 1 --format json \
  > "$work/follow.out" 2> "$work/follow.err" &
follow_pid=$!
for _ in $(seq 1 120); do
  grep -q '"event":"table_copied"' "$work/follow.out" 2> /dev/null && break
  sleep 0.5
done
writes_during_copy
for _ in $(seq 1 120); do
  grep -q '"event":"copied"' "$work/follow.out" 2> /dev/null && break
  sleep 0.5
done
grep -q '"event":"copied"' "$work/follow.out" || fail "the copy did not finish"
grep -q '"table":"dbo.customers".*"changes":"cdc"' "$work/follow.out" || fail "customers was not read through CDC"
grep -q '"table":"dbo.orders".*"changes":"cdc"' "$work/follow.out" || fail "orders was not read through CDC"
grep -q '"table":"dbo.notes".*"changes":"change_tracking"' "$work/follow.out" || fail "notes was not read through change tracking"
writes_after_copy
wait_for_capture 'last write'
sleep 4
kill -0 "$follow_pid" 2> /dev/null || fail "follow exited while following, before SIGTERM: $(tail -n 3 "$work/follow.out")"
kill -TERM "$follow_pid"
wait "$follow_pid" || fail "follow did not exit 0 after SIGTERM"
follow_pid=
grep -q '"event":"stopped","ok":true' "$work/follow.out" || fail "no stopped event: $(tail -n 3 "$work/follow.out")"
grep -q '"lsn":"0x' "$work/follow.out" || fail "the stopped event has no CDC position"
compare "$work/follow.db"

rm -f "$work/tracking.db"
SQLCMDPASSWORD=$source_password "$eggdb" follow --create "$work/tracking.db" "$url" --changes change-tracking \
  --until caught-up --format json > "$work/tracking.out" 2> "$work/follow.err" \
  || fail "follow --changes change-tracking --until caught-up failed"
grep -q '"reason":"caught_up"' "$work/tracking.out" || fail "no caught_up stop"
grep -q '"changes":"cdc"' "$work/tracking.out" && fail "--changes change-tracking read a table through CDC"
compare "$work/tracking.db"

rm -f "$work/refused.db"
if SQLCMDPASSWORD=$source_password "$eggdb" follow --create "$work/refused.db" "$url" --changes cdc \
  > "$work/refused.out" 2> "$work/refused.err"; then
  fail "--changes cdc followed a table without a capture instance"
fi
grep -q "dbo.notes" "$work/refused.err" || fail "--changes cdc did not name dbo.notes: $(cat "$work/refused.err")"
grep -q "sp_cdc_enable_table" "$work/refused.err" || fail "--changes cdc did not say how to capture: $(cat "$work/refused.err")"

rm -f "$work/existing.db"
"$eggdb" sql --create --dialect sqlserver "$work/existing.db" -c "CREATE TABLE dbo.orders (id int PRIMARY KEY)" \
  > /dev/null || fail "creating a file that already has dbo.orders failed"
if SQLCMDPASSWORD=$source_password "$eggdb" follow "$work/existing.db" "$url" --changes change-tracking \
  --until caught-up > "$work/existing.out" 2> "$work/existing.err"; then
  fail "follow copied into a file that already has dbo.orders"
fi
grep -q "already has the source's table dbo.orders" "$work/existing.err" \
  || fail "the refusal does not name dbo.orders: $(cat "$work/existing.err")"
for created in dbo.customers dbo.notes; do
  if "$eggdb" sql --dialect sqlserver "$work/existing.db" -c "SELECT COUNT(*) FROM $created" > /dev/null 2>&1; then
    fail "the failed copy left $created in the file"
  fi
done
"$eggdb" sql --dialect sqlserver "$work/existing.db" -c "DROP TABLE dbo.orders" > /dev/null \
  || fail "dropping dbo.orders from the existing file failed"
SQLCMDPASSWORD=$source_password "$eggdb" follow "$work/existing.db" "$url" --changes change-tracking \
  --until caught-up --format json > "$work/existing.out" 2> "$work/follow.err" \
  || fail "running the same follow again after the failed copy failed"
grep -q '"reason":"caught_up"' "$work/existing.out" || fail "the second follow into the existing file did not catch up"

rm -f "$work/cleaned.db"
SQLCMDPASSWORD=$source_password "$eggdb" follow --create "$work/cleaned.db" "$url" --schema dbo --poll 20 --format json \
  > "$work/cleaned.out" 2> "$work/follow.err" &
follow_pid=$!
for _ in $(seq 1 120); do
  grep -q '"event":"copied"' "$work/cleaned.out" 2> /dev/null && break
  sleep 0.5
done
grep -q '"event":"copied"' "$work/cleaned.out" || fail "the copy before the cleanup did not finish"
sleep 2
source_sql -Q "SET NOCOUNT ON; UPDATE dbo.orders SET note = N'cleaned away' WHERE id = 1; INSERT INTO dbo.customers (id, name) VALUES (8888, N'cleanup marker');" \
  || fail "the writes before the cleanup failed"
wait_for_capture 'cleanup marker'
source_sql -Q "SET NOCOUNT ON; DECLARE @mark binary(10) = sys.fn_cdc_get_max_lsn(); EXEC sys.sp_cdc_cleanup_change_table @capture_instance = N'dbo_orders', @low_water_mark = @mark, @threshold = 100000; EXEC sys.sp_cdc_cleanup_change_table @capture_instance = N'dbo_customers', @low_water_mark = @mark, @threshold = 100000;" \
  || fail "the CDC cleanup failed"
status=0
wait "$follow_pid" || status=$?
follow_pid=
[ "$status" = "3" ] || fail "follow exited $status after the cleanup, not 3"
grep -q "CDC cleanup removed changes" "$work/follow.err" || fail "no cleanup message: $(cat "$work/follow.err")"
grep -q "Follow again into a new file" "$work/follow.err" || fail "the cleanup message does not say to copy afresh"

echo "ss-follow check passed: CDC and change tracking chosen per table, live inserts, updates, deletes and a key update, --changes change-tracking with --until caught-up, --changes cdc refused, a failed copy into an existing file undone, CDC cleanup detected"
