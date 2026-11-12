//! A MongoDB client of EggDB's own, for following a MongoDB deployment (D13): `OP_MSG`
//! commands over TCP or TLS and SCRAM-SHA-256 sign-in, written from MongoDB's published wire
//! protocol and authentication specifications.

use eggdb_bson::{Bson, Document, read_document};
use eggdb_tls::verify::TrustStore;
use eggdb_tls::{ClientConfig, ServerVerification, TlsStream};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::time::Duration;

const OP_MSG: i32 = 2013;
const HEADER_BYTES: usize = 16;
const HEADER_AFTER_LENGTH_BYTES: usize = 12;
const FLAG_BYTES: usize = 4;
const DEFAULT_PORT: u16 = 27017;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const SCRAM_MECHANISM: &str = "SCRAM-SHA-256";
const TRUST_VARIABLE: &str = "SSL_CERT_FILE";
const SYSTEM_TRUST_FILES: [&str; 4] = [
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/cert.pem",
    "/usr/local/etc/openssl/cert.pem",
];

/// Where a `mongodb://` URL points, and how to sign in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MongoUrl {
    /// The host of the first seed listed.
    pub host: String,
    /// Its port, 27017 unless the URL gives one.
    pub port: u16,
    /// The user, when the URL names one.
    pub user: Option<String>,
    /// The database the user is defined in (`authSource`, else the URL's database, else `admin`).
    pub auth_source: String,
    /// Whether to connect with TLS (`tls=true` or `ssl=true`).
    pub tls: bool,
    /// The certificates to trust (`tlsCAFile`), else the system's.
    pub tls_ca_file: Option<PathBuf>,
    /// Whether to skip checking the server's certificate (`tlsInsecure` or
    /// `tlsAllowInvalidCertificates`).
    pub tls_insecure: bool,
    /// The password, never taken from the URL.
    pub password: Option<String>,
}

impl MongoUrl {
    /// The URL as it is shown in progress lines, without a password.
    pub fn shown(&self) -> String {
        let user = self.user.as_ref().map(|user| format!("{user}@")).unwrap_or_default();
        format!("mongodb://{user}{}:{}", self.host, self.port)
    }
}

fn percent_decoded(text: &str) -> Result<String, String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        if byte == b'%' {
            let digits = text
                .get(at + 1..at + 3)
                .and_then(|digits| u8::from_str_radix(digits, 16).ok())
                .ok_or_else(|| format!("'{text}' has a % that is not two hex digits"))?;
            out.push(digits);
            at += 3;
        } else {
            out.push(byte);
            at += 1;
        }
    }
    String::from_utf8(out).map_err(|_| format!("'{text}' is not UTF-8 once decoded"))
}

fn flag(value: &str) -> bool {
    matches!(value.to_ascii_lowercase().as_str(), "true" | "1" | "yes")
}

/// Reads `mongodb://[user@]host[:port][,more hosts][/database][?options]`. A password in the
/// URL is refused, since process listings show it; `mongodb+srv://` needs DNS SRV records,
/// which EggDB does not look up yet.
pub fn parse_mongodb_url(url: &str) -> Result<MongoUrl, String> {
    if url.starts_with("mongodb+srv://") {
        return Err("mongodb+srv:// URLs need DNS SRV lookups; give the hosts with mongodb://host:port,... instead".to_owned());
    }
    let rest = url
        .strip_prefix("mongodb://")
        .ok_or_else(|| format!("{url}: give a mongodb:// URL"))?;
    let (authority, after) = match rest.find(['/', '?']) {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, ""),
    };
    let (user, hosts) = match authority.rsplit_once('@') {
        Some((user, hosts)) => {
            if user.contains(':') {
                return Err("put the password in MONGODB_PASSWORD or --password-file, not in the URL".to_owned());
            }
            (Some(percent_decoded(user)?), hosts)
        }
        None => (None, authority),
    };
    let first = hosts.split(',').next().unwrap_or_default();
    if first.is_empty() {
        return Err(format!("{url}: the URL names no host"));
    }
    let (host, port) = match first.rsplit_once(':') {
        Some((host, port)) if !host.ends_with(']') || first.starts_with('[') => (
            host.trim_start_matches('[').trim_end_matches(']').to_owned(),
            port.parse::<u16>().map_err(|_| format!("{url}: '{port}' is not a port"))?,
        ),
        _ => (first.trim_start_matches('[').trim_end_matches(']').to_owned(), DEFAULT_PORT),
    };
    let (path, query) = match after.split_once('?') {
        Some((path, query)) => (path, query),
        None => (after, ""),
    };
    let database = path.trim_start_matches('/');
    let mut parsed = MongoUrl {
        host,
        port,
        user,
        auth_source: if database.is_empty() { "admin".to_owned() } else { percent_decoded(database)? },
        tls: false,
        tls_ca_file: None,
        tls_insecure: false,
        password: None,
    };
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decoded(value)?;
        match name.to_ascii_lowercase().as_str() {
            "tls" | "ssl" => parsed.tls = flag(&value),
            "tlscafile" => {
                parsed.tls = true;
                parsed.tls_ca_file = Some(PathBuf::from(value));
            }
            "tlsinsecure" | "tlsallowinvalidcertificates" => parsed.tls_insecure = flag(&value),
            "authsource" => parsed.auth_source = value,
            "authmechanism" if value != SCRAM_MECHANISM => {
                return Err(format!("authMechanism={value}: EggDB signs in with {SCRAM_MECHANISM}"));
            }
            _ => {}
        }
    }
    Ok(parsed)
}

/// What went wrong talking to a MongoDB server.
#[derive(Debug)]
pub enum MongoClientError {
    /// The connection failed or was lost.
    Io(io::Error),
    /// TLS could not be set up.
    Tls(String),
    /// The server answered with an error.
    Server {
        /// MongoDB's error code.
        code: i32,
        /// MongoDB's code name.
        name: String,
        /// The message.
        message: String,
    },
    /// The server's answer could not be read.
    Protocol(String),
}

impl std::fmt::Display for MongoClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Tls(reason) => write!(f, "TLS: {reason}"),
            Self::Server { code, name, message } => write!(f, "{message} ({name}, code {code})"),
            Self::Protocol(reason) => write!(f, "{reason}"),
        }
    }
}

impl From<io::Error> for MongoClientError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl MongoClientError {
    /// Whether the connection was lost, so connecting again may help.
    pub fn connection_lost(&self) -> bool {
        matches!(self, Self::Io(_))
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

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::Plain(stream) => stream.write_all(bytes),
            Self::Tls(stream) => stream.write_all(bytes),
        }
    }

    fn read_exact(&mut self, bytes: &mut [u8]) -> io::Result<()> {
        match self {
            Self::Plain(stream) => stream.read_exact(bytes),
            Self::Tls(stream) => stream.read_exact(bytes),
        }
    }
}

/// A connection to a MongoDB server.
pub struct MongoClient {
    transport: Transport,
    next_request: i32,
}

impl std::fmt::Debug for MongoClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MongoClient").finish_non_exhaustive()
    }
}

fn system_trust() -> Result<TrustStore, MongoClientError> {
    let candidates: Vec<String> = match std::env::var(TRUST_VARIABLE) {
        Ok(path) => vec![path],
        Err(_) => SYSTEM_TRUST_FILES.iter().map(|path| (*path).to_owned()).collect(),
    };
    candidates
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .find_map(|text| TrustStore::from_pem(&text).ok())
        .ok_or_else(|| {
            MongoClientError::Tls(format!(
                "no trusted certificates to check the server with: give tlsCAFile or set {TRUST_VARIABLE}"
            ))
        })
}

fn tls_config(url: &MongoUrl) -> Result<ClientConfig, MongoClientError> {
    let verification = if url.tls_insecure {
        ServerVerification::EncryptionOnly
    } else {
        let trust = match &url.tls_ca_file {
            Some(path) => {
                let text = std::fs::read_to_string(path).map_err(|error| {
                    MongoClientError::Tls(format!("cannot read tlsCAFile {}: {error}", path.display()))
                })?;
                TrustStore::from_pem(&text)
                    .map_err(|error| MongoClientError::Tls(format!("tlsCAFile {}: {error}", path.display())))?
            }
            None => system_trust()?,
        };
        ServerVerification::Full {
            trust,
            server_name: eggdb_tls::verify::ServerName::parse(&url.host)
                .map_err(|error| MongoClientError::Tls(error.to_string()))?,
        }
    };
    Ok(ClientConfig::new(verification, &url.host))
}

fn client_nonce() -> String {
    let seed = format!("{:?}{}{:p}", std::time::SystemTime::now(), std::process::id(), &CONNECT_TIMEOUT);
    eggdb_crypto::base64_encode(&eggdb_crypto::sha256(seed.as_bytes())[..18])
}

fn scram_field(message: &str, name: char) -> Option<&str> {
    message
        .split(',')
        .find_map(|part| part.strip_prefix(name).and_then(|rest| rest.strip_prefix('=')))
}

fn payload_of(reply: &Document) -> Result<Vec<u8>, MongoClientError> {
    match reply.get("payload") {
        Some(Bson::Binary { bytes, .. }) => Ok(bytes.clone()),
        _ => Err(MongoClientError::Protocol("the SASL reply has no payload".to_owned())),
    }
}

fn protocol(message: impl Into<String>) -> MongoClientError {
    MongoClientError::Protocol(message.into())
}

impl MongoClient {
    /// Connects to the URL's first host, with TLS when it asks, and signs in when it names a
    /// user.
    pub fn connect(url: &MongoUrl) -> Result<Self, MongoClientError> {
        let address = (url.host.as_str(), url.port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| MongoClientError::Io(io::Error::new(io::ErrorKind::NotFound, "the host has no address")))?;
        let tcp = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)?;
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(CONNECT_TIMEOUT))?;
        let transport = if url.tls {
            let config = tls_config(url)?;
            let stream = TlsStream::connect(tcp, &config).map_err(|error| MongoClientError::Tls(error.to_string()))?;
            Transport::Tls(Box::new(stream))
        } else {
            Transport::Plain(tcp)
        };
        let mut client = Self { transport, next_request: 1 };
        if let Some(user) = &url.user {
            client.authenticate(&url.auth_source, user, url.password.as_deref().unwrap_or_default())?;
        }
        Ok(client)
    }

    /// How long a read waits before failing; `None` waits for ever.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<(), MongoClientError> {
        Ok(self.transport.tcp().set_read_timeout(timeout)?)
    }

    fn exchange(&mut self, body: &Document) -> Result<Document, MongoClientError> {
        let bson = body.to_bytes();
        let length = i32::try_from(HEADER_BYTES + FLAG_BYTES + 1 + bson.len())
            .map_err(|_| protocol("the command is too large"))?;
        let mut message = Vec::with_capacity(bson.len() + HEADER_BYTES + FLAG_BYTES + 1);
        message.extend_from_slice(&length.to_le_bytes());
        message.extend_from_slice(&self.next_request.to_le_bytes());
        message.extend_from_slice(&0i32.to_le_bytes());
        message.extend_from_slice(&OP_MSG.to_le_bytes());
        message.extend_from_slice(&[0u8; FLAG_BYTES]);
        message.push(0);
        message.extend_from_slice(&bson);
        self.next_request = self.next_request.wrapping_add(1);
        self.transport.write_all(&message)?;
        let mut length = [0u8; 4];
        self.transport.read_exact(&mut length)?;
        let length = usize::try_from(i32::from_le_bytes(length)).map_err(|_| protocol("a negative reply length"))?;
        if !(HEADER_BYTES + FLAG_BYTES + 1..=crate::MAX_MESSAGE_BYTES).contains(&length) {
            return Err(protocol(format!("a reply of {length} bytes")));
        }
        let mut rest = vec![0u8; length - 4];
        self.transport.read_exact(&mut rest)?;
        let op_code = rest
            .get(8..HEADER_AFTER_LENGTH_BYTES)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .map(i32::from_le_bytes);
        if op_code != Some(OP_MSG) {
            return Err(protocol(format!("a reply with op code {op_code:?}")));
        }
        let sections = rest
            .get(HEADER_AFTER_LENGTH_BYTES + FLAG_BYTES..)
            .ok_or_else(|| protocol("a reply without sections"))?;
        let Some((&0, document)) = sections.split_first() else {
            return Err(protocol("a reply that does not start with a body section"));
        };
        let (reply, _) = read_document(document).map_err(|error| protocol(format!("reading the reply: {error:?}")))?;
        Ok(reply)
    }

    /// Runs `body` on `database` and returns the reply, or the server's error.
    pub fn command(&mut self, database: &str, mut body: Document) -> Result<Document, MongoClientError> {
        body.insert("$db", Bson::String(database.to_owned()));
        let reply = self.exchange(&body)?;
        if reply.get("ok").and_then(Bson::as_f64) != Some(1.0) {
            return Err(MongoClientError::Server {
                code: reply.get("code").and_then(Bson::as_i64).and_then(|code| i32::try_from(code).ok()).unwrap_or(0),
                name: reply.get_str("codeName").unwrap_or_default().to_owned(),
                message: reply.get_str("errmsg").unwrap_or("the command failed").to_owned(),
            });
        }
        Ok(reply)
    }

    /// Signs in as `user` defined in `database`, with SCRAM-SHA-256 (RFC 5802 and RFC 7677).
    pub fn authenticate(&mut self, database: &str, user: &str, password: &str) -> Result<(), MongoClientError> {
        let nonce = client_nonce();
        let first_bare = format!("n={},r={nonce}", user.replace('=', "=3D").replace(',', "=2C"));
        let start = Document::from_entries(vec![
            ("saslStart".to_owned(), Bson::Int32(1)),
            ("mechanism".to_owned(), Bson::String(SCRAM_MECHANISM.to_owned())),
            ("payload".to_owned(), Bson::Binary { subtype: 0, bytes: format!("n,,{first_bare}").into_bytes() }),
            ("autoAuthorize".to_owned(), Bson::Int32(1)),
        ]);
        let reply = self.command(database, start)?;
        let conversation = reply.get("conversationId").cloned().unwrap_or(Bson::Int32(1));
        let server_first =
            String::from_utf8(payload_of(&reply)?).map_err(|_| protocol("a server-first message that is not UTF-8"))?;
        let combined = scram_field(&server_first, 'r').ok_or_else(|| protocol("no nonce in the server-first message"))?;
        if !combined.starts_with(&nonce) {
            return Err(protocol("the server's nonce does not extend ours"));
        }
        let salt = scram_field(&server_first, 's')
            .and_then(|salt| eggdb_crypto::base64_decode(salt.as_bytes()).ok())
            .ok_or_else(|| protocol("no salt in the server-first message"))?;
        let iterations: u32 = scram_field(&server_first, 'i')
            .and_then(|count| count.parse().ok())
            .filter(|count| *count >= 4096)
            .ok_or_else(|| protocol("no acceptable iteration count in the server-first message"))?;
        let mut salted = [0u8; 32];
        eggdb_crypto::pbkdf2_hmac_sha256(password.as_bytes(), &salt, iterations, &mut salted);
        let client_key = eggdb_crypto::hmac_sha256(&salted, b"Client Key");
        let server_key = eggdb_crypto::hmac_sha256(&salted, b"Server Key");
        let stored_key = eggdb_crypto::sha256(&client_key);
        let without_proof = format!("c=biws,r={combined}");
        let auth_message = format!("{first_bare},{server_first},{without_proof}");
        let signature = eggdb_crypto::hmac_sha256(&stored_key, auth_message.as_bytes());
        let proof: Vec<u8> = client_key.iter().zip(signature).map(|(key, signed)| key ^ signed).collect();
        let expected_server = eggdb_crypto::base64_encode(&eggdb_crypto::hmac_sha256(&server_key, auth_message.as_bytes()));
        let mut payload = format!("{without_proof},p={}", eggdb_crypto::base64_encode(&proof)).into_bytes();
        let mut server_checked = false;
        loop {
            let next = Document::from_entries(vec![
                ("saslContinue".to_owned(), Bson::Int32(1)),
                ("conversationId".to_owned(), conversation.clone()),
                ("payload".to_owned(), Bson::Binary { subtype: 0, bytes: payload }),
            ]);
            let reply = self.command(database, next)?;
            let answer = String::from_utf8(payload_of(&reply).unwrap_or_default()).unwrap_or_default();
            if let Some(verifier) = scram_field(&answer, 'v') {
                if !eggdb_crypto::constant_time_eq(verifier.as_bytes(), expected_server.as_bytes()) {
                    return Err(protocol("the server's SCRAM signature is wrong"));
                }
                server_checked = true;
            }
            if reply.get_bool("done") == Some(true) {
                return if server_checked {
                    Ok(())
                } else {
                    Err(protocol("the server never proved it knows the password"))
                };
            }
            payload = Vec::new();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_name_the_first_host_and_refuse_passwords() {
        let url = parse_mongodb_url("mongodb://ada@db1:27019,db2/app?authSource=admin&tls=true").unwrap();
        assert_eq!((url.host.as_str(), url.port), ("db1", 27019));
        assert_eq!(url.user.as_deref(), Some("ada"));
        assert_eq!(url.auth_source, "admin");
        assert!(url.tls);
        assert_eq!(parse_mongodb_url("mongodb://localhost").unwrap().port, 27017);
        assert!(parse_mongodb_url("mongodb://ada:secret@db1").is_err());
        assert!(parse_mongodb_url("mongodb+srv://cluster.example").is_err());
        assert_eq!(scram_field("r=n1,s=c2E=,i=4096", 'i'), Some("4096"));
    }
}
