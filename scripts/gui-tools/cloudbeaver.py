import hashlib
import http.cookiejar
import json
import sys
import time
import urllib.error
import urllib.request


class CloudBeaver:
    def __init__(self, base):
        self.endpoint = base.rstrip("/") + "/api/gql"
        self.opener = urllib.request.build_opener(
            urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar())
        )

    def call(self, query, **variables):
        body = json.dumps({"query": query, "variables": variables}).encode()
        request = urllib.request.Request(
            self.endpoint,
            data=body,
            headers={"Content-Type": "application/json", "Accept": "application/json"},
            method="POST",
        )
        try:
            with self.opener.open(request, timeout=60) as response:
                answer = json.loads(response.read().decode("utf-8", "replace"))
        except urllib.error.HTTPError as error:
            raise SystemExit(f"CloudBeaver answered {error.code}: {error.read()[:2000]!r}")
        if answer.get("errors"):
            raise SystemExit(f"CloudBeaver refused {query.split('(')[0].strip()}: {answer['errors']}")
        return answer["data"]


def wait_until_up(beaver):
    deadline = time.time() + 240
    while True:
        try:
            beaver.call("query { serverConfig { name version } }")
            return
        except (SystemExit, urllib.error.URLError, OSError, ValueError):
            if time.time() > deadline:
                raise SystemExit("CloudBeaver did not come up within 240 seconds")
            time.sleep(3)


def children(beaver, path):
    return beaver.call(
        "query children($path: ID!) { navNodeChildren(parentPath: $path) { id name nodeType hasChildren } }",
        path=path,
    )["navNodeChildren"]


def find(beaver, path, wanted, depth):
    for node in children(beaver, path):
        if node["name"] == wanted:
            return node
        if depth > 0 and node["hasChildren"]:
            found = find(beaver, node["id"], wanted, depth - 1)
            if found is not None:
                return found
    return None


def names(nodes):
    return sorted(node["name"] for node in nodes)


def run_sql(beaver, connection, context, sql):
    task = beaver.call(
        "mutation run($connection: ID!, $context: ID!, $sql: String!) {"
        " asyncSqlExecuteQuery(connectionId: $connection, contextId: $context, sql: $sql) { id } }",
        connection=connection,
        context=context,
        sql=sql,
    )["asyncSqlExecuteQuery"]["id"]
    deadline = time.time() + 60
    while time.time() < deadline:
        info = beaver.call(
            "query task($id: String!) { asyncTaskInfo(id: $id, removeOnFinish: false) { running error { message } } }",
            id=task,
        )["asyncTaskInfo"]
        if info["error"]:
            raise SystemExit(f"CloudBeaver's SQL editor failed on {sql!r}: {info['error']['message']}")
        if not info["running"]:
            break
        time.sleep(0.5)
    results = beaver.call(
        "query results($id: ID!) { asyncSqlExecuteResults(taskId: $id) { results { resultSet { columns { name } rows } } } }",
        id=task,
    )["asyncSqlExecuteResults"]["results"]
    return results[0]["resultSet"]["rows"] if results else []


def main():
    if len(sys.argv) != 6:
        raise SystemExit("usage: cloudbeaver.py <CloudBeaver URL> <admin> <admin password> <eggdb port> <eggdb password>")
    base, admin, admin_password, port, eggdb_password = sys.argv[1:]
    beaver = CloudBeaver(base)
    wait_until_up(beaver)
    beaver.call("query { openSession { valid } }")
    beaver.call(
        "mutation login($user: Object!) { authLogin(provider: \"local\", credentials: $user) { authStatus } }",
        user={"user": admin, "password": hashlib.md5(admin_password.encode()).hexdigest().upper()},
    )
    created = beaver.call(
        "mutation create($config: ConnectionConfig!) { createConnection(config: $config) { id } }",
        config={
            "driverId": "postgresql:postgres-jdbc",
            "name": "EggDB",
            "host": "127.0.0.1",
            "port": port,
            "databaseName": "library",
            "authModelId": "native",
            "credentials": {"userName": "eggdb", "userPassword": eggdb_password},
            "saveCredentials": True,
        },
    )["createConnection"]["id"]
    connected = beaver.call(
        "mutation open($id: ID!, $credentials: Object) { initConnection(id: $id, credentials: $credentials) { connected } }",
        id=created,
        credentials={"userName": "eggdb", "userPassword": eggdb_password},
    )["initConnection"]["connected"]
    if not connected:
        raise SystemExit("CloudBeaver did not connect to EggDB")
    root = f"database://{created}"
    tables = find(beaver, root, "books", 5)
    if tables is None:
        raise SystemExit("CloudBeaver's navigator found no table books")
    book_children = children(beaver, tables["id"])
    columns_folder = next((node for node in book_children if node["name"].lower() == "columns"), None)
    if columns_folder is None:
        raise SystemExit(f"CloudBeaver showed no Columns folder under books: {names(book_children)}")
    print("books columns:", ", ".join(node["name"] for node in children(beaver, columns_folder["id"])))
    for wanted in ("loans", "book_titles", "ticket_numbers", "book_count"):
        if find(beaver, root, wanted, 5) is None:
            raise SystemExit(f"CloudBeaver's navigator found no {wanted}")
        print("found:", wanted)
    context = beaver.call(
        "mutation context($id: ID!) { sqlContextCreate(connectionId: $id) { id } }",
        id=created,
    )["sqlContextCreate"]["id"]
    rows = run_sql(beaver, created, context, "SELECT title, author FROM book_titles ORDER BY title")
    print("SQL editor:", "; ".join(" | ".join(str(value) for value in row) for row in rows))


main()
