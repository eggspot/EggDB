#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/kerberos/check.sh <eggdb binary> <work directory>

Runs a Kerberos realm of its own with MIT Kerberos's KDC (krb5-kdc, a test tool only, never shipped)
in the work directory: the realm EGGDB.TEST on 127.0.0.1, the service principal postgres/localhost
with an aes256-cts-hmac-sha1-96 key written to a keytab, and the user principal alice. Then it
serves a new database with eggdb serve --auth-rules naming that keytab for the gss method (D1184),
takes alice's ticket with kinit, and connects with psql over GSSAPI (gssencmode=disable, so the
login itself is Kerberos): the session must say current_user is alice. A second connection
without a ticket must be refused. Fails on the first step that does not.

PSQL names the psql command (default psql); KDB5_UTIL, KADMIN_LOCAL, KRB5KDC and KINIT name MIT
Kerberos's tools (defaults on PATH and in /usr/sbin)."

[ $# -eq 2 ] || { echo "$usage" >&2; exit 2; }
eggdb=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
work=$2
mkdir -p "$work"
work=$(cd "$work" && pwd)
psql=${PSQL:-psql}
kdb5_util=${KDB5_UTIL:-/usr/sbin/kdb5_util}
kadmin_local=${KADMIN_LOCAL:-/usr/sbin/kadmin.local}
krb5kdc=${KRB5KDC:-/usr/sbin/krb5kdc}
kinit=${KINIT:-kinit}
realm=EGGDB.TEST
kdc_port=${KDC_PORT:-18888}
password=kerberos-check-password
server_pid=
kdc_pid=

fail() {
  echo "kerberos check FAILED: $*" >&2
  [ -f "$work/serve.err" ] && sed 's/^/  serve: /' "$work/serve.err" >&2
  [ -f "$work/kdc.log" ] && sed 's/^/  kdc: /' "$work/kdc.log" >&2
  exit 1
}

stop_all() {
  [ -n "$server_pid" ] && kill -TERM "$server_pid" 2> /dev/null || true
  [ -n "$kdc_pid" ] && kill -TERM "$kdc_pid" 2> /dev/null || true
}
trap stop_all EXIT

export KRB5_CONFIG="$work/krb5.conf"
export KRB5_KDC_PROFILE="$work/kdc.conf"
export KRB5CCNAME="FILE:$work/ccache"
mkdir -p "$work/kdc"
cat > "$KRB5_CONFIG" <<CONF
[libdefaults]
    default_realm = $realm
    dns_lookup_kdc = false
    dns_lookup_realm = false
    rdns = false
    permitted_enctypes = aes256-cts-hmac-sha1-96 aes128-cts-hmac-sha1-96
[realms]
    $realm = {
        kdc = 127.0.0.1:$kdc_port
    }
CONF
cat > "$KRB5_KDC_PROFILE" <<CONF
[kdcdefaults]
    kdc_ports = $kdc_port
    kdc_tcp_ports = $kdc_port
[realms]
    $realm = {
        database_name = $work/kdc/principal
        key_stash_file = $work/kdc/stash
        supported_enctypes = aes256-cts-hmac-sha1-96:normal aes128-cts-hmac-sha1-96:normal
        acl_file = $work/kdc/kadm5.acl
    }
[logging]
    kdc = FILE:$work/kdc.log
CONF
"$kdb5_util" create -s -r "$realm" -P kdc-master-password > "$work/kdb5.out" 2>&1 || fail "kdb5_util create: $(cat "$work/kdb5.out")"
"$kadmin_local" -r "$realm" -q "addprinc -randkey postgres/localhost" > "$work/kadmin.out" 2>&1 || fail "addprinc postgres: $(cat "$work/kadmin.out")"
"$kadmin_local" -r "$realm" -q "ktadd -k $work/eggdb.keytab -e aes256-cts-hmac-sha1-96:normal postgres/localhost" >> "$work/kadmin.out" 2>&1 ||
  fail "ktadd: $(cat "$work/kadmin.out")"
"$kadmin_local" -r "$realm" -q "addprinc -pw alice-kerberos-password alice" >> "$work/kadmin.out" 2>&1 || fail "addprinc alice: $(cat "$work/kadmin.out")"
"$krb5kdc" -n -r "$realm" > "$work/kdc.out" 2>&1 &
kdc_pid=$!
sleep 1

printf '%s\n' "$password" > "$work/password.txt"
"$eggdb" create "$work/kerberos.db" > /dev/null || fail "eggdb create failed"
"$eggdb" sql "$work/kerberos.db" -c "CREATE ROLE alice LOGIN" > "$work/role.out" 2>&1 || fail "CREATE ROLE: $(cat "$work/role.out")"
printf 'alice gss keytab=%s krb_realm=%s\n' "$work/eggdb.keytab" "$realm" > "$work/auth.rules"
"$eggdb" serve "$work/kerberos.db" --listen 127.0.0.1:0 --password-file "$work/password.txt" --database kerberos \
  --auth-rules "$work/auth.rules" > "$work/serve.out" 2> "$work/serve.err" &
server_pid=$!
for _ in $(seq 1 100); do
  [ -s "$work/serve.out" ] && break
  sleep 0.1
done
port=$(sed -n 's/.*"port":\([0-9]*\).*/\1/p' "$work/serve.out" | head -n 1)
[ -n "$port" ] || fail "eggdb serve printed no port: $(cat "$work/serve.out")"

connection="host=localhost port=$port dbname=kerberos user=alice gssencmode=disable krbsrvname=postgres"
if "$psql" "$connection" -Atc "SELECT current_user" > "$work/without-ticket.out" 2>&1; then
  fail "a login without a ticket was accepted: $(cat "$work/without-ticket.out")"
fi
echo alice-kerberos-password | "$kinit" alice > "$work/kinit.out" 2>&1 || fail "kinit alice: $(cat "$work/kinit.out")"
answer=$("$psql" "$connection" -Atc "SELECT current_user" 2> "$work/psql.err") || fail "psql over GSSAPI: $(cat "$work/psql.err")"
[ "$answer" = "alice" ] || fail "current_user is '$answer', not alice"
echo "kerberos check passed: psql logged in as alice with a Kerberos ticket"
