//! A MySQL client for the client/server protocol, written from MySQL's public protocol
//! documentation like the server (D1): it signs in with `mysql_native_password` or the fast
//! path of `caching_sha2_password` (following an authentication switch), runs `COM_QUERY` and
//! reads text result sets, several per query. TLS, and `caching_sha2_password`'s full
//! authentication (which needs TLS or the server's RSA key), are not supported yet.

use crate::auth::{CACHING_SHA2_PASSWORD, MYSQL_NATIVE_PASSWORD, NONCE_BYTES, caching_sha2_response, native_response};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// MySQL's port when a URL names none.
pub const DEFAULT_PORT: u16 = 3306;
const LONG_PACKET: usize = 0xFF_FFFF;
const MOST_RESULT_BYTES: usize = 1 << 30;
const MOST_COLUMNS: u64 = 4096;
const CLIENT_LONG_PASSWORD: u32 = 0x1;
const CLIENT_CONNECT_WITH_DB: u32 = 0x8;
const CLIENT_PROTOCOL_41: u32 = 0x200;
const CLIENT_TRANSACTIONS: u32 = 0x2000;
const CLIENT_SECURE_CONNECTION: u32 = 0x8000;
const CLIENT_MULTI_RESULTS: u32 = 0x2_0000;
const CLIENT_PLUGIN_AUTH: u32 = 0x8_0000;
const SERVER_MORE_RESULTS_EXISTS: u16 = 0x8;
const UTF8MB4_GENERAL_CI: u8 = 45;
const COM_QUERY: u8 = 0x03;
const COM_BINLOG_DUMP: u8 = 0x12;

/// Where and how to connect, read from a `mysql://` URL. Its `Debug` output shows whether a
/// password is set, never the password.
#[derive(Clone, PartialEq, Eq)]
pub struct MySqlConnectOptions {
    /// The host.
    pub host: String,
    /// The port.
    pub port: u16,
    /// The user.
    pub user: String,
    /// The database, empty for none.
    pub database: String,
    /// The password, from outside the URL.
    pub password: Option<String>,
}

impl std::fmt::Debug for MySqlConnectOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MySqlConnectOptions")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("database", &self.database)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .finish()
    }
}

impl MySqlConnectOptions {
    /// The URL without a password, for messages.
    pub fn shown(&self) -> String {
        format!("mysql://{}@{}:{}/{}", self.user, self.host, self.port, self.database)
    }
}

/// Reads `mysql://user@host[:port][/database]`; a password in the URL is refused, as every
/// user's process list would show it.
pub fn parse_mysql_url(url: &str) -> Result<MySqlConnectOptions, String> {
    let rest = url
        .strip_prefix("mysql://")
        .ok_or_else(|| "the source must start with mysql://".to_owned())?;
    let (main, _) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, database) = main.split_once('/').unwrap_or((main, ""));
    let (user, host_part) = authority
        .rsplit_once('@')
        .ok_or_else(|| "the URL names no user: write mysql://user@host/database".to_owned())?;
    if user.contains(':') {
        return Err("the password is in the URL, where every user's ps shows it; set MYSQL_PWD or use a password file".to_owned());
    }
    if user.is_empty() {
        return Err("the URL names no user: write mysql://user@host/database".to_owned());
    }
    let (host, port) = match host_part.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().map_err(|_| format!("'{port}' is not a port number"))?),
        None => (host_part, DEFAULT_PORT),
    };
    if host.is_empty() {
        return Err("the URL names no host".to_owned());
    }
    Ok(MySqlConnectOptions {
        host: host.to_owned(),
        port,
        user: user.to_owned(),
        database: database.to_owned(),
        password: None,
    })
}

/// One result of a query: its column names and its rows as text (`None` for `NULL`); a
/// statement that returns no rows gives no columns.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TextResult {
    /// The column names.
    pub columns: Vec<String>,
    /// The rows.
    pub rows: Vec<Vec<Option<String>>>,
}

/// Why a client call failed.
#[derive(Debug)]
pub enum MySqlClientError {
    /// The connection failed or timed out.
    Io(io::Error),
    /// The server answered with an error.
    Server {
        /// MySQL's error number.
        code: u16,
        /// The SQLSTATE.
        state: String,
        /// The message.
        message: String,
    },
    /// The server's bytes did not follow the protocol, or used what this client does not support.
    Protocol(String),
}

impl std::fmt::Display for MySqlClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "MySQL connection: {error}"),
            Self::Server { code, state, message } => write!(f, "MySQL error {code} ({state}): {message}"),
            Self::Protocol(problem) => write!(f, "MySQL protocol: {problem}"),
        }
    }
}

impl std::error::Error for MySqlClientError {}

impl From<io::Error> for MySqlClientError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn protocol(problem: &str) -> MySqlClientError {
    MySqlClientError::Protocol(problem.to_owned())
}

struct Cursor<'p> {
    bytes: &'p [u8],
    at: usize,
}

impl<'p> Cursor<'p> {
    fn take(&mut self, count: usize) -> Result<&'p [u8], MySqlClientError> {
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len()).ok_or_else(|| protocol("a packet is cut short"))?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn byte(&mut self) -> Result<u8, MySqlClientError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, MySqlClientError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn null_terminated(&mut self) -> Result<&'p [u8], MySqlClientError> {
        let rest = &self.bytes[self.at.min(self.bytes.len())..];
        let length = rest.iter().position(|byte| *byte == 0).ok_or_else(|| protocol("a string has no end"))?;
        let text = self.take(length)?;
        self.at += 1;
        Ok(text)
    }

    fn length_encoded(&mut self) -> Result<u64, MySqlClientError> {
        let first = self.byte()?;
        let width = match first {
            0..=0xFA => return Ok(u64::from(first)),
            0xFC => 2,
            0xFD => 3,
            0xFE => 8,
            _ => return Err(protocol("an integer has an invalid first byte")),
        };
        let bytes = self.take(width)?;
        let mut value = [0u8; 8];
        value[..width].copy_from_slice(bytes);
        Ok(u64::from_le_bytes(value))
    }

    fn length_encoded_bytes(&mut self) -> Result<&'p [u8], MySqlClientError> {
        let length = usize::try_from(self.length_encoded()?).map_err(|_| protocol("a string is too long"))?;
        self.take(length)
    }

    fn rest(&self) -> &'p [u8] {
        &self.bytes[self.at.min(self.bytes.len())..]
    }
}

fn server_error(packet: &[u8]) -> MySqlClientError {
    let mut cursor = Cursor { bytes: packet, at: 1 };
    let code = cursor.u16().unwrap_or(0);
    let state = if cursor.rest().first() == Some(&b'#') {
        cursor.take(6).map(|marked| String::from_utf8_lossy(&marked[1..]).into_owned()).unwrap_or_default()
    } else {
        "HY000".to_owned()
    };
    MySqlClientError::Server {
        code,
        state,
        message: String::from_utf8_lossy(cursor.rest()).into_owned(),
    }
}

fn auth_response(plugin: &[u8], password: &[u8], nonce: &[u8]) -> Result<Vec<u8>, MySqlClientError> {
    let nonce: [u8; NONCE_BYTES] = nonce
        .get(..NONCE_BYTES)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| protocol("the server's nonce is short"))?;
    match plugin {
        name if name == MYSQL_NATIVE_PASSWORD.as_bytes() => Ok(native_response(password, &nonce)),
        name if name == CACHING_SHA2_PASSWORD.as_bytes() => Ok(caching_sha2_response(password, &nonce)),
        other => Err(MySqlClientError::Protocol(format!(
            "the server asks for the authentication plugin {}, which this client does not support",
            String::from_utf8_lossy(other)
        ))),
    }
}

/// A signed-in connection to a MySQL server.
pub struct MySqlClient {
    stream: TcpStream,
    sequence: u8,
}

impl std::fmt::Debug for MySqlClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MySqlClient").finish_non_exhaustive()
    }
}

impl MySqlClient {
    /// Connects to `options`' server and signs in, waiting at most `timeout` for each step.
    pub fn connect(options: &MySqlConnectOptions, timeout: Duration) -> Result<Self, MySqlClientError> {
        let address = (options.host.trim_start_matches('[').trim_end_matches(']'), options.port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the host has no address"))?;
        let stream = TcpStream::connect_timeout(&address, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let mut client = Self { stream, sequence: 0 };
        client.sign_in(options)?;
        Ok(client)
    }

    fn read_packet(&mut self) -> Result<Vec<u8>, MySqlClientError> {
        let mut payload = Vec::new();
        loop {
            let mut header = [0u8; 4];
            self.stream.read_exact(&mut header)?;
            let length = usize::from(header[0]) | (usize::from(header[1]) << 8) | (usize::from(header[2]) << 16);
            self.sequence = header[3].wrapping_add(1);
            if payload.len() + length > MOST_RESULT_BYTES {
                return Err(protocol("a packet is larger than 1 GiB"));
            }
            let start = payload.len();
            payload.resize(start + length, 0);
            self.stream.read_exact(&mut payload[start..])?;
            if length < LONG_PACKET {
                return Ok(payload);
            }
        }
    }

    fn write_packet(&mut self, payload: &[u8]) -> Result<(), MySqlClientError> {
        let mut chunks = payload.chunks(LONG_PACKET).peekable();
        let mut wrote_any = false;
        while let Some(chunk) = chunks.next() {
            let length = chunk.len();
            let header = [length as u8, (length >> 8) as u8, (length >> 16) as u8, self.sequence];
            self.sequence = self.sequence.wrapping_add(1);
            self.stream.write_all(&header)?;
            self.stream.write_all(chunk)?;
            wrote_any = true;
            if chunks.peek().is_none() && length == LONG_PACKET {
                let empty = [0, 0, 0, self.sequence];
                self.sequence = self.sequence.wrapping_add(1);
                self.stream.write_all(&empty)?;
            }
        }
        if !wrote_any {
            let empty = [0, 0, 0, self.sequence];
            self.sequence = self.sequence.wrapping_add(1);
            self.stream.write_all(&empty)?;
        }
        self.stream.flush()?;
        Ok(())
    }

    fn sign_in(&mut self, options: &MySqlConnectOptions) -> Result<(), MySqlClientError> {
        let greeting = self.read_packet()?;
        if greeting.first() == Some(&0xFF) {
            return Err(server_error(&greeting));
        }
        let mut cursor = Cursor { bytes: &greeting, at: 0 };
        if cursor.byte()? != 10 {
            return Err(protocol("the server speaks a protocol version other than 10"));
        }
        cursor.null_terminated()?;
        cursor.take(4)?;
        let mut nonce = cursor.take(8)?.to_vec();
        cursor.take(1)?;
        let lower = u32::from(cursor.u16()?);
        cursor.byte()?;
        cursor.u16()?;
        let upper = u32::from(cursor.u16()?);
        let capabilities = lower | (upper << 16);
        if capabilities & CLIENT_PROTOCOL_41 == 0 {
            return Err(protocol("the server does not speak the 4.1 protocol"));
        }
        let data_length = usize::from(cursor.byte()?);
        cursor.take(10)?;
        let second = data_length.saturating_sub(8).max(13);
        nonce.extend_from_slice(cursor.take(second)?);
        let plugin = if capabilities & CLIENT_PLUGIN_AUTH != 0 {
            cursor.null_terminated().unwrap_or(MYSQL_NATIVE_PASSWORD.as_bytes()).to_vec()
        } else {
            MYSQL_NATIVE_PASSWORD.as_bytes().to_vec()
        };
        let password = options.password.as_deref().unwrap_or_default().as_bytes();
        let mut flags = CLIENT_LONG_PASSWORD
            | CLIENT_PROTOCOL_41
            | CLIENT_TRANSACTIONS
            | CLIENT_SECURE_CONNECTION
            | CLIENT_MULTI_RESULTS
            | CLIENT_PLUGIN_AUTH;
        if !options.database.is_empty() {
            flags |= CLIENT_CONNECT_WITH_DB;
        }
        let response = auth_response(&plugin, password, &nonce)?;
        let mut payload = flags.to_le_bytes().to_vec();
        payload.extend_from_slice(&(LONG_PACKET as u32).to_le_bytes());
        payload.push(UTF8MB4_GENERAL_CI);
        payload.extend_from_slice(&[0u8; 23]);
        payload.extend_from_slice(options.user.as_bytes());
        payload.push(0);
        payload.push(u8::try_from(response.len()).map_err(|_| protocol("the authentication response is too long"))?);
        payload.extend_from_slice(&response);
        if !options.database.is_empty() {
            payload.extend_from_slice(options.database.as_bytes());
            payload.push(0);
        }
        payload.extend_from_slice(&plugin);
        payload.push(0);
        self.write_packet(&payload)?;
        loop {
            let reply = self.read_packet()?;
            match reply.first() {
                Some(0x00) => return Ok(()),
                Some(0xFF) => return Err(server_error(&reply)),
                Some(0xFE) => {
                    let mut cursor = Cursor { bytes: &reply, at: 1 };
                    let plugin = cursor.null_terminated()?.to_vec();
                    let nonce = cursor.rest().to_vec();
                    let response = auth_response(&plugin, password, &nonce)?;
                    self.write_packet(&response)?;
                }
                Some(0x01) => match reply.get(1) {
                    Some(0x03) => {}
                    Some(0x04) => {
                        return Err(protocol(
                            "the server wants caching_sha2_password's full authentication, which needs TLS or its RSA key; sign in once with the mysql client to fill its cache, or use mysql_native_password",
                        ));
                    }
                    _ => return Err(protocol("the server sent authentication data this client does not understand")),
                },
                _ => return Err(protocol("the server answered the sign-in with an unknown packet")),
            }
        }
    }

    /// How long a read waits; `None` waits for ever.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<(), MySqlClientError> {
        Ok(self.stream.set_read_timeout(timeout)?)
    }

    /// Asks the server to stream its binary log from `file` at `position`, as a replica with
    /// `server_id`, with checksums kept and a heartbeat every `heartbeat` so a quiet log still
    /// sends something.
    pub fn start_binlog_dump(
        &mut self,
        server_id: u32,
        file: &str,
        position: u32,
        heartbeat: Duration,
    ) -> Result<(), MySqlClientError> {
        self.query("SET @master_binlog_checksum = @@global.binlog_checksum, @source_binlog_checksum = @@global.binlog_checksum")?;
        self.query(&format!("SET @master_heartbeat_period = {}", heartbeat.as_nanos()))?;
        self.sequence = 0;
        let mut payload = vec![COM_BINLOG_DUMP];
        payload.extend_from_slice(&position.to_le_bytes());
        payload.extend_from_slice(&0u16.to_le_bytes());
        payload.extend_from_slice(&server_id.to_le_bytes());
        payload.extend_from_slice(file.as_bytes());
        self.write_packet(&payload)
    }

    /// The next event of a binary log dump (without the packet's leading byte), or `None`
    /// when the server ends the dump.
    pub fn next_binlog_event(&mut self) -> Result<Option<Vec<u8>>, MySqlClientError> {
        let packet = self.read_packet()?;
        match packet.split_first() {
            Some((0x00, event)) => Ok(Some(event.to_vec())),
            Some((0xFE, _)) if packet.len() < 9 => Ok(None),
            Some((0xFF, _)) => Err(server_error(&packet)),
            _ => Err(protocol("the binary log dump sent an unknown packet")),
        }
    }

    /// Runs `sql` (one or more statements) and returns each statement's result, in order.
    pub fn query(&mut self, sql: &str) -> Result<Vec<TextResult>, MySqlClientError> {
        self.sequence = 0;
        let mut payload = vec![COM_QUERY];
        payload.extend_from_slice(sql.as_bytes());
        self.write_packet(&payload)?;
        let mut results = Vec::new();
        let mut total = 0usize;
        loop {
            let first = self.read_packet()?;
            match first.first() {
                Some(0x00) => {
                    let mut cursor = Cursor { bytes: &first, at: 1 };
                    cursor.length_encoded()?;
                    cursor.length_encoded()?;
                    let status = cursor.u16()?;
                    results.push(TextResult::default());
                    if status & SERVER_MORE_RESULTS_EXISTS == 0 {
                        return Ok(results);
                    }
                }
                Some(0xFF) => return Err(server_error(&first)),
                Some(0xFB) => return Err(protocol("the server asked for a local file, which this client never sends")),
                Some(_) => {
                    let count = Cursor { bytes: &first, at: 0 }.length_encoded()?;
                    if count == 0 || count > MOST_COLUMNS {
                        return Err(protocol("a result set has no columns or too many"));
                    }
                    let mut columns = Vec::new();
                    for _ in 0..count {
                        let definition = self.read_packet()?;
                        let mut cursor = Cursor { bytes: &definition, at: 0 };
                        for _ in 0..4 {
                            cursor.length_encoded_bytes()?;
                        }
                        columns.push(String::from_utf8_lossy(cursor.length_encoded_bytes()?).into_owned());
                    }
                    let end_of_columns = self.read_packet()?;
                    if end_of_columns.first() != Some(&0xFE) {
                        return Err(protocol("the column definitions do not end with EOF"));
                    }
                    let mut rows = Vec::new();
                    let more = loop {
                        let packet = self.read_packet()?;
                        total += packet.len();
                        if total > MOST_RESULT_BYTES {
                            return Err(protocol("the result is larger than 1 GiB"));
                        }
                        match packet.first() {
                            Some(0xFE) if packet.len() < 9 => {
                                let status = Cursor { bytes: &packet, at: 3 }.u16()?;
                                break status & SERVER_MORE_RESULTS_EXISTS != 0;
                            }
                            Some(0xFF) => return Err(server_error(&packet)),
                            _ => {
                                let mut cursor = Cursor { bytes: &packet, at: 0 };
                                let mut row = Vec::with_capacity(columns.len());
                                for _ in 0..columns.len() {
                                    if cursor.rest().first() == Some(&0xFB) {
                                        cursor.byte()?;
                                        row.push(None);
                                    } else {
                                        row.push(Some(String::from_utf8_lossy(cursor.length_encoded_bytes()?).into_owned()));
                                    }
                                }
                                rows.push(row);
                            }
                        }
                    };
                    results.push(TextResult { columns, rows });
                    if !more {
                        return Ok(results);
                    }
                }
                None => return Err(protocol("the server sent an empty packet")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_name_the_server_and_never_hold_a_password() {
        let options = parse_mysql_url("mysql://reader@db.internal:3307/shop").unwrap();
        assert_eq!((options.host.as_str(), options.port, options.user.as_str(), options.database.as_str()), ("db.internal", 3307, "reader", "shop"));
        assert_eq!(parse_mysql_url("mysql://reader@db").unwrap().port, DEFAULT_PORT);
        assert!(parse_mysql_url("mysql://reader:secret@db/shop").is_err());
        assert!(parse_mysql_url("postgres://reader@db/shop").is_err());
        assert!(!format!("{:?}", MySqlConnectOptions { password: Some("secret".to_owned()), ..options }).contains("secret"));
    }

    #[test]
    fn packets_read_lengths_and_errors_without_panics() {
        let mut cursor = Cursor { bytes: &[0xFC, 0x10, 0x01, 0xFA], at: 0 };
        assert_eq!(cursor.length_encoded().unwrap(), 0x0110);
        assert_eq!(cursor.length_encoded().unwrap(), 0xFA);
        assert!(Cursor { bytes: &[0xFE, 1], at: 0 }.length_encoded().is_err());
        let error = server_error(b"\xff\x15\x04#28000Access denied");
        assert!(matches!(error, MySqlClientError::Server { code: 1045, ref state, ref message } if state == "28000" && message == "Access denied"));
        assert!(auth_response(b"sha256_password", b"x", &[0; 20]).is_err());
        assert!(auth_response(b"mysql_native_password", b"x", &[0; 4]).is_err());
    }
}
