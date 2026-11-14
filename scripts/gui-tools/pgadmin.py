import http.cookiejar
import json
import sys
import time
import urllib.error
import urllib.parse
import urllib.request


class PgAdmin:
    def __init__(self, base):
        self.base = base.rstrip("/")
        self.opener = urllib.request.build_opener(
            urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar())
        )

    def request(self, method, path, body=None, form=None):
        data = None
        headers = {"Accept": "application/json"}
        if body is not None:
            data = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
        elif form is not None:
            data = urllib.parse.urlencode(form).encode()
            headers["Content-Type"] = "application/x-www-form-urlencoded"
        request = urllib.request.Request(self.base + path, data=data, headers=headers, method=method)
        try:
            with self.opener.open(request, timeout=60) as response:
                text = response.read().decode("utf-8", "replace")
        except urllib.error.HTTPError as error:
            text = error.read().decode("utf-8", "replace")
            raise SystemExit(f"pgAdmin {method} {path} answered {error.code}: {text[:2000]}")
        try:
            return json.loads(text)
        except ValueError:
            return text

    def nodes(self, path):
        answer = self.request("GET", path)
        if not isinstance(answer, dict):
            raise SystemExit(f"pgAdmin {path} answered no JSON: {str(answer)[:2000]}")
        if answer.get("success") == 0:
            raise SystemExit(f"pgAdmin {path} failed: {answer.get('errormsg')}")
        return answer.get("data") or []


def labelled(nodes, label, what):
    for node in nodes:
        if node.get("label") == label:
            return node["_id"]
    seen = ", ".join(str(node.get("label")) for node in nodes)
    raise SystemExit(f"pgAdmin listed no {what} {label!r}; it listed: {seen}")


def labels(nodes):
    return sorted(str(node.get("label")) for node in nodes)


def cells(row):
    return list(row.values()) if isinstance(row, dict) else list(row)


def wait_until_up(admin):
    deadline = time.time() + 180
    while True:
        try:
            with urllib.request.urlopen(admin.base + "/misc/ping", timeout=5) as response:
                if response.status == 200:
                    return
        except (urllib.error.URLError, OSError):
            pass
        if time.time() > deadline:
            raise SystemExit("pgAdmin did not come up within 180 seconds")
        time.sleep(2)


def query_tool(admin, server, database, sql):
    transaction = int(time.time() * 1000) % 9_000_000 + 1_000_000
    admin.request("POST", f"/sqleditor/initialize/sqleditor/{transaction}/1/{server}/{database}", body={})
    started = admin.request("POST", f"/sqleditor/query_tool/start/{transaction}", body={"sql": sql})
    if isinstance(started, dict) and started.get("success") == 0:
        raise SystemExit(f"pgAdmin's query tool refused {sql!r}: {started.get('errormsg')}")
    deadline = time.time() + 60
    while time.time() < deadline:
        polled = admin.request("GET", f"/sqleditor/poll/{transaction}")
        data = polled.get("data", {}) if isinstance(polled, dict) else {}
        status = data.get("status")
        if status == "Success":
            return data.get("result") or []
        if status not in ("Busy", "NotConnected", None):
            raise SystemExit(f"pgAdmin's query tool failed on {sql!r}: {data.get('result')}")
        time.sleep(0.5)
    raise SystemExit(f"pgAdmin's query tool did not finish {sql!r}")


def main():
    if len(sys.argv) != 6:
        raise SystemExit("usage: pgadmin.py <pgAdmin URL> <email> <password> <eggdb port> <eggdb password>")
    base, email, password, port, eggdb_password = sys.argv[1:]
    admin = PgAdmin(base)
    wait_until_up(admin)
    admin.request("POST", "/authenticate/login", form={"email": email, "password": password, "language": "en", "internal_button": "Login"})
    group = admin.nodes("/browser/server_group/nodes/")[0]["_id"]
    created = admin.request(
        "POST",
        f"/browser/server/obj/{group}/",
        body={
            "name": "EggDB",
            "host": "127.0.0.1",
            "port": int(port),
            "db": "library",
            "username": "eggdb",
            "password": eggdb_password,
            "save_password": True,
            "connect_now": True,
            "sslmode": "disable",
        },
    )
    server = created["node"]["_id"]
    connected = admin.request("POST", f"/browser/server/connect/{group}/{server}", body={"password": eggdb_password})
    if isinstance(connected, dict) and connected.get("success") == 0:
        raise SystemExit(f"pgAdmin could not connect: {connected.get('errormsg')}")
    root = f"{group}/{server}"
    database = labelled(admin.nodes(f"/browser/database/nodes/{root}/"), "library", "database")
    root = f"{root}/{database}"
    schemas = admin.nodes(f"/browser/schema/nodes/{root}/")
    public = labelled(schemas, "public", "schema")
    lending = labelled(schemas, "lending", "schema")
    print("schemas:", ", ".join(name for name in labels(schemas) if name in ("lending", "public")))
    tables = admin.nodes(f"/browser/table/nodes/{root}/{public}/")
    print("public tables:", ", ".join(labels(tables)))
    books = labelled(tables, "books", "table")
    columns = admin.nodes(f"/browser/column/nodes/{root}/{public}/{books}/")
    print("books columns:", ", ".join(str(column.get("label")) for column in columns))
    for kind in ("index", "constraints", "foreign_key", "check_constraint", "trigger", "rule"):
        admin.request("GET", f"/browser/{kind}/nodes/{root}/{public}/{books}/")
    properties = admin.request("GET", f"/browser/table/obj/{root}/{public}/{books}")
    if not isinstance(properties, dict) or properties.get("name") != "books":
        raise SystemExit(f"pgAdmin's table properties did not name books: {str(properties)[:2000]}")
    print("books comment:", properties.get("description"))
    ddl = admin.request("GET", f"/browser/table/sql/{root}/{public}/{books}")
    ddl = ddl if isinstance(ddl, str) else json.dumps(ddl)
    for expected in ("CREATE TABLE IF NOT EXISTS public.books", "title character varying(200)", "books_title"):
        if expected not in ddl:
            raise SystemExit(f"pgAdmin's SQL tab for books lacks {expected!r}:\n{ddl}")
    print("books SQL tab: CREATE TABLE, columns and index present")
    print("lending tables:", ", ".join(labels(admin.nodes(f"/browser/table/nodes/{root}/{lending}/"))))
    print("public views:", ", ".join(labels(admin.nodes(f"/browser/view/nodes/{root}/{public}/"))))
    print("lending sequences:", ", ".join(labels(admin.nodes(f"/browser/sequence/nodes/{root}/{lending}/"))))
    functions = labels(admin.nodes(f"/browser/function/nodes/{root}/{public}/"))
    print("public functions:", ", ".join(name for name in functions if name.startswith("book_count")))
    admin.request("GET", f"/dashboard/dashboard_stats/{server}?chart_names=session_stats,tps_stats,ti_stats,to_stats,bio_stats")
    admin.request("GET", f"/dashboard/dashboard_stats/{server}/{database}?chart_names=session_stats,tps_stats,ti_stats,to_stats,bio_stats")
    admin.request("GET", f"/dashboard/activity/{server}")
    rows = query_tool(admin, server, database, "SELECT title, author FROM book_titles ORDER BY title")
    print("query tool:", "; ".join(" | ".join(str(value) for value in cells(row)) for row in rows))


main()
