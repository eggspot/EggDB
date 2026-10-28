#![allow(dead_code)]

use eggdb_sql::{
    EntryVisitor, KeyValueRead, KeyValueStore, KeyValueWrite, ScanDirection, SqlError, SqlResult,
    StoreFailure,
};
use eggdb_tdswire::{ServerConfig, TdsCredentials};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub const USER: &str = "ada";
pub const PASSWORD: &str = "correct horse";
pub const DATABASE: &str = "shop";
pub const TDS_7_4: u32 = 0x7400_0004;

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
        Self::start_with(max_connections, Duration::from_secs(10))
    }

    pub fn start_with(max_connections: usize, authentication_timeout: Duration) -> Self {
        let listener: TcpListener =
            eggdb_tdswire::listen_on_loopback("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let logged = Arc::new(Mutex::new(Vec::new()));
        let config = ServerConfig {
            user: USER.to_owned(),
            credentials: TdsCredentials::derive_with_salt(PASSWORD.as_bytes(), [3; 16], 2),
            database: DATABASE.to_owned(),
            max_connections,
            authentication_timeout,
            log_batches: true,
        };
        let thread_stop = Arc::clone(&stop);
        let thread_logged = Arc::clone(&logged);
        let thread = std::thread::spawn(move || {
            let store = SharedMemoryStore::default();
            let stop = move || thread_stop.load(Ordering::Acquire);
            let log = move |line: &str| thread_logged.lock().unwrap().push(line.to_owned());
            eggdb_tdswire::serve(&store, &listener, &config, &stop, &log).unwrap()
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

    pub fn log(&self) -> Vec<String> {
        self.logged.lock().unwrap().clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn utf16(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

pub fn prelogin_body(encryption: u8) -> Vec<u8> {
    vec![
        0, 0, 21, 0, 6, 1, 0, 27, 0, 1, 2, 0, 28, 0, 1, 4, 0, 29, 0, 1, 0xFF, 17, 0, 0, 0, 0, 0,
        encryption, 0, 0,
    ]
}

pub struct LoginOptions<'o> {
    pub user: &'o str,
    pub password: &'o str,
    pub database: &'o str,
    pub version: u32,
    pub packet_size: u32,
    pub option_flags2: u8,
    pub option_flags3: u8,
}

impl Default for LoginOptions<'_> {
    fn default() -> Self {
        Self {
            user: USER,
            password: PASSWORD,
            database: DATABASE,
            version: TDS_7_4,
            packet_size: 4096,
            option_flags2: 0x03,
            option_flags3: 0,
        }
    }
}

pub fn login7_body(options: &LoginOptions<'_>) -> Vec<u8> {
    let obfuscated: Vec<u8> = utf16(options.password)
        .into_iter()
        .map(|byte| byte.rotate_left(4) ^ 0xA5)
        .collect();
    let names: [Vec<u8>; 9] = [
        utf16("testhost"),
        utf16(options.user),
        obfuscated,
        utf16("eggdb-tests"),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        utf16(options.database),
    ];
    let mut data = Vec::new();
    let mut table = Vec::new();
    for name in &names {
        table.extend_from_slice(&((94 + data.len()) as u16).to_le_bytes());
        table.extend_from_slice(&((name.len() / 2) as u16).to_le_bytes());
        data.extend_from_slice(name);
    }
    let mut body = Vec::new();
    body.extend_from_slice(&((94 + data.len()) as u32).to_le_bytes());
    body.extend_from_slice(&options.version.to_le_bytes());
    body.extend_from_slice(&options.packet_size.to_le_bytes());
    body.extend_from_slice(&[0; 12]);
    body.extend_from_slice(&[0xE0, options.option_flags2, 0, options.option_flags3]);
    body.extend_from_slice(&[0; 8]);
    body.extend_from_slice(&table);
    body.extend_from_slice(&[0; 6]);
    let end = (94 + data.len()) as u16;
    for _ in 0..3 {
        body.extend_from_slice(&end.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
    }
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&data);
    body
}

pub fn batch_body(text: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&22u32.to_le_bytes());
    body.extend_from_slice(&18u32.to_le_bytes());
    body.extend_from_slice(&2u16.to_le_bytes());
    body.extend_from_slice(&0u64.to_le_bytes());
    body.extend_from_slice(&1u32.to_le_bytes());
    body.extend(utf16(text));
    body
}

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    EnvChange {
        kind: u8,
        new: Vec<u8>,
        old: Vec<u8>,
    },
    Info {
        number: u32,
        message: String,
    },
    Error {
        number: u32,
        severity: u8,
        message: String,
        line: u32,
    },
    LoginAck {
        version: [u8; 4],
        program: String,
    },
    Columns(Vec<(String, u8)>),
    Row(Vec<Option<String>>),
    Done {
        status: u16,
        command: u16,
        rows: u64,
    },
}

pub struct Client {
    stream: TcpStream,
    pub packet_size: usize,
}

fn read_b_varchar(body: &[u8], at: &mut usize) -> String {
    let count = usize::from(body[*at]);
    *at += 1;
    let text = from_utf16(&body[*at..*at + count * 2]);
    *at += count * 2;
    text
}

fn from_utf16(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    String::from_utf16(&units).unwrap()
}

fn u16_at(body: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([body[at], body[at + 1]])
}

fn u32_at(body: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(body[at..at + 4].try_into().unwrap())
}

fn u64_at(body: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(body[at..at + 8].try_into().unwrap())
}

#[derive(Debug, Clone, Copy)]
struct ColumnType {
    kind: u8,
    length: usize,
    scale: u8,
}

fn read_type(body: &[u8], at: &mut usize) -> ColumnType {
    let kind = body[*at];
    *at += 1;
    match kind {
        0x26 | 0x6D | 0x68 => {
            let length = usize::from(body[*at]);
            *at += 1;
            ColumnType {
                kind,
                length,
                scale: 0,
            }
        }
        0x6A => {
            let length = usize::from(body[*at]);
            let scale = body[*at + 2];
            *at += 3;
            ColumnType {
                kind,
                length,
                scale,
            }
        }
        0x2A | 0x29 | 0x2B => {
            let scale = body[*at];
            *at += 1;
            ColumnType {
                kind,
                length: 0,
                scale,
            }
        }
        0x28 => ColumnType {
            kind,
            length: 0,
            scale: 0,
        },
        0x24 => {
            let length = usize::from(body[*at]);
            *at += 1;
            ColumnType {
                kind,
                length,
                scale: 0,
            }
        }
        0xE7 => {
            let length = usize::from(u16_at(body, *at));
            *at += 7;
            ColumnType {
                kind,
                length,
                scale: 0,
            }
        }
        0xA5 => {
            let length = usize::from(u16_at(body, *at));
            *at += 2;
            ColumnType {
                kind,
                length,
                scale: 0,
            }
        }
        other => panic!("unexpected type 0x{other:02x}"),
    }
}

fn read_plp(body: &[u8], at: &mut usize) -> Option<Vec<u8>> {
    let total = u64_at(body, *at);
    *at += 8;
    if total == u64::MAX {
        return None;
    }
    let mut data = Vec::new();
    loop {
        let chunk = u32_at(body, *at) as usize;
        *at += 4;
        if chunk == 0 {
            return Some(data);
        }
        data.extend_from_slice(&body[*at..*at + chunk]);
        *at += chunk;
    }
}

fn read_value(body: &[u8], at: &mut usize, column: ColumnType) -> Option<String> {
    match column.kind {
        0x26 => {
            let length = usize::from(body[*at]);
            *at += 1;
            let mut bytes = [0u8; 8];
            bytes[..length].copy_from_slice(&body[*at..*at + length]);
            *at += length;
            if length == 0 {
                return None;
            }
            let shift = 64 - 8 * length as u32;
            Some(((i64::from_le_bytes(bytes) << shift) >> shift).to_string())
        }
        0x68 => {
            let length = usize::from(body[*at]);
            *at += 1;
            let value = (length == 1).then(|| body[*at].to_string());
            *at += length;
            value
        }
        0x6D => {
            let length = usize::from(body[*at]);
            *at += 1;
            let value = match length {
                4 => Some(f32::from_le_bytes(body[*at..*at + 4].try_into().unwrap()).to_string()),
                8 => Some(f64::from_le_bytes(body[*at..*at + 8].try_into().unwrap()).to_string()),
                _ => None,
            };
            *at += length;
            value
        }
        0x6A => {
            let length = usize::from(body[*at]);
            *at += 1;
            if length == 0 {
                return None;
            }
            let positive = body[*at] == 1;
            let mut bytes = [0u8; 16];
            bytes[..length - 1].copy_from_slice(&body[*at + 1..*at + length]);
            *at += length;
            let units = u128::from_le_bytes(bytes).to_string();
            let scale = usize::from(column.scale);
            let padded = format!("{units:0>width$}", width = scale + 1);
            let (whole, fraction) = padded.split_at(padded.len() - scale);
            let sign = if positive { "" } else { "-" };
            Some(if scale == 0 {
                format!("{sign}{whole}")
            } else {
                format!("{sign}{whole}.{fraction}")
            })
        }
        0x2A => {
            let length = usize::from(body[*at]);
            *at += 1;
            if length == 0 {
                return None;
            }
            let time_length = length - 3;
            let mut time = [0u8; 8];
            time[..time_length].copy_from_slice(&body[*at..*at + time_length]);
            let mut days = [0u8; 4];
            days[..3].copy_from_slice(&body[*at + time_length..*at + length]);
            *at += length;
            Some(format!(
                "days={} units={}",
                u32::from_le_bytes(days),
                u64::from_le_bytes(time)
            ))
        }
        0x28 => {
            let length = usize::from(body[*at]);
            *at += 1;
            let mut days = [0u8; 4];
            days[..length].copy_from_slice(&body[*at..*at + length]);
            *at += length;
            (length != 0).then(|| format!("days={}", u32::from_le_bytes(days)))
        }
        0x29 => {
            let length = usize::from(body[*at]);
            *at += 1;
            let mut time = [0u8; 8];
            time[..length].copy_from_slice(&body[*at..*at + length]);
            *at += length;
            (length != 0).then(|| format!("units={}", u64::from_le_bytes(time)))
        }
        0x2B => {
            let length = usize::from(body[*at]);
            *at += 1;
            if length == 0 {
                return None;
            }
            let time_length = length - 5;
            let mut time = [0u8; 8];
            time[..time_length].copy_from_slice(&body[*at..*at + time_length]);
            let mut days = [0u8; 4];
            days[..3].copy_from_slice(&body[*at + time_length..*at + time_length + 3]);
            let offset = i16::from_le_bytes([body[*at + length - 2], body[*at + length - 1]]);
            *at += length;
            Some(format!(
                "days={} units={} offset={offset}",
                u32::from_le_bytes(days),
                u64::from_le_bytes(time)
            ))
        }
        0x24 => {
            let length = usize::from(body[*at]);
            *at += 1;
            let hex = body[*at..*at + length]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            *at += length;
            (length != 0).then_some(hex)
        }
        0xE7 if column.length == 0xFFFF => read_plp(body, at).map(|data| from_utf16(&data)),
        0xE7 => {
            let length = u16_at(body, *at);
            *at += 2;
            if length == 0xFFFF {
                return None;
            }
            let text = from_utf16(&body[*at..*at + usize::from(length)]);
            *at += usize::from(length);
            Some(text)
        }
        0xA5 if column.length == 0xFFFF => {
            read_plp(body, at).map(|data| data.iter().map(|byte| format!("{byte:02x}")).collect())
        }
        0xA5 => {
            let length = u16_at(body, *at);
            *at += 2;
            if length == 0xFFFF {
                return None;
            }
            let hex = body[*at..*at + usize::from(length)]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            *at += usize::from(length);
            Some(hex)
        }
        other => panic!("unexpected type 0x{other:02x}"),
    }
}

pub fn parse_tokens(body: &[u8]) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut at = 0;
    let mut columns: Vec<ColumnType> = Vec::new();
    while at < body.len() {
        let token = body[at];
        at += 1;
        match token {
            0xE3 => {
                let length = usize::from(u16_at(body, at));
                let end = at + 2 + length;
                let kind = body[at + 2];
                let mut cursor = at + 3;
                let mut value = || {
                    let count = usize::from(body[cursor]);
                    let width = if matches!(kind, 1 | 2 | 4) { 2 } else { 1 };
                    let bytes = body[cursor + 1..cursor + 1 + count * width].to_vec();
                    cursor += 1 + count * width;
                    bytes
                };
                let new = value();
                let old = value();
                tokens.push(Token::EnvChange { kind, new, old });
                at = end;
            }
            0xAA | 0xAB => {
                let length = usize::from(u16_at(body, at));
                let end = at + 2 + length;
                let number = u32_at(body, at + 2);
                let severity = body[at + 7];
                let count = usize::from(u16_at(body, at + 8));
                let message = from_utf16(&body[at + 10..at + 10 + count * 2]);
                let mut cursor = at + 10 + count * 2;
                read_b_varchar(body, &mut cursor);
                read_b_varchar(body, &mut cursor);
                let line = u32_at(body, cursor);
                tokens.push(if token == 0xAA {
                    Token::Error {
                        number,
                        severity,
                        message,
                        line,
                    }
                } else {
                    Token::Info { number, message }
                });
                at = end;
            }
            0xAD => {
                let length = usize::from(u16_at(body, at));
                let end = at + 2 + length;
                let mut version = [0u8; 4];
                version.copy_from_slice(&body[at + 3..at + 7]);
                let mut cursor = at + 7;
                let program = read_b_varchar(body, &mut cursor);
                tokens.push(Token::LoginAck { version, program });
                at = end;
            }
            0x81 => {
                let count = usize::from(u16_at(body, at));
                at += 2;
                columns.clear();
                let mut described = Vec::new();
                for _ in 0..count {
                    at += 6;
                    let column = read_type(body, &mut at);
                    let name = read_b_varchar(body, &mut at);
                    described.push((name, column.kind));
                    columns.push(column);
                }
                tokens.push(Token::Columns(described));
            }
            0xD1 => {
                let row = columns
                    .clone()
                    .into_iter()
                    .map(|column| read_value(body, &mut at, column))
                    .collect();
                tokens.push(Token::Row(row));
            }
            0xFD..=0xFF => {
                tokens.push(Token::Done {
                    status: u16_at(body, at),
                    command: u16_at(body, at + 2),
                    rows: u64_at(body, at + 4),
                });
                at += 12;
            }
            other => panic!("unexpected token 0x{other:02x} at {at}"),
        }
    }
    tokens
}

impl Client {
    pub fn connect(address: SocketAddr) -> Self {
        let stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        Self {
            stream,
            packet_size: 4096,
        }
    }

    pub fn send(&mut self, kind: u8, body: &[u8]) {
        let payload = self.packet_size - 8;
        let mut chunks: Vec<&[u8]> = body.chunks(payload).collect();
        if chunks.is_empty() {
            chunks.push(&[]);
        }
        let last = chunks.len() - 1;
        for (index, chunk) in chunks.iter().enumerate() {
            let mut packet = vec![kind, u8::from(index == last)];
            packet.extend_from_slice(&((chunk.len() + 8) as u16).to_be_bytes());
            packet.extend_from_slice(&[0, 0, (index + 1) as u8, 0]);
            packet.extend_from_slice(chunk);
            self.stream.write_all(&packet).unwrap();
        }
    }

    pub fn send_raw(&mut self, bytes: &[u8]) {
        let _ = self.stream.write_all(bytes);
    }

    pub fn read_message(&mut self) -> Option<(u8, Vec<u8>)> {
        let mut body = Vec::new();
        loop {
            let mut header = [0u8; 8];
            self.stream.read_exact(&mut header).ok()?;
            let length = usize::from(u16::from_be_bytes([header[2], header[3]]));
            let mut payload = vec![0u8; length.checked_sub(8)?];
            self.stream.read_exact(&mut payload).ok()?;
            body.extend_from_slice(&payload);
            if header[1] & 1 != 0 {
                return Some((header[0], body));
            }
        }
    }

    pub fn is_closed(&mut self) -> bool {
        let mut buffer = [0u8; 4096];
        loop {
            match self.stream.read(&mut buffer) {
                Ok(0) => return true,
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return false;
                }
                Err(_) => return true,
            }
        }
    }

    pub fn prelogin(&mut self, encryption: u8) -> Option<Vec<u8>> {
        self.send(0x12, &prelogin_body(encryption));
        self.read_message().map(|(_, body)| body)
    }

    pub fn login_with(address: SocketAddr, options: &LoginOptions<'_>) -> (Self, Vec<Token>) {
        let mut client = Self::connect(address);
        client.prelogin(0x02).expect("a PRELOGIN answer");
        client.send(0x10, &login7_body(options));
        let tokens = client
            .read_message()
            .map(|(_, body)| parse_tokens(&body))
            .unwrap_or_default();
        if let Some(size) = tokens.iter().find_map(|token| match token {
            Token::EnvChange { kind: 4, new, .. } => from_utf16(new).parse().ok(),
            _ => None,
        }) {
            client.packet_size = size;
        }
        (client, tokens)
    }

    pub fn login(address: SocketAddr) -> Self {
        let (client, tokens) = Self::login_with(address, &LoginOptions::default());
        assert!(
            tokens
                .iter()
                .any(|token| matches!(token, Token::LoginAck { .. })),
            "{tokens:?}"
        );
        client
    }

    pub fn batch(&mut self, text: &str) -> Vec<Token> {
        self.send(0x01, &batch_body(text));
        let (_, body) = self.read_message().expect("a batch reply");
        parse_tokens(&body)
    }
}

pub fn rows(tokens: &[Token]) -> Vec<Vec<Option<String>>> {
    tokens
        .iter()
        .filter_map(|token| match token {
            Token::Row(row) => Some(row.clone()),
            _ => None,
        })
        .collect()
}

pub fn errors(tokens: &[Token]) -> Vec<(u32, u8, String)> {
    tokens
        .iter()
        .filter_map(|token| match token {
            Token::Error {
                number,
                severity,
                message,
                ..
            } => Some((*number, *severity, message.clone())),
            _ => None,
        })
        .collect()
}

pub fn dones(tokens: &[Token]) -> Vec<(u16, u64)> {
    tokens
        .iter()
        .filter_map(|token| match token {
            Token::Done { status, rows, .. } => Some((*status, *rows)),
            _ => None,
        })
        .collect()
}

pub fn text(value: &[u8]) -> String {
    from_utf16(value)
}
