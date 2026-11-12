#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 3 ]; then
  echo "usage: scripts/dashboards.sh <eggdb binary> <matrix file> <work directory>" >&2
  exit 2
fi
eggdb="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
matrix="$2"
work="$3"
export EGGDB_PASSWORD="${EGGDB_PASSWORD:-dashboards}"
es="http://127.0.0.1:59200"

"$eggdb" serve "$work/dashboards.eggdb" --create --listen 127.0.0.1:55433 --es-listen 127.0.0.1:59200 &
server=$!
containers=()
cleanup() {
  for container in "${containers[@]}"; do
    docker rm -f "$container" > /dev/null 2>&1 || true
  done
  kill "$server" 2> /dev/null || true
}
trap cleanup EXIT
for _ in $(seq 1 50); do
  curl -fsS -u "eggdb:$EGGDB_PASSWORD" "$es/" > /dev/null 2>&1 && break
  sleep 0.2
done
curl -fsS -u "eggdb:$EGGDB_PASSWORD" -X PUT "$es/orders" -H 'content-type: application/json' \
  -d '{"mappings":{"properties":{"placed_on":{"type":"date"},"total":{"type":"double"},"customer":{"type":"keyword"}}}}' > /dev/null
curl -fsS -u "eggdb:$EGGDB_PASSWORD" -X PUT "$es/orders/_doc/1?refresh=true" -H 'content-type: application/json' \
  -d '{"placed_on":"2026-11-13T10:00:00Z","total":12.5,"customer":"ada"}' > /dev/null

report() { echo "| $1 | $2 |" >> "$matrix"; }
failed=0

check_dashboard() {
  local name="$1" port="$2" xsrf="$3"
  shift 3
  containers+=("$name")
  docker run -d --name "$name" --network host "$@" > /dev/null
  local up=0
  for _ in $(seq 1 120); do
    if curl -fsS "http://127.0.0.1:$port/api/status" 2> /dev/null | grep -q '"state":"green"'; then
      up=1
      break
    fi
    sleep 2
  done
  if [ "$up" = 0 ]; then
    report "$name: starts against eggdb" "fails"
    docker logs --tail 40 "$name" || true
    failed=1
    return
  fi
  report "$name: starts against eggdb" "passes"
  if curl -fsS -X POST "http://127.0.0.1:$port/api/saved_objects/index-pattern" -H "$xsrf: true" \
    -H 'content-type: application/json' -d '{"attributes":{"title":"orders*","timeFieldName":"placed_on"}}' > /dev/null \
    && curl -fsS "http://127.0.0.1:$port/api/index_patterns/_fields_for_wildcard?pattern=orders*" -H "$xsrf: true" | grep -q '"total"'; then
    report "$name: index pattern with its fields" "passes"
  else
    report "$name: index pattern with its fields" "fails"
    docker logs --tail 40 "$name" || true
    failed=1
  fi
}

check_dashboard opensearch-dashboards 5601 osd-xsrf \
  -e OPENSEARCH_HOSTS="[\"$es\"]" -e OPENSEARCH_USERNAME=eggdb -e OPENSEARCH_PASSWORD="$EGGDB_PASSWORD" \
  -e DISABLE_SECURITY_DASHBOARDS_PLUGIN=true -e SERVER_PORT=5601 \
  opensearchproject/opensearch-dashboards:2.11.1
check_dashboard kibana-oss 5602 kbn-xsrf \
  -e ELASTICSEARCH_HOSTS="$es" -e ELASTICSEARCH_USERNAME=eggdb -e ELASTICSEARCH_PASSWORD="$EGGDB_PASSWORD" \
  -e SERVER_PORT=5602 -e TELEMETRY_ENABLED=false \
  docker.elastic.co/kibana/kibana-oss:7.10.2

if [ "${EGGDB_COMPATIBILITY_STRICT:-0}" = 1 ] && [ "$failed" = 1 ]; then
  exit 1
fi
