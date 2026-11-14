#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/ldap/check.sh <eggdb binary> <work directory>

Checks EggDB's ldap rules (D185, D802) against a real OpenLDAP directory listening on LDAP_URL
(default ldap://127.0.0.1:1389), with the users alice and bob under LDAP_USERS_DN (default
ou=users,dc=example,dc=org), whose passwords are ALICE_PASSWORD and BOB_PASSWORD, and an admin
LDAP_ADMIN_DN with LDAP_ADMIN_PASSWORD for searches. It serves a new database over TLS
(--tls-self-signed, since an ldap rule takes the password in clear text) with a rule file that
signs alice in by a simple bind to her DN and bob by search-and-bind, then connects with psql:
each must log in with the directory's password, and fail with a wrong one.

PSQL names the psql command (default psql)."

[ $# -eq 2 ] || { echo "$usage" >&2; exit 2; }
eggdb=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
work=$2
mkdir -p "$work"
work=$(cd "$work" && pwd)
psql=${PSQL:-psql}
ldap_url=${LDAP_URL:-ldap://127.0.0.1:1389}
users_dn=${LDAP_USERS_DN:-ou=users,dc=example,dc=org}
admin_dn=${LDAP_ADMIN_DN:-cn=admin,dc=example,dc=org}
: "${LDAP_ADMIN_PASSWORD:?LDAP_ADMIN_PASSWORD must name the password of the directory admin}"
: "${ALICE_PASSWORD:?ALICE_PASSWORD must name the directory password of alice}"
: "${BOB_PASSWORD:?BOB_PASSWORD must name the directory password of bob}"
password=ldap-check-server-password
server_pid=

fail() {
  echo "ldap check FAILED: $*" >&2
  [ -f "$work/serve.err" ] && sed 's/^/  serve: /' "$work/serve.err" >&2
  exit 1
}
trap '[ -n "$server_pid" ] && kill -TERM "$server_pid" 2> /dev/null || true' EXIT

printf '%s\n' "$password" > "$work/password.txt"
rm -f "$work/ldap.db"
"$eggdb" create "$work/ldap.db" > /dev/null || fail "eggdb create failed"
"$eggdb" sql "$work/ldap.db" -c "CREATE ROLE alice LOGIN; CREATE ROLE bob LOGIN" > "$work/roles.out" 2>&1 ||
  fail "CREATE ROLE: $(cat "$work/roles.out")"
cat > "$work/auth.rules" <<RULES
alice ldap server=$ldap_url prefix=cn= suffix=,$users_dn
bob ldap server=$ldap_url basedn=$users_dn search_attribute=cn bind_dn=$admin_dn bind_password=$LDAP_ADMIN_PASSWORD
RULES
"$eggdb" serve "$work/ldap.db" --listen 127.0.0.1:0 --password-file "$work/password.txt" --database ldap \
  --tls-self-signed --auth-rules "$work/auth.rules" > "$work/serve.out" 2> "$work/serve.err" &
server_pid=$!
for _ in $(seq 1 100); do
  [ -s "$work/serve.out" ] && break
  sleep 0.1
done
port=$(sed -n 's/.*"port":\([0-9]*\).*/\1/p' "$work/serve.out" | head -n 1)
[ -n "$port" ] || fail "eggdb serve printed no port: $(cat "$work/serve.out")"

login() {
  local user=$1 secret=$2
  PGPASSWORD=$secret "$psql" "host=127.0.0.1 port=$port dbname=ldap user=$user sslmode=require" -X -A -t \
    -c 'SELECT current_user' 2> "$work/$user.err"
}

for user in alice bob; do
  case "$user" in
    alice) secret=$ALICE_PASSWORD ;;
    *) secret=$BOB_PASSWORD ;;
  esac
  answer=$(login "$user" "$secret") || fail "$user could not log in with the directory's password: $(cat "$work/$user.err")"
  [ "$answer" = "$user" ] || fail "$user logged in as '$answer'"
  if login "$user" "not-the-password" > /dev/null; then
    fail "$user logged in with a wrong password"
  fi
done
echo "ldap check passed: a simple bind and a search-and-bind signed alice and bob in"
