//! A client for PostgreSQL's wire protocol v3, so EggDB can read a running PostgreSQL database:
//! TCP with TLS as libpq's `sslmode` asks, SCRAM-SHA-256 (with `-PLUS` channel binding over TLS)
//! or a cleartext password over TLS, the simple query protocol, `COPY ... TO STDOUT`, and the
//! copy-both stream of logical replication.

use eggdb_crypto::{
    SHA256_DIGEST_BYTES, base64_decode, base64_encode, constant_time_eq, hmac_sha256,
    pbkdf2_hmac_sha256, sha256,
};
use eggdb_tls::verify::TrustStore;
use eggdb_tls::{ClientConfig, ServerVerification, TlsError, TlsStream};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::time::Duration;

const PROTOCOL_3_0: u32 = 196_608;
const SSL_REQUEST_CODE: u32 = 80_877_103;
const DEFAULT_PORT: u16 = 5432;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_MESSAGE_BYTES: usize = 1 << 30;
const CLIENT_NONCE_BYTES: usize = 18;

/// How the client uses TLS, as libpq's `sslmode` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SslMode {
    /// Never TLS.
    Disable,
    /// TLS when the server offers it, without checking the certificate (libpq's default).
    Prefer,
    /// TLS always, without checking the certificate.
    Require,
    /// TLS always, with a certificate chain to `sslrootcert`.
    VerifyCa,
    /// TLS always, with a certificate chain to `sslrootcert` that names the host.
    VerifyFull,
}

impl SslMode {
    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "disable" => Self::Disable,
            "allow" | "prefer" => Self::Prefer,
            "require" => Self::Require,
            "verify-ca" => Self::VerifyCa,
            "verify-full" => Self::VerifyFull,
            _ => return None,
        })
    }

    /// The name libpq uses.
    pub fn name(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::Prefer => "prefer",
            Self::Require => "require",
            Self::VerifyCa => "verify-ca",
            Self::VerifyFull => "verify-full",
        }
    }
}

/// Where and how to connect, read from a `postgres://` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectOptions {
    /// The host name or address.
    pub host: String,
    /// The port.
    pub port: u16,
    /// The user to log in as.
    pub user: String,
    /// The database.
    pub database: String,
    /// The password, from outside the URL.
    pub password: Option<String>,
    /// The TLS mode.
    pub ssl_mode: SslMode,
    /// The trusted roots for `verify-ca` and `verify-full`.
    pub ssl_root_cert: Option<PathBuf>,
    /// Whether to open a logical replication connection (`replication=database`).
    pub replication: bool,
}

impl ConnectOptions {
    /// The URL without a password, for messages.
    pub fn shown(&self) -> String {
        format!("postgres://{}@{}:{}/{}", self.user, self.host, self.port, self.database)
    }
}

fn percent_decoded(text: &str) -> Result<String, String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = text
                .get(index + 1..index + 3)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or_else(|| format!("'{text}' has a broken %-escape"))?;
            decoded.push(hex);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| format!("'{text}' is not UTF-8 once decoded"))
}

/// Reads `postgres://user@host[:port]/database[?sslmode=...&sslrootcert=...]`. A password in the
/// URL is refused: process listings show it.
pub fn parse_postgres_url(url: &str) -> Result<ConnectOptions, String> {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .ok_or_else(|| "the source must start with postgres:// or postgresql://".to_owned())?;
    let (main, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, database) = main.split_once('/').unwrap_or((main, ""));
    let (user_part, host_part) = authority
        .rsplit_once('@')
        .ok_or_else(|| "the URL names no user: write postgres://user@host/database".to_owned())?;
    if user_part.contains(':') {
        return Err(
            "the password is in the URL, where every user's ps shows it; set PGPASSWORD or pass --password-file"
                .to_owned(),
        );
    }
    let user = percent_decoded(user_part)?;
    if user.is_empty() {
        return Err("the URL names no user: write postgres://user@host/database".to_owned());
    }
    let (host, port) = if let Some(bracketed) = host_part.strip_prefix('[') {
        let (address, after) = bracketed
            .split_once(']')
            .ok_or_else(|| "the URL's IPv6 address has no closing ]".to_owned())?;
        let port = after.strip_prefix(':').unwrap_or("");
        (address.to_owned(), port)
    } else {
        match host_part.rsplit_once(':') {
            Some((host, port)) => (host.to_owned(), port),
            None => (host_part.to_owned(), ""),
        }
    };
    if host.is_empty() {
        return Err("the URL names no host".to_owned());
    }
    let port = if port.is_empty() {
        DEFAULT_PORT
    } else {
        port.parse()
            .map_err(|_| format!("'{port}' is not a port number"))?
    };
    let database = if database.is_empty() { user.clone() } else { percent_decoded(database)? };
    let mut options = ConnectOptions {
        host,
        port,
        user,
        database,
        password: None,
        ssl_mode: SslMode::Prefer,
        ssl_root_cert: None,
        replication: false,
    };
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decoded(value)?;
        match name {
            "sslmode" => {
                options.ssl_mode = SslMode::named(&value).ok_or_else(|| {
                    format!("sslmode must be disable, prefer, require, verify-ca or verify-full, not '{value}'")
                })?;
            }
            "sslrootcert" => options.ssl_root_cert = Some(PathBuf::from(value)),
            "password" => {
                return Err(
                    "the password is in the URL, where every user's ps shows it; set PGPASSWORD or pass --password-file"
                        .to_owned(),
                );
            }
            "application_name" | "connect_timeout" | "sslnegotiation" => {}
            _ => return Err(format!("the URL parameter '{name}' is not supported")),
        }
    }
    if matches!(options.ssl_mode, SslMode::VerifyCa | SslMode::VerifyFull) && options.ssl_root_cert.is_none() {
        return Err(format!(
            "sslmode={} needs sslrootcert, the certificate of the authority to trust",
            options.ssl_mode.name()
        ));
    }
    Ok(options)
}

/// Why talking to PostgreSQL failed.
#[derive(Debug)]
pub enum ClientError {
    /// The connection could not be made or broke.
    Io(io::Error),
    /// A read timed out; nothing was lost and the read may be repeated.
    TimedOut,
    /// TLS failed, or the server refused it where the mode requires it.
    Tls(String),
    /// Logging in failed.
    Authentication(String),
    /// The server reported an error.
    Server {
        /// The SQLSTATE.
        code: String,
        /// The message.
        message: String,
    },
    /// The server sent something this client does not understand.
    Protocol(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::TimedOut => formatter.write_str("the server did not answer in time"),
            Self::Tls(message) | Self::Authentication(message) | Self::Protocol(message) => {
                formatter.write_str(message)
            }
            Self::Server { code, message } => write!(formatter, "{message} (SQLSTATE {code})"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<io::Error> for ClientError {
    fn from(error: io::Error) -> Self {
        if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) {
            Self::TimedOut
        } else {
            Self::Io(error)
        }
    }
}

enum Transport {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl Transport {
    fn tcp(&self) -> &TcpStream {
        match self {
            Self::Plain(stream) => stream,
            Self::Tls(stream) => stream.get_ref(),
        }
    }
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

/// The rows of one statement of a simple query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryRows {
    /// The column names.
    pub columns: Vec<String>,
    /// Each row's values as text, `None` for null.
    pub rows: Vec<Vec<Option<String>>>,
    /// The command tag.
    pub tag: String,
}

/// A connection to a PostgreSQL server.
pub struct PgClient {
    transport: Transport,
    incoming: Vec<u8>,
    server_version: String,
    tls_version: Option<u16>,
}

impl std::fmt::Debug for PgClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PgClient")
            .field("server_version", &self.server_version)
            .finish_non_exhaustive()
    }
}

fn frontend_message(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut message = vec![tag];
    message.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    message.extend_from_slice(body);
    message
}

fn cstring(text: &str) -> Vec<u8> {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    bytes
}

fn error_fields(body: &[u8]) -> (String, String) {
    let mut code = String::new();
    let mut message = String::new();
    for field in body.split(|byte| *byte == 0) {
        if let Some((&kind, value)) = field.split_first() {
            let value = String::from_utf8_lossy(value).into_owned();
            match kind {
                b'C' => code = value,
                b'M' => message = value,
                _ => {}
            }
        }
    }
    (code, message)
}

fn tls_config(options: &ConnectOptions) -> Result<ClientConfig, ClientError> {
    let trust = || -> Result<TrustStore, ClientError> {
        let path = options
            .ssl_root_cert
            .as_ref()
            .ok_or_else(|| ClientError::Tls("sslrootcert is needed to check the certificate".to_owned()))?;
        let text = std::fs::read_to_string(path)
            .map_err(|error| ClientError::Tls(format!("cannot read sslrootcert {}: {error}", path.display())))?;
        TrustStore::from_pem(&text)
            .map_err(|error| ClientError::Tls(format!("sslrootcert {}: {error}", path.display())))
    };
    let verification = match options.ssl_mode {
        SslMode::VerifyFull => ServerVerification::Full {
            trust: trust()?,
            server_name: eggdb_tls::verify::ServerName::parse(&options.host)
                .map_err(|error| ClientError::Tls(error.to_string()))?,
        },
        SslMode::VerifyCa => ServerVerification::CertificateAuthority { trust: trust()? },
        _ => ServerVerification::EncryptionOnly,
    };
    Ok(ClientConfig::new(verification, &options.host))
}

impl PgClient {
    /// Connects, starts TLS as `options.ssl_mode` asks, and logs in.
    pub fn connect(options: &ConnectOptions) -> Result<Self, ClientError> {
        let address = (options.host.as_str(), options.port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| ClientError::Io(io::Error::new(io::ErrorKind::NotFound, "the host has no address")))?;
        let tcp = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)?;
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(CONNECT_TIMEOUT))?;
        let transport = Self::start_tls(tcp, options)?;
        let mut client = Self { transport, incoming: Vec::new(), server_version: String::new(), tls_version: None };
        if let Transport::Tls(stream) = &client.transport {
            client.tls_version = Some(stream.info().version);
        }
        client.start_up(options)?;
        client.transport.tcp().set_read_timeout(None)?;
        Ok(client)
    }

    fn start_tls(mut tcp: TcpStream, options: &ConnectOptions) -> Result<Transport, ClientError> {
        if options.ssl_mode == SslMode::Disable {
            return Ok(Transport::Plain(tcp));
        }
        let mut request = 8u32.to_be_bytes().to_vec();
        request.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
        tcp.write_all(&request)?;
        let mut answer = [0u8; 1];
        tcp.read_exact(&mut answer)?;
        match answer[0] {
            b'S' => {
                let config = tls_config(options)?;
                let stream = TlsStream::connect(tcp, &config).map_err(|error| match error {
                    TlsError::Certificate(reason) => ClientError::Tls(format!(
                        "the server's certificate was refused: {reason} (sslmode={})",
                        options.ssl_mode.name()
                    )),
                    other => ClientError::Tls(other.to_string()),
                })?;
                Ok(Transport::Tls(Box::new(stream)))
            }
            b'N' if options.ssl_mode == SslMode::Prefer => Ok(Transport::Plain(tcp)),
            b'N' => Err(ClientError::Tls(format!(
                "the server does not support TLS and the URL says sslmode={}",
                options.ssl_mode.name()
            ))),
            other => Err(ClientError::Protocol(format!("the server answered the SSL request with byte {other}"))),
        }
    }

    /// The server's `server_version`.
    pub fn server_version(&self) -> &str {
        &self.server_version
    }

    /// The TLS version in use, if any.
    pub fn tls_version(&self) -> Option<u16> {
        self.tls_version
    }

    /// Sets how long a read waits before [`ClientError::TimedOut`].
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<(), ClientError> {
        self.transport.tcp().set_read_timeout(timeout)?;
        Ok(())
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), ClientError> {
        self.transport.write_all(bytes)?;
        self.transport.flush()?;
        Ok(())
    }

    fn fill(&mut self, wanted: usize) -> Result<(), ClientError> {
        let mut chunk = [0u8; 16 * 1024];
        while self.incoming.len() < wanted {
            match self.transport.read(&mut chunk) {
                Ok(0) => {
                    return Err(ClientError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the server closed the connection",
                    )));
                }
                Ok(read) => self.incoming.extend_from_slice(&chunk[..read]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    /// The next backend message's tag and body. A timeout keeps what was read so far.
    pub fn read_message(&mut self) -> Result<(u8, Vec<u8>), ClientError> {
        self.fill(5)?;
        let length = u32::from_be_bytes([self.incoming[1], self.incoming[2], self.incoming[3], self.incoming[4]]) as usize;
        if !(4..=MAX_MESSAGE_BYTES).contains(&length) {
            return Err(ClientError::Protocol(format!("a message claims {length} bytes")));
        }
        self.fill(1 + length)?;
        let message: Vec<u8> = self.incoming.drain(..1 + length).collect();
        Ok((message[0], message[5..].to_vec()))
    }

    fn start_up(&mut self, options: &ConnectOptions) -> Result<(), ClientError> {
        let mut body = PROTOCOL_3_0.to_be_bytes().to_vec();
        let mut parameters = vec![
            ("user", options.user.as_str()),
            ("database", options.database.as_str()),
            ("application_name", "eggdb follow"),
            ("client_encoding", "UTF8"),
        ];
        if options.replication {
            parameters.push(("replication", "database"));
        }
        for (name, value) in parameters {
            body.extend_from_slice(&cstring(name));
            body.extend_from_slice(&cstring(value));
        }
        body.push(0);
        let mut packet = ((body.len() + 4) as u32).to_be_bytes().to_vec();
        packet.extend_from_slice(&body);
        self.send(&packet)?;
        loop {
            let (tag, body) = self.read_message()?;
            match tag {
                b'R' => self.authenticate(&body, options)?,
                b'S' => {
                    let mut parts = body.split(|byte| *byte == 0);
                    if parts.next() == Some(b"server_version")
                        && let Some(value) = parts.next()
                    {
                        self.server_version = String::from_utf8_lossy(value).into_owned();
                    }
                }
                b'K' | b'N' => {}
                b'Z' => return Ok(()),
                b'E' => {
                    let (code, message) = error_fields(&body);
                    return Err(if code.starts_with("28") {
                        ClientError::Authentication(message)
                    } else {
                        ClientError::Server { code, message }
                    });
                }
                other => {
                    return Err(ClientError::Protocol(format!(
                        "unexpected message {} while logging in",
                        char::from(other)
                    )));
                }
            }
        }
    }

    fn password<'o>(options: &'o ConnectOptions) -> Result<&'o str, ClientError> {
        options.password.as_deref().ok_or_else(|| {
            ClientError::Authentication(
                "a password is needed and none was given (PGPASSWORD is unset)".to_owned(),
            )
        })
    }

    fn authenticate(&mut self, body: &[u8], options: &ConnectOptions) -> Result<(), ClientError> {
        let kind = body
            .first_chunk::<4>()
            .map(|bytes| i32::from_be_bytes(*bytes))
            .ok_or_else(|| ClientError::Protocol("a short authentication message".to_owned()))?;
        match kind {
            0 => Ok(()),
            3 => {
                if !matches!(self.transport, Transport::Tls(_)) {
                    return Err(ClientError::Authentication(
                        "the server asks for the password in cleartext over an unencrypted connection; use sslmode=require or stronger".to_owned(),
                    ));
                }
                let password = cstring(Self::password(options)?);
                self.send(&frontend_message(b'p', &password))
            }
            10 => self.scram(&body[4..], options),
            5 => Err(ClientError::Authentication(
                "the server asks for MD5 password authentication, which EggDB does not use; set password_encryption to scram-sha-256 for this user".to_owned(),
            )),
            other => Err(ClientError::Authentication(format!(
                "the server asks for authentication method {other}, which EggDB does not support"
            ))),
        }
    }

    fn scram(&mut self, mechanisms: &[u8], options: &ConnectOptions) -> Result<(), ClientError> {
        let offered: Vec<&[u8]> = mechanisms.split(|byte| *byte == 0).filter(|name| !name.is_empty()).collect();
        let binding = match &self.transport {
            Transport::Tls(stream) => stream.info().tls_server_end_point(),
            Transport::Plain(_) => None,
        };
        let plus = binding.is_some() && offered.contains(&&b"SCRAM-SHA-256-PLUS"[..]);
        if !plus && !offered.contains(&&b"SCRAM-SHA-256"[..]) {
            return Err(ClientError::Authentication("the server offers no SCRAM mechanism EggDB supports".to_owned()));
        }
        let (mechanism, gs2_header) = if plus {
            ("SCRAM-SHA-256-PLUS", "p=tls-server-end-point,,")
        } else if binding.is_some() {
            ("SCRAM-SHA-256", "y,,")
        } else {
            ("SCRAM-SHA-256", "n,,")
        };
        let mut nonce_bytes = [0u8; CLIENT_NONCE_BYTES];
        eggdb_platform::secure_random::fill(&mut nonce_bytes)?;
        let client_nonce = base64_encode(&nonce_bytes);
        let client_first_bare = format!("n=,r={client_nonce}");
        let client_first = format!("{gs2_header}{client_first_bare}");
        let mut initial = cstring(mechanism);
        initial.extend_from_slice(&(client_first.len() as i32).to_be_bytes());
        initial.extend_from_slice(client_first.as_bytes());
        self.send(&frontend_message(b'p', &initial))?;

        let server_first = self.sasl_message(11)?;
        let mut nonce = "";
        let mut salt = Vec::new();
        let mut iterations = 0u32;
        for attribute in server_first.split(',') {
            if let Some(value) = attribute.strip_prefix("r=") {
                nonce = value;
            } else if let Some(value) = attribute.strip_prefix("s=") {
                salt = base64_decode(value.as_bytes())
                    .map_err(|_| ClientError::Authentication("the server's salt is not base64".to_owned()))?;
            } else if let Some(value) = attribute.strip_prefix("i=") {
                iterations = value
                    .parse()
                    .map_err(|_| ClientError::Authentication("the server's iteration count is not a number".to_owned()))?;
            }
        }
        if !nonce.starts_with(&client_nonce) || iterations == 0 || salt.is_empty() {
            return Err(ClientError::Authentication("the server's SCRAM message is malformed".to_owned()));
        }
        let mut salted = [0u8; SHA256_DIGEST_BYTES];
        pbkdf2_hmac_sha256(Self::password(options)?.as_bytes(), &salt, iterations, &mut salted);
        let client_key = hmac_sha256(&salted, b"Client Key");
        let stored_key = sha256(&client_key);
        let mut channel = gs2_header.as_bytes().to_vec();
        if plus && let Some(binding) = &binding {
            channel.extend_from_slice(binding);
        }
        let without_proof = format!("c={},r={nonce}", base64_encode(&channel));
        let auth_message = format!("{client_first_bare},{server_first},{without_proof}");
        let signature = hmac_sha256(&stored_key, auth_message.as_bytes());
        let proof: Vec<u8> = client_key.iter().zip(signature).map(|(key, signed)| key ^ signed).collect();
        let client_final = format!("{without_proof},p={}", base64_encode(&proof));
        self.send(&frontend_message(b'p', client_final.as_bytes()))?;

        let server_final = self.sasl_message(12)?;
        let server_key = hmac_sha256(&salted, b"Server Key");
        let expected = format!("v={}", base64_encode(&hmac_sha256(&server_key, auth_message.as_bytes())));
        if !constant_time_eq(server_final.as_bytes(), expected.as_bytes()) {
            return Err(ClientError::Authentication("the server could not prove it knows the password".to_owned()));
        }
        Ok(())
    }

    fn sasl_message(&mut self, expected_kind: i32) -> Result<String, ClientError> {
        let (tag, body) = self.read_message()?;
        if tag == b'E' {
            let (_, message) = error_fields(&body);
            return Err(ClientError::Authentication(message));
        }
        let kind = body.first_chunk::<4>().map(|bytes| i32::from_be_bytes(*bytes));
        if tag != b'R' || kind != Some(expected_kind) {
            return Err(ClientError::Protocol("unexpected message during SCRAM".to_owned()));
        }
        String::from_utf8(body[4..].to_vec()).map_err(|_| ClientError::Protocol("a SCRAM message is not UTF-8".to_owned()))
    }

    /// Runs `sql` with the simple query protocol and returns each statement's rows.
    pub fn simple_query(&mut self, sql: &str) -> Result<Vec<QueryRows>, ClientError> {
        self.send(&frontend_message(b'Q', &cstring(sql)))?;
        let mut results = Vec::new();
        let mut current = QueryRows::default();
        let mut failure = None;
        loop {
            let (tag, body) = self.read_message()?;
            match tag {
                b'T' => current.columns = row_description_names(&body)?,
                b'D' => current.rows.push(data_row(&body)?),
                b'C' => {
                    current.tag = String::from_utf8_lossy(body.strip_suffix(&[0]).unwrap_or(&body)).into_owned();
                    results.push(std::mem::take(&mut current));
                }
                b'E' => {
                    let (code, message) = error_fields(&body);
                    failure = Some(ClientError::Server { code, message });
                }
                b'I' | b'N' | b'S' => {}
                b'Z' => {
                    return match failure {
                        Some(error) => Err(error),
                        None => Ok(results),
                    };
                }
                other => {
                    return Err(ClientError::Protocol(format!(
                        "unexpected message {} in a query's answer",
                        char::from(other)
                    )));
                }
            }
        }
    }

    /// Starts `sql`, a `COPY ... TO STDOUT`; read its data with [`PgClient::read_copy_out`].
    pub fn start_copy_out(&mut self, sql: &str) -> Result<(), ClientError> {
        self.send(&frontend_message(b'Q', &cstring(sql)))?;
        loop {
            let (tag, body) = self.read_message()?;
            match tag {
                b'H' => return Ok(()),
                b'N' => {}
                b'E' => {
                    let (code, message) = error_fields(&body);
                    self.wait_ready()?;
                    return Err(ClientError::Server { code, message });
                }
                other => {
                    return Err(ClientError::Protocol(format!(
                        "expected a copy-out response, got {}",
                        char::from(other)
                    )));
                }
            }
        }
    }

    /// The next piece of a `COPY ... TO STDOUT`, or `None` once it has ended.
    pub fn read_copy_out(&mut self) -> Result<Option<Vec<u8>>, ClientError> {
        loop {
            let (tag, body) = self.read_message()?;
            match tag {
                b'd' => return Ok(Some(body)),
                b'c' | b'C' | b'N' => {}
                b'Z' => return Ok(None),
                b'E' => {
                    let (code, message) = error_fields(&body);
                    self.wait_ready()?;
                    return Err(ClientError::Server { code, message });
                }
                other => {
                    return Err(ClientError::Protocol(format!(
                        "unexpected message {} in a copy",
                        char::from(other)
                    )));
                }
            }
        }
    }

    fn wait_ready(&mut self) -> Result<(), ClientError> {
        loop {
            let (tag, _) = self.read_message()?;
            if tag == b'Z' {
                return Ok(());
            }
        }
    }

    /// Starts a replication command that answers with a copy-both stream, such as
    /// `START_REPLICATION`.
    pub fn start_copy_both(&mut self, sql: &str) -> Result<(), ClientError> {
        self.send(&frontend_message(b'Q', &cstring(sql)))?;
        loop {
            let (tag, body) = self.read_message()?;
            match tag {
                b'W' => return Ok(()),
                b'N' => {}
                b'E' => {
                    let (code, message) = error_fields(&body);
                    return Err(ClientError::Server { code, message });
                }
                other => {
                    return Err(ClientError::Protocol(format!(
                        "expected a copy-both response, got {}",
                        char::from(other)
                    )));
                }
            }
        }
    }

    /// The next message of a copy-both stream, or `None` when the server ended it.
    pub fn read_copy_both(&mut self) -> Result<Option<Vec<u8>>, ClientError> {
        loop {
            let (tag, body) = self.read_message()?;
            match tag {
                b'd' => return Ok(Some(body)),
                b'c' => return Ok(None),
                b'N' => {}
                b'E' => {
                    let (code, message) = error_fields(&body);
                    return Err(ClientError::Server { code, message });
                }
                other => {
                    return Err(ClientError::Protocol(format!(
                        "unexpected message {} in a replication stream",
                        char::from(other)
                    )));
                }
            }
        }
    }

    /// Sends one copy-data message on a copy-both stream.
    pub fn send_copy_data(&mut self, data: &[u8]) -> Result<(), ClientError> {
        self.send(&frontend_message(b'd', data))
    }

    /// Ends the session politely.
    pub fn terminate(mut self) {
        let _ = self.send(&frontend_message(b'X', &[]));
    }
}

fn row_description_names(body: &[u8]) -> Result<Vec<String>, ClientError> {
    let malformed = || ClientError::Protocol("a malformed row description".to_owned());
    let count = usize::from(u16::from_be_bytes(*body.first_chunk::<2>().ok_or_else(malformed)?));
    let mut names = Vec::with_capacity(count);
    let mut at = 2;
    for _ in 0..count {
        let end = body.get(at..).and_then(|rest| rest.iter().position(|byte| *byte == 0)).ok_or_else(malformed)?;
        names.push(String::from_utf8_lossy(&body[at..at + end]).into_owned());
        at += end + 1 + 18;
    }
    Ok(names)
}

fn data_row(body: &[u8]) -> Result<Vec<Option<String>>, ClientError> {
    let malformed = || ClientError::Protocol("a malformed data row".to_owned());
    let count = usize::from(u16::from_be_bytes(*body.first_chunk::<2>().ok_or_else(malformed)?));
    let mut values = Vec::with_capacity(count);
    let mut at = 2;
    for _ in 0..count {
        let length = i32::from_be_bytes(*body.get(at..).and_then(|rest| rest.first_chunk::<4>()).ok_or_else(malformed)?);
        at += 4;
        if length < 0 {
            values.push(None);
            continue;
        }
        let length = length as usize;
        let value = body.get(at..at + length).ok_or_else(malformed)?;
        values.push(Some(String::from_utf8_lossy(value).into_owned()));
        at += length;
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_read_like_libpq_and_refuse_passwords() {
        let options = parse_postgres_url("postgres://ada@db.example.com/app?sslmode=verify-full&sslrootcert=ca.pem").unwrap();
        assert_eq!(options.host, "db.example.com");
        assert_eq!(options.port, 5432);
        assert_eq!(options.user, "ada");
        assert_eq!(options.database, "app");
        assert_eq!(options.ssl_mode, SslMode::VerifyFull);
        assert_eq!(options.ssl_root_cert, Some(PathBuf::from("ca.pem")));
        let ipv6 = parse_postgres_url("postgresql://b%40x@[::1]:6543").unwrap();
        assert_eq!((ipv6.host.as_str(), ipv6.port, ipv6.user.as_str(), ipv6.database.as_str()), ("::1", 6543, "b@x", "b@x"));
        assert!(parse_postgres_url("postgres://ada:secret@host/db").unwrap_err().contains("PGPASSWORD"));
        assert!(parse_postgres_url("postgres://ada@host/db?password=x").is_err());
        assert!(parse_postgres_url("mysql://ada@host/db").is_err());
        assert!(parse_postgres_url("postgres://ada@host/db?sslmode=verify-ca").unwrap_err().contains("sslrootcert"));
        assert!(parse_postgres_url("postgres://ada@host:port/db").is_err());
    }

    #[test]
    fn rows_and_descriptions_decode_and_refuse_truncation() {
        let mut row = 2u16.to_be_bytes().to_vec();
        row.extend_from_slice(&2i32.to_be_bytes());
        row.extend_from_slice(b"42");
        row.extend_from_slice(&(-1i32).to_be_bytes());
        assert_eq!(data_row(&row).unwrap(), vec![Some("42".to_owned()), None]);
        for cut in 0..row.len() {
            assert!(data_row(&row[..cut]).is_err() || cut == row.len());
        }
        let mut description = 1u16.to_be_bytes().to_vec();
        description.extend_from_slice(b"answer\0");
        description.extend_from_slice(&[0; 18]);
        assert_eq!(row_description_names(&description).unwrap(), vec!["answer".to_owned()]);
        assert!(row_description_names(&description[..5]).is_err());
    }
}
