use super::*;
use eggdb_sql::{EntryVisitor, KeyValueRead, KeyValueWrite, ScanDirection, SqlResult};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

type Map = BTreeMap<Vec<u8>, Vec<u8>>;

#[derive(Default)]
struct MemoryStore {
    committed: Mutex<Arc<Map>>,
}

struct Snapshot(Arc<Map>);

struct Writer<'s> {
    store: &'s MemoryStore,
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

impl KeyValueRead for Snapshot {
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

impl KeyValueRead for Writer<'_> {
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

impl KeyValueWrite for Writer<'_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()> {
        self.map.insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn delete(&mut self, key: &[u8]) -> SqlResult<bool> {
        Ok(self.map.remove(key).is_some())
    }

    fn commit(self) -> SqlResult<()> {
        *self.store.committed.lock().unwrap() = Arc::new(self.map);
        Ok(())
    }

    fn rollback(self) -> SqlResult<()> {
        Ok(())
    }
}

impl KeyValueStore for MemoryStore {
    type Read<'s> = Snapshot;
    type Write<'s> = Writer<'s>;

    fn begin_read(&self) -> SqlResult<Snapshot> {
        Ok(Snapshot(Arc::clone(&self.committed.lock().unwrap())))
    }

    fn begin_write(&self) -> SqlResult<Writer<'_>> {
        let map = self.committed.lock().unwrap().as_ref().clone();
        Ok(Writer { store: self, map })
    }
}


fn request(method: &str, query: &str, body: &str) -> Request {
    Request {
        method: method.to_owned(),
        path: "/".to_owned(),
        query: Some(query.to_owned()),
        headers: vec![
            ("x-clickhouse-user".to_owned(), "eggdb".to_owned()),
            ("x-clickhouse-key".to_owned(), "secret".to_owned()),
        ],
        body: body.as_bytes().to_vec(),
    }
}

#[test]
fn queries_answer_in_clickhouse_formats() {
    let store = MemoryStore::default();
    let endpoint = Endpoint::new(&store, "eggdb", eggdb_crypto::sha256(b"secret"), "eggdb");
    let created = endpoint.answer(&request("POST", "", "CREATE TABLE t (id bigint, name text)"));
    assert_eq!(created.status, 200, "{}", String::from_utf8_lossy(&created.body));
    let inserted = endpoint.answer(&request(
        "POST",
        "query=INSERT%20INTO%20t%20FORMAT%20JSONEachRow",
        "{\"id\":1,\"name\":\"a\\tb\"}\n{\"id\":2,\"name\":null}\n",
    ));
    assert_eq!(inserted.status, 200, "{}", String::from_utf8_lossy(&inserted.body));
    let tsv = endpoint.answer(&request("GET", "query=SELECT%20id,%20name%20FROM%20t%20ORDER%20BY%20id", ""));
    assert_eq!(String::from_utf8_lossy(&tsv.body), "1\ta\\tb\n2\t\\N\n");
    let json = endpoint.answer(&request("POST", "", "SELECT id FROM t ORDER BY id FORMAT JSONEachRow"));
    assert_eq!(String::from_utf8_lossy(&json.body), "{\"id\":\"1\"}\n{\"id\":\"2\"}\n");
    let refused = endpoint.answer(&request("GET", "query=DROP%20TABLE%20t", ""));
    assert_eq!(refused.status, 400);
    assert!(String::from_utf8_lossy(&refused.body).starts_with("Code: 164."));
}

#[test]
fn format_clause_and_csv_input_split() {
    let split = split_format("INSERT INTO t (a, b) FORMAT CSV\n1,\"x, y\"\n2,\\N\n");
    assert_eq!(split.format, Some("CSV"));
    let input = parse_input(Format::named("CSV").unwrap(), split.data.trim_start()).unwrap();
    assert_eq!(input.rows.len(), 2);
    assert_eq!(input.rows[1].1[1], Field::Null);
    let statements = insert_statements(split.statement, input).unwrap();
    assert_eq!(statements, vec!["INSERT INTO t (a, b) VALUES ('1', 'x, y'), ('2', NULL)".to_owned()]);
    assert_eq!(split_format("SELECT 'FORMAT JSON' AS f").format, None);
}
