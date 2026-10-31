#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/sqlalchemy-suite/check.sh <eggdb binary> <work directory>

Runs SQLAlchemy's own dialect compliance suite (test/dialect/test_suite.py from the SQLAlchemy
2.0.36 source release, MIT) with its PostgreSQL dialect and psycopg 3 against eggdb serve, and
fails when fewer tests pass than scripts/sqlalchemy-suite/minimum-passes.txt says: the pass
count may only rise (D45, D163). Raise the number in the same pull request that makes more
tests pass. PYTHON names a Python 3 with pip (default python3)."

[ $# -eq 2 ] || { echo "$usage" >&2; exit 2; }
eggdb=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
work=$2
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$work"
work=$(cd "$work" && pwd)
python=${PYTHON:-python3}
minimum=$(tr -d '[:space:]' < "$here/minimum-passes.txt")

fail() {
  echo "sqlalchemy-suite check FAILED: $*" >&2
  exit 1
}

"$python" -m venv "$work/venv"
"$work/venv/bin/pip" install --quiet "sqlalchemy==2.0.36" "psycopg[binary]==3.2.3" "pytest==8.3.3"
"$work/venv/bin/pip" download --quiet --no-binary :all: --no-deps "sqlalchemy==2.0.36" -d "$work/source"
rm -rf "$work/sqlalchemy-2.0.36"
tar -xzf "$work/source/"[Ss][Qq][Ll][Aa]lchemy-2.0.36.tar.gz -C "$work"
[ -f "$work/sqlalchemy-2.0.36/test/dialect/test_suite.py" ] || fail "the source release has no dialect suite"

rm -f "$work/suite.db"
EGGDB_PASSWORD=suite "$eggdb" serve "$work/suite.db" --create --listen 127.0.0.1:0 --database suite --format json \
  > "$work/serve.out" 2> "$work/serve.err" &
serve_pid=$!
trap 'kill -TERM "$serve_pid" 2> /dev/null || true' EXIT
for _ in $(seq 1 100); do
  [ -s "$work/serve.out" ] && break
  sleep 0.1
done
port=$(sed -E 's/.*"port":([0-9]+).*/\1/' "$work/serve.out" | head -n 1)
[ -n "$port" ] || fail "eggdb serve printed no port"

cd "$work/sqlalchemy-2.0.36"
set +e
"$work/venv/bin/python" -m pytest test/dialect/test_suite.py -p no:cacheprovider -q -rN \
  --dburi "postgresql+psycopg://eggdb:suite@127.0.0.1:$port/suite" > "$work/pytest.out" 2>&1
set -e
summary=$(tail -n 5 "$work/pytest.out" | grep -E '[0-9]+ (passed|failed)' | tail -n 1 || true)
passed=$(printf '%s' "$summary" | grep -oE '[0-9]+ passed' | grep -oE '[0-9]+' || echo 0)
echo "SQLAlchemy dialect compliance suite on EggDB: $summary (at least $minimum must pass)"
[ "$passed" -ge "$minimum" ] || fail "$passed tests passed, fewer than the committed $minimum"
