#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 4 ]; then
  echo "usage: scripts/compatibility.sh <eggdb binary> <python venv with the tools> <matrix file> <work directory>" >&2
  exit 2
fi
eggdb="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
tools="$2"
matrix="$3"
work="$4"
root="$(cd "$(dirname "$0")/.." && pwd)"
export EGGDB_PASSWORD="${EGGDB_PASSWORD:-compatibility}"
connection="host=127.0.0.1 port=55432 user=eggdb dbname=eggdb sslmode=disable"

"$eggdb" serve "$work/compat.eggdb" --create --listen 127.0.0.1:55432 &
server=$!
trap 'kill "$server" 2> /dev/null || true' EXIT
for _ in $(seq 1 50); do
  PGPASSWORD="$EGGDB_PASSWORD" psql "$connection" -qc "SELECT 1" > /dev/null 2>&1 && break
  sleep 0.2
done
PGPASSWORD="$EGGDB_PASSWORD" psql "$connection" \
  -c "CREATE TABLE IF NOT EXISTS orders (id integer PRIMARY KEY, placed_on date NOT NULL, total numeric NOT NULL)" \
  -c "INSERT INTO orders VALUES (1, '2026-10-30', 12.5), (2, '2026-10-30', 3), (3, '2026-10-31', 8) ON CONFLICT DO NOTHING" || true

report() { echo "| $1 | $2 |" >> "$matrix"; }
echo "| Tool | Result |" >> "$matrix"
echo "|------|--------|" >> "$matrix"
dbt_version="$("$tools/bin/pip" show dbt-postgres 2> /dev/null | sed -n 's/^Version: //p')"
cd "$root/crates/eggdb/tests/clients/dbt"
failed=0
for step in debug run test; do
  if "$tools/bin/dbt" "$step" --profiles-dir . > "$work/dbt-$step.log" 2>&1; then
    report "dbt-postgres ${dbt_version:-unknown}: dbt $step" "passes"
  else
    report "dbt-postgres ${dbt_version:-unknown}: dbt $step" "fails"
    tail -n 30 "$work/dbt-$step.log"
    failed=1
  fi
done
if [ "${EGGDB_COMPATIBILITY_STRICT:-0}" = 1 ] && [ "$failed" = 1 ]; then
  exit 1
fi
