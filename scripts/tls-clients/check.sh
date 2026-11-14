#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/tls-clients/check.sh <eggdb binary> <work directory>

Makes a test certificate authority and a server certificate for localhost with openssl, serves a
new database with eggdb serve --tls-certificate --tls-key on all three protocols, and connects
with each protocol's own client over TLS:
- psql with sslmode=verify-full and the authority as sslrootcert, by SSLRequest and by
  PostgreSQL 17's direct TLS (sslnegotiation=direct), each printing TLS in \\conninfo;
- the mysql client with --ssl-mode=VERIFY_CA;
- go-sqlcmd with encryption mandatory (-N true, trusting the server's certificate with -C) and
  strict (-N strict, TDS 8.0, which ignores -C and checks the chain against the authority given
  through SSL_CERT_FILE).
Then it serves the RESP, REST and MQTT listeners on a non-loopback address, where serve ends TLS in
front of each protocol (D1155), and connects with redis-cli --tls, curl over HTTPS and an
openssl s_client handshake, all checking the chain against the authority, and resumes a TLS 1.3
session from the server's ticket with openssl s_client -sess_out and -sess_in (D1159). Then it serves with
--tls-self-signed and checks psql with sslmode=require. Fails on the first
client that cannot connect over TLS or prints the wrong answer.

PSQL names the psql command (default psql), MYSQL the mysql client (default mysql) and SQLCMD
go-sqlcmd (default sqlcmd), REDIS_CLI redis-cli (default redis-cli); RELAY_RESP_PORT, RELAY_ES_PORT and
RELAY_MQTT_PORT choose the relayed listeners' ports. Certificates and logs stay in the work directory."

[ $# -eq 2 ] || { echo "$usage" >&2; exit 2; }
eggdb=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
work=$2
mkdir -p "$work"
work=$(cd "$work" && pwd)
read -r -a psql_words <<< "${PSQL:-psql}"
mysql=${MYSQL:-mysql}
sqlcmd=${SQLCMD:-sqlcmd}
redis_cli=${REDIS_CLI:-redis-cli}
RELAY_RESP_PORT=${RELAY_RESP_PORT:-16390}
RELAY_ES_PORT=${RELAY_ES_PORT:-19290}
RELAY_MQTT_PORT=${RELAY_MQTT_PORT:-18890}
password='tls-check-password'
server_pid=

fail() {
  echo "tls-clients check FAILED: $*" >&2
  [ -f "$work/serve.err" ] && sed 's/^/  serve: /' "$work/serve.err" >&2
  exit 1
}

stop_server() {
  if [ -n "$server_pid" ]; then
    kill -TERM "$server_pid" 2> /dev/null || true
    wait "$server_pid" 2> /dev/null || true
    server_pid=
  fi
}
trap stop_server EXIT

openssl ecparam -name prime256v1 -genkey -noout -out "$work/ca.key"
openssl req -x509 -new -key "$work/ca.key" -sha256 -days 2 -subj "/CN=EggDB TLS check CA" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign" \
  -out "$work/ca.pem"
openssl ecparam -name prime256v1 -genkey -noout -out "$work/server.key"
openssl req -new -key "$work/server.key" -subj "/CN=localhost" -out "$work/server.csr"
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\nbasicConstraints=critical,CA:FALSE\n' > "$work/server.ext"
openssl x509 -req -in "$work/server.csr" -CA "$work/ca.pem" -CAkey "$work/ca.key" -CAcreateserial \
  -days 2 -sha256 -extfile "$work/server.ext" -out "$work/server.pem"
openssl pkcs8 -topk8 -nocrypt -in "$work/server.key" -out "$work/server-pkcs8.key"

start_server() {
  rm -f "$work/tls.db"
  printf '%s\n' "$password" > "$work/password.txt"
  "$eggdb" serve "$work/tls.db" --create --listen 127.0.0.1:0 --mysql-listen 127.0.0.1:0 \
    --tds-listen 127.0.0.1:0 --password-file "$work/password.txt" --format json "$@" \
    > "$work/serve.out" 2> "$work/serve.err" &
  server_pid=$!
  for _ in $(seq 1 100); do
    [ -s "$work/serve.out" ] && break
    sleep 0.1
  done
  first=$(head -n 1 "$work/serve.out")
  [ -n "$first" ] || fail "eggdb serve printed nothing"
  pg_port=$(printf '%s' "$first" | sed -E 's/.*"port":([0-9]+).*/\1/')
  mysql_port=$(printf '%s' "$first" | sed -E 's/.*"mysql_port":([0-9]+).*/\1/')
  tds_port=$(printf '%s' "$first" | sed -E 's/.*"tds_port":([0-9]+).*/\1/')
  printf '%s' "$first" | grep -q '"tls":true' || fail "the server does not report TLS: $first"
}

psql_tls() {
  PGPASSWORD=$password "${psql_words[@]}" "$1" -X -q -A -t -c '\conninfo' -c 'SELECT 6 * 7' > "$work/psql.out" 2> "$work/psql.err" \
    || fail "psql ($1) failed: $(cat "$work/psql.err")"
  grep -q 'SSL' "$work/psql.out" || fail "psql ($1) did not use TLS: $(cat "$work/psql.out")"
  grep -qx '42' "$work/psql.out" || fail "psql ($1) printed $(cat "$work/psql.out")"
}

start_server --tls-certificate "$work/server.pem" --tls-key "$work/server-pkcs8.key"
psql_tls "host=localhost port=$pg_port user=eggdb dbname=tls sslmode=verify-full sslrootcert=$work/ca.pem"
psql_tls "host=localhost port=$pg_port user=eggdb dbname=tls sslmode=verify-full sslrootcert=$work/ca.pem sslnegotiation=direct"

MYSQL_PWD=$password "$mysql" -h 127.0.0.1 -P "$mysql_port" -u eggdb -D tls --batch --skip-column-names \
  --connect-timeout=10 --ssl-mode=VERIFY_CA --ssl-ca="$work/ca.pem" \
  -e "SELECT 6 * 7" > "$work/mysql.out" 2> "$work/mysql.err" \
  || fail "mysql failed: $(cat "$work/mysql.err")"
grep -qx '42' "$work/mysql.out" || fail "mysql printed $(cat "$work/mysql.out")"

for encryption in true strict; do
  SSL_CERT_FILE="$work/ca.pem" SQLCMDPASSWORD=$password \
    "$sqlcmd" -S "localhost,$tds_port" -U eggdb -d tls -N "$encryption" \
    -C -h -1 -W -Q "SET NOCOUNT ON; SELECT 6 * 7" > "$work/sqlcmd.out" 2> "$work/sqlcmd.err" \
    || fail "sqlcmd -N $encryption failed: $(cat "$work/sqlcmd.out" "$work/sqlcmd.err")"
  grep -qx '42' "$work/sqlcmd.out" || fail "sqlcmd -N $encryption printed $(cat "$work/sqlcmd.out")"
done
stop_server

start_server --tls-certificate "$work/server.pem" --tls-key "$work/server-pkcs8.key" \
  --resp-listen "0.0.0.0:$RELAY_RESP_PORT" --es-listen "0.0.0.0:$RELAY_ES_PORT" --mqtt-listen "0.0.0.0:$RELAY_MQTT_PORT"
for _ in $(seq 1 50); do
  (exec 3<> "/dev/tcp/127.0.0.1/$RELAY_MQTT_PORT") 2> /dev/null && break
  sleep 0.1
done
REDISCLI_AUTH=$password "$redis_cli" -h localhost -p "$RELAY_RESP_PORT" --tls --cacert "$work/ca.pem" PING \
  > "$work/redis.out" 2> "$work/redis.err" || fail "redis-cli over TLS failed: $(cat "$work/redis.err")"
grep -qx 'PONG' "$work/redis.out" || fail "redis-cli over TLS printed $(cat "$work/redis.out")"
es_status=$(curl -s -o /dev/null -w '%{http_code}' --cacert "$work/ca.pem" "https://localhost:$RELAY_ES_PORT/") \
  || fail "curl could not reach the REST API over TLS"
case "$es_status" in 2?? | 401) ;; *) fail "the REST API over TLS answered HTTP $es_status" ;; esac
openssl s_client -connect "127.0.0.1:$RELAY_RESP_PORT" -servername localhost -CAfile "$work/ca.pem" \
  -tls1_3 -sess_out "$work/tls-session.pem" < /dev/null > "$work/first-session.out" 2>&1 \
  || fail "the first TLS 1.3 handshake failed: $(tail -n 5 "$work/first-session.out")"
[ -s "$work/tls-session.pem" ] || fail "the server sent no TLS 1.3 session ticket"
openssl s_client -connect "127.0.0.1:$RELAY_RESP_PORT" -servername localhost -CAfile "$work/ca.pem" \
  -tls1_3 -sess_in "$work/tls-session.pem" < /dev/null > "$work/resumed-session.out" 2>&1 \
  || fail "the resumed TLS 1.3 handshake failed: $(tail -n 5 "$work/resumed-session.out")"
grep -q '^Reused, TLSv1.3' "$work/resumed-session.out" \
  || fail "the second handshake did not resume the session: $(grep -E '^(New|Reused)' "$work/resumed-session.out")"
openssl s_client -connect "127.0.0.1:$RELAY_MQTT_PORT" -servername localhost -CAfile "$work/ca.pem" \
  -verify_return_error < /dev/null > "$work/mqtt-tls.out" 2>&1 || fail "the MQTT listener's TLS handshake failed: $(tail -n 5 "$work/mqtt-tls.out")"
stop_server

start_server --tls-self-signed --tls-min-version 1.3
psql_tls "host=127.0.0.1 port=$pg_port user=eggdb dbname=tls sslmode=require"
stop_server

echo "tls-clients check passed: psql (verify-full, direct TLS, self-signed), mysql (VERIFY_CA), sqlcmd (-N true, -N strict), redis-cli --tls, HTTPS REST, MQTT over TLS"
