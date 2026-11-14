#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/oauth/check.sh <eggdb binary> <work directory>

Checks PostgreSQL 18's OAUTHBEARER login (D801) with libpq's own device flow: it runs a small
OpenID issuer of its own (scripts/oauth/issuer.py, with an RSA key made by openssl) that grants
the device code at once, serves a new database over TLS with an oauth rule for dana naming the
issuer's discovery document and key set, and connects with psql 18 and oauth_issuer: libpq must
ask EggDB where to sign in, run the device flow against the issuer, and log in as dana with the
token. PGOAUTHDEBUG=UNSAFE lets libpq use the issuer over plain HTTP on 127.0.0.1.

PSQL names psql 18 (default /usr/lib/postgresql/18/bin/psql), PYTHON python3, ISSUER_PORT the
issuer's port (default 18765)."

[ $# -eq 2 ] || { echo "$usage" >&2; exit 2; }
eggdb=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
work=$2
mkdir -p "$work"
work=$(cd "$work" && pwd)
here=$(cd "$(dirname "$0")" && pwd)
psql=${PSQL:-/usr/lib/postgresql/18/bin/psql}
python=${PYTHON:-python3}
issuer_port=${ISSUER_PORT:-18765}
password=oauth-check-server-password
server_pid=
issuer_pid=

fail() {
  echo "oauth check FAILED: $*" >&2
  [ -f "$work/serve.err" ] && sed 's/^/  serve: /' "$work/serve.err" >&2
  [ -f "$work/issuer.err" ] && sed 's/^/  issuer: /' "$work/issuer.err" >&2
  exit 1
}
stop_all() {
  [ -n "$server_pid" ] && kill -TERM "$server_pid" 2> /dev/null || true
  [ -n "$issuer_pid" ] && kill -TERM "$issuer_pid" 2> /dev/null || true
}
trap stop_all EXIT

openssl genrsa -out "$work/issuer.key" 2048 2> /dev/null
"$python" "$here/jwks.py" "$work/issuer.key" > "$work/jwks.json" || fail "the key set was not written"
rm -f "$work/issuer.ready"
"$python" "$here/issuer.py" "$issuer_port" "$work/issuer.key" eggdb-check dana "$work/issuer.ready" 2> "$work/issuer.err" &
issuer_pid=$!
for _ in $(seq 1 50); do
  [ -s "$work/issuer.ready" ] && break
  sleep 0.1
done
issuer=$(cat "$work/issuer.ready" 2> /dev/null) || fail "the issuer did not start"

printf '%s\n' "$password" > "$work/password.txt"
rm -f "$work/oauth.db"
"$eggdb" create "$work/oauth.db" > /dev/null || fail "eggdb create failed"
"$eggdb" sql "$work/oauth.db" -c "CREATE ROLE dana LOGIN" > "$work/role.out" 2>&1 || fail "CREATE ROLE: $(cat "$work/role.out")"
printf 'dana oauth jwks=%s issuer=%s audience=eggdb-check discovery=%s/.well-known/openid-configuration scope=openid\n' \
  "$work/jwks.json" "$issuer" "$issuer" > "$work/auth.rules"
"$eggdb" serve "$work/oauth.db" --listen 127.0.0.1:0 --password-file "$work/password.txt" --database oauth \
  --tls-self-signed --auth-rules "$work/auth.rules" > "$work/serve.out" 2> "$work/serve.err" &
server_pid=$!
for _ in $(seq 1 100); do
  [ -s "$work/serve.out" ] && break
  sleep 0.1
done
port=$(sed -n 's/.*"port":\([0-9]*\).*/\1/p' "$work/serve.out" | head -n 1)
[ -n "$port" ] || fail "eggdb serve printed no port: $(cat "$work/serve.out")"

answer=$(PGOAUTHDEBUG=UNSAFE timeout 60 "$psql" \
  "host=127.0.0.1 port=$port dbname=oauth user=dana sslmode=require oauth_issuer=$issuer oauth_client_id=eggdb-check" \
  -X -A -t -c 'SELECT current_user' 2> "$work/psql.err") || fail "psql's device flow did not log in: $(cat "$work/psql.err")"
[ "$answer" = "dana" ] || fail "the session is '$answer', not dana"
grep -q 'EGGD-BCHK' "$work/psql.err" || fail "libpq did not show the device code: $(cat "$work/psql.err")"
echo "oauth check passed: psql 18 ran the device flow and logged in as dana"
