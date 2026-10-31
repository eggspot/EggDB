#![allow(dead_code)]

use eggdb_crypto::{base64_decode, base64_encode, hmac_sha256, pbkdf2_hmac_sha256, sha256};
use eggdb_pgwire::{ScramVerifier, ServerConfig};
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
        Self::start_configured(max_connections, authentication_timeout, None)
    }

    pub fn start_with_tls(tls: Arc<eggdb_tls::ServerConfig>) -> Self {
        Self::start_configured(8, Duration::from_secs(5), Some(tls))
    }

    fn start_configured(
        max_connections: usize,
        authentication_timeout: Duration,
        tls: Option<Arc<eggdb_tls::ServerConfig>>,
    ) -> Self {
        let listener: TcpListener =
            eggdb_pgwire::listen_on_loopback("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let logged = Arc::new(Mutex::new(Vec::new()));
        let config = ServerConfig {
            user: USER.to_owned(),
            verifier: ScramVerifier::derive(PASSWORD.as_bytes(), b"0123456789abcdef", 4096),
            database: DATABASE.to_owned(),
            max_connections,
            authentication_timeout,
            tls,
        };
        let thread_stop = Arc::clone(&stop);
        let thread_logged = Arc::clone(&logged);
        let thread = std::thread::spawn(move || {
            let store = SharedMemoryStore::default();
            let stop = move || thread_stop.load(Ordering::Acquire);
            let log = move |line: &str| thread_logged.lock().unwrap().push(line.to_owned());
            eggdb_pgwire::serve(&store, &listener, &config, &stop, &log).unwrap()
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

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub tag: u8,
    pub body: Vec<u8>,
}

impl Message {
    pub fn field(&self, code: u8) -> Option<String> {
        let mut rest = &self.body[..];
        while let [field, tail @ ..] = rest {
            if *field == 0 {
                return None;
            }
            let end = tail.iter().position(|&byte| byte == 0)?;
            if *field == code {
                return Some(String::from_utf8_lossy(&tail[..end]).into_owned());
            }
            rest = &tail[end + 1..];
        }
        None
    }

    pub fn code(&self) -> String {
        self.field(b'C').unwrap_or_default()
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(self.body.strip_suffix(&[0]).unwrap_or(&self.body)).into_owned()
    }

    pub fn row(&self) -> Vec<Option<String>> {
        let count = i16::from_be_bytes([self.body[0], self.body[1]]) as usize;
        let mut at = 2;
        let mut values = Vec::new();
        for _ in 0..count {
            let length = i32::from_be_bytes(self.body[at..at + 4].try_into().unwrap());
            at += 4;
            if length < 0 {
                values.push(None);
            } else {
                let end = at + length as usize;
                values.push(Some(
                    String::from_utf8_lossy(&self.body[at..end]).into_owned(),
                ));
                at = end;
            }
        }
        values
    }

    pub fn raw_row(&self) -> Vec<Option<Vec<u8>>> {
        let count = i16::from_be_bytes([self.body[0], self.body[1]]) as usize;
        let mut at = 2;
        let mut values = Vec::new();
        for _ in 0..count {
            let length = i32::from_be_bytes(self.body[at..at + 4].try_into().unwrap());
            at += 4;
            if length < 0 {
                values.push(None);
            } else {
                let end = at + length as usize;
                values.push(Some(self.body[at..end].to_vec()));
                at = end;
            }
        }
        values
    }

    pub fn column_types(&self) -> Vec<(String, u32, i16)> {
        let count = i16::from_be_bytes([self.body[0], self.body[1]]) as usize;
        let mut at = 2;
        let mut columns = Vec::new();
        for _ in 0..count {
            let end = at + self.body[at..].iter().position(|&byte| byte == 0).unwrap();
            let name = String::from_utf8_lossy(&self.body[at..end]).into_owned();
            at = end + 1 + 4 + 2;
            let oid = u32::from_be_bytes(self.body[at..at + 4].try_into().unwrap());
            at += 4 + 2 + 4;
            let format = i16::from_be_bytes([self.body[at], self.body[at + 1]]);
            at += 2;
            columns.push((name, oid, format));
        }
        columns
    }
}

pub struct Client {
    pub stream: TcpStream,
}

pub fn cstring(text: &str) -> Vec<u8> {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    bytes
}

impl Client {
    pub fn connect(address: SocketAddr) -> Self {
        let stream = TcpStream::connect(address).unwrap();
        let _ = stream.set_read_timeout(Some(Duration::from_secs(20)));
        Self { stream }
    }

    pub fn send_raw(&mut self, bytes: &[u8]) {
        let _ = self.stream.write_all(bytes);
    }

    pub fn send(&mut self, tag: u8, body: &[u8]) {
        let mut bytes = vec![tag];
        bytes.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
        bytes.extend_from_slice(body);
        self.send_raw(&bytes);
    }

    pub fn send_request(&mut self, code: u32) {
        let mut bytes = 8u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(&code.to_be_bytes());
        self.send_raw(&bytes);
    }

    pub fn send_startup(&mut self, code: u32, parameters: &[(&str, &str)]) {
        let mut body = code.to_be_bytes().to_vec();
        for (name, value) in parameters {
            body.extend(cstring(name));
            body.extend(cstring(value));
        }
        body.push(0);
        let mut bytes = ((body.len() + 4) as u32).to_be_bytes().to_vec();
        bytes.extend(body);
        self.send_raw(&bytes);
    }

    pub fn read_byte(&mut self) -> Option<u8> {
        let mut byte = [0u8; 1];
        self.stream.read_exact(&mut byte).ok().map(|()| byte[0])
    }

    pub fn read(&mut self) -> Option<Message> {
        let tag = self.read_byte()?;
        let mut length = [0u8; 4];
        self.stream.read_exact(&mut length).ok()?;
        let length = u32::from_be_bytes(length) as usize;
        let mut body = vec![0u8; length.checked_sub(4)?];
        self.stream.read_exact(&mut body).ok()?;
        Some(Message { tag, body })
    }

    pub fn read_until_ready(&mut self) -> Vec<Message> {
        let mut messages = Vec::new();
        loop {
            let message = self.read().expect("the server closed the connection");
            let ready = message.tag == b'Z';
            messages.push(message);
            if ready {
                return messages;
            }
        }
    }

    pub fn authenticate(&mut self, password: &str) -> Message {
        let message = self.read().expect("an authentication request");
        assert_eq!(message.tag, b'R');
        assert_eq!(&message.body[..4], &10i32.to_be_bytes());
        assert_eq!(&message.body[4..], b"SCRAM-SHA-256\0\0");
        let client_nonce = "fyko+d2lbbFgONRv9qkxdawL";
        let client_first_bare = format!("n=,r={client_nonce}");
        let mut initial = cstring("SCRAM-SHA-256");
        let client_first = format!("n,,{client_first_bare}");
        initial.extend_from_slice(&(client_first.len() as i32).to_be_bytes());
        initial.extend_from_slice(client_first.as_bytes());
        self.send(b'p', &initial);
        let message = self.read().expect("a SASL continue");
        if message.tag == b'E' {
            return message;
        }
        assert_eq!(&message.body[..4], &11i32.to_be_bytes());
        let server_first = String::from_utf8(message.body[4..].to_vec()).unwrap();
        let mut nonce = "";
        let mut salt = Vec::new();
        let mut iterations = 0;
        for attribute in server_first.split(',') {
            if let Some(value) = attribute.strip_prefix("r=") {
                nonce = value;
            } else if let Some(value) = attribute.strip_prefix("s=") {
                salt = base64_decode(value.as_bytes()).unwrap();
            } else if let Some(value) = attribute.strip_prefix("i=") {
                iterations = value.parse().unwrap();
            }
        }
        assert!(nonce.starts_with(client_nonce));
        let mut salted = [0u8; 32];
        pbkdf2_hmac_sha256(password.as_bytes(), &salt, iterations, &mut salted);
        let client_key = hmac_sha256(&salted, b"Client Key");
        let stored_key = sha256(&client_key);
        let without_proof = format!("c=biws,r={nonce}");
        let auth_message = format!("{client_first_bare},{server_first},{without_proof}");
        let signature = hmac_sha256(&stored_key, auth_message.as_bytes());
        let proof: Vec<u8> = client_key
            .iter()
            .zip(signature)
            .map(|(a, b)| a ^ b)
            .collect();
        let client_final = format!("{without_proof},p={}", base64_encode(&proof));
        self.send(b'p', client_final.as_bytes());
        let message = self.read().expect("a SASL final");
        if message.tag == b'E' {
            return message;
        }
        assert_eq!(&message.body[..4], &12i32.to_be_bytes());
        let server_key = hmac_sha256(&salted, b"Server Key");
        let expected = format!(
            "v={}",
            base64_encode(&hmac_sha256(&server_key, auth_message.as_bytes()))
        );
        assert_eq!(&message.body[4..], expected.as_bytes());
        self.read().expect("authentication ok")
    }

    pub fn login(address: SocketAddr) -> (Self, Vec<Message>) {
        let mut client = Self::connect(address);
        client.send_startup(
            196_608,
            &[
                ("user", USER),
                ("database", DATABASE),
                ("application_name", "tests"),
            ],
        );
        let ok = client.authenticate(PASSWORD);
        assert_eq!(
            (ok.tag, ok.body.clone()),
            (b'R', 0i32.to_be_bytes().to_vec())
        );
        let greeting = client.read_until_ready();
        (client, greeting)
    }

    pub fn query(&mut self, text: &str) -> Vec<Message> {
        self.send(b'Q', &cstring(text));
        self.read_until_ready()
    }
}

pub fn tags(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|message| char::from(message.tag))
        .collect()
}

pub fn ready_status(messages: &[Message]) -> u8 {
    messages.last().map_or(0, |message| message.body[0])
}
