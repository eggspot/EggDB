//! Reading the messages a TDS client sends and building the server's PRELOGIN answer, as
//! [MS-TDS] 2.2.6 defines them. Every reader takes untrusted bytes and returns
//! [`MalformedMessage`] for anything truncated, oversized or inconsistent; none panics.
//! Remote procedure calls are read by [`crate::rpc`].

use std::fmt;

/// The packet type of a SQL batch request.
pub const SQL_BATCH: u8 = 0x01;
/// The packet type of a remote procedure call request.
pub const RPC: u8 = 0x03;
/// The packet type of every server answer (a tabular result).
pub const TABULAR_RESULT: u8 = 0x04;
/// The packet type of an attention (cancel) signal.
pub const ATTENTION: u8 = 0x06;
/// The packet type of bulk load data.
pub const BULK_LOAD: u8 = 0x07;
/// The packet type of a transaction manager request.
pub const TRANSACTION_MANAGER: u8 = 0x0E;
/// The packet type of a LOGIN7 request.
pub const LOGIN7: u8 = 0x10;
/// The packet type of an SSPI message.
pub const SSPI: u8 = 0x11;
/// The packet type of a PRELOGIN request.
pub const PRELOGIN: u8 = 0x12;

/// The packet status bit with which a client asks the server to reset the connection before
/// running the request, as a connection pool does when it hands a connection out again.
pub const RESET_CONNECTION: u8 = 0x08;
/// The packet status bit that resets the connection but keeps its open transaction.
pub const RESET_CONNECTION_SKIP_TRANSACTION: u8 = 0x10;

/// The transaction manager request that begins a transaction.
pub const TM_BEGIN_XACT: u16 = 5;
/// The transaction manager request that commits the transaction.
pub const TM_COMMIT_XACT: u16 = 7;
/// The transaction manager request that rolls the transaction back.
pub const TM_ROLLBACK_XACT: u16 = 8;
/// The transaction manager request that sets a save point.
pub const TM_SAVE_XACT: u16 = 9;
const TM_GET_DTC_ADDRESS: u16 = 0;
const TM_PROPAGATE_XACT: u16 = 1;
const TM_PROMOTE_XACT: u16 = 6;
const BEGIN_NEXT_TRANSACTION: u8 = 0x01;
const HIGHEST_ISOLATION_LEVEL: u8 = 5;

/// The only TDS version EggDB speaks: 7.4, as LOGIN7 carries it in a little-endian DWORD.
pub const TDS_7_4: u32 = 0x7400_0004;

/// Encryption is available but off: the client encrypts only its login.
pub const ENCRYPT_OFF: u8 = 0x00;
/// Encryption is available and on.
pub const ENCRYPT_ON: u8 = 0x01;
/// Encryption is not available.
pub const ENCRYPT_NOT_SUP: u8 = 0x02;
/// Encryption is required.
pub const ENCRYPT_REQ: u8 = 0x03;
/// The bit a client adds to ask for certificate-based authentication.
pub const ENCRYPT_CLIENT_CERT: u8 = 0x80;

const OPTION_VERSION: u8 = 0x00;
const OPTION_ENCRYPTION: u8 = 0x01;
const OPTION_INSTANCE: u8 = 0x02;
const OPTION_THREAD_ID: u8 = 0x03;
const OPTION_MARS: u8 = 0x04;
const OPTION_FED_AUTH_REQUIRED: u8 = 0x06;
const OPTION_TERMINATOR: u8 = 0xFF;

const LOGIN7_FIXED_BYTES: usize = 94;
const MAX_LOGIN_NAME_CHARACTERS: usize = 128;
const INTEGRATED_SECURITY: u8 = 0x80;
const CHANGE_PASSWORD: u8 = 0x01;
const EXTENSION: u8 = 0x10;
const FEATURE_FED_AUTH: u8 = 0x02;
const FEATURE_TERMINATOR: u8 = 0xFF;

const HEADER_QUERY_NOTIFICATIONS: u16 = 0x0001;
const HEADER_TRANSACTION_DESCRIPTOR: u16 = 0x0002;
const HEADER_TRACE_ACTIVITY: u16 = 0x0003;

/// Why a client message could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MalformedMessage {
    /// What was wrong, for the server's log.
    pub reason: &'static str,
}

impl fmt::Display for MalformedMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason)
    }
}

impl std::error::Error for MalformedMessage {}

fn malformed(reason: &'static str) -> MalformedMessage {
    MalformedMessage { reason }
}

fn u16_be(bytes: &[u8], at: usize) -> Result<u16, MalformedMessage> {
    bytes
        .get(at..at + 2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .ok_or(malformed("the message ends early"))
}

fn u16_le(bytes: &[u8], at: usize) -> Result<u16, MalformedMessage> {
    bytes
        .get(at..at + 2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .ok_or(malformed("the message ends early"))
}

fn u32_le(bytes: &[u8], at: usize) -> Result<u32, MalformedMessage> {
    bytes
        .get(at..at + 4)
        .map(|four| u32::from_le_bytes([four[0], four[1], four[2], four[3]]))
        .ok_or(malformed("the message ends early"))
}

/// What a client said in its PRELOGIN message.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PreLogin {
    /// The client's version: major, minor, build (big-endian) and sub-build.
    pub version: [u8; 6],
    /// The client's ENCRYPTION value, when it sent one.
    pub encryption: Option<u8>,
    /// The instance the client names, without its terminating zero.
    pub instance: Vec<u8>,
    /// Whether the client asked for multiple active result sets.
    pub mars: bool,
    /// Whether the client sent the FEDAUTHREQUIRED option.
    pub asks_fed_auth_required: bool,
}

impl PreLogin {
    /// Whether the client asks for encryption EggDB cannot give: on, required, or
    /// certificate-based authentication. Such a client ends the connection when told
    /// encryption is not available ([MS-TDS] 2.2.6.5).
    pub fn wants_encryption(&self) -> bool {
        self.encryption.is_some_and(|value| {
            value & ENCRYPT_CLIENT_CERT != 0
                || value & 0x7F != ENCRYPT_OFF && value & 0x7F != ENCRYPT_NOT_SUP
        })
    }

    /// Whether the client named an instance other than the default one.
    pub fn names_another_instance(&self) -> bool {
        let name: Vec<u8> = self
            .instance
            .iter()
            .copied()
            .take_while(|&byte| byte != 0)
            .collect();
        !name.is_empty() && !name.eq_ignore_ascii_case(b"MSSQLServer")
    }
}

/// Reads a PRELOGIN message's body: its option table (token, big-endian offset and length)
/// and the options' data. VERSION must come first and the table must end with the
/// terminator; every offset and length must lie inside the body.
pub fn parse_prelogin(body: &[u8]) -> Result<PreLogin, MalformedMessage> {
    let mut prelogin = PreLogin::default();
    let mut at = 0;
    let mut first = true;
    loop {
        let Some(&token) = body.get(at) else {
            return Err(malformed("the PRELOGIN option table has no terminator"));
        };
        if token == OPTION_TERMINATOR {
            break;
        }
        if first && token != OPTION_VERSION {
            return Err(malformed("the first PRELOGIN option is not VERSION"));
        }
        first = false;
        let offset = usize::from(u16_be(body, at + 1)?);
        let length = usize::from(u16_be(body, at + 3)?);
        at += 5;
        let data = body
            .get(offset..offset.saturating_add(length))
            .ok_or(malformed("a PRELOGIN option lies outside the message"))?;
        match token {
            OPTION_VERSION => {
                if data.len() < 6 {
                    return Err(malformed("the PRELOGIN VERSION is shorter than 6 bytes"));
                }
                prelogin.version.copy_from_slice(&data[..6]);
            }
            OPTION_ENCRYPTION => prelogin.encryption = data.first().copied(),
            OPTION_INSTANCE => prelogin.instance = data.iter().take(256).copied().collect(),
            OPTION_MARS => prelogin.mars = data.first().is_some_and(|&mars| mars == 1),
            OPTION_FED_AUTH_REQUIRED => prelogin.asks_fed_auth_required = true,
            _ => {}
        }
    }
    if first {
        return Err(malformed("the PRELOGIN message has no VERSION option"));
    }
    Ok(prelogin)
}

/// The server's PRELOGIN answer to `request`: `version` (major, minor, build), encryption
/// not available, the default instance unless the client named another one, no MARS, and
/// FEDAUTHREQUIRED 0 when the client sent that option.
pub fn prelogin_response(request: &PreLogin, version: [u8; 4], encryption: u8) -> Vec<u8> {
    let mut options: Vec<(u8, Vec<u8>)> = vec![
        (
            OPTION_VERSION,
            vec![version[0], version[1], version[2], version[3], 0, 0],
        ),
        (OPTION_ENCRYPTION, vec![encryption]),
        (
            OPTION_INSTANCE,
            vec![u8::from(request.names_another_instance())],
        ),
        (OPTION_THREAD_ID, Vec::new()),
        (OPTION_MARS, vec![0]),
    ];
    if request.asks_fed_auth_required {
        options.push((OPTION_FED_AUTH_REQUIRED, vec![0]));
    }
    let table_bytes = options.len() * 5 + 1;
    let mut body = Vec::new();
    let mut data = Vec::new();
    for (token, value) in &options {
        let offset = (table_bytes + data.len()) as u16;
        body.push(*token);
        body.extend_from_slice(&offset.to_be_bytes());
        body.extend_from_slice(&(value.len() as u16).to_be_bytes());
        data.extend_from_slice(value);
    }
    body.push(OPTION_TERMINATOR);
    body.extend_from_slice(&data);
    body
}

/// A password as a client sent it in LOGIN7, decoded, and overwritten when dropped.
pub struct Password(Vec<u8>);

impl Password {
    /// The password's UTF-8 bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for Password {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&self.0);
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(..)")
    }
}

/// What a client said in its LOGIN7 message.
#[derive(Debug)]
pub struct Login7 {
    /// The TDS version the client asks for, as a number (7.4 is `0x74000004`).
    pub tds_version: u32,
    /// The packet size the client asks for, or 0 for the server's default.
    pub packet_size: u32,
    /// The client's host name.
    pub host_name: String,
    /// The SQL login's name.
    pub user_name: String,
    /// The SQL login's password, de-obfuscated, or `None` when it was not valid UTF-16.
    pub password: Option<Password>,
    /// The client application's name.
    pub app_name: String,
    /// The server name the client connected to.
    pub server_name: String,
    /// The language the client asks for, or empty for the default.
    pub language: String,
    /// The database the client asks for, or empty for the default.
    pub database: String,
    /// Whether the client asks for integrated (SSPI) authentication.
    pub integrated_security: bool,
    /// Whether the client sent SSPI data.
    pub sspi_bytes: usize,
    /// Whether the client asks to change its password.
    pub change_password: bool,
    /// Whether the client asks for federated authentication in its feature extensions.
    pub federated_authentication: bool,
    /// Whether the client asked the server to set the ANSI options on (`fODBC`).
    pub odbc_defaults: bool,
}

fn utf16_units(bytes: &[u8]) -> Vec<u16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect()
}

fn field<'b>(body: &'b [u8], at: usize, name: &'static str) -> Result<&'b [u8], MalformedMessage> {
    let offset = usize::from(u16_le(body, at)?);
    let characters = usize::from(u16_le(body, at + 2)?);
    if characters > MAX_LOGIN_NAME_CHARACTERS {
        return Err(MalformedMessage { reason: name });
    }
    body.get(offset..offset + characters * 2)
        .ok_or(malformed("a LOGIN7 field lies outside the message"))
}

fn text_field(body: &[u8], at: usize, name: &'static str) -> Result<String, MalformedMessage> {
    let bytes = field(body, at, name)?;
    char::decode_utf16(utf16_units(bytes))
        .collect::<Result<String, _>>()
        .map_err(|_| malformed("a LOGIN7 name is not valid UTF-16"))
}

fn deobfuscated(bytes: &[u8]) -> Option<Password> {
    let mut clear: Vec<u8> = bytes
        .iter()
        .map(|&byte| (byte ^ 0xA5).rotate_left(4))
        .collect();
    let mut units = utf16_units(&clear);
    clear.fill(0);
    std::hint::black_box(&clear);
    let mut text = String::with_capacity(units.len() * 3);
    let mut valid = true;
    for decoded in char::decode_utf16(units.iter().copied()) {
        match decoded {
            Ok(character) => text.push(character),
            Err(_) => {
                valid = false;
                break;
            }
        }
    }
    units.fill(0);
    std::hint::black_box(&units);
    let password = Password(text.into_bytes());
    valid.then_some(password)
}

/// Reads a LOGIN7 message's body ([MS-TDS] 2.2.6.4): its fixed part, the offset and length of
/// each name (at most 128 characters each, every one inside the message), the password
/// de-obfuscated (each byte XOR 0xA5, then its two halves swapped), and the feature
/// extensions walked only to find a request for federated authentication.
pub fn parse_login7(body: &[u8]) -> Result<Login7, MalformedMessage> {
    if body.len() < LOGIN7_FIXED_BYTES {
        return Err(malformed(
            "the LOGIN7 message is shorter than its fixed part",
        ));
    }
    let length = u32_le(body, 0)? as usize;
    if length < LOGIN7_FIXED_BYTES || length > body.len() {
        return Err(malformed("the LOGIN7 length does not match the message"));
    }
    let body = &body[..length];
    let option_flags2 = body[25];
    let option_flags3 = body[27];
    let password_bytes = field(
        body,
        44,
        "the LOGIN7 password is longer than 128 characters",
    )?;
    let password = deobfuscated(password_bytes);
    let sspi_bytes = usize::from(u16_le(body, 80)?);
    let federated_authentication = if option_flags3 & EXTENSION != 0 {
        asks_federated_authentication(body)?
    } else {
        false
    };
    Ok(Login7 {
        tds_version: u32_le(body, 4)?,
        packet_size: u32_le(body, 8)?,
        host_name: text_field(
            body,
            36,
            "the LOGIN7 host name is longer than 128 characters",
        )?,
        user_name: text_field(
            body,
            40,
            "the LOGIN7 user name is longer than 128 characters",
        )?,
        password,
        app_name: text_field(
            body,
            48,
            "the LOGIN7 application name is longer than 128 characters",
        )?,
        server_name: text_field(
            body,
            52,
            "the LOGIN7 server name is longer than 128 characters",
        )?,
        language: text_field(
            body,
            64,
            "the LOGIN7 language is longer than 128 characters",
        )?,
        database: text_field(
            body,
            68,
            "the LOGIN7 database is longer than 128 characters",
        )?,
        integrated_security: option_flags2 & INTEGRATED_SECURITY != 0,
        sspi_bytes,
        change_password: option_flags3 & CHANGE_PASSWORD != 0,
        federated_authentication,
        odbc_defaults: option_flags2 & 0x02 != 0,
    })
}

fn asks_federated_authentication(body: &[u8]) -> Result<bool, MalformedMessage> {
    let pointer = usize::from(u16_le(body, 56)?);
    let pointer_length = usize::from(u16_le(body, 58)?);
    if pointer_length < 4 {
        return Err(malformed(
            "the LOGIN7 feature extension pointer is too short",
        ));
    }
    let mut at = u32_le(body, pointer)? as usize;
    loop {
        let Some(&feature) = body.get(at) else {
            return Err(malformed(
                "the LOGIN7 feature extensions have no terminator",
            ));
        };
        if feature == FEATURE_TERMINATOR {
            return Ok(false);
        }
        if feature == FEATURE_FED_AUTH {
            return Ok(true);
        }
        let length = u32_le(body, at + 1)? as usize;
        at = at
            .checked_add(5)
            .and_then(|start| start.checked_add(length))
            .filter(|&end| end <= body.len())
            .ok_or(malformed(
                "a LOGIN7 feature extension lies outside the message",
            ))?;
    }
}

/// A SQL batch request: its Transact-SQL text and the transaction descriptor of its headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlBatch {
    /// The batch's text.
    pub text: String,
    /// The transaction descriptor the client sent, read but not enforced in this version.
    pub transaction_descriptor: u64,
}

/// Reads a SQL batch request's body ([MS-TDS] 2.2.6.7): its ALL_HEADERS (a total length, then
/// each header's length, type and data, with exactly one transaction descriptor header, each
/// type at most once) and the batch text in UTF-16, refused when it is not valid UTF-16.
pub fn parse_sql_batch(body: &[u8]) -> Result<SqlBatch, MalformedMessage> {
    let (transaction_descriptor, total) = read_all_headers(body)?;
    let text_bytes = &body[total..];
    if !text_bytes.len().is_multiple_of(2) {
        return Err(malformed("the batch text has an odd number of bytes"));
    }
    let text = char::decode_utf16(utf16_units(text_bytes))
        .collect::<Result<String, _>>()
        .map_err(|_| malformed("the batch text is not valid UTF-16"))?;
    Ok(SqlBatch {
        text,
        transaction_descriptor,
    })
}

/// What a transaction manager request asks for ([MS-TDS] 2.2.6.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransactionRequest {
    /// Begin a transaction (`TM_BEGIN_XACT`).
    Begin {
        /// The isolation level asked for: 0 to keep the current one, then read uncommitted,
        /// read committed, repeatable read, serializable and snapshot (1 to 5).
        isolation_level: u8,
        /// The transaction's name, or empty.
        name: String,
    },
    /// Commit the transaction (`TM_COMMIT_XACT`), and begin another one when asked.
    Commit {
        /// The transaction's name, or empty.
        name: String,
        /// The isolation level and name of a transaction to begin right after, when asked.
        begin_next: Option<(u8, String)>,
    },
    /// Roll the transaction back (`TM_ROLLBACK_XACT`), and begin another one when asked.
    Rollback {
        /// The transaction's or a save point's name, or empty.
        name: String,
        /// The isolation level and name of a transaction to begin right after, when asked.
        begin_next: Option<(u8, String)>,
    },
    /// Set a save point (`TM_SAVE_XACT`).
    Save {
        /// The save point's name.
        name: String,
    },
    /// A request for a distributed transaction (get the DTC address, propagate or promote a
    /// transaction), named by its request type.
    Distributed(u16),
}

/// A transaction manager request: what it asks for and the transaction descriptor of its
/// headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionManagerRequest {
    /// The request.
    pub request: TransactionRequest,
    /// The transaction descriptor the client sent, read but not enforced in this version.
    pub transaction_descriptor: u64,
}

fn b_varchar(body: &[u8], at: &mut usize) -> Result<String, MalformedMessage> {
    let Some(&characters) = body.get(*at) else {
        return Err(malformed("the message ends early"));
    };
    let start = *at + 1;
    let end = start + usize::from(characters) * 2;
    let bytes = body
        .get(start..end)
        .ok_or(malformed("the message ends early"))?;
    *at = end;
    char::decode_utf16(utf16_units(bytes))
        .collect::<Result<String, _>>()
        .map_err(|_| malformed("a transaction name is not valid UTF-16"))
}

fn isolation_level(body: &[u8], at: &mut usize) -> Result<u8, MalformedMessage> {
    let level = *body.get(*at).ok_or(malformed("the message ends early"))?;
    if level > HIGHEST_ISOLATION_LEVEL {
        return Err(malformed("an unknown isolation level"));
    }
    *at += 1;
    Ok(level)
}

fn optional_name(body: &[u8], at: &mut usize) -> Result<String, MalformedMessage> {
    if *at == body.len() {
        return Ok(String::new());
    }
    b_varchar(body, at)
}

fn begin_next(body: &[u8], at: &mut usize) -> Result<Option<(u8, String)>, MalformedMessage> {
    let Some(&flags) = body.get(*at) else {
        return Ok(None);
    };
    *at += 1;
    if flags & BEGIN_NEXT_TRANSACTION == 0 {
        return Ok(None);
    }
    let level = isolation_level(body, at)?;
    let name = optional_name(body, at)?;
    Ok(Some((level, name)))
}

/// Reads a transaction manager request's body ([MS-TDS] 2.2.6.9): its ALL_HEADERS, as
/// [`parse_sql_batch`] reads them, the request type and its payload. A name a client leaves
/// out at the end of the message reads as empty; anything after the payload is refused.
pub fn parse_transaction_manager_request(
    body: &[u8],
) -> Result<TransactionManagerRequest, MalformedMessage> {
    let (transaction_descriptor, start) = read_all_headers(body)?;
    let kind = u16_le(body, start)?;
    let mut at = start + 2;
    let request = match kind {
        TM_BEGIN_XACT => {
            let isolation_level = if at == body.len() {
                0
            } else {
                isolation_level(body, &mut at)?
            };
            TransactionRequest::Begin {
                isolation_level,
                name: optional_name(body, &mut at)?,
            }
        }
        TM_COMMIT_XACT | TM_ROLLBACK_XACT => {
            let name = optional_name(body, &mut at)?;
            let begin_next = begin_next(body, &mut at)?;
            if kind == TM_COMMIT_XACT {
                TransactionRequest::Commit { name, begin_next }
            } else {
                TransactionRequest::Rollback { name, begin_next }
            }
        }
        TM_SAVE_XACT => TransactionRequest::Save {
            name: optional_name(body, &mut at)?,
        },
        TM_GET_DTC_ADDRESS | TM_PROPAGATE_XACT | TM_PROMOTE_XACT => {
            at = body.len();
            TransactionRequest::Distributed(kind)
        }
        _ => return Err(malformed("an unknown transaction manager request")),
    };
    if at != body.len() {
        return Err(malformed(
            "a transaction manager request has bytes after its payload",
        ));
    }
    Ok(TransactionManagerRequest {
        request,
        transaction_descriptor,
    })
}

/// Reads the ALL_HEADERS that start a SQL batch, an RPC and a transaction manager request:
/// the transaction descriptor and where the headers end.
pub(crate) fn read_all_headers(body: &[u8]) -> Result<(u64, usize), MalformedMessage> {
    let total = u32_le(body, 0)? as usize;
    if total < 4 || total > body.len() {
        return Err(malformed(
            "the ALL_HEADERS length does not match the message",
        ));
    }
    let mut at = 4;
    let mut seen = [false; 4];
    let mut transaction_descriptor = None;
    while at < total {
        let length = u32_le(body, at)? as usize;
        let kind = u16_le(body, at + 4)?;
        let end = at
            .checked_add(length)
            .filter(|&end| length >= 6 && end <= total)
            .ok_or(malformed("a header lies outside ALL_HEADERS"))?;
        let slot = match kind {
            HEADER_QUERY_NOTIFICATIONS | HEADER_TRANSACTION_DESCRIPTOR | HEADER_TRACE_ACTIVITY => {
                usize::from(kind)
            }
            _ => return Err(malformed("ALL_HEADERS holds an unknown header type")),
        };
        if seen[slot] {
            return Err(malformed("ALL_HEADERS holds a header type twice"));
        }
        seen[slot] = true;
        if kind == HEADER_TRANSACTION_DESCRIPTOR {
            if length != 18 {
                return Err(malformed(
                    "the transaction descriptor header is not 18 bytes",
                ));
            }
            let mut descriptor = [0u8; 8];
            descriptor.copy_from_slice(&body[at + 6..at + 14]);
            transaction_descriptor = Some(u64::from_le_bytes(descriptor));
        }
        at = end;
    }
    let Some(transaction_descriptor) = transaction_descriptor else {
        return Err(malformed("ALL_HEADERS holds no transaction descriptor"));
    };
    Ok((transaction_descriptor, total))
}

#[cfg(test)]
mod tests {
    use super::{
        ENCRYPT_NOT_SUP, ENCRYPT_OFF, ENCRYPT_ON, PreLogin, TransactionRequest, parse_login7,
        parse_prelogin, parse_sql_batch, parse_transaction_manager_request, prelogin_response,
    };

    fn obfuscated(password: &str) -> Vec<u8> {
        password
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .map(|byte| byte.rotate_left(4) ^ 0xA5)
            .collect()
    }

    pub(crate) fn login7(user: &str, password: &str, database: &str, version: u32) -> Vec<u8> {
        let mut data = Vec::new();
        let mut table = Vec::new();
        let names: [Vec<u8>; 9] = [
            "host".encode_utf16().flat_map(u16::to_le_bytes).collect(),
            user.encode_utf16().flat_map(u16::to_le_bytes).collect(),
            obfuscated(password),
            "app".encode_utf16().flat_map(u16::to_le_bytes).collect(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            database.encode_utf16().flat_map(u16::to_le_bytes).collect(),
        ];
        for name in &names {
            let offset = (94 + data.len()) as u16;
            table.extend_from_slice(&offset.to_le_bytes());
            table.extend_from_slice(&((name.len() / 2) as u16).to_le_bytes());
            data.extend_from_slice(name);
        }
        let mut body = Vec::new();
        body.extend_from_slice(&((94 + data.len()) as u32).to_le_bytes());
        body.extend_from_slice(&version.to_le_bytes());
        body.extend_from_slice(&4096u32.to_le_bytes());
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
        assert_eq!(body.len(), 94);
        body.extend_from_slice(&data);
        body
    }

    #[test]
    fn prelogin_reads_the_options_and_answers_encryption_not_available() {
        let request = parse_prelogin(&[
            0,
            0,
            21,
            0,
            6,
            1,
            0,
            27,
            0,
            1,
            2,
            0,
            28,
            0,
            1,
            4,
            0,
            29,
            0,
            1,
            0xFF,
            16,
            0,
            3,
            0xE8,
            0,
            0,
            ENCRYPT_OFF,
            0,
            1,
        ])
        .unwrap();
        assert_eq!(request.encryption, Some(ENCRYPT_OFF));
        assert!(request.mars);
        assert!(!request.wants_encryption());
        let answer = prelogin_response(&request, [16, 0, 0x03, 0xE8], ENCRYPT_NOT_SUP);
        let parsed = parse_prelogin(&answer).unwrap();
        assert_eq!(parsed.encryption, Some(ENCRYPT_NOT_SUP));
        assert_eq!(parsed.version, [16, 0, 0x03, 0xE8, 0, 0]);
        assert!(!parsed.mars);
        let wants = PreLogin {
            encryption: Some(ENCRYPT_ON),
            ..PreLogin::default()
        };
        assert!(wants.wants_encryption());
        let certificate = PreLogin {
            encryption: Some(0x80),
            ..PreLogin::default()
        };
        assert!(certificate.wants_encryption());
    }

    #[test]
    fn prelogin_without_version_first_or_terminator_or_inside_bounds_is_refused() {
        assert!(parse_prelogin(&[1, 0, 6, 0, 1, 0xFF, 0]).is_err());
        assert!(parse_prelogin(&[0, 0, 6, 0, 6]).is_err());
        assert!(parse_prelogin(&[0, 0, 200, 0, 6, 0xFF]).is_err());
        assert!(parse_prelogin(&[0xFF]).is_err());
        assert!(parse_prelogin(&[]).is_err());
    }

    #[test]
    fn login7_names_and_the_password_come_back_as_the_client_wrote_them() {
        let body = login7("eggdb", "s3cr\u{e9}t", "shop", 0x7400_0004);
        let login = parse_login7(&body).unwrap();
        assert_eq!(login.tds_version, 0x7400_0004);
        assert_eq!(&body[4..8], &[0x04, 0x00, 0x00, 0x74]);
        assert_eq!(login.user_name, "eggdb");
        assert_eq!(login.database, "shop");
        assert_eq!(login.host_name, "host");
        assert_eq!(
            login.password.as_ref().map(|password| password.bytes()),
            Some("s3cr\u{e9}t".as_bytes())
        );
        assert!(login.odbc_defaults);
        assert!(!login.integrated_security);
        for cut in 0..body.len() {
            let _ = parse_login7(&body[..cut]);
        }
    }

    #[test]
    fn a_password_with_an_unpaired_surrogate_reads_as_no_password() {
        let mut body = login7("eggdb", "ab", "shop", 0x7400_0004);
        let offset = usize::from(u16::from_le_bytes([body[44], body[45]]));
        body[offset] = 0x00u8.rotate_left(4) ^ 0xA5;
        body[offset + 1] = 0xD8u8.rotate_left(4) ^ 0xA5;
        let login = parse_login7(&body).unwrap();
        assert!(login.password.is_none());
    }

    fn batch(headers: &[u8], text: &str) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&((headers.len() + 4) as u32).to_le_bytes());
        body.extend_from_slice(headers);
        body.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        body
    }

    fn descriptor_header() -> Vec<u8> {
        let mut header = Vec::new();
        header.extend_from_slice(&18u32.to_le_bytes());
        header.extend_from_slice(&2u16.to_le_bytes());
        header.extend_from_slice(&7u64.to_le_bytes());
        header.extend_from_slice(&1u32.to_le_bytes());
        header
    }

    #[test]
    fn a_batch_needs_exactly_one_transaction_descriptor_header() {
        let parsed = parse_sql_batch(&batch(&descriptor_header(), "SELECT 1")).unwrap();
        assert_eq!(parsed.text, "SELECT 1");
        assert_eq!(parsed.transaction_descriptor, 7);
        assert!(parse_sql_batch(&batch(&[], "SELECT 1")).is_err());
        let twice = [descriptor_header(), descriptor_header()].concat();
        assert!(parse_sql_batch(&batch(&twice, "SELECT 1")).is_err());
        let mut overrun = descriptor_header();
        overrun[0] = 200;
        assert!(parse_sql_batch(&batch(&overrun, "SELECT 1")).is_err());
        let mut odd = batch(&descriptor_header(), "SELECT 1");
        odd.push(0);
        assert!(parse_sql_batch(&odd).is_err());
        let mut surrogate = batch(&descriptor_header(), "");
        surrogate.extend_from_slice(&0xD800u16.to_le_bytes());
        assert!(parse_sql_batch(&surrogate).is_err());
        assert!(parse_sql_batch(&[1, 0, 0]).is_err());
    }

    fn transaction_request(kind: u16, payload: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&22u32.to_le_bytes());
        body.extend_from_slice(&descriptor_header());
        body.extend_from_slice(&kind.to_le_bytes());
        body.extend_from_slice(payload);
        body
    }

    #[test]
    fn transaction_manager_requests_read_their_kind_isolation_level_and_names() {
        let begin =
            parse_transaction_manager_request(&transaction_request(5, &[2, 2, b't', 0, b'x', 0]))
                .unwrap();
        assert_eq!(begin.transaction_descriptor, 7);
        assert_eq!(
            begin.request,
            TransactionRequest::Begin {
                isolation_level: 2,
                name: "tx".into()
            }
        );
        assert_eq!(
            parse_transaction_manager_request(&transaction_request(7, &[0, 0]))
                .unwrap()
                .request,
            TransactionRequest::Commit {
                name: String::new(),
                begin_next: None
            }
        );
        assert_eq!(
            parse_transaction_manager_request(&transaction_request(8, &[0, 1, 4, 0]))
                .unwrap()
                .request,
            TransactionRequest::Rollback {
                name: String::new(),
                begin_next: Some((4, String::new()))
            }
        );
        assert_eq!(
            parse_transaction_manager_request(&transaction_request(7, &[]))
                .unwrap()
                .request,
            TransactionRequest::Commit {
                name: String::new(),
                begin_next: None
            }
        );
        assert_eq!(
            parse_transaction_manager_request(&transaction_request(1, &[9, 9, 9]))
                .unwrap()
                .request,
            TransactionRequest::Distributed(1)
        );
        for bad in [
            transaction_request(5, &[6]),
            transaction_request(5, &[2, 3, b'a', 0]),
            transaction_request(7, &[0, 0, 0]),
            transaction_request(42, &[]),
            transaction_request(5, &[2, 1, 0x00, 0xD8]),
        ] {
            assert!(parse_transaction_manager_request(&bad).is_err());
        }
        let whole = transaction_request(7, &[1, b'n', 0, 1, 2, 1, b'm', 0]);
        for cut in 0..whole.len() {
            let _ = parse_transaction_manager_request(&whole[..cut]);
        }
    }
}
