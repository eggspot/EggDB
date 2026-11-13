#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/cold-start.sh <eggdb binary> [results file]

Times a serverless cold start (D226, D1102): a first server keeps a database of 20,000 rows in a
page store on EggDB's own S3 endpoint and stops; then a second server with an empty disk starts
over the same page store, and the script measures the milliseconds until it answers a query,
and the same for a server restoring the newest archive with --restore-from. Prints
'linux full cold_start_page_store_ms' and 'linux full cold_start_restore_ms' in
benchmarks/footprint.txt's layout and fails when one is over its budget. Needs bash, psql, curl 7.75
or later (for its SigV4 signing) and python3; uses ports 19000 to 19003 on 127.0.0.1."

[ $# -ge 1 ] && [ $# -le 2 ] || { echo "$usage" >&2; exit 2; }
eggdb=$1
results=${2:-}
root=$(cd "$(dirname "$0")/.." && pwd)
budgets="$root/benchmarks/footprint.txt"
work=$(mktemp -d)
export EGGDB_PASSWORD=cold-start-secret
export AWS_ENDPOINT_URL=http://127.0.0.1:19000 AWS_ACCESS_KEY_ID=eggdb AWS_SECRET_ACCESS_KEY=$EGGDB_PASSWORD AWS_REGION=us-east-1
pids=()
cleanup() {
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
}
trap cleanup EXIT

wait_for_port() {
  local port=$1
  for _ in $(seq 1 600); do
    if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then return 0; fi
    sleep 0.01
  done
  echo "nothing listened on $port" >&2
  return 1
}

now_ms() {
  python3 -c 'import time; print(int(time.monotonic() * 1000))' 2>/dev/null || date +%s000
}

"$eggdb" serve "$work/buckets.eggdb" --create --listen 127.0.0.1:19003 --s3-listen 127.0.0.1:19000 >/dev/null 2>&1 &
pids+=($!)
wait_for_port 19000
curl -sf --aws-sigv4 "aws:amz:us-east-1:s3" --user "eggdb:$EGGDB_PASSWORD" -X PUT http://127.0.0.1:19000/cold >/dev/null
"$eggdb" sql "$work/seed.eggdb" --create -c "CREATE TABLE t (id integer PRIMARY KEY, body text); INSERT INTO t SELECT g, repeat('x', 100) FROM generate_series(1, 20000) AS g" >/dev/null
"$eggdb" backup "$work/seed.eggdb" s3://cold/archive >/dev/null

"$eggdb" serve "$work/writer.eggdb" --create --listen 127.0.0.1:19001 --page-store s3://cold/pages >/dev/null 2>&1 &
writer=$!
wait_for_port 19001
PGPASSWORD=$EGGDB_PASSWORD psql -qAt "postgresql://eggdb@127.0.0.1:19001/writer" -c "CREATE TABLE t (id integer PRIMARY KEY, body text); INSERT INTO t SELECT g, repeat('x', 100) FROM generate_series(1, 20000) AS g" >/dev/null
kill "$writer"; wait "$writer" 2>/dev/null || true

started=$(now_ms)
"$eggdb" serve "$work/fresh.eggdb" --create --listen 127.0.0.1:19002 --page-store s3://cold/pages >/dev/null 2>&1 &
fresh=$!
pids+=("$fresh")
wait_for_port 19002
PGPASSWORD=$EGGDB_PASSWORD psql -qAt "postgresql://eggdb@127.0.0.1:19002/fresh" -c "SELECT count(*) FROM t" >/dev/null
page_store_ms=$(( $(now_ms) - started ))
kill "$fresh"; wait "$fresh" 2>/dev/null || true

started=$(now_ms)
"$eggdb" serve "$work/restored.eggdb" --listen 127.0.0.1:19002 --restore-from s3://cold/archive >/dev/null 2>&1 &
restored=$!
pids+=("$restored")
wait_for_port 19002
PGPASSWORD=$EGGDB_PASSWORD psql -qAt "postgresql://eggdb@127.0.0.1:19002/restored" -c "SELECT count(*) FROM t" >/dev/null
restore_ms=$(( $(now_ms) - started ))

lines=("linux full cold_start_page_store_ms $page_store_ms" "linux full cold_start_restore_ms $restore_ms")
over=0
for line in "${lines[@]}"; do
  echo "$line"
  read -r system profile metric value <<< "$line"
  budget=$(awk -v s="$system" -v p="$profile" -v m="$metric" '$1 == s && $2 == p && $3 == m { print $4 }' "$budgets")
  if [ -n "$budget" ] && [ "$value" -gt "$budget" ]; then
    echo "over budget: $metric is $value, the budget is $budget" >&2
    over=1
  fi
done
[ -z "$results" ] || printf '%s\n' "${lines[@]}" > "$results"
exit "$over"
