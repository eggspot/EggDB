#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/syslog-senders/check.sh <eggdb binary> <work directory>

Serves two new databases with eggdb serve --syslog-listen, one plain (UDP and TCP on
127.0.0.1:5514) and one with --tls-self-signed (syslog over TLS on 127.0.0.1:6514), then runs
the two syslog daemons people forward with, each fed one message by logger:
- rsyslog (rsyslogd from the system, RSYSLOG env to override), forwarding over TCP with
  octet-counted RFC 5424 and over UDP with the traditional RFC 3164 format;
- syslog-ng (the balabit/syslog-ng image, SYSLOG_NG_IMAGE to override), forwarding with its
  syslog() driver over TCP (RFC 5424, octet counting) and over TLS, and with network() over UDP
  (RFC 3164).
Each message must land as rows of the syslog table, read back with psql (PSQL, default psql):
two rows per daemon on the plain server and one on the TLS server, with the sender's tag as
app_name. Fails on the first count that does not arrive within 30 seconds."

[ $# -eq 2 ] || { echo "$usage" >&2; exit 2; }
eggdb=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
work=$2
mkdir -p "$work"
work=$(cd "$work" && pwd)
read -r -a psql_words <<< "${PSQL:-psql}"
rsyslogd=${RSYSLOG:-rsyslogd}
syslog_ng_image=${SYSLOG_NG_IMAGE:-balabit/syslog-ng:4.7.1}
password='syslog-check-password'
token="eggdb-$$-$(date +%s)"
plain_pid=
tls_pid=
rsyslog_pid=

fail() {
  echo "syslog-senders check FAILED: $*" >&2
  for log in "$work"/*.err; do
    [ -f "$log" ] && sed "s|^|  $(basename "$log"): |" "$log" >&2
  done
  exit 1
}

stop_all() {
  for pid in "$plain_pid" "$tls_pid" "$rsyslog_pid"; do
    [ -n "$pid" ] && kill -TERM "$pid" 2> /dev/null || true
  done
  docker rm -f eggdb-syslog-ng > /dev/null 2>&1 || true
}
trap stop_all EXIT

printf '%s\n' "$password" > "$work/password.txt"

serve() {
  local name=$1
  shift
  "$eggdb" serve "$work/$name.db" --create --listen 127.0.0.1:0 --password-file "$work/password.txt" \
    --format json "$@" > "$work/$name.out" 2> "$work/$name.err" &
  echo $!
}

port_of() {
  local name=$1
  for _ in $(seq 1 100); do
    [ -s "$work/$name.out" ] && break
    sleep 0.1
  done
  local first
  first=$(head -n 1 "$work/$name.out")
  [ -n "$first" ] || fail "eggdb serve ($name) printed nothing"
  printf '%s' "$first" | sed -E 's/.*"port":([0-9]+).*/\1/'
}

plain_pid=$(serve plain --syslog-listen 127.0.0.1:5514)
tls_pid=$(serve tls --syslog-listen 127.0.0.1:6514 --tls-self-signed)
plain_port=$(port_of plain)
tls_port=$(port_of tls)

count_rows() {
  local connection=$1 condition=$2
  PGPASSWORD=$password "${psql_words[@]}" "$connection" -X -q -A -t \
    -c "SELECT count(*) FROM syslog WHERE $condition" 2>> "$work/psql.err" || echo 0
}

expect_rows() {
  local what=$1 connection=$2 condition=$3 wanted=$4 found=0
  for _ in $(seq 1 60); do
    found=$(count_rows "$connection" "$condition")
    [ "$found" = "$wanted" ] && { echo "$what: $found rows"; return; }
    sleep 0.5
  done
  fail "$what: wanted $wanted rows where $condition, found $found"
}

mkdir -p "$work/rsyslog-spool"
cat > "$work/rsyslog.conf" <<CONF
global(workDirectory="$work/rsyslog-spool")
module(load="imtcp")
input(type="imtcp" address="127.0.0.1" port="10514" ruleset="forward")
ruleset(name="forward") {
  action(type="omfwd" target="127.0.0.1" port="5514" protocol="tcp" TCP_Framing="octet-counted" template="RSYSLOG_SyslogProtocol23Format")
  action(type="omfwd" target="127.0.0.1" port="5514" protocol="udp" template="RSYSLOG_TraditionalForwardFormat")
}
CONF
"$rsyslogd" -n -f "$work/rsyslog.conf" -i "$work/rsyslog.pid" > "$work/rsyslog.out" 2> "$work/rsyslog.err" &
rsyslog_pid=$!

cat > "$work/syslog-ng.conf" <<'CONF'
@version: 4.7
options { keep-hostname(yes); };
source s_logger { network(ip("127.0.0.1") port(10601) transport("tcp") flags(syslog-protocol)); };
destination d_tcp { syslog("127.0.0.1" port(5514) transport("tcp")); };
destination d_udp { network("127.0.0.1" port(5514) transport("udp")); };
destination d_tls { syslog("127.0.0.1" port(6514) transport("tls") tls(peer-verify(optional-untrusted))); };
log { source(s_logger); destination(d_tcp); destination(d_udp); destination(d_tls); };
CONF
docker run -d --name eggdb-syslog-ng --network host \
  -v "$work/syslog-ng.conf:/etc/syslog-ng/syslog-ng.conf:ro" "$syslog_ng_image" > /dev/null \
  || fail "syslog-ng did not start"

send() {
  local port=$1 tag=$2
  for _ in $(seq 1 60); do
    if (exec 3<> "/dev/tcp/127.0.0.1/$port") 2> /dev/null; then
      logger --tcp --server 127.0.0.1 --port "$port" --rfc5424 --tag "$tag" "$token from $tag" \
        || fail "logger could not send to $tag"
      return
    fi
    sleep 0.5
  done
  fail "$tag never listened on port $port"
}

send 10514 rsyslog-check
send 10601 syslog-ng-check

plain="host=127.0.0.1 port=$plain_port user=eggdb dbname=plain"
tls="host=127.0.0.1 port=$tls_port user=eggdb dbname=tls sslmode=require"
expect_rows "rsyslog over TCP and UDP" "$plain" "message LIKE '%$token from rsyslog-check%'" 2
expect_rows "rsyslog's tag as app_name" "$plain" "app_name = 'rsyslog-check' AND message LIKE '%$token%'" 2
expect_rows "syslog-ng over TCP and UDP" "$plain" "message LIKE '%$token from syslog-ng-check%'" 2
expect_rows "syslog-ng's tag as app_name" "$plain" "app_name = 'syslog-ng-check' AND message LIKE '%$token%'" 2
expect_rows "syslog-ng over TLS" "$tls" "app_name = 'syslog-ng-check' AND message LIKE '%$token%'" 1

echo "syslog-senders check passed: rsyslog (TCP RFC 5424, UDP RFC 3164) and syslog-ng (TCP, UDP, TLS)"
