#!/usr/bin/env bash
set -euo pipefail

usage="usage: scripts/farm-driver.sh <eggdb binary> <driver: psycopg2|psycopg|sqlalchemy|node-postgres> <version> <row file>"
[ $# -eq 4 ] || { echo "$usage" >&2; exit 2; }
eggdb=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
driver=$2
version=$3
row=$4
work=$(mktemp -d)
export EGGDB_PASSWORD=${EGGDB_PASSWORD:-farm}
port=55433
"$eggdb" serve "$work/farm.eggdb" --create --database eggdb --listen "127.0.0.1:$port" > "$work/serve.log" 2>&1 &
server=$!
trap 'kill "$server" 2> /dev/null || true; rm -rf "$work"' EXIT
for _ in $(seq 1 50); do
  (exec 3<> "/dev/tcp/127.0.0.1/$port") 2> /dev/null && break
  sleep 0.2
done
url="postgresql://eggdb:$EGGDB_PASSWORD@127.0.0.1:$port/eggdb?sslmode=disable"

python_check() {
  python3 -m venv "$work/venv"
  "$work/venv/bin/pip" install --quiet "$1==$version"
  if [ "$driver" = sqlalchemy ]; then
    "$work/venv/bin/pip" install --quiet psycopg2-binary
  fi
  "$work/venv/bin/python" - "$url" "$driver" << 'PY'
import sys
url, driver = sys.argv[1], sys.argv[2]
if driver == "sqlalchemy":
    import sqlalchemy
    engine = sqlalchemy.create_engine(url.replace("postgresql://", "postgresql+psycopg2://", 1))
    with engine.begin() as connection:
        connection.execute(sqlalchemy.text("CREATE TABLE farm (id integer PRIMARY KEY, name text)"))
        connection.execute(sqlalchemy.text("INSERT INTO farm VALUES (:id, :name)"), [{"id": 1, "name": "a"}, {"id": 2, "name": "b"}])
    with engine.connect() as connection:
        rows = connection.execute(sqlalchemy.text("SELECT name FROM farm ORDER BY id")).fetchall()
    assert [row[0] for row in rows] == ["a", "b"], rows
else:
    module = __import__("psycopg2" if driver == "psycopg2" else "psycopg")
    connection = module.connect(url)
    cursor = connection.cursor()
    cursor.execute("CREATE TABLE farm (id integer PRIMARY KEY, name text)")
    cursor.executemany("INSERT INTO farm VALUES (%s, %s)", [(1, "a"), (2, "b")])
    connection.commit()
    cursor.execute("SELECT name FROM farm WHERE id = %s", (2,))
    assert cursor.fetchone()[0] == "b"
    connection.close()
PY
}

node_check() {
  mkdir -p "$work/node"
  (cd "$work/node" && npm init -y > /dev/null && npm install --silent "pg@$version" > /dev/null)
  (cd "$work/node" && node - "$url" << 'JS'
const { Client } = require("pg");
(async () => {
  const client = new Client({ connectionString: process.argv[2] });
  await client.connect();
  await client.query("CREATE TABLE farm (id integer PRIMARY KEY, name text)");
  await client.query("INSERT INTO farm VALUES ($1, $2), ($3, $4)", [1, "a", 2, "b"]);
  const result = await client.query("SELECT name FROM farm WHERE id = $1", [2]);
  if (result.rows[0].name !== "b") throw new Error(JSON.stringify(result.rows));
  await client.end();
})().catch((error) => { console.error(error); process.exit(1); });
JS
  )
}

status=passes
case "$driver" in
  psycopg2) python_check psycopg2-binary || status=fails ;;
  psycopg) python_check "psycopg[binary]" || status=fails ;;
  sqlalchemy) python_check sqlalchemy || status=fails ;;
  node-postgres) node_check || status=fails ;;
  *) echo "$usage" >&2; exit 2 ;;
esac
echo "| $driver $version | $status |" > "$row"
cat "$row"
[ "$status" = passes ]
