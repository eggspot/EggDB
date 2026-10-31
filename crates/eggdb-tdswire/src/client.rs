//! A client for SQL Server's TDS 7.4 ([MS-TDS]), so EggDB can read a running SQL Server
//! database: PRELOGIN with encryption as the URL asks (TLS inside PRELOGIN packets, or TDS 8
//! strict), a SQL login, and SQL batches whose results come back as text. Every column of a
//! result must be `nvarchar`: callers cast their columns to `nvarchar(max)` in the query, so
//! this client never needs SQL Server's other type encodings.

use crate::messages::{ENCRYPT_NOT_SUP, ENCRYPT_ON, ENCRYPT_REQ, LOGIN7, PRELOGIN, SQL_BATCH, TDS_7_4};
use crate::tls_transport::TdsTransport;
use crate::wire::{DEFAULT_PACKET_SIZE, HEADER_BYTES};
use eggdb_tls::verify::{ServerName, TrustStore};
use eggdb_tls::{ClientConfig, ServerVerification, TlsStream};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::time::Duration;

const DEFAULT_PORT: u16 = 1433;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const QUERY_TIMEOUT: Duration = Duration::from_secs(300);
const END_OF_MESSAGE: u8 = 0x01;
const MAX_MESSAGE_BYTES: usize = 1 << 30;
const TOKEN_COLMETADATA: u8 = 0x81;
const TOKEN_ROW: u8 = 0xD1;
const TOKEN_NBCROW: u8 = 0xD2;
const TOKEN_DONE: u8 = 0xFD;
const TOKEN_DONEPROC: u8 = 0xFE;
const TOKEN_DONEINPROC: u8 = 0xFF;
const TOKEN_ERROR: u8 = 0xAA;
const TOKEN_RETURNSTATUS: u8 = 0x79;
const TYPE_NVARCHAR: u8 = 0xE7;
const PLP_NULL: u64 = u64::MAX;
const TDS_8_ALPN: &[u8] = b"tds/8.0";

/// How the client encrypts, in the words of SqlClient's `Encrypt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encrypt {
    /// No encryption (login-only when the server insists on nothing more).
    No,
    /// Full encryption negotiated in PRELOGIN.
    Yes,
    /// TDS 8: TLS before anything else.
    Strict,
}

/// Where and how to connect, read from a `sqlserver://` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TdsConnectOptions {
    /// The host.
    pub host: String,
    /// The port.
    pub port: u16,
    /// The SQL login.
    pub user: String,
    /// The database.
    pub database: String,
    /// The password, from outside the URL.
    pub password: Option<String>,
    /// The encryption mode.
    pub encrypt: Encrypt,
    /// Whether the server's certificate is accepted without checking it.
    pub trust_server_certificate: bool,
    /// The trusted roots when it is checked.
    pub root_certificate: Option<PathBuf>,
}

impl TdsConnectOptions {
    /// The URL without a password, for messages.
    pub fn shown(&self) -> String {
        format!("sqlserver://{}@{}:{}/{}", self.user, self.host, self.port, self.database)
    }
}

/// Reads `sqlserver://user@host[:port]/database[?encrypt=yes|strict|no&trustServerCertificate=true&sslrootcert=...]`.
pub fn parse_sql_server_url(url: &str) -> Result<TdsConnectOptions, String> {
    let rest = url
        .strip_prefix("sqlserver://")
        .ok_or_else(|| "the source must start with sqlserver://".to_owned())?;
    let (main, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, database) = main.split_once('/').unwrap_or((main, ""));
    let (user, host_part) = authority
        .rsplit_once('@')
        .ok_or_else(|| "the URL names no user: write sqlserver://user@host/database".to_owned())?;
    if user.contains(':') {
        return Err("the password is in the URL, where every user's ps shows it; set SQLCMDPASSWORD or pass --password-file".to_owned());
    }
    if user.is_empty() {
        return Err("the URL names no user: write sqlserver://user@host/database".to_owned());
    }
    let (host, port) = match host_part.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().map_err(|_| format!("'{port}' is not a port number"))?),
        None => (host_part, DEFAULT_PORT),
    };
    if host.is_empty() {
        return Err("the URL names no host".to_owned());
    }
    let mut options = TdsConnectOptions {
        host: host.to_owned(),
        port,
        user: user.to_owned(),
        database: if database.is_empty() { "master".to_owned() } else { database.to_owned() },
        password: None,
        encrypt: Encrypt::Yes,
        trust_server_certificate: false,
        root_certificate: None,
    };
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        match name {
            "encrypt" => {
                options.encrypt = match value.to_ascii_lowercase().as_str() {
                    "yes" | "true" | "mandatory" => Encrypt::Yes,
                    "strict" => Encrypt::Strict,
                    "no" | "false" | "optional" => Encrypt::No,
                    _ => return Err(format!("encrypt must be yes, strict or no, not '{value}'")),
                };
            }
            "trustServerCertificate" => options.trust_server_certificate = matches!(value, "true" | "yes"),
            "sslrootcert" => options.root_certificate = Some(PathBuf::from(value)),
            "password" => {
                return Err("the password is in the URL, where every user's ps shows it; set SQLCMDPASSWORD or pass --password-file".to_owned());
            }
            _ => return Err(format!("the URL parameter '{name}' is not supported")),
        }
    }
    if options.encrypt != Encrypt::No && !options.trust_server_certificate && options.root_certificate.is_none() {
        return Err(
            "an encrypted connection needs the certificate of the authority to trust (sslrootcert=...) or trustServerCertificate=true".to_owned(),
        );
    }
    Ok(options)
}

/// Why talking to SQL Server failed.
#[derive(Debug)]
pub enum TdsClientError {
    /// The connection could not be made or broke.
    Io(io::Error),
    /// Encryption failed.
    Tls(String),
    /// The login was refused.
    Login(String),
    /// The server reported an error.
    Server {
        /// The error number.
        number: u32,
        /// The message.
        message: String,
    },
    /// The server sent something this client does not understand.
    Protocol(String),
}

impl std::fmt::Display for TdsClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Tls(message) | Self::Login(message) | Self::Protocol(message) => formatter.write_str(message),
            Self::Server { number, message } => write!(formatter, "{message} (error {number})"),
        }
    }
}

impl std::error::Error for TdsClientError {}

impl From<io::Error> for TdsClientError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

enum Transport {
    Plain(TcpStream),
    Tls(Box<TlsStream<TdsTransport>>),
}

impl Read for Transport {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(buffer),
            Self::Tls(stream) => stream.read(buffer),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.write(buffer),
            Self::Tls(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(stream) => stream.flush(),
            Self::Tls(stream) => stream.flush(),
        }
    }
}

/// The rows of one result set, each value as text or `None` for null.
pub type TextRows = Vec<Vec<Option<String>>>;

/// A connection to a SQL Server.
pub struct TdsClient {
    transport: Transport,
    next_packet_id: u8,
    encrypted: bool,
}

impl std::fmt::Debug for TdsClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("TdsClient").field("encrypted", &self.encrypted).finish_non_exhaustive()
    }
}

fn utf16(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

fn prelogin_body(encryption: u8) -> Vec<u8> {
    vec![
        0, 0, 21, 0, 6, 1, 0, 27, 0, 1, 2, 0, 28, 0, 1, 4, 0, 29, 0, 1, 0xFF, 17, 0, 0, 0, 0, 0,
        encryption, 0, 0,
    ]
}

fn prelogin_encryption(answer: &[u8]) -> Option<u8> {
    let mut at = 0;
    while *answer.get(at)? != 0xFF {
        let offset = usize::from(u16::from_be_bytes([*answer.get(at + 1)?, *answer.get(at + 2)?]));
        if answer[at] == 1 {
            return answer.get(offset).copied();
        }
        at += 5;
    }
    None
}

fn login7_body(options: &TdsConnectOptions, password: &str) -> Vec<u8> {
    let obfuscated: Vec<u8> = utf16(password).into_iter().map(|byte| byte.rotate_left(4) ^ 0xA5).collect();
    let names: [Vec<u8>; 9] = [
        utf16("eggdb"),
        utf16(&options.user),
        obfuscated,
        utf16("eggdb follow"),
        utf16(&options.host),
        Vec::new(),
        utf16("eggdb"),
        Vec::new(),
        utf16(&options.database),
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
    body.extend_from_slice(&TDS_7_4.to_le_bytes());
    body.extend_from_slice(&(DEFAULT_PACKET_SIZE as u32).to_le_bytes());
    body.extend_from_slice(&[0; 12]);
    body.extend_from_slice(&[0xE0, 0x03, 0, 0]);
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

fn batch_body(text: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&22u32.to_le_bytes());
    body.extend_from_slice(&18u32.to_le_bytes());
    body.extend_from_slice(&2u16.to_le_bytes());
    body.extend_from_slice(&0u64.to_le_bytes());
    body.extend_from_slice(&1u32.to_le_bytes());
    body.extend(utf16(text));
    body
}

fn tls_config(options: &TdsConnectOptions, strict: bool) -> Result<ClientConfig, TdsClientError> {
    let verification = if options.trust_server_certificate {
        ServerVerification::EncryptionOnly
    } else {
        let path = options
            .root_certificate
            .as_ref()
            .ok_or_else(|| TdsClientError::Tls("sslrootcert is needed to check the certificate".to_owned()))?;
        let text = std::fs::read_to_string(path)
            .map_err(|error| TdsClientError::Tls(format!("cannot read sslrootcert {}: {error}", path.display())))?;
        let trust = TrustStore::from_pem(&text).map_err(|error| TdsClientError::Tls(error.to_string()))?;
        let server_name = ServerName::parse(&options.host).map_err(|error| TdsClientError::Tls(error.to_string()))?;
        ServerVerification::Full { trust, server_name }
    };
    let mut config = ClientConfig::new(verification, &options.host);
    if strict {
        config.application_protocols = vec![TDS_8_ALPN.to_vec()];
    }
    Ok(config)
}

impl TdsClient {
    /// Connects, encrypts as `options.encrypt` asks, and logs in.
    pub fn connect(options: &TdsConnectOptions) -> Result<Self, TdsClientError> {
        let password = options
            .password
            .as_deref()
            .ok_or_else(|| TdsClientError::Login("a password is needed and none was given (SQLCMDPASSWORD is unset)".to_owned()))?;
        let address = (options.host.as_str(), options.port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| TdsClientError::Io(io::Error::new(io::ErrorKind::NotFound, "the host has no address")))?;
        let tcp = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)?;
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(QUERY_TIMEOUT))?;
        let mut client = if options.encrypt == Encrypt::Strict {
            let tls = TlsStream::connect(TdsTransport::new(tcp, Vec::new(), false), &tls_config(options, true)?)
                .map_err(|error| TdsClientError::Tls(error.to_string()))?;
            let mut client = Self { transport: Transport::Tls(Box::new(tls)), next_packet_id: 1, encrypted: true };
            client.send_message(PRELOGIN, &prelogin_body(ENCRYPT_ON))?;
            client.read_message()?;
            client
        } else {
            let mut client = Self { transport: Transport::Plain(tcp), next_packet_id: 1, encrypted: false };
            let asked = if options.encrypt == Encrypt::Yes { ENCRYPT_ON } else { ENCRYPT_NOT_SUP };
            client.send_message(PRELOGIN, &prelogin_body(asked))?;
            let answer = client.read_message()?;
            let answered = prelogin_encryption(&answer)
                .ok_or_else(|| TdsClientError::Protocol("the PRELOGIN answer has no encryption option".to_owned()))?;
            let encrypt = match (options.encrypt, answered) {
                (Encrypt::Yes, ENCRYPT_NOT_SUP) => {
                    return Err(TdsClientError::Tls("the server does not support encryption and the URL says encrypt=yes".to_owned()));
                }
                (Encrypt::Yes, _) | (_, ENCRYPT_ON | ENCRYPT_REQ) => true,
                _ => false,
            };
            if encrypt {
                let Transport::Plain(tcp) = client.transport else {
                    return Err(TdsClientError::Protocol("encryption started twice".to_owned()));
                };
                let mut tls = TlsStream::connect(TdsTransport::new(tcp, Vec::new(), true), &tls_config(options, false)?)
                    .map_err(|error| TdsClientError::Tls(error.to_string()))?;
                tls.get_mut().framed = false;
                client = Self { transport: Transport::Tls(Box::new(tls)), next_packet_id: client.next_packet_id, encrypted: true };
            }
            client
        };
        client.send_message(LOGIN7, &login7_body(options, password))?;
        let answer = client.read_message()?;
        if let Some((number, message)) = first_error(&answer) {
            return Err(if number == 18456 {
                TdsClientError::Login(message)
            } else {
                TdsClientError::Server { number, message }
            });
        }
        Ok(client)
    }

    /// Whether the connection is encrypted.
    pub fn encrypted(&self) -> bool {
        self.encrypted
    }

    fn send_message(&mut self, kind: u8, body: &[u8]) -> Result<(), TdsClientError> {
        let payload_bytes = DEFAULT_PACKET_SIZE - HEADER_BYTES;
        let chunks: Vec<&[u8]> = if body.is_empty() { vec![&[]] } else { body.chunks(payload_bytes).collect() };
        let mut bytes = Vec::with_capacity(body.len() + chunks.len() * HEADER_BYTES);
        for (index, chunk) in chunks.iter().enumerate() {
            bytes.push(kind);
            bytes.push(if index + 1 == chunks.len() { END_OF_MESSAGE } else { 0 });
            bytes.extend_from_slice(&((chunk.len() + HEADER_BYTES) as u16).to_be_bytes());
            bytes.extend_from_slice(&[0, 0, self.next_packet_id, 0]);
            self.next_packet_id = self.next_packet_id.wrapping_add(1);
            bytes.extend_from_slice(chunk);
        }
        self.transport.write_all(&bytes)?;
        self.transport.flush()?;
        Ok(())
    }

    fn read_message(&mut self) -> Result<Vec<u8>, TdsClientError> {
        let mut body = Vec::new();
        loop {
            let mut header = [0u8; HEADER_BYTES];
            self.transport.read_exact(&mut header)?;
            let length = usize::from(u16::from_be_bytes([header[2], header[3]]));
            if length < HEADER_BYTES || body.len() + length > MAX_MESSAGE_BYTES {
                return Err(TdsClientError::Protocol("a packet with an impossible length".to_owned()));
            }
            let start = body.len();
            body.resize(start + length - HEADER_BYTES, 0);
            self.transport.read_exact(&mut body[start..])?;
            if header[1] & END_OF_MESSAGE != 0 {
                return Ok(body);
            }
        }
    }

    /// Runs a batch and returns each result set; the batch's first error fails it.
    pub fn batch(&mut self, text: &str) -> Result<Vec<TextRows>, TdsClientError> {
        self.send_message(SQL_BATCH, &batch_body(text))?;
        let answer = self.read_message()?;
        if let Some((number, message)) = first_error(&answer) {
            return Err(TdsClientError::Server { number, message });
        }
        parse_results(&answer).map_err(TdsClientError::Protocol)
    }
}

struct TokenReader<'a> {
    bytes: &'a [u8],
}

impl<'a> TokenReader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        let (taken, rest) = self.bytes.split_at_checked(count).ok_or_else(|| "the token stream ends early".to_owned())?;
        self.bytes = rest;
        Ok(taken)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, String> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let bytes = self.take(8)?;
        let mut array = [0u8; 8];
        array.copy_from_slice(bytes);
        Ok(u64::from_le_bytes(array))
    }

    fn utf16(&mut self, characters: usize) -> Result<String, String> {
        let bytes = self.take(characters * 2)?;
        let units: Vec<u16> = bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
        String::from_utf16(&units).map_err(|_| "a string is not UTF-16".to_owned())
    }
}

fn first_error(stream: &[u8]) -> Option<(u32, String)> {
    let mut reader = TokenReader { bytes: stream };
    while let Ok(token) = reader.u8() {
        match token {
            TOKEN_ERROR => {
                let length = usize::from(reader.u16().ok()?);
                let body = reader.take(length).ok()?;
                let mut fields = TokenReader { bytes: body };
                let number = fields.u32().ok()?;
                fields.take(2).ok()?;
                let message_length = usize::from(fields.u16().ok()?);
                let message = fields.utf16(message_length).ok()?;
                return Some((number, message));
            }
            TOKEN_DONE | TOKEN_DONEPROC | TOKEN_DONEINPROC => {
                reader.take(12).ok()?;
            }
            TOKEN_RETURNSTATUS => {
                reader.take(4).ok()?;
            }
            TOKEN_COLMETADATA | TOKEN_ROW | TOKEN_NBCROW => return None,
            _ => {
                let length = usize::from(reader.u16().ok()?);
                reader.take(length).ok()?;
            }
        }
    }
    None
}

fn read_value(reader: &mut TokenReader<'_>, plp: bool) -> Result<Option<String>, String> {
    if plp {
        let total = reader.u64()?;
        if total == PLP_NULL {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        loop {
            let chunk = reader.u32()? as usize;
            if chunk == 0 {
                break;
            }
            bytes.extend_from_slice(reader.take(chunk)?);
        }
        let units: Vec<u16> = bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
        return String::from_utf16(&units).map(Some).map_err(|_| "a value is not UTF-16".to_owned());
    }
    let length = reader.u16()?;
    if length == 0xFFFF {
        return Ok(None);
    }
    reader.utf16(usize::from(length) / 2).map(Some)
}

/// Every result set of a batch's answer, each value as text.
pub fn parse_results(stream: &[u8]) -> Result<Vec<TextRows>, String> {
    let mut reader = TokenReader { bytes: stream };
    let mut results = Vec::new();
    let mut columns: Vec<bool> = Vec::new();
    while !reader.bytes.is_empty() {
        match reader.u8()? {
            TOKEN_COLMETADATA => {
                let count = reader.u16()?;
                columns.clear();
                if count == 0xFFFF {
                    continue;
                }
                for _ in 0..count {
                    reader.u32()?;
                    reader.u16()?;
                    let kind = reader.u8()?;
                    if kind != TYPE_NVARCHAR {
                        return Err(format!("a result column has type 0x{kind:02X}; cast it to nvarchar(max)"));
                    }
                    let maximum = reader.u16()?;
                    reader.take(5)?;
                    let name_length = usize::from(reader.u8()?);
                    reader.utf16(name_length)?;
                    columns.push(maximum == 0xFFFF);
                }
                results.push(Vec::new());
            }
            TOKEN_ROW => {
                let mut row = Vec::with_capacity(columns.len());
                for plp in &columns {
                    row.push(read_value(&mut reader, *plp)?);
                }
                results.last_mut().ok_or("a row before its columns")?.push(row);
            }
            TOKEN_NBCROW => {
                let bitmap = reader.take(columns.len().div_ceil(8))?.to_vec();
                let mut row = Vec::with_capacity(columns.len());
                for (index, plp) in columns.iter().enumerate() {
                    if bitmap[index / 8] & (1 << (index % 8)) != 0 {
                        row.push(None);
                    } else {
                        row.push(read_value(&mut reader, *plp)?);
                    }
                }
                results.last_mut().ok_or("a row before its columns")?.push(row);
            }
            TOKEN_DONE | TOKEN_DONEPROC | TOKEN_DONEINPROC => {
                reader.take(12)?;
            }
            TOKEN_RETURNSTATUS => {
                reader.take(4)?;
            }
            _ => {
                let length = usize::from(reader.u16()?);
                reader.take(length)?;
            }
        }
    }
    Ok(results)
}

/// `name` quoted as a Transact-SQL identifier.
pub fn quote_name(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

/// `text` as a Transact-SQL Unicode string literal.
pub fn quote_unicode(text: &str) -> String {
    format!("N'{}'", text.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_read_their_encryption_and_refuse_passwords() {
        let options = parse_sql_server_url("sqlserver://sa@sql.example.com/Shop?encrypt=strict&sslrootcert=ca.pem").unwrap();
        assert_eq!((options.host.as_str(), options.port, options.database.as_str()), ("sql.example.com", 1433, "Shop"));
        assert_eq!(options.encrypt, Encrypt::Strict);
        let plain = parse_sql_server_url("sqlserver://sa@127.0.0.1:1434/Shop?encrypt=no").unwrap();
        assert_eq!((plain.port, plain.encrypt), (1434, Encrypt::No));
        assert!(parse_sql_server_url("sqlserver://sa:pw@host/db").unwrap_err().contains("SQLCMDPASSWORD"));
        assert!(parse_sql_server_url("sqlserver://sa@host/db").unwrap_err().contains("sslrootcert"));
        assert!(parse_sql_server_url("sqlserver://sa@host/db?trustServerCertificate=true").is_ok());
    }

    fn nvarchar_column(name: &str, plp: bool) -> Vec<u8> {
        let mut bytes = 0u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.push(TYPE_NVARCHAR);
        bytes.extend_from_slice(&(if plp { 0xFFFFu16 } else { 100 }).to_le_bytes());
        bytes.extend_from_slice(&[9, 4, 0xD0, 0, 0x34]);
        bytes.push(name.encode_utf16().count() as u8);
        bytes.extend(utf16(name));
        bytes
    }

    #[test]
    fn results_of_nvarchar_columns_decode_with_nulls_and_plp() {
        let mut stream = vec![TOKEN_COLMETADATA];
        stream.extend_from_slice(&2u16.to_le_bytes());
        stream.extend(nvarchar_column("a", false));
        stream.extend(nvarchar_column("b", true));
        stream.push(TOKEN_ROW);
        stream.extend_from_slice(&4u16.to_le_bytes());
        stream.extend(utf16("hi"));
        stream.extend_from_slice(&4u64.to_le_bytes());
        stream.extend_from_slice(&4u32.to_le_bytes());
        stream.extend(utf16("yo"));
        stream.extend_from_slice(&0u32.to_le_bytes());
        stream.push(TOKEN_NBCROW);
        stream.push(0b10);
        stream.extend_from_slice(&2u16.to_le_bytes());
        stream.extend(utf16("x"));
        stream.push(TOKEN_DONE);
        stream.extend_from_slice(&[0; 12]);
        let results = parse_results(&stream).unwrap();
        assert_eq!(
            results,
            vec![vec![
                vec![Some("hi".to_owned()), Some("yo".to_owned())],
                vec![Some("x".to_owned()), None]
            ]]
        );
        for cut in 1..stream.len() - 13 {
            let _ = parse_results(&stream[..cut]);
        }
    }

    #[test]
    fn literals_and_names_are_quoted_for_transact_sql() {
        assert_eq!(quote_name("a]b"), "[a]]b]");
        assert_eq!(quote_unicode("it's"), "N'it''s'");
    }
}
