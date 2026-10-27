#![allow(dead_code)]

use eggdb_mysqlwire::{
    MySqlCredentials, NONCE_BYTES, ServerConfig, caching_sha2_response, native_response,
};
use eggdb_sql::{
    EntryVisitor, KeyValueRead, KeyValueStore, KeyValueWrite, ScanDirection, SqlError, SqlResult,
    StoreFailure,
};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub const USER: &str = "ada";
pub const PASSWORD: &str = "correct horse";
pub const DATABASE: &str = "shop";

type Map = BTreeMap<Vec<u8>, Vec<u8>>;

#[derive(Default)]
pub struct SharedMemoryStore {
    committed: Mutex<Arc<Map>>,
    writer: Mutex<()>,
}

pub struct Snapshot(Arc<Map>);

pub struct Writer<'s> {
    store: &'s SharedMemoryStore,
    map: Map,
    _turn: MutexGuard<'s, ()>,
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

impl KeyValueStore for SharedMemoryStore {
    type Read<'s> = Snapshot;
    type Write<'s> = Writer<'s>;

    fn begin_read(&self) -> SqlResult<Snapshot> {
        Ok(Snapshot(Arc::clone(&self.committed.lock().unwrap())))
    }

    fn begin_write(&self) -> SqlResult<Writer<'_>> {
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            if let Ok(turn) = self.writer.try_lock() {
                let map = self.committed.lock().unwrap().as_ref().clone();
                return Ok(Writer {
                    store: self,
                    map,
                    _turn: turn,
                });
            }
            if Instant::now() >= deadline {
                return Err(SqlError::store(
                    StoreFailure::Busy,
                    "another write transaction is still open",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

pub struct TestServer {
    pub address: SocketAddr,
    pub stop: Arc<AtomicBool>,
    pub logged: Arc<Mutex<Vec<String>>>,
    thread: Option<std::thread::JoinHandle<usize>>,
}

impl TestServer {
    pub fn start(max_connections: usize) -> Self {
        Self::start_with(max_connections, Duration::from_secs(5))
    }

    pub fn start_with(max_connections: usize, authentication_timeout: Duration) -> Self {
        let listener: TcpListener =
            eggdb_mysqlwire::listen_on_loopback("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let logged = Arc::new(Mutex::new(Vec::new()));
        let config = ServerConfig {
            user: USER.to_owned(),
            credentials: MySqlCredentials::derive(PASSWORD.as_bytes()),
            database: DATABASE.to_owned(),
            max_connections,
            authentication_timeout,
        };
        let thread_stop = Arc::clone(&stop);
        let thread_logged = Arc::clone(&logged);
        let thread = std::thread::spawn(move || {
            let store = SharedMemoryStore::default();
            let stop = move || thread_stop.load(Ordering::Acquire);
            let log = move |line: &str| thread_logged.lock().unwrap().push(line.to_owned());
            eggdb_mysqlwire::serve(&store, &listener, &config, &stop, &log).unwrap()
        });
        Self {
            address,
            stop,
            logged,
            thread: Some(thread),
        }
    }

    pub fn stop(&mut self) -> usize {
        self.stop.store(true, Ordering::Release);
        self.thread
            .take()
            .map_or(0, |thread| thread.join().unwrap())
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop();
    }
}

pub const PROTOCOL_41: u32 = 1 << 9;
pub const SECURE_CONNECTION: u32 = 1 << 15;
pub const CONNECT_WITH_DB: u32 = 1 << 3;
pub const MULTI_STATEMENTS: u32 = 1 << 16;
pub const MULTI_RESULTS: u32 = 1 << 17;
pub const PLUGIN_AUTH: u32 = 1 << 19;
pub const CLIENT_FLAGS: u32 = PROTOCOL_41
    | SECURE_CONNECTION
    | CONNECT_WITH_DB
    | MULTI_STATEMENTS
    | MULTI_RESULTS
    | PLUGIN_AUTH;

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Ok {
        affected: u64,
        last_insert_id: u64,
        status: u16,
        warnings: u16,
    },
    Err {
        code: u16,
        state: String,
        message: String,
    },
    Rows {
        columns: Vec<(String, u8)>,
        rows: Vec<Vec<Option<String>>>,
        status: u16,
    },
}

pub struct Handshake {
    pub nonce: [u8; NONCE_BYTES],
    pub plugin: String,
    pub capabilities: u32,
    pub version: String,
}

pub struct Client {
    pub stream: TcpStream,
    pub sequence: u8,
}

pub fn length_encoded(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let first = *bytes.get(*at)?;
    *at += 1;
    let width = match first {
        0..=0xFA => return Some(u64::from(first)),
        0xFC => 2,
        0xFD => 3,
        0xFE => 8,
        _ => return None,
    };
    let mut value = 0u64;
    for index in 0..width {
        value |= u64::from(*bytes.get(*at + index)?) << (8 * index);
    }
    *at += width;
    Some(value)
}

pub fn length_encoded_text(bytes: &[u8], at: &mut usize) -> Option<Option<String>> {
    if bytes.get(*at) == Some(&0xFB) {
        *at += 1;
        return Some(None);
    }
    let length = length_encoded(bytes, at)? as usize;
    let text = String::from_utf8_lossy(bytes.get(*at..*at + length)?).into_owned();
    *at += length;
    Some(Some(text))
}

pub fn handshake_response(user: &str, response: &[u8], database: &str, plugin: &str) -> Vec<u8> {
    let mut body = CLIENT_FLAGS.to_le_bytes().to_vec();
    body.extend_from_slice(&(16u32 << 20).to_le_bytes());
    body.push(255);
    body.extend_from_slice(&[0; 23]);
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    body.push(response.len() as u8);
    body.extend_from_slice(response);
    body.extend_from_slice(database.as_bytes());
    body.push(0);
    body.extend_from_slice(plugin.as_bytes());
    body.push(0);
    body
}

impl Client {
    pub fn connect(address: SocketAddr) -> Self {
        let stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        Self {
            stream,
            sequence: 0,
        }
    }

    pub fn send(&mut self, payload: &[u8]) {
        let mut packet = (payload.len() as u32).to_le_bytes()[..3].to_vec();
        packet.push(self.sequence);
        self.sequence = self.sequence.wrapping_add(1);
        packet.extend_from_slice(payload);
        self.send_raw(&packet);
    }

    pub fn send_raw(&mut self, bytes: &[u8]) {
        let _ = self.stream.write_all(bytes);
    }

    pub fn read(&mut self) -> Option<Vec<u8>> {
        let mut header = [0u8; 4];
        self.stream.read_exact(&mut header).ok()?;
        let length = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
        self.sequence = header[3].wrapping_add(1);
        let mut body = vec![0u8; length];
        self.stream.read_exact(&mut body).ok()?;
        Some(body)
    }

    pub fn handshake(&mut self) -> Handshake {
        let body = self.read().expect("a handshake");
        assert_eq!(body[0], 10);
        let version_end = body[1..].iter().position(|&byte| byte == 0).unwrap() + 1;
        let version = String::from_utf8_lossy(&body[1..version_end]).into_owned();
        let mut at = version_end + 1 + 4;
        let mut nonce = [0u8; NONCE_BYTES];
        nonce[..8].copy_from_slice(&body[at..at + 8]);
        at += 9;
        let lower = u16::from_le_bytes([body[at], body[at + 1]]);
        at += 2 + 1 + 2;
        let upper = u16::from_le_bytes([body[at], body[at + 1]]);
        at += 2 + 1 + 10;
        nonce[8..].copy_from_slice(&body[at..at + 12]);
        at += 13;
        let plugin_end = body[at..].iter().position(|&byte| byte == 0).unwrap() + at;
        Handshake {
            nonce,
            plugin: String::from_utf8_lossy(&body[at..plugin_end]).into_owned(),
            capabilities: u32::from(lower) | u32::from(upper) << 16,
            version,
        }
    }

    pub fn login_with(address: SocketAddr, password: &str, plugin: &str) -> (Self, Reply) {
        let mut client = Self::connect(address);
        let handshake = client.handshake();
        let response = match plugin {
            "mysql_native_password" => native_response(password.as_bytes(), &handshake.nonce),
            "caching_sha2_password" => caching_sha2_response(password.as_bytes(), &handshake.nonce),
            _ => Vec::new(),
        };
        client.send(&handshake_response(USER, &response, DATABASE, plugin));
        let mut reply = client.read().unwrap_or_default();
        if reply.first() == Some(&0xFE) && reply.len() > 1 {
            let name_end = reply[1..].iter().position(|&byte| byte == 0).unwrap() + 1;
            let mut nonce = [0u8; NONCE_BYTES];
            nonce.copy_from_slice(&reply[name_end + 1..name_end + 1 + NONCE_BYTES]);
            client.send(&native_response(password.as_bytes(), &nonce));
            reply = client.read().unwrap_or_default();
        }
        if reply == [0x01, 0x03] {
            reply = client.read().unwrap_or_default();
        }
        let parsed = client.parse_reply(reply);
        (client, parsed)
    }

    pub fn login(address: SocketAddr) -> Self {
        let (client, reply) = Self::login_with(address, PASSWORD, "caching_sha2_password");
        assert!(matches!(reply, Reply::Ok { .. }), "{reply:?}");
        client
    }

    pub fn parse_reply(&mut self, body: Vec<u8>) -> Reply {
        match body.first() {
            Some(0x00) => {
                let mut at = 1;
                let affected = length_encoded(&body, &mut at).unwrap();
                let last_insert_id = length_encoded(&body, &mut at).unwrap();
                let status = u16::from_le_bytes([body[at], body[at + 1]]);
                let warnings = u16::from_le_bytes([body[at + 2], body[at + 3]]);
                Reply::Ok {
                    affected,
                    last_insert_id,
                    status,
                    warnings,
                }
            }
            Some(0xFF) => Reply::Err {
                code: u16::from_le_bytes([body[1], body[2]]),
                state: String::from_utf8_lossy(&body[4..9]).into_owned(),
                message: String::from_utf8_lossy(&body[9..]).into_owned(),
            },
            _ => {
                let mut at = 0;
                let count = length_encoded(&body, &mut at).unwrap() as usize;
                let mut columns = Vec::new();
                for _ in 0..count {
                    let definition = self.read().unwrap();
                    let mut at = 0;
                    for _ in 0..4 {
                        length_encoded_text(&definition, &mut at);
                    }
                    let name = length_encoded_text(&definition, &mut at).unwrap().unwrap();
                    length_encoded_text(&definition, &mut at);
                    at += 1 + 2 + 4;
                    columns.push((name, definition[at]));
                }
                let eof = self.read().unwrap();
                assert_eq!(eof[0], 0xFE);
                let mut rows = Vec::new();
                loop {
                    let row = self.read().unwrap();
                    if row.first() == Some(&0xFE) && row.len() < 9 {
                        let status = u16::from_le_bytes([row[3], row[4]]);
                        return Reply::Rows {
                            columns,
                            rows,
                            status,
                        };
                    }
                    if row.first() == Some(&0xFF) {
                        return self.parse_reply(row);
                    }
                    let mut at = 0;
                    rows.push(
                        (0..count)
                            .map(|_| length_encoded_text(&row, &mut at).unwrap())
                            .collect(),
                    );
                }
            }
        }
    }

    pub fn command(&mut self, code: u8, payload: &[u8]) {
        self.sequence = 0;
        let mut body = vec![code];
        body.extend_from_slice(payload);
        self.send(&body);
    }

    pub fn query(&mut self, text: &str) -> Reply {
        self.command(0x03, text.as_bytes());
        let body = self.read().unwrap_or_default();
        self.parse_reply(body)
    }

    pub fn next_reply(&mut self) -> Reply {
        let body = self.read().unwrap_or_default();
        self.parse_reply(body)
    }
}

pub fn texts(reply: &Reply) -> Vec<Vec<String>> {
    match reply {
        Reply::Rows { rows, .. } => rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| cell.clone().unwrap_or_else(|| "NULL".into()))
                    .collect()
            })
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}
