//! The MySQL client/server protocol's payloads, as MySQL's public protocol documentation
//! describes them: [`parse_handshake_response`], [`parse_command`] and
//! [`parse_execute`] read what clients send and refuse truncated or inconsistent input with a
//! [`PacketError`], never a panic; [`Payload`] builds what the server sends.

use std::fmt;

/// `CLIENT_LONG_PASSWORD`.
pub const CLIENT_LONG_PASSWORD: u32 = 1;
/// `CLIENT_FOUND_ROWS`: report rows matched rather than rows changed.
pub const CLIENT_FOUND_ROWS: u32 = 1 << 1;
/// `CLIENT_LONG_FLAG`.
pub const CLIENT_LONG_FLAG: u32 = 1 << 2;
/// `CLIENT_CONNECT_WITH_DB`: the handshake response names a database.
pub const CLIENT_CONNECT_WITH_DB: u32 = 1 << 3;
/// `CLIENT_COMPRESS`, never offered.
pub const CLIENT_COMPRESS: u32 = 1 << 5;
/// `CLIENT_LOCAL_FILES`, never offered.
pub const CLIENT_LOCAL_FILES: u32 = 1 << 7;
/// `CLIENT_PROTOCOL_41`: the protocol every client since MySQL 4.1 speaks; required.
pub const CLIENT_PROTOCOL_41: u32 = 1 << 9;
/// `CLIENT_SSL`, never offered until EggDB has TLS (D102).
pub const CLIENT_SSL: u32 = 1 << 11;
/// `CLIENT_TRANSACTIONS`: status flags in OK and EOF packets.
pub const CLIENT_TRANSACTIONS: u32 = 1 << 13;
/// `CLIENT_SECURE_CONNECTION`: a length before the authentication response.
pub const CLIENT_SECURE_CONNECTION: u32 = 1 << 15;
/// `CLIENT_MULTI_STATEMENTS`: `COM_QUERY` may hold several statements.
pub const CLIENT_MULTI_STATEMENTS: u32 = 1 << 16;
/// `CLIENT_MULTI_RESULTS`.
pub const CLIENT_MULTI_RESULTS: u32 = 1 << 17;
/// `CLIENT_PS_MULTI_RESULTS`.
pub const CLIENT_PS_MULTI_RESULTS: u32 = 1 << 18;
/// `CLIENT_PLUGIN_AUTH`: authentication methods are named.
pub const CLIENT_PLUGIN_AUTH: u32 = 1 << 19;
/// `CLIENT_CONNECT_ATTRS`: the handshake response carries attributes.
pub const CLIENT_CONNECT_ATTRS: u32 = 1 << 20;
/// `CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA`: a length-encoded authentication response.
pub const CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA: u32 = 1 << 21;
/// `CLIENT_DEPRECATE_EOF`, not offered yet: result sets end with EOF packets.
pub const CLIENT_DEPRECATE_EOF: u32 = 1 << 24;

/// Everything EggDB's server offers in its handshake.
pub const SERVER_CAPABILITIES: u32 = CLIENT_LONG_PASSWORD
    | CLIENT_FOUND_ROWS
    | CLIENT_LONG_FLAG
    | CLIENT_CONNECT_WITH_DB
    | CLIENT_PROTOCOL_41
    | CLIENT_TRANSACTIONS
    | CLIENT_SECURE_CONNECTION
    | CLIENT_MULTI_STATEMENTS
    | CLIENT_MULTI_RESULTS
    | CLIENT_PS_MULTI_RESULTS
    | CLIENT_PLUGIN_AUTH
    | CLIENT_CONNECT_ATTRS
    | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA;

/// `SERVER_STATUS_IN_TRANS`.
pub const STATUS_IN_TRANSACTION: u16 = 1;
/// `SERVER_STATUS_AUTOCOMMIT`.
pub const STATUS_AUTOCOMMIT: u16 = 1 << 1;
/// `SERVER_MORE_RESULTS_EXISTS`.
pub const STATUS_MORE_RESULTS: u16 = 1 << 3;

/// The collation number of `utf8mb4_0900_ai_ci`, the server's and every text column's.
pub const UTF8MB4_0900_AI_CI: u8 = 255;
/// The collation number of `binary`, for numeric columns.
pub const BINARY_COLLATION: u16 = 63;

/// `MYSQL_TYPE_TINY`.
pub const TYPE_TINY: u8 = 1;
/// `MYSQL_TYPE_SHORT`.
pub const TYPE_SHORT: u8 = 2;
/// `MYSQL_TYPE_LONG`.
pub const TYPE_LONG: u8 = 3;
/// `MYSQL_TYPE_FLOAT`.
pub const TYPE_FLOAT: u8 = 4;
/// `MYSQL_TYPE_DOUBLE`.
pub const TYPE_DOUBLE: u8 = 5;
/// `MYSQL_TYPE_NULL`.
pub const TYPE_NULL: u8 = 6;
/// `MYSQL_TYPE_TIMESTAMP`.
pub const TYPE_TIMESTAMP: u8 = 7;
/// `MYSQL_TYPE_LONGLONG`.
pub const TYPE_LONGLONG: u8 = 8;
/// `MYSQL_TYPE_INT24`.
pub const TYPE_INT24: u8 = 9;
/// `MYSQL_TYPE_DATE`.
pub const TYPE_DATE: u8 = 10;
/// `MYSQL_TYPE_TIME`.
pub const TYPE_TIME: u8 = 11;
/// `MYSQL_TYPE_DATETIME`.
pub const TYPE_DATETIME: u8 = 12;
/// `MYSQL_TYPE_YEAR`.
pub const TYPE_YEAR: u8 = 13;
/// `MYSQL_TYPE_JSON`.
pub const TYPE_JSON: u8 = 245;
/// `MYSQL_TYPE_TINY_BLOB`; `MYSQL_TYPE_MEDIUM_BLOB` and `MYSQL_TYPE_LONG_BLOB` follow it.
pub const TYPE_TINY_BLOB: u8 = 249;
/// `MYSQL_TYPE_BLOB`.
pub const TYPE_BLOB: u8 = 252;
/// `MYSQL_TYPE_NEWDECIMAL`.
pub const TYPE_NEWDECIMAL: u8 = 246;
/// `MYSQL_TYPE_VAR_STRING`.
pub const TYPE_VAR_STRING: u8 = 253;

/// Why a payload could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacketError(pub String);

impl fmt::Display for PacketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PacketError {}

fn truncated(what: &str) -> PacketError {
    PacketError(format!("the packet ends inside its {what}"))
}

struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Reader<'b> {
    fn new(bytes: &'b [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn rest(&self) -> &'b [u8] {
        self.bytes.get(self.at..).unwrap_or(&[])
    }

    fn is_empty(&self) -> bool {
        self.at >= self.bytes.len()
    }

    fn take(&mut self, count: usize, what: &str) -> Result<&'b [u8], PacketError> {
        let end = self.at.checked_add(count).ok_or_else(|| truncated(what))?;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| truncated(what))?;
        self.at = end;
        Ok(slice)
    }

    fn u8(&mut self, what: &str) -> Result<u8, PacketError> {
        Ok(self.take(1, what)?[0])
    }

    fn u16(&mut self, what: &str) -> Result<u16, PacketError> {
        let bytes = self.take(2, what)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self, what: &str) -> Result<u32, PacketError> {
        let bytes = self.take(4, what)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self, what: &str) -> Result<u64, PacketError> {
        let bytes = self.take(8, what)?;
        let mut array = [0u8; 8];
        array.copy_from_slice(bytes);
        Ok(u64::from_le_bytes(array))
    }

    fn null_terminated(&mut self, what: &str) -> Result<&'b [u8], PacketError> {
        let rest = self.rest();
        let length = rest
            .iter()
            .position(|&byte| byte == 0)
            .ok_or_else(|| PacketError(format!("the {what} has no terminating zero byte")))?;
        self.at += length + 1;
        Ok(&rest[..length])
    }

    fn length_encoded(&mut self, what: &str) -> Result<u64, PacketError> {
        match self.u8(what)? {
            first @ 0..=0xFA => Ok(u64::from(first)),
            0xFC => self.u16(what).map(u64::from),
            0xFD => {
                let bytes = self.take(3, what)?;
                Ok(u64::from(bytes[0]) | u64::from(bytes[1]) << 8 | u64::from(bytes[2]) << 16)
            }
            0xFE => self.u64(what),
            _ => Err(PacketError(format!("the {what} has an invalid length"))),
        }
    }

    fn length_encoded_bytes(&mut self, what: &str) -> Result<&'b [u8], PacketError> {
        let length = self.length_encoded(what)?;
        let length = usize::try_from(length).map_err(|_| truncated(what))?;
        self.take(length, what)
    }
}

fn text(bytes: &[u8], what: &str) -> Result<String, PacketError> {
    String::from_utf8(bytes.to_vec()).map_err(|_| PacketError(format!("the {what} is not UTF-8")))
}

/// A client's `HandshakeResponse41`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeResponse {
    /// The capabilities the client asks for.
    pub capabilities: u32,
    /// The largest packet the client sends.
    pub max_packet_size: u32,
    /// The client's collation number.
    pub collation: u8,
    /// The user name.
    pub user: String,
    /// The authentication response, computed from the password and the server's nonce.
    pub auth_response: Vec<u8>,
    /// The database to start in, when the client names one.
    pub database: Option<String>,
    /// The authentication method the response was computed with, when the client names one.
    pub auth_plugin: Option<String>,
}

/// Reads a client's `HandshakeResponse41` (protocol 4.1 only: older clients are refused).
pub fn parse_handshake_response(body: &[u8]) -> Result<HandshakeResponse, PacketError> {
    let mut reader = Reader::new(body);
    let capabilities = reader.u32("capability flags")?;
    if capabilities & CLIENT_PROTOCOL_41 == 0 {
        return Err(PacketError(
            "the client does not speak protocol 4.1 (CLIENT_PROTOCOL_41)".into(),
        ));
    }
    let max_packet_size = reader.u32("maximum packet size")?;
    let collation = reader.u8("character set")?;
    reader.take(23, "reserved bytes")?;
    if capabilities & CLIENT_SSL != 0 && reader.is_empty() {
        return Err(PacketError(
            "the client asked for TLS, which this server does not offer yet".into(),
        ));
    }
    let user = text(reader.null_terminated("user name")?, "user name")?;
    let auth_response = if capabilities & CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA != 0 {
        reader.length_encoded_bytes("authentication response")?
    } else if capabilities & CLIENT_SECURE_CONNECTION != 0 {
        let length = reader.u8("authentication response")?;
        reader.take(usize::from(length), "authentication response")?
    } else {
        reader.null_terminated("authentication response")?
    }
    .to_vec();
    let database = if capabilities & CLIENT_CONNECT_WITH_DB != 0 && !reader.is_empty() {
        Some(text(
            reader.null_terminated("database name")?,
            "database name",
        )?)
        .filter(|database| !database.is_empty())
    } else {
        None
    };
    let auth_plugin = if capabilities & CLIENT_PLUGIN_AUTH != 0 && !reader.is_empty() {
        Some(text(
            reader.null_terminated("authentication method")?,
            "authentication method",
        )?)
    } else {
        None
    };
    if capabilities & CLIENT_CONNECT_ATTRS != 0 && !reader.is_empty() {
        let length = reader.length_encoded("connection attributes")?;
        let length = usize::try_from(length).map_err(|_| truncated("connection attributes"))?;
        let attributes = reader.take(length, "connection attributes")?;
        let mut attributes = Reader::new(attributes);
        while !attributes.is_empty() {
            attributes.length_encoded_bytes("connection attribute")?;
            attributes.length_encoded_bytes("connection attribute")?;
        }
    }
    Ok(HandshakeResponse {
        capabilities,
        max_packet_size,
        collation,
        user,
        auth_response,
        database,
        auth_plugin,
    })
}

/// A command a client sends once it is authenticated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `COM_QUIT`.
    Quit,
    /// `COM_INIT_DB`: change the default database.
    InitDb(String),
    /// `COM_QUERY`: run SQL text.
    Query(String),
    /// `COM_FIELD_LIST`: describe a table's columns.
    FieldList(String),
    /// `COM_PING`.
    Ping,
    /// `COM_STMT_PREPARE`.
    Prepare(String),
    /// `COM_STMT_EXECUTE`, whose parameters are read with [`parse_execute`] once the
    /// statement's parameter count is known.
    Execute {
        /// The statement.
        statement: u32,
        /// The rest of the payload, from the flags on.
        body: Vec<u8>,
    },
    /// `COM_STMT_SEND_LONG_DATA`: part of a parameter's value, sent ahead of the execute.
    SendLongData {
        /// The statement.
        statement: u32,
        /// The parameter, from 0.
        parameter: u16,
        /// The bytes to add to its value.
        data: Vec<u8>,
    },
    /// `COM_STMT_CLOSE`.
    Close(u32),
    /// `COM_STMT_RESET`.
    Reset(u32),
    /// `COM_SET_OPTION`: multi-statements on (0) or off (1).
    SetOption(u16),
    /// `COM_RESET_CONNECTION`.
    ResetConnection,
    /// Any other command, by its number.
    Other(u8),
}

/// Reads one command payload.
pub fn parse_command(body: &[u8]) -> Result<Command, PacketError> {
    let mut reader = Reader::new(body);
    let code = reader.u8("command")?;
    let statement = |reader: &mut Reader<'_>| reader.u32("statement id");
    Ok(match code {
        0x01 => Command::Quit,
        0x02 => Command::InitDb(text(reader.rest(), "database name")?),
        0x03 => Command::Query(text(reader.rest(), "query")?),
        0x04 => {
            let table = text(reader.null_terminated("table name")?, "table name")?;
            Command::FieldList(table)
        }
        0x0E => Command::Ping,
        0x16 => Command::Prepare(text(reader.rest(), "statement")?),
        0x17 => Command::Execute {
            statement: statement(&mut reader)?,
            body: reader.rest().to_vec(),
        },
        0x18 => Command::SendLongData {
            statement: statement(&mut reader)?,
            parameter: reader.u16("parameter number")?,
            data: reader.rest().to_vec(),
        },
        0x19 => Command::Close(statement(&mut reader)?),
        0x1A => Command::Reset(statement(&mut reader)?),
        0x1B => Command::SetOption(reader.u16("option")?),
        0x1F => Command::ResetConnection,
        other => Command::Other(other),
    })
}

/// A parameter value a client bound in `COM_STMT_EXECUTE`.
#[derive(Debug, Clone, PartialEq)]
pub enum BoundValue {
    /// NULL.
    Null,
    /// A signed integer.
    Integer(i64),
    /// An unsigned integer above `i64::MAX`.
    Unsigned(u64),
    /// A floating-point number.
    Float(f64),
    /// Text, or bytes, or a date and time written as text.
    Bytes(Vec<u8>),
}

/// The type a parameter was bound with: the MySQL type and whether it is unsigned.
pub type ParameterType = (u8, bool);

/// Reads the parameters of a `COM_STMT_EXECUTE` payload after its statement id, for a
/// statement with `count` parameters. `previous` are the types bound last time, used when the
/// client does not send them again; `long_data` marks parameters whose value came by
/// `COM_STMT_SEND_LONG_DATA`, which are not in the payload. Returns the types and values.
pub fn parse_execute(
    body: &[u8],
    count: usize,
    previous: &[ParameterType],
    long_data: &[bool],
) -> Result<(Vec<ParameterType>, Vec<BoundValue>), PacketError> {
    let mut reader = Reader::new(body);
    reader.u8("cursor flags")?;
    reader.u32("iteration count")?;
    if count == 0 {
        return Ok((Vec::new(), Vec::new()));
    }
    let bitmap = reader.take(count.div_ceil(8), "null bitmap")?;
    let types = if reader.u8("new parameters bound flag")? == 1 {
        let mut types = Vec::with_capacity(count);
        for _ in 0..count {
            let bytes = reader.take(2, "parameter types")?;
            types.push((bytes[0], bytes[1] & 0x80 != 0));
        }
        types
    } else if previous.len() == count {
        previous.to_vec()
    } else {
        return Err(PacketError(
            "the first execute of a statement must send its parameter types".into(),
        ));
    };
    let mut values = Vec::with_capacity(count);
    for (index, &(kind, unsigned)) in types.iter().enumerate() {
        if bitmap[index / 8] & (1 << (index % 8)) != 0 {
            values.push(BoundValue::Null);
            continue;
        }
        if long_data.get(index).copied().unwrap_or(false) {
            values.push(BoundValue::Bytes(Vec::new()));
            continue;
        }
        values.push(bound_value(&mut reader, kind, unsigned)?);
    }
    Ok((types, values))
}

fn signed(value: u64, unsigned: bool, bits: u32) -> BoundValue {
    if unsigned {
        return i64::try_from(value).map_or(BoundValue::Unsigned(value), BoundValue::Integer);
    }
    let shift = 64 - bits;
    BoundValue::Integer(((value << shift) as i64) >> shift)
}

fn bound_value(
    reader: &mut Reader<'_>,
    kind: u8,
    unsigned: bool,
) -> Result<BoundValue, PacketError> {
    let what = "parameter value";
    Ok(match kind {
        TYPE_NULL => BoundValue::Null,
        TYPE_TINY => signed(u64::from(reader.u8(what)?), unsigned, 8),
        TYPE_SHORT | TYPE_YEAR => signed(u64::from(reader.u16(what)?), unsigned, 16),
        TYPE_LONG | TYPE_INT24 => signed(u64::from(reader.u32(what)?), unsigned, 32),
        TYPE_LONGLONG => signed(reader.u64(what)?, unsigned, 64),
        TYPE_FLOAT => BoundValue::Float(f64::from(f32::from_bits(reader.u32(what)?))),
        TYPE_DOUBLE => BoundValue::Float(f64::from_bits(reader.u64(what)?)),
        TYPE_DATE | TYPE_DATETIME | TYPE_TIMESTAMP => {
            let length = reader.u8(what)?;
            let fields = reader.take(usize::from(length), what)?;
            BoundValue::Bytes(date_time_text(fields)?.into_bytes())
        }
        TYPE_TIME => {
            let length = reader.u8(what)?;
            let fields = reader.take(usize::from(length), what)?;
            BoundValue::Bytes(time_text(fields)?.into_bytes())
        }
        _ => BoundValue::Bytes(reader.length_encoded_bytes(what)?.to_vec()),
    })
}

fn date_time_text(fields: &[u8]) -> Result<String, PacketError> {
    let mut reader = Reader::new(fields);
    let what = "date";
    if fields.is_empty() {
        return Ok("0000-00-00 00:00:00".into());
    }
    let year = reader.u16(what)?;
    let month = reader.u8(what)?;
    let day = reader.u8(what)?;
    if fields.len() == 4 {
        return Ok(format!("{year:04}-{month:02}-{day:02}"));
    }
    let hour = reader.u8(what)?;
    let minute = reader.u8(what)?;
    let second = reader.u8(what)?;
    let mut shown = format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}");
    if fields.len() >= 11 {
        shown.push_str(&format!(".{:06}", reader.u32(what)?));
    } else if fields.len() != 7 {
        return Err(PacketError("a date has an invalid length".into()));
    }
    Ok(shown)
}

fn time_text(fields: &[u8]) -> Result<String, PacketError> {
    if fields.is_empty() {
        return Ok("00:00:00".into());
    }
    let mut reader = Reader::new(fields);
    let what = "time";
    let negative = reader.u8(what)? == 1;
    let days = reader.u32(what)?;
    let hour = reader.u8(what)?;
    let minute = reader.u8(what)?;
    let second = reader.u8(what)?;
    let hours = u64::from(days) * 24 + u64::from(hour);
    let sign = if negative { "-" } else { "" };
    let mut shown = format!("{sign}{hours:02}:{minute:02}:{second:02}");
    if fields.len() >= 12 {
        shown.push_str(&format!(".{:06}", reader.u32(what)?));
    } else if fields.len() != 8 {
        return Err(PacketError("a time has an invalid length".into()));
    }
    Ok(shown)
}

/// A payload the server is building.
#[derive(Debug, Default)]
pub struct Payload(pub Vec<u8>);

impl Payload {
    /// An empty payload.
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Adds one byte.
    pub fn u8(&mut self, value: u8) -> &mut Self {
        self.0.push(value);
        self
    }

    /// Adds a little-endian 16-bit integer.
    pub fn u16(&mut self, value: u16) -> &mut Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }

    /// Adds a little-endian 32-bit integer.
    pub fn u32(&mut self, value: u32) -> &mut Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }

    /// Adds bytes as they are.
    pub fn bytes(&mut self, value: &[u8]) -> &mut Self {
        self.0.extend_from_slice(value);
        self
    }

    /// Adds bytes and a terminating zero byte.
    pub fn null_terminated(&mut self, value: &[u8]) -> &mut Self {
        self.0.extend_from_slice(value);
        self.0.push(0);
        self
    }

    /// Adds a length-encoded integer.
    pub fn length_encoded(&mut self, value: u64) -> &mut Self {
        match value {
            0..=0xFA => self.0.push(value as u8),
            0xFB..=0xFFFF => {
                self.0.push(0xFC);
                self.0.extend_from_slice(&(value as u16).to_le_bytes());
            }
            0x1_0000..=0xFF_FFFF => {
                self.0.push(0xFD);
                self.0.extend_from_slice(&(value as u32).to_le_bytes()[..3]);
            }
            _ => {
                self.0.push(0xFE);
                self.0.extend_from_slice(&value.to_le_bytes());
            }
        }
        self
    }

    /// Adds a length-encoded string.
    pub fn length_encoded_bytes(&mut self, value: &[u8]) -> &mut Self {
        self.length_encoded(value.len() as u64);
        self.0.extend_from_slice(value);
        self
    }
}

/// An OK packet's payload.
pub fn ok_packet(affected_rows: u64, last_insert_id: u64, status: u16, warnings: u16) -> Vec<u8> {
    let mut payload = Payload::new();
    payload
        .u8(0x00)
        .length_encoded(affected_rows)
        .length_encoded(last_insert_id)
        .u16(status)
        .u16(warnings);
    payload.0
}

/// An EOF packet's payload.
pub fn eof_packet(warnings: u16, status: u16) -> Vec<u8> {
    let mut payload = Payload::new();
    payload.u8(0xFE).u16(warnings).u16(status);
    payload.0
}

/// An ERR packet's payload: MySQL's error number, SQLSTATE and message.
pub fn err_packet(code: u16, state: &str, message: &str) -> Vec<u8> {
    let mut payload = Payload::new();
    let state = if state.len() == 5 { state } else { "HY000" };
    payload
        .u8(0xFF)
        .u16(code)
        .u8(b'#')
        .bytes(state.as_bytes())
        .bytes(message.as_bytes());
    payload.0
}

/// A column of a result set, as a `ColumnDefinition41` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDefinition {
    /// The column's name.
    pub name: String,
    /// The MySQL type.
    pub kind: u8,
    /// The longest value's length, in bytes.
    pub length: u32,
    /// The decimals shown, 31 for floating point.
    pub decimals: u8,
}

/// A `ColumnDefinition41` payload.
pub fn column_definition_packet(column: &ColumnDefinition, schema: &str) -> Vec<u8> {
    let collation = if column.kind == TYPE_VAR_STRING {
        u16::from(UTF8MB4_0900_AI_CI)
    } else {
        BINARY_COLLATION
    };
    let flags: u16 = if column.kind == TYPE_VAR_STRING {
        0
    } else {
        0x80
    };
    let mut payload = Payload::new();
    payload
        .length_encoded_bytes(b"def")
        .length_encoded_bytes(schema.as_bytes())
        .length_encoded_bytes(b"")
        .length_encoded_bytes(b"")
        .length_encoded_bytes(column.name.as_bytes())
        .length_encoded_bytes(column.name.as_bytes())
        .length_encoded(0x0C)
        .u16(collation)
        .u32(column.length)
        .u8(column.kind)
        .u16(flags)
        .u8(column.decimals)
        .u16(0);
    payload.0
}

#[cfg(test)]
mod tests {
    use super::{
        BoundValue, CLIENT_CONNECT_WITH_DB, CLIENT_PLUGIN_AUTH,
        CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA, CLIENT_PROTOCOL_41, CLIENT_SECURE_CONNECTION,
        Command, Payload, TYPE_LONGLONG, TYPE_TINY, TYPE_VAR_STRING, parse_command, parse_execute,
        parse_handshake_response,
    };

    fn response(capabilities: u32) -> Vec<u8> {
        let mut payload = Payload::new();
        payload
            .u32(capabilities)
            .u32(1 << 24)
            .u8(255)
            .bytes(&[0; 23])
            .null_terminated(b"ada")
            .u8(3)
            .bytes(b"abc")
            .null_terminated(b"shop")
            .null_terminated(b"caching_sha2_password");
        payload.0
    }

    #[test]
    fn a_handshake_response_is_read_by_its_capabilities() {
        let capabilities = CLIENT_PROTOCOL_41
            | CLIENT_SECURE_CONNECTION
            | CLIENT_CONNECT_WITH_DB
            | CLIENT_PLUGIN_AUTH;
        let parsed = parse_handshake_response(&response(capabilities)).unwrap();
        assert_eq!(parsed.user, "ada");
        assert_eq!(parsed.auth_response, b"abc");
        assert_eq!(parsed.database.as_deref(), Some("shop"));
        assert_eq!(parsed.auth_plugin.as_deref(), Some("caching_sha2_password"));
        assert!(parse_handshake_response(&response(CLIENT_SECURE_CONNECTION)).is_err());
        let lenenc = capabilities | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA;
        assert_eq!(
            parse_handshake_response(&response(lenenc))
                .unwrap()
                .auth_response,
            b"abc"
        );
        let whole = response(capabilities);
        for cut in 0..whole.len() - 1 {
            let _ = parse_handshake_response(&whole[..cut]);
        }
    }

    #[test]
    fn execute_reads_null_bitmaps_types_and_values() {
        let mut payload = Payload::new();
        payload
            .u8(0)
            .u32(1)
            .u8(0b010)
            .u8(1)
            .bytes(&[TYPE_LONGLONG, 0, TYPE_TINY, 0, TYPE_VAR_STRING, 0])
            .bytes(&(-5i64).to_le_bytes())
            .length_encoded_bytes(b"hi");
        let (types, values) = parse_execute(&payload.0, 3, &[], &[]).unwrap();
        assert_eq!(types.len(), 3);
        assert_eq!(
            values,
            vec![
                BoundValue::Integer(-5),
                BoundValue::Null,
                BoundValue::Bytes(b"hi".to_vec())
            ]
        );
        assert!(parse_execute(&payload.0[..payload.0.len() - 1], 3, &[], &[]).is_err());
        assert!(parse_execute(&[0, 1, 0, 0, 0, 0, 0], 3, &[], &[]).is_err());
    }

    #[test]
    fn commands_are_read_by_their_first_byte() {
        assert_eq!(parse_command(&[0x01]), Ok(Command::Quit));
        assert_eq!(
            parse_command(b"\x03SELECT 1"),
            Ok(Command::Query("SELECT 1".into()))
        );
        assert!(parse_command(&[]).is_err());
        assert!(parse_command(&[0x17, 1, 2]).is_err());
        assert_eq!(parse_command(&[0x99]), Ok(Command::Other(0x99)));
    }
}
