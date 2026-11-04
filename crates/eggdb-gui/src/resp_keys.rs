use crate::catalog::{Catalog, KeyCount, KeySpace};
use crate::connection::{CancelHandle, ResultTable, StatementFailure};
use crate::grid::ValueKind;
use crate::key_scan::{
    KEY_SCAN_LIMIT, KeyPage, KeySpaceScan, PageEnd, PageStart, SHOWN_VALUE_BYTES, spelled,
};
use crate::resp_client::{RespClient, RespError, RespReply, prefix_pattern};
use crate::status_line::thousands;

/// How many databases a RESP server's tree lists: `resp0` … `resp15`.
pub const RESP_DATABASES: u32 = 16;
/// A RESP keys grid's columns: `type` sits between `key` and `value`.
pub const RESP_KEY_COLUMNS: [&str; 4] = ["key", "type", "value", "bytes"];
/// The most `SCAN` round trips one page makes; a server whose cursor keeps yielding nothing
/// stops there, and `Next 100` carries on from its cursor.
pub const MAX_SCAN_ROUND_TRIPS: usize = 1_000;
/// What a `value` or `bytes` cell shows when it has nothing to say.
pub const NO_VALUE: &str = "–";
/// What a `value` cell shows for a key that expired between the page's two reads.
pub const GONE: &str = "(gone)";

/// The key space name a RESP database has in the tree and the keys tab: `resp3`.
pub fn database_name(index: u32) -> String {
    format!("resp{index}")
}

/// The database a key space name means: `resp3` is 3.
pub fn database_index(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("resp")?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The failure a server's error line makes, with our `Try:` line under it
/// (`docs/ux/workspace.md` 10.10's table).
pub fn reply_failure(line: &str) -> StatementFailure {
    let fix = if line.starts_with("NOAUTH") || line.starts_with("WRONGPASS") {
        Some("connect again with the password (Ctrl/Cmd+O)")
    } else if line.starts_with("ERR unknown command") && line.to_ascii_uppercase().contains("SCAN")
    {
        Some("this server has no SCAN; EggDB's key list needs it")
    } else if line.starts_with("LOADING") {
        Some("wait for the server to finish loading and press F5")
    } else if line.starts_with("ERR DB index is out of range")
        || line.starts_with("ERR invalid DB index")
    {
        Some("press F5 in the tree; this database is gone")
    } else {
        None
    };
    failure(line.to_owned(), fix)
}

/// The failure when the connection to `place` dropped.
pub fn lost_failure(place: &str) -> StatementFailure {
    failure(
        format!("connection to {place} lost"),
        Some("press F5 to reconnect and scan again"),
    )
}

fn failure(message: String, fix: Option<&str>) -> StatementFailure {
    let mut report = vec![message.clone()];
    if let Some(fix) = fix {
        report.push(format!("Try: {fix}"));
    }
    StatementFailure {
        code: String::new(),
        message,
        hint: None,
        position: None,
        report,
    }
}

fn exchange_failure(error: &RespError, place: &str, cancel: &CancelHandle) -> StatementFailure {
    if cancel.is_cancelled() {
        return StatementFailure::cancelled();
    }
    match error {
        RespError::Io(_) => lost_failure(place),
        RespError::Protocol(message) => failure(
            format!("connection to {place} lost: {message}"),
            Some("press F5 to reconnect and scan again"),
        ),
    }
}

fn answered(reply: RespReply) -> Result<RespReply, StatementFailure> {
    match reply.error_line() {
        Some(line) => Err(reply_failure(&line)),
        None => Ok(reply),
    }
}

fn command(parts: &[&[u8]]) -> Vec<Vec<u8>> {
    parts.iter().map(|part| part.to_vec()).collect()
}

/// A RESP server's catalog: one `Keys` group of `resp0` (always) and each of `resp1` …
/// `resp15` whose `DBSIZE` is above 0, read in one pipeline of `SELECT n` and `DBSIZE`.
pub fn read_catalog(client: &mut RespClient, place: &str) -> Result<Catalog, StatementFailure> {
    let indexes: Vec<String> = (0..RESP_DATABASES).map(|index| index.to_string()).collect();
    let mut commands = Vec::with_capacity(indexes.len() * 2 + 1);
    for index in &indexes {
        commands.push(command(&[b"SELECT", index.as_bytes()]));
        commands.push(command(&[b"DBSIZE"]));
    }
    commands.push(command(&[b"SELECT", b"0"]));
    let replies = client
        .pipeline(&commands)
        .map_err(|error| exchange_failure(&error, place, &CancelHandle::default()))?;
    let mut key_spaces = Vec::new();
    let mut replies = replies.into_iter();
    for index in 0..RESP_DATABASES {
        let (Some(selected), Some(size)) = (replies.next(), replies.next()) else {
            break;
        };
        if let Some(line) = selected.error_line() {
            if index == 0 {
                return Err(reply_failure(&line));
            }
            continue;
        }
        let size = answered(size)?;
        let keys = u64::try_from(size.integer().unwrap_or(0)).unwrap_or(0);
        if index == 0 || keys > 0 {
            key_spaces.push(KeySpace {
                collection: Some(database_name(index)),
                keys: KeyCount::Exactly(keys),
            });
        }
    }
    Ok(Catalog {
        key_spaces,
        over_resp: true,
        ..Catalog::default()
    })
}

fn scan_reply(reply: RespReply) -> Result<(u64, Vec<Vec<u8>>), StatementFailure> {
    let malformed = || {
        failure(
            "the server's SCAN reply is not a cursor and keys".to_owned(),
            None,
        )
    };
    let RespReply::Array(parts) = answered(reply)? else {
        return Err(malformed());
    };
    let mut parts = parts.into_iter();
    let (Some(cursor), Some(RespReply::Array(keys))) = (parts.next(), parts.next()) else {
        return Err(malformed());
    };
    let cursor = cursor
        .bytes()
        .and_then(|text| std::str::from_utf8(text).ok())
        .and_then(|text| text.parse::<u64>().ok())
        .ok_or_else(malformed)?;
    let keys = keys
        .into_iter()
        .filter_map(|key| key.bytes().map(<[u8]>::to_vec))
        .collect();
    Ok((cursor, keys))
}

/// Reads a keys tab's page from a RESP server (`docs/ux/workspace.md` 10.10): `SELECT n` and
/// `DBSIZE`, then `SCAN <cursor> [MATCH <prefix>*] COUNT <needed>` until the page holds one
/// key more than [`KEY_SCAN_LIMIT`] or the cursor is `0`, the keys an earlier reply carried
/// first; keys past the page start the next one. Then `TYPE` for every key, and per key
/// `GETRANGE 0 63` and `STRLEN` for a string or the aggregate's count, in two pipelines.
/// `cancel` stops it between round trips.
pub fn scan_page(
    client: &mut RespClient,
    scan: &KeySpaceScan,
    place: &str,
    cancel: &CancelHandle,
) -> Result<KeyPage, StatementFailure> {
    let lost = |error: RespError| exchange_failure(&error, place, cancel);
    let index = scan
        .collection
        .as_deref()
        .and_then(database_index)
        .ok_or_else(|| failure("this key space is not a RESP database".to_owned(), None))?;
    let opened = client
        .pipeline(&[
            command(&[b"SELECT", index.to_string().as_bytes()]),
            command(&[b"DBSIZE"]),
        ])
        .map_err(lost)?;
    let mut opened = opened.into_iter();
    if let Some(selected) = opened.next() {
        answered(selected)?;
    }
    let total = opened
        .next()
        .map(answered)
        .transpose()?
        .and_then(|size| size.integer())
        .and_then(|size| u64::try_from(size).ok())
        .map(KeyCount::Exactly);
    let (mut keys, mut cursor) = match &scan.start {
        PageStart::Cursor { cursor: 0, carried } => (carried.clone(), None),
        PageStart::Cursor { cursor, carried } => (carried.clone(), Some(*cursor)),
        PageStart::First | PageStart::After(_) => (Vec::new(), Some(0)),
    };
    let pattern = (!scan.prefix.is_empty()).then(|| prefix_pattern(&scan.prefix));
    let mut listed: std::collections::HashSet<Vec<u8>> = keys.iter().cloned().collect();
    let mut trips = 0;
    while keys.len() <= KEY_SCAN_LIMIT && trips < MAX_SCAN_ROUND_TRIPS {
        let Some(sent) = cursor else {
            break;
        };
        if cancel.is_cancelled() {
            return Err(StatementFailure::cancelled());
        }
        trips += 1;
        let needed = (KEY_SCAN_LIMIT + 1).saturating_sub(keys.len()).to_string();
        let sent_text = sent.to_string();
        let mut parts: Vec<&[u8]> = vec![b"SCAN".as_slice(), sent_text.as_bytes()];
        if let Some(pattern) = &pattern {
            parts.push(b"MATCH".as_slice());
            parts.push(pattern);
        }
        parts.push(b"COUNT".as_slice());
        parts.push(needed.as_bytes());
        let (next, found) = scan_reply(client.command(&parts).map_err(lost)?)?;
        for key in found {
            if key.starts_with(&scan.prefix) && listed.insert(key.clone()) {
                keys.push(key);
            }
        }
        cursor = (next != 0).then_some(next);
    }
    let carried = keys.split_off(keys.len().min(KEY_SCAN_LIMIT));
    let next = match (carried.is_empty(), cursor) {
        (true, None) => None,
        (_, cursor) => Some(PageStart::Cursor {
            cursor: cursor.unwrap_or(0),
            carried,
        }),
    };
    if cancel.is_cancelled() {
        return Err(StatementFailure::cancelled());
    }
    let rows = value_rows(client, &keys).map_err(|error| match error {
        Exchange::Lost(error) => lost(error),
        Exchange::Failed(failure) => failure,
    })?;
    let shown = rows.len();
    let more = next.is_some();
    let table = ResultTable {
        columns: RESP_KEY_COLUMNS.map(str::to_owned).to_vec(),
        kinds: vec![
            ValueKind::SpelledBytes,
            ValueKind::Text,
            ValueKind::SpelledBytes,
            ValueKind::Number,
        ],
        tag: format!("SCAN {shown}"),
        row_count: KeySpaceScan {
            more,
            total: total.unwrap_or(scan.total),
            ..scan.clone()
        }
        .row_count(shown),
        rows,
    };
    Ok(KeyPage {
        table,
        end: PageEnd { next, total },
    })
}

enum Exchange {
    Lost(RespError),
    Failed(StatementFailure),
}

fn aggregate_text(kind: &str, count: i64) -> Option<String> {
    let (one, many) = match kind {
        "hash" => ("field", "fields"),
        "list" => ("item", "items"),
        "set" | "zset" => ("member", "members"),
        "stream" => ("entry", "entries"),
        _ => return None,
    };
    let count = usize::try_from(count).ok()?;
    Some(if count == 1 {
        format!("1 {one}")
    } else {
        format!("{} {many}", thousands(count))
    })
}

fn size_command(kind: &str) -> Option<&'static [u8]> {
    Some(match kind {
        "hash" => b"HLEN".as_slice(),
        "list" => b"LLEN".as_slice(),
        "set" => b"SCARD".as_slice(),
        "zset" => b"ZCARD".as_slice(),
        "stream" => b"XLEN".as_slice(),
        _ => return None,
    })
}

fn value_rows(
    client: &mut RespClient,
    keys: &[Vec<u8>],
) -> Result<Vec<Vec<Option<String>>>, Exchange> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let typed: Vec<Vec<Vec<u8>>> = keys
        .iter()
        .map(|key| vec![b"TYPE".to_vec(), key.clone()])
        .collect();
    let kinds: Vec<String> = client
        .pipeline(&typed)
        .map_err(Exchange::Lost)?
        .into_iter()
        .map(|reply| {
            answered(reply).map(|reply| {
                String::from_utf8_lossy(reply.bytes().unwrap_or_default()).into_owned()
            })
        })
        .collect::<Result<_, _>>()
        .map_err(Exchange::Failed)?;
    let shown_end = (SHOWN_VALUE_BYTES - 1).to_string();
    let mut sized = Vec::new();
    for (key, kind) in keys.iter().zip(&kinds) {
        if kind == "string" {
            sized.push(vec![
                b"GETRANGE".to_vec(),
                key.clone(),
                b"0".to_vec(),
                shown_end.as_bytes().to_vec(),
            ]);
            sized.push(vec![b"STRLEN".to_vec(), key.clone()]);
        } else if let Some(name) = size_command(kind) {
            sized.push(vec![name.to_vec(), key.clone()]);
        }
    }
    let sizes = if sized.is_empty() {
        Vec::new()
    } else {
        client.pipeline(&sized).map_err(Exchange::Lost)?
    };
    let mut replies = sizes.into_iter();
    let mut rows = Vec::with_capacity(keys.len());
    for (key, kind) in keys.iter().zip(&kinds) {
        let (type_cell, value, bytes) = if kind == "string" {
            let start = replies.next();
            let length = replies.next().and_then(|reply| reply.integer());
            match (start.as_ref().and_then(RespReply::bytes), length) {
                (Some(start), Some(length)) => {
                    let mut cell = spelled(start);
                    if usize::try_from(length).is_ok_and(|length| length > start.len()) {
                        cell.push('…');
                    }
                    let bytes =
                        usize::try_from(length).map_or_else(|_| NO_VALUE.to_owned(), thousands);
                    (kind.clone(), cell, bytes)
                }
                _ => (kind.clone(), GONE.to_owned(), NO_VALUE.to_owned()),
            }
        } else if size_command(kind).is_some() {
            let count = replies.next().and_then(|reply| reply.integer());
            let value = match count {
                Some(count) if count > 0 => {
                    aggregate_text(kind, count).unwrap_or_else(|| NO_VALUE.to_owned())
                }
                _ => GONE.to_owned(),
            };
            (kind.clone(), value, NO_VALUE.to_owned())
        } else if kind == "none" {
            (NO_VALUE.to_owned(), GONE.to_owned(), NO_VALUE.to_owned())
        } else {
            (kind.clone(), NO_VALUE.to_owned(), NO_VALUE.to_owned())
        };
        rows.push(vec![
            Some(spelled(key)),
            Some(type_cell),
            Some(value),
            Some(bytes),
        ]);
    }
    Ok(rows)
}

#[cfg(test)]
pub(crate) mod fake_server {
    use std::collections::BTreeMap;
    use std::io::{BufReader, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use crate::resp_client::{RespOptions, RespReply, read_reply};

    pub(crate) enum Stored {
        Text(Vec<u8>),
        Counted(&'static str, i64),
    }

    pub(crate) struct FakeServer {
        pub(crate) port: u16,
        pub(crate) commands: Arc<Mutex<Vec<Vec<Vec<u8>>>>>,
        pub(crate) connections: Arc<AtomicUsize>,
    }

    impl FakeServer {
        pub(crate) fn options(&self, password: Option<&str>) -> RespOptions {
            RespOptions {
                host: "127.0.0.1".to_owned(),
                port: self.port,
                user: None,
                password: password.map(str::to_owned),
            }
        }

        pub(crate) fn sent(&self, name: &str) -> Vec<Vec<Vec<u8>>> {
            self.commands
                .lock()
                .unwrap()
                .iter()
                .filter(|command| {
                    command
                        .first()
                        .is_some_and(|first| first.eq_ignore_ascii_case(name.as_bytes()))
                })
                .cloned()
                .collect()
        }
    }

    pub(crate) fn simple(text: &str) -> Vec<u8> {
        format!("+{text}\r\n").into_bytes()
    }

    pub(crate) fn error(text: &str) -> Vec<u8> {
        format!("-{text}\r\n").into_bytes()
    }

    pub(crate) fn integer(value: i64) -> Vec<u8> {
        format!(":{value}\r\n").into_bytes()
    }

    pub(crate) fn bulk(bytes: &[u8]) -> Vec<u8> {
        [format!("${}\r\n", bytes.len()).as_bytes(), bytes, b"\r\n"].concat()
    }

    pub(crate) fn array(items: &[Vec<u8>]) -> Vec<u8> {
        [format!("*{}\r\n", items.len()).into_bytes(), items.concat()].concat()
    }

    pub(crate) fn start(
        answer: impl Fn(&[Vec<u8>]) -> Option<Vec<u8>> + Send + Sync + 'static,
    ) -> FakeServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let answer = Arc::new(answer);
        let (log, count) = (Arc::clone(&commands), Arc::clone(&connections));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };
                count.fetch_add(1, Ordering::SeqCst);
                let (log, answer) = (Arc::clone(&log), Arc::clone(&answer));
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    while let Ok(RespReply::Array(parts)) = read_reply(&mut reader) {
                        let parts: Vec<Vec<u8>> = parts
                            .into_iter()
                            .map(|part| part.bytes().unwrap_or_default().to_vec())
                            .collect();
                        log.lock().unwrap().push(parts.clone());
                        let Some(reply) = answer(parts.as_slice()) else {
                            let _ = stream.shutdown(std::net::Shutdown::Both);
                            return;
                        };
                        if stream.write_all(&reply).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        FakeServer {
            port,
            commands,
            connections,
        }
    }

    fn unescaped_prefix(pattern: &[u8]) -> Vec<u8> {
        let mut prefix = Vec::new();
        let mut bytes = pattern.strip_suffix(b"*").unwrap_or(pattern).iter();
        while let Some(&byte) = bytes.next() {
            if byte == b'\\' {
                prefix.extend(bytes.next());
            } else {
                prefix.push(byte);
            }
        }
        prefix
    }

    pub(crate) fn keyspace(
        databases: BTreeMap<u32, BTreeMap<Vec<u8>, Stored>>,
        hello: Vec<u8>,
        batch: usize,
    ) -> impl Fn(&[Vec<u8>]) -> Option<Vec<u8>> + Send + Sync + 'static {
        let selected = Mutex::new(0_u32);
        move |parts: &[Vec<u8>]| {
            let name = String::from_utf8_lossy(parts.first()?).to_ascii_uppercase();
            let text = |at: usize| {
                String::from_utf8_lossy(parts.get(at).map_or(&[][..], Vec::as_slice)).into_owned()
            };
            let empty = BTreeMap::new();
            let database = databases.get(&*selected.lock().unwrap()).unwrap_or(&empty);
            let key = parts.get(1).cloned().unwrap_or_default();
            Some(match name.as_str() {
                "HELLO" => hello.clone(),
                "AUTH" => simple("OK"),
                "SELECT" => match text(1).parse::<u32>() {
                    Ok(index) if index < 16 => {
                        *selected.lock().unwrap() = index;
                        simple("OK")
                    }
                    _ => error("ERR DB index is out of range"),
                },
                "DBSIZE" => integer(database.len() as i64),
                "SCAN" => {
                    let cursor: usize = text(1).parse().ok()?;
                    let prefix = parts
                        .iter()
                        .position(|part| part.eq_ignore_ascii_case(b"MATCH"))
                        .and_then(|at| parts.get(at + 1))
                        .map(|pattern| unescaped_prefix(pattern))
                        .unwrap_or_default();
                    let keys: Vec<&Vec<u8>> = database.keys().skip(cursor).take(batch).collect();
                    let next = if cursor + batch >= database.len() {
                        0
                    } else {
                        cursor + batch
                    };
                    let found: Vec<Vec<u8>> = keys
                        .into_iter()
                        .filter(|key| key.starts_with(&prefix))
                        .map(|key| bulk(key))
                        .collect();
                    array(&[bulk(next.to_string().as_bytes()), array(&found)])
                }
                "TYPE" => simple(match database.get(&key) {
                    Some(Stored::Text(_)) => "string",
                    Some(Stored::Counted(kind, _)) => kind,
                    None => "none",
                }),
                "GETRANGE" => match database.get(&key) {
                    Some(Stored::Text(value)) => {
                        let end: usize = text(3).parse().ok()?;
                        bulk(&value[..value.len().min(end + 1)])
                    }
                    _ => bulk(b""),
                },
                "STRLEN" => match database.get(&key) {
                    Some(Stored::Text(value)) => integer(value.len() as i64),
                    _ => integer(0),
                },
                "HLEN" | "LLEN" | "SCARD" | "ZCARD" | "XLEN" => match database.get(&key) {
                    Some(Stored::Counted(_, count)) => integer(*count),
                    _ => integer(0),
                },
                _ => error(&format!("ERR unknown command '{name}'")),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use super::fake_server::{Stored, array, bulk, error, keyspace, simple, start};
    use crate::connection::{ConnectTarget, Connection, Connector, EngineConnector};

    fn shop_cache() -> BTreeMap<u32, BTreeMap<Vec<u8>, Stored>> {
        let mut zero = BTreeMap::new();
        zero.insert(b"board".to_vec(), Stored::Counted("zset", 8));
        zero.insert(b"cart:1".to_vec(), Stored::Counted("hash", 12));
        zero.insert(b"events".to_vec(), Stored::Counted("stream", 40));
        zero.insert(b"gone".to_vec(), Stored::Counted("hash", 0));
        zero.insert(b"queue".to_vec(), Stored::Counted("list", 3));
        zero.insert(b"tags".to_vec(), Stored::Counted("set", 8));
        zero.insert(b"user:000".to_vec(), Stored::Text(vec![b'x'; 100]));
        for number in 1..236 {
            zero.insert(
                format!("user:{number:03}").into_bytes(),
                Stored::Text(format!("v{number}").into_bytes()),
            );
        }
        let mut three = BTreeMap::new();
        for number in 0..5 {
            three.insert(
                format!("job:{number}").into_bytes(),
                Stored::Text(b"queued".to_vec()),
            );
        }
        BTreeMap::from([(0, zero), (3, three)])
    }

    fn connect(options: crate::resp_client::RespOptions) -> Connection {
        EngineConnector
            .connect(&ConnectTarget::Resp(options))
            .expect("the RESP server answers")
    }

    fn resp0(total: u64) -> KeySpaceScan {
        KeySpaceScan {
            over_resp: true,
            ..KeySpaceScan::new(Some("resp0".to_owned()), KeyCount::Exactly(total))
        }
    }

    fn column(page: &KeyPage, at: usize) -> Vec<String> {
        page.table
            .rows
            .iter()
            .map(|row| row.get(at).cloned().flatten().unwrap_or_default())
            .collect()
    }

    #[test]
    fn a_resp_server_lists_its_databases_and_pages_every_key_once_with_its_type() {
        let server = start(keyspace(
            shop_cache(),
            error("ERR unknown command 'HELLO'"),
            30,
        ));
        let mut connection = connect(server.options(None));
        assert_eq!(
            connection.status_text(),
            format!("Connected · 127.0.0.1:{} · RESP", server.port)
        );
        assert!(connection.is_resp());
        let catalog = connection.catalog().expect("the catalog reads");
        assert!(catalog.over_resp && catalog.tables.is_empty());
        let spaces: Vec<(&str, KeyCount)> = catalog
            .key_spaces
            .iter()
            .map(|space| (space.label(), space.keys))
            .collect();
        assert_eq!(
            spaces,
            [
                ("resp0", KeyCount::Exactly(242)),
                ("resp3", KeyCount::Exactly(5))
            ]
        );
        let cancel = CancelHandle::default();
        let first = connection
            .scan_keys(&resp0(1), &cancel)
            .expect("page 1 reads");
        assert_eq!(first.table.columns, RESP_KEY_COLUMNS);
        assert_eq!(first.table.rows.len(), KEY_SCAN_LIMIT);
        assert_eq!(first.table.tag, "SCAN 100");
        assert_eq!(first.end.total, Some(KeyCount::Exactly(242)));
        assert_eq!(first.table.row_count, 242);
        let row = |page: &KeyPage, key: &str| {
            page.table
                .rows
                .iter()
                .find(|row| row.first().cloned().flatten().as_deref() == Some(key))
                .map(|row| {
                    row.iter()
                        .map(|cell| cell.clone().unwrap_or_default())
                        .collect::<Vec<_>>()
                })
                .expect("the key is on the page")
        };
        assert_eq!(row(&first, "cart:1"), ["cart:1", "hash", "12 fields", "–"]);
        assert_eq!(row(&first, "queue"), ["queue", "list", "3 items", "–"]);
        assert_eq!(row(&first, "tags"), ["tags", "set", "8 members", "–"]);
        assert_eq!(row(&first, "board"), ["board", "zset", "8 members", "–"]);
        assert_eq!(
            row(&first, "events"),
            ["events", "stream", "40 entries", "–"]
        );
        assert_eq!(row(&first, "gone"), ["gone", "hash", "(gone)", "–"]);
        assert_eq!(
            row(&first, "user:000")[2],
            format!("{}…", "x".repeat(SHOWN_VALUE_BYTES))
        );
        assert_eq!(row(&first, "user:000")[3], "100");
        assert_eq!(row(&first, "user:001"), ["user:001", "string", "v1", "2"]);
        let Some(PageStart::Cursor { cursor, carried }) = first.end.next.clone() else {
            panic!("more keys follow page 1 as a cursor");
        };
        assert_eq!((cursor, carried.len()), (120, 20));
        let mut seen = column(&first, 0);
        let mut scan = resp0(242);
        let mut start = first.end.next.clone();
        let mut pages = 1;
        while let Some(next) = start {
            scan = KeySpaceScan {
                start: next,
                first_position: seen.len() + 1,
                ..scan
            };
            let page = connection
                .scan_keys(&scan, &cancel)
                .expect("a later page reads");
            seen.extend(column(&page, 0));
            start = page.end.next;
            pages += 1;
        }
        assert_eq!(pages, 3);
        assert!(scan.statement().starts_with("SCAN resp0 CURSOR "));
        let all: Vec<String> = shop_cache()
            .remove(&0)
            .unwrap()
            .into_keys()
            .map(|key| String::from_utf8(key).unwrap())
            .collect();
        assert_eq!(seen, all);
        assert!(server.sent("SCAN").iter().all(|command| command.len() == 4));
        let third = connection
            .scan_keys(
                &KeySpaceScan::new(Some("resp3".to_owned()), KeyCount::Exactly(5)),
                &cancel,
            )
            .expect("resp3 reads");
        assert_eq!(
            column(&third, 0),
            ["job:0", "job:1", "job:2", "job:3", "job:4"]
        );
        assert_eq!(third.end.next, None);
        assert!(
            server
                .sent("SELECT")
                .iter()
                .any(|command| command.get(1).map(Vec::as_slice) == Some(b"3".as_slice()))
        );
        assert_eq!(
            server.connections.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[test]
    fn a_prefix_goes_out_as_an_escaped_match_pattern() {
        let mut zero = BTreeMap::new();
        for key in ["a*b:1", "a*b:2", "axb:1", "zz"] {
            zero.insert(key.as_bytes().to_vec(), Stored::Text(b"1".to_vec()));
        }
        let server = start(keyspace(BTreeMap::from([(0, zero)]), simple("OK"), 2));
        let mut connection = connect(server.options(None));
        let scan = KeySpaceScan {
            prefix: b"a*b:".to_vec(),
            ..resp0(4)
        };
        let page = connection
            .scan_keys(&scan, &CancelHandle::default())
            .expect("the filtered page reads");
        assert_eq!(column(&page, 0), ["a*b:1", "a*b:2"]);
        assert_eq!(page.end.next, None);
        let scans = server.sent("SCAN");
        assert_eq!(scans.len(), 2);
        assert_eq!(scans[0][2], b"MATCH");
        assert_eq!(scans[0][3], b"a\\*b:*");
        assert_eq!(scans[0][5], b"101");
        assert_eq!(scan.statement(), "SCAN resp0 PREFIX 'a*b:' LIMIT 100");
    }

    #[test]
    fn hello_falls_back_to_resp2_with_auth_and_a_wrong_password_points_at_the_password() {
        let server = start(keyspace(
            shop_cache(),
            error("ERR unknown command 'HELLO'"),
            30,
        ));
        connect(server.options(Some("pw")));
        assert_eq!(
            server.sent("AUTH"),
            [vec![b"AUTH".to_vec(), b"pw".to_vec()]]
        );
        let resp3 = start(keyspace(
            shop_cache(),
            [b"%1\r\n".to_vec(), simple("proto"), b":3\r\n".to_vec()].concat(),
            30,
        ));
        connect(resp3.options(Some("pw")));
        assert_eq!(
            resp3.sent("HELLO"),
            [["HELLO", "3", "AUTH", "default", "pw"]
                .map(|part| part.as_bytes().to_vec())
                .to_vec()]
        );
        assert!(resp3.sent("AUTH").is_empty());
        let refusing = start(|parts: &[Vec<u8>]| {
            Some(
                if parts
                    .first()
                    .is_some_and(|name| name.eq_ignore_ascii_case(b"HELLO"))
                {
                    error("WRONGPASS invalid username-password pair or user is disabled.")
                } else {
                    simple("OK")
                },
            )
        });
        let failure = EngineConnector
            .connect(&ConnectTarget::Resp(refusing.options(Some("nope"))))
            .unwrap_err();
        assert_eq!(
            failure.error_line(),
            "Error: WRONGPASS invalid username-password pair or user is disabled."
        );
        assert_eq!(failure.field, crate::connection::FailureField::Password);
    }

    #[test]
    fn server_errors_and_a_dropped_connection_keep_the_server_line_and_add_a_try_line() {
        let locked = start(|parts: &[Vec<u8>]| {
            let name = String::from_utf8_lossy(parts.first()?).to_ascii_uppercase();
            Some(match name.as_str() {
                "HELLO" => error("ERR unknown command 'HELLO'"),
                _ => error("NOAUTH Authentication required."),
            })
        });
        let mut connection = connect(locked.options(None));
        let failure = connection
            .scan_keys(&resp0(0), &CancelHandle::default())
            .unwrap_err();
        assert_eq!(
            failure.report,
            [
                "NOAUTH Authentication required.",
                "Try: connect again with the password (Ctrl/Cmd+O)"
            ]
        );
        assert!(connection.catalog().is_err());
        let dropping = start(|parts: &[Vec<u8>]| {
            let name = String::from_utf8_lossy(parts.first()?).to_ascii_uppercase();
            match name.as_str() {
                "SCAN" => None,
                "DBSIZE" => Some(b":1\r\n".to_vec()),
                _ => Some(simple("OK")),
            }
        });
        let mut connection = connect(dropping.options(None));
        let failure = connection
            .scan_keys(&resp0(1), &CancelHandle::default())
            .unwrap_err();
        assert_eq!(
            failure.report,
            [
                format!("connection to 127.0.0.1:{} lost", dropping.port),
                "Try: press F5 to reconnect and scan again".to_owned()
            ]
        );
        assert!(
            connection
                .scan_keys(&resp0(1), &CancelHandle::default())
                .is_err()
        );
        assert_eq!(
            dropping
                .connections
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        let no_scan = start(|parts: &[Vec<u8>]| {
            let name = String::from_utf8_lossy(parts.first()?).to_ascii_uppercase();
            Some(match name.as_str() {
                "SCAN" => error("ERR unknown command 'SCAN', with args beginning with: '0'"),
                "DBSIZE" => b":1\r\n".to_vec(),
                _ => simple("OK"),
            })
        });
        let failure = connect(no_scan.options(None))
            .scan_keys(&resp0(1), &CancelHandle::default())
            .unwrap_err();
        assert_eq!(
            failure.report.get(1).map(String::as_str),
            Some("Try: this server has no SCAN; EggDB's key list needs it")
        );
    }

    #[test]
    fn stop_closes_the_socket_and_the_next_scan_reconnects() {
        let server = start(keyspace(shop_cache(), simple("OK"), 30));
        let mut connection = connect(server.options(None));
        let cancel = CancelHandle::default();
        cancel.cancel();
        let failure = connection.scan_keys(&resp0(242), &cancel).unwrap_err();
        assert_eq!(failure.code, "57014");
        let page = connection
            .scan_keys(&resp0(242), &CancelHandle::default())
            .expect("the next run reconnects");
        assert_eq!(page.table.rows.len(), KEY_SCAN_LIMIT);
        assert_eq!(
            server.connections.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
    }

    #[test]
    fn a_hostile_scan_reply_is_an_error_not_a_panic() {
        let replies = [
            array(&[bulk(b"not a number"), array(&[])]),
            array(&[bulk(b"0")]),
            bulk(b"0"),
            array(&[bulk(b"-1"), array(&[])]),
        ];
        for reply in replies {
            let server = start(move |parts: &[Vec<u8>]| {
                let name = String::from_utf8_lossy(parts.first()?).to_ascii_uppercase();
                Some(match name.as_str() {
                    "SCAN" => reply.clone(),
                    "DBSIZE" => b":1\r\n".to_vec(),
                    _ => simple("OK"),
                })
            });
            assert!(
                connect(server.options(None))
                    .scan_keys(&resp0(1), &CancelHandle::default())
                    .is_err()
            );
        }
        let endless = start(|parts: &[Vec<u8>]| {
            let name = String::from_utf8_lossy(parts.first()?).to_ascii_uppercase();
            Some(match name.as_str() {
                "SCAN" => array(&[bulk(b"7"), array(&[])]),
                "DBSIZE" => b":1\r\n".to_vec(),
                _ => simple("OK"),
            })
        });
        let page = connect(endless.options(None))
            .scan_keys(&resp0(1), &CancelHandle::default())
            .expect("a cursor that never ends stops after its round trips");
        assert!(page.table.rows.is_empty());
        assert_eq!(
            page.end.next,
            Some(PageStart::Cursor {
                cursor: 7,
                carried: Vec::new()
            })
        );
        assert_eq!(endless.sent("SCAN").len(), MAX_SCAN_ROUND_TRIPS);
    }

    #[test]
    fn database_names_round_trip_and_reject_other_names() {
        assert_eq!(database_name(3), "resp3");
        assert_eq!(database_index("resp3"), Some(3));
        assert_eq!(database_index("resp15"), Some(15));
        assert_eq!(database_index("resp"), None);
        assert_eq!(database_index("resp-1"), None);
        assert_eq!(database_index("users"), None);
        assert_eq!(database_index("resp99999999999"), None);
    }

    #[test]
    fn server_errors_get_their_try_lines() {
        let cases = [
            (
                "NOAUTH Authentication required.",
                Some("Try: connect again with the password (Ctrl/Cmd+O)"),
            ),
            (
                "WRONGPASS invalid username-password pair",
                Some("Try: connect again with the password (Ctrl/Cmd+O)"),
            ),
            (
                "ERR unknown command 'SCAN', with args beginning with: ",
                Some("Try: this server has no SCAN; EggDB's key list needs it"),
            ),
            (
                "LOADING Redis is loading the dataset in memory",
                Some("Try: wait for the server to finish loading and press F5"),
            ),
            (
                "ERR DB index is out of range",
                Some("Try: press F5 in the tree; this database is gone"),
            ),
            ("ERR something else", None),
        ];
        for (line, fix) in cases {
            let failure = reply_failure(line);
            assert_eq!(failure.report.first().map(String::as_str), Some(line));
            assert_eq!(failure.report.get(1).map(String::as_str), fix);
        }
        assert_eq!(
            lost_failure("127.0.0.1:6379").report,
            [
                "connection to 127.0.0.1:6379 lost",
                "Try: press F5 to reconnect and scan again"
            ]
        );
    }

    #[test]
    fn aggregate_sizes_read_as_counts_of_their_parts() {
        assert_eq!(aggregate_text("hash", 12).as_deref(), Some("12 fields"));
        assert_eq!(aggregate_text("list", 3).as_deref(), Some("3 items"));
        assert_eq!(aggregate_text("set", 8).as_deref(), Some("8 members"));
        assert_eq!(aggregate_text("zset", 1).as_deref(), Some("1 member"));
        assert_eq!(
            aggregate_text("stream", 1_204).as_deref(),
            Some("1,204 entries")
        );
        assert_eq!(aggregate_text("string", 4), None);
    }
}
