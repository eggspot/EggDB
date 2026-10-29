#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/pg-catalog/record.sh <data directory> <label>

Records the data of EggDB's always-on catalog test (crates/eggdb/tests/pg_catalog.rs): fills
DATABASE (default eggdb_catalog) of a running PostgreSQL server with
scripts/pg-catalog/fixture.sql, writes its pg_dump to <data directory>/fixture.sql, runs every
command of scripts/pg-catalog/commands.txt with the psql named by PSQL and -E, and writes each
distinct query psql sent, with its column types and rows on PostgreSQL, to
<data directory>/<label>.txt. Relation OIDs are written as {schema.name}, so the test can put
EggDB's in their place.

PostgreSQL's tools come from PG_BIN (a directory) or PATH; PGHOST, PGPORT, PGUSER and
PGPASSWORD reach the server."

[ $# -eq 2 ] || { echo "$usage" >&2; exit 2; }
data=$1
label=$2
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$data"
database=${DATABASE:-eggdb_catalog}
export PGTZ=UTC PGCLIENTENCODING=UTF8
psql_command=${PSQL:-${PG_BIN:+$PG_BIN/}psql}
read -r -a psql_words <<< "$psql_command"
tool() {
  local name=$1
  shift
  "${PG_BIN:+$PG_BIN/}$name" "$@"
}
scratch=${TMPDIR:-/tmp}/eggdb-pg-catalog-record
mkdir -p "$scratch"

PGDATABASE=postgres tool psql -X -v ON_ERROR_STOP=1 -q -c "DROP DATABASE IF EXISTS $database" \
  -c "CREATE DATABASE $database TEMPLATE template0 ENCODING 'UTF8' LC_COLLATE 'C' LC_CTYPE 'C'"
export PGDATABASE=$database
tool psql -X -v ON_ERROR_STOP=1 -q -f - < "$here/fixture.sql" > /dev/null
tool pg_dump > "$data/fixture.sql"
tool psql -X -At -F ' ' -c "SELECT c.oid, quote_ident(n.nspname) || '.' || quote_ident(c.relname)
  FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.oid >= 16384" > "$scratch/oids"

"${psql_words[@]}" -X -E -v database="$database" -f - < "$here/commands.txt" > "$scratch/log" 2>&1
awk '
  /^(\*{9} QUERY \*{10}|\/\*{8} QUERY \*{9}\/)$/ { inside = 1; buffer = ""; next }
  /^(\*{26}|\/\*{24}\/)$/ { if (inside) printf "%s\036", buffer; inside = 0; next }
  inside { buffer = buffer $0 "\n" }
' "$scratch/log" > "$scratch/queries"

placeholders='
  FNR == NR { name[$1] = $2; next }
  {
    line = $0
    for (oid in name) {
      gsub("\x27" oid "\x27", "\x27{" name[oid] "}\x27", line)
    }
    if (rows) {
      fields = split(line, parts, "|")
      line = ""
      for (at = 1; at <= fields; at++) {
        if (parts[at] in name) parts[at] = "{" name[parts[at]] "}"
        line = line (at > 1 ? "|" : "") parts[at]
      }
    }
    print line
  }'

: > "$data/$label.txt"
: > "$scratch/seen"
count=0
while IFS= read -r -d $'\036' recorded; do
  query=$(printf '%s' "$recorded" | sed -e '$s/;[[:space:]]*$//')
  shown=$(printf '%s\n' "$query" | awk -v rows=0 "$placeholders" "$scratch/oids" -)
  if grep -qxF "$(printf '%s' "$shown" | cksum)" "$scratch/seen"; then
    continue
  fi
  printf '%s' "$shown" | cksum >> "$scratch/seen"
  types=$(printf '%s\n\\gdesc\n' "$query" | "${psql_words[@]}" -X -At -F '|' -f - | cut -d '|' -f 2 | paste -sd '|' -)
  {
    echo "=== query"
    printf '%s\n' "$shown"
    echo "=== types"
    printf '%s\n' "$types"
    echo "=== rows"
    printf '%s;\n' "$query" | "${psql_words[@]}" -X -At -F '|' -v ON_ERROR_STOP=1 -f - |
      awk -v rows=1 "$placeholders" "$scratch/oids" -
  } >> "$data/$label.txt"
  count=$((count + 1))
done < "$scratch/queries"
echo "$count distinct queries of $("${psql_words[@]}" --version) written to $data/$label.txt"
