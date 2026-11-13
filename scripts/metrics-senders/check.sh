#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/metrics-senders/check.sh <eggdb binary> <work directory>

Serves a new database with eggdb serve --observability-listen 127.0.0.1:18086 and
--metrics-listen 127.0.0.1:19187, then runs the two collectors people send metrics with, on the
host network:
- Telegraf (TELEGRAF_IMAGE, default telegraf:1.30), collecting cpu and mem every second and
  writing them with its influxdb output (InfluxDB 1.x /write, user and password) and its
  influxdb_v2 output (/api/v2/write, the password as token);
- Prometheus (PROMETHEUS_IMAGE, default prom/prometheus:v2.53.0), scraping eggdb's own
  --metrics-listen endpoint every second and sending what it scrapes with remote_write (Basic
  authentication).
Then it asks eggdb's Prometheus query API, with curl, for what each one sent: Telegraf's
cpu_usage_idle and mem_used_percent, and Prometheus's up and eggdb_sessions_open for the scraped
job. Fails on the first query that has no result within 60 seconds."

[ $# -eq 2 ] || { echo "$usage" >&2; exit 2; }
eggdb=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
work=$2
mkdir -p "$work"
work=$(cd "$work" && pwd)
telegraf_image=${TELEGRAF_IMAGE:-telegraf:1.30}
prometheus_image=${PROMETHEUS_IMAGE:-prom/prometheus:v2.53.0}
password='metrics-check-password'
server_pid=

fail() {
  echo "metrics-senders check FAILED: $*" >&2
  for log in "$work"/*.err; do
    [ -f "$log" ] && sed "s|^|  $(basename "$log"): |" "$log" >&2
  done
  for container in eggdb-telegraf eggdb-prometheus; do
    docker logs "$container" 2>&1 | tail -n 20 | sed "s|^|  $container: |" >&2 || true
  done
  exit 1
}

stop_all() {
  [ -n "$server_pid" ] && kill -TERM "$server_pid" 2> /dev/null || true
  docker rm -f eggdb-telegraf eggdb-prometheus > /dev/null 2>&1 || true
}
trap stop_all EXIT

printf '%s\n' "$password" > "$work/password.txt"
"$eggdb" serve "$work/metrics.db" --create --listen 127.0.0.1:0 --password-file "$work/password.txt" \
  --observability-listen 127.0.0.1:18086 --metrics-listen 127.0.0.1:19187 \
  > "$work/serve.out" 2> "$work/serve.err" &
server_pid=$!
for _ in $(seq 1 100); do
  (exec 3<> /dev/tcp/127.0.0.1/18086) 2> /dev/null && break
  sleep 0.1
done

cat > "$work/telegraf.conf" <<CONF
[agent]
  interval = "1s"
  flush_interval = "1s"
  omit_hostname = false
[[inputs.cpu]]
  percpu = false
  totalcpu = true
[[inputs.mem]]
[[outputs.influxdb]]
  urls = ["http://127.0.0.1:18086"]
  database = "telegraf"
  skip_database_creation = true
  username = "eggdb"
  password = "$password"
[[outputs.influxdb_v2]]
  urls = ["http://127.0.0.1:18086"]
  token = "$password"
  organization = "eggdb"
  bucket = "telegraf"
CONF
docker run -d --name eggdb-telegraf --network host \
  -v "$work/telegraf.conf:/etc/telegraf/telegraf.conf:ro" "$telegraf_image" > /dev/null \
  || fail "Telegraf did not start"

cat > "$work/prometheus.yml" <<CONF
global:
  scrape_interval: 1s
scrape_configs:
  - job_name: eggdb
    static_configs:
      - targets: ["127.0.0.1:19187"]
remote_write:
  - url: http://127.0.0.1:18086/api/v1/write
    basic_auth:
      username: eggdb
      password: $password
    queue_config:
      batch_send_deadline: 1s
CONF
docker run -d --name eggdb-prometheus --network host \
  -v "$work/prometheus.yml:/etc/prometheus/prometheus.yml:ro" "$prometheus_image" \
  --config.file=/etc/prometheus/prometheus.yml --web.listen-address=127.0.0.1:19090 > /dev/null \
  || fail "Prometheus did not start"

expect_series() {
  local what=$1 query=$2 answer=
  for _ in $(seq 1 120); do
    answer=$(curl -fsS -u "eggdb:$password" --get --data-urlencode "query=$query" \
      http://127.0.0.1:18086/api/v1/query 2>> "$work/curl.err" || true)
    if printf '%s' "$answer" | grep -q '"status":"success"' && ! printf '%s' "$answer" | grep -q '"result":\[\]'; then
      echo "$what: $(printf '%s' "$answer" | head -c 200)"
      return
    fi
    sleep 0.5
  done
  fail "$what: $query has no result: $answer"
}

expect_series "Telegraf cpu (InfluxDB outputs)" 'cpu_usage_idle{cpu="cpu-total"}'
expect_series "Telegraf mem (InfluxDB outputs)" 'mem_used_percent'
expect_series "Prometheus up (remote write)" 'up{job="eggdb"}'
expect_series "Prometheus scraped eggdb metrics (remote write)" 'eggdb_sessions_open{job="eggdb"}'

echo "metrics-senders check passed: Telegraf (influxdb, influxdb_v2) and Prometheus (scrape, remote_write)"
