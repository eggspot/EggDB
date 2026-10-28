#![allow(dead_code)]

use eggdb_mcp::audit::AuditLog;
use eggdb_mcp::{DatabaseOpener, OpenRefusal, Server, ServerConfig};
use eggdb_sql::json::{Json, Node, parse_document};
use eggdb_sql::{
    Dialect, EntryVisitor, KeyValueRead, KeyValueStore, KeyValueWrite, ScanDirection, Session,
    SqlResult,
};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

pub type Map = BTreeMap<Vec<u8>, Vec<u8>>;

#[derive(Clone, Default)]
pub struct MapStore {
    committed: Rc<RefCell<Rc<Map>>>,
}

pub struct MapRead(Rc<Map>);

pub struct MapWrite<'s> {
    store: &'s MapStore,
    map: Map,
}

fn scan(
    map: &Map,
    start: &[u8],
    end: &[u8],
    direction: ScanDirection,
    visit: &mut EntryVisitor<'_>,
) -> SqlResult<()> {
    if start >= end {
        return Ok(());
    }
    let range = map.range(start.to_vec()..end.to_vec());
    let entries: Box<dyn Iterator<Item = (&Vec<u8>, &Vec<u8>)>> = match direction {
        ScanDirection::Forward => Box::new(range),
        ScanDirection::Backward => Box::new(range.rev()),
    };
    for (key, value) in entries {
        if !visit(key, value)? {
            break;
        }
    }
    Ok(())
}

impl KeyValueRead for MapRead {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        Ok(self.0.get(key).cloned())
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        scan(&self.0, start, end, direction, visit)
    }
}

impl KeyValueRead for MapWrite<'_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        Ok(self.map.get(key).cloned())
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        scan(&self.map, start, end, direction, visit)
    }
}

impl KeyValueWrite for MapWrite<'_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()> {
        self.map.insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn delete(&mut self, key: &[u8]) -> SqlResult<bool> {
        Ok(self.map.remove(key).is_some())
    }

    fn commit(self) -> SqlResult<()> {
        *self.store.committed.borrow_mut() = Rc::new(self.map);
        Ok(())
    }

    fn rollback(self) -> SqlResult<()> {
        Ok(())
    }
}

impl KeyValueStore for MapStore {
    type Read<'s> = MapRead;
    type Write<'s> = MapWrite<'s>;

    fn begin_read(&self) -> SqlResult<MapRead> {
        Ok(MapRead(Rc::clone(&self.committed.borrow())))
    }

    fn begin_write(&self) -> SqlResult<MapWrite<'_>> {
        Ok(MapWrite {
            store: self,
            map: self.committed.borrow().as_ref().clone(),
        })
    }
}

impl MapStore {
    pub fn snapshot(&self) -> Map {
        self.committed.borrow().as_ref().clone()
    }

    pub fn sql(&self, dialect: Dialect, text: &str) {
        let mut session = Session::with_dialect(self, dialect);
        session
            .execute(text, &mut |_| {})
            .unwrap_or_else(|error| panic!("{text}: {error}"));
        session.close().unwrap();
    }

    pub fn put(&self, key: &[u8], value: &[u8]) {
        let mut writer = self.begin_write().unwrap();
        writer.put(key, value).unwrap();
        writer.commit().unwrap();
    }
}

pub struct MapOpener {
    pub store: MapStore,
    pub busy: Cell<bool>,
    pub opens: Cell<u32>,
    pub write_opens: Cell<u32>,
    pub not_writable: Cell<bool>,
    pub close_fails: Cell<bool>,
    pub close_delay: Cell<Duration>,
    pub after_close: RefCell<Option<Box<dyn Fn()>>>,
}

impl MapOpener {
    pub fn new(store: MapStore) -> Self {
        Self {
            store,
            busy: Cell::new(false),
            opens: Cell::new(0),
            write_opens: Cell::new(0),
            not_writable: Cell::new(false),
            close_fails: Cell::new(false),
            close_delay: Cell::new(Duration::ZERO),
            after_close: RefCell::new(None),
        }
    }
}

impl DatabaseOpener for MapOpener {
    type Store = MapStore;

    fn open_read_only(&self) -> Result<MapStore, OpenRefusal> {
        if self.busy.get() {
            return Err(OpenRefusal::Busy);
        }
        self.opens.set(self.opens.get() + 1);
        Ok(self.store.clone())
    }

    fn open_read_write(&self) -> Result<MapStore, OpenRefusal> {
        if self.busy.get() {
            return Err(OpenRefusal::Busy);
        }
        if self.not_writable.get() {
            return Err(OpenRefusal::NotWritable("permission denied".to_owned()));
        }
        self.write_opens.set(self.write_opens.get() + 1);
        Ok(self.store.clone())
    }

    fn close(&self, _store: MapStore) -> Result<(), OpenRefusal> {
        std::thread::sleep(self.close_delay.get());
        if let Some(after) = self.after_close.borrow().as_ref() {
            after();
        }
        if self.close_fails.get() {
            return Err(OpenRefusal::Failed("the disk is full".to_owned()));
        }
        Ok(())
    }
}

static NEXT: AtomicU32 = AtomicU32::new(0);

pub struct TempAudit {
    pub path: PathBuf,
}

impl TempAudit {
    pub fn new(name: &str) -> Self {
        let number = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "eggdb-mcp-test-{name}-{}-{number}.mcp-audit.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        Self { path }
    }

    pub fn lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.path)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

impl Drop for TempAudit {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn config(audit: &TempAudit) -> ServerConfig {
    ServerConfig {
        database_name: "app.db".to_owned(),
        server_version: "0.0.0-test".to_owned(),
        time_limit: Duration::from_secs(5),
        audit: AuditLog::new(audit.path.clone()),
        allow_write: false,
        exit_process: None,
    }
}

pub fn server<'o>(opener: &'o MapOpener, audit: &TempAudit) -> Server<'o, MapOpener> {
    Server::new(opener, config(audit)).unwrap()
}

pub fn initialized<'o>(opener: &'o MapOpener, audit: &TempAudit) -> Server<'o, MapOpener> {
    let mut server = server(opener, audit);
    let answer = server
        .handle(br#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test-client","version":"1.0"}}}"#)
        .unwrap();
    assert!(
        answer.contains("\"protocolVersion\":\"2025-11-25\""),
        "{answer}"
    );
    server
}

pub fn parsed(answer: &str) -> Node {
    assert!(answer.is_ascii(), "{answer}");
    assert!(!answer.contains('\n'), "{answer}");
    parse_document(answer).unwrap_or_else(|error| panic!("{answer}: {error:?}"))
}

pub fn get<'n>(node: &'n Node, path: &[&str]) -> &'n Node {
    let mut node = node;
    for step in path {
        node = match &node.value {
            Json::Object(members) => members
                .iter()
                .find(|(name, _)| name == step)
                .map(|(_, value)| value)
                .unwrap_or_else(|| panic!("no {step} in {node:?}")),
            Json::Array(items) => &items[step.parse::<usize>().unwrap()],
            _ => panic!("cannot step into {step} of {node:?}"),
        };
    }
    node
}

pub fn text_at(node: &Node, path: &[&str]) -> String {
    match &get(node, path).value {
        Json::String(text) => text.clone(),
        Json::Number(text) => text.clone(),
        Json::Boolean(value) => value.to_string(),
        Json::Null => "null".to_owned(),
        other => panic!("not a scalar: {other:?}"),
    }
}

pub fn call(server: &mut Server<'_, MapOpener>, id: u32, tool: &str, arguments: &str) -> Node {
    let request = format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{tool}","arguments":{arguments}}}}}"#
    );
    parsed(&server.handle(request.as_bytes()).unwrap())
}

pub fn structured(answer: &Node) -> &Node {
    get(answer, &["result", "structuredContent"])
}

pub fn is_error(answer: &Node) -> bool {
    text_at(answer, &["result", "isError"]) == "true"
}

pub fn error_code(answer: &Node) -> String {
    assert!(is_error(answer), "{answer:?}");
    text_at(structured(answer), &["error", "code"])
}

pub fn sample_store() -> MapStore {
    let store = MapStore::default();
    store.sql(
        Dialect::PostgreSql,
        "CREATE TABLE users (id integer PRIMARY KEY, name text NOT NULL, email text); \
         CREATE INDEX users_name ON users (name); \
         COMMENT ON TABLE users IS 'people'; \
         INSERT INTO users VALUES (1, 'Ada', 'ada@example.com'), (2, 'Grace', NULL), (3, 'Margaret', 'mh@example.com'); \
         CREATE VIEW named AS SELECT id, name FROM users",
    );
    store.put(b"user:1", b"Ada");
    store.put(b"user:2", &[0, 159, 146, 150]);
    store
}

pub fn canonical(node: &Node) -> String {
    match &node.value {
        Json::Null => "null".to_owned(),
        Json::Boolean(value) => value.to_string(),
        Json::Number(text) => text.clone(),
        Json::String(text) => format!("{text:?}"),
        Json::Array(items) => format!(
            "[{}]",
            items.iter().map(canonical).collect::<Vec<_>>().join(",")
        ),
        Json::Object(members) => format!(
            "{{{}}}",
            members
                .iter()
                .map(|(name, value)| format!("{name:?}:{}", canonical(value)))
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

pub fn writing_config(audit: &TempAudit) -> ServerConfig {
    ServerConfig {
        allow_write: true,
        ..config(audit)
    }
}

pub fn writing_server<'o>(
    opener: &'o MapOpener,
    audit: &TempAudit,
    capabilities: &str,
) -> Server<'o, MapOpener> {
    let mut server = Server::new(opener, writing_config(audit)).unwrap();
    let request = format!(
        r#"{{"jsonrpc":"2.0","id":0,"method":"initialize","params":{{"protocolVersion":"2025-11-25","capabilities":{capabilities},"clientInfo":{{"name":"test-client","version":"1.0"}}}}}}"#
    );
    let answer = server.handle(request.as_bytes()).unwrap();
    assert!(answer.contains("write_sql"), "{answer}");
    server
}

pub fn served(server: &mut Server<'_, MapOpener>, input: &str) -> Vec<Node> {
    let mut input = input.as_bytes();
    let mut output = Vec::new();
    server.serve(&mut input, &mut output).unwrap();
    String::from_utf8(output)
        .unwrap()
        .lines()
        .map(parsed)
        .collect()
}

pub fn count(store: &MapStore, dialect: Dialect, query: &str) -> i64 {
    let mut session = Session::with_dialect(store, dialect);
    let mut found = None;
    session
        .execute(query, &mut |result| {
            if let Some(eggdb_sql::Value::BigInt(value)) =
                result.rows.first().and_then(|row| row.first())
            {
                found = Some(*value);
            }
        })
        .unwrap_or_else(|error| panic!("{query}: {error}"));
    session.close().unwrap();
    found.unwrap_or_else(|| panic!("{query} gave no count"))
}

pub fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Node> {
    match &node.value {
        Json::Object(members) => members
            .iter()
            .find(|(member, _)| member == name)
            .map(|(_, value)| value),
        _ => None,
    }
}
