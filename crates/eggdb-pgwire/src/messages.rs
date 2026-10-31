//! The protocol's messages: parsing what clients send, from the startup packet on, and
//! building what the server sends back.

use std::fmt;

/// The request code of an `SSLRequest` startup packet.
pub const SSL_REQUEST_CODE: u32 = 80_877_103;
/// The request code of a `GSSENCRequest` startup packet.
pub const GSSENC_REQUEST_CODE: u32 = 80_877_104;
/// The request code of a `CancelRequest` startup packet.
pub const CANCEL_REQUEST_CODE: u32 = 80_877_102;
/// The protocol version 3.0, as a startup packet carries it.
pub const PROTOCOL_3_0: u32 = 196_608;

/// The largest startup packet a server accepts, as PostgreSQL limits it.
pub const MAX_STARTUP_PACKET_BYTES: usize = 10_000;

/// A frontend message this server could not read, with why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedMessage(pub String);

impl fmt::Display for MalformedMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MalformedMessage {}

fn malformed(what: impl Into<String>) -> MalformedMessage {
    MalformedMessage(what.into())
}

/// Reads the fields of one message body in order, refusing to read past its end.
#[derive(Debug)]
pub struct Fields<'b> {
    body: &'b [u8],
    at: usize,
}

impl<'b> Fields<'b> {
    /// Fields over `body`.
    pub fn new(body: &'b [u8]) -> Self {
        Self { body, at: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'b [u8], MalformedMessage> {
        let end = self
            .at
            .checked_add(count)
            .filter(|end| *end <= self.body.len())
            .ok_or_else(|| malformed("insufficient data left in message"))?;
        let bytes = &self.body[self.at..end];
        self.at = end;
        Ok(bytes)
    }

    /// A byte.
    pub fn u8(&mut self) -> Result<u8, MalformedMessage> {
        Ok(self.take(1)?[0])
    }

    /// A big-endian 16-bit integer.
    pub fn i16(&mut self) -> Result<i16, MalformedMessage> {
        let bytes = self.take(2)?;
        Ok(i16::from_be_bytes([bytes[0], bytes[1]]))
    }

    /// A big-endian 32-bit integer.
    pub fn i32(&mut self) -> Result<i32, MalformedMessage> {
        let bytes = self.take(4)?;
        Ok(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// A count given as a 16-bit integer, refused when negative.
    pub fn count(&mut self) -> Result<usize, MalformedMessage> {
        usize::try_from(self.i16()?).map_err(|_| malformed("negative count in message"))
    }

    /// `count` bytes.
    pub fn bytes(&mut self, count: usize) -> Result<&'b [u8], MalformedMessage> {
        self.take(count)
    }

    /// A NUL-terminated UTF-8 string.
    pub fn string(&mut self) -> Result<&'b str, MalformedMessage> {
        let rest = &self.body[self.at..];
        let end = rest
            .iter()
            .position(|&byte| byte == 0)
            .ok_or_else(|| malformed("invalid string in message"))?;
        let text = std::str::from_utf8(&rest[..end])
            .map_err(|_| malformed("invalid byte sequence for encoding \"UTF8\""))?;
        self.at += end + 1;
        Ok(text)
    }

    /// Fails unless every byte was read.
    pub fn finish(&self) -> Result<(), MalformedMessage> {
        if self.at == self.body.len() {
            Ok(())
        } else {
            Err(malformed("invalid message format"))
        }
    }
}

/// The first packet of a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupRequest {
    /// `SSLRequest`: the client asks for TLS.
    Ssl,
    /// `GSSENCRequest`: the client asks for GSSAPI encryption.
    GssEncryption,
    /// `CancelRequest`: cancel a query running on another connection.
    Cancel {
        /// The process ID from that connection's `BackendKeyData`.
        process_id: i32,
        /// The secret key from that connection's `BackendKeyData`.
        secret_key: Vec<u8>,
    },
    /// `StartupMessage`: the protocol version and the parameters, in order.
    Startup {
        /// The major version, 3 for this protocol.
        major: u16,
        /// The minor version.
        minor: u16,
        /// The parameter names and values, `user`, `database` and the others.
        parameters: Vec<(String, String)>,
    },
}

/// Parses a startup packet's body, after its length word.
pub fn parse_startup(body: &[u8]) -> Result<StartupRequest, MalformedMessage> {
    let mut fields = Fields::new(body);
    let code = fields.i32()? as u32;
    match code {
        SSL_REQUEST_CODE => {
            fields.finish()?;
            Ok(StartupRequest::Ssl)
        }
        GSSENC_REQUEST_CODE => {
            fields.finish()?;
            Ok(StartupRequest::GssEncryption)
        }
        CANCEL_REQUEST_CODE => {
            let process_id = fields.i32()?;
            let secret_key = body[8..].to_vec();
            if secret_key.is_empty() || secret_key.len() > 256 {
                return Err(malformed("invalid cancel request"));
            }
            Ok(StartupRequest::Cancel {
                process_id,
                secret_key,
            })
        }
        _ => {
            let major = (code >> 16) as u16;
            let minor = (code & 0xffff) as u16;
            let mut parameters = Vec::new();
            if major == 3 {
                loop {
                    let name = fields.string()?;
                    if name.is_empty() {
                        break;
                    }
                    let value = fields.string()?;
                    parameters.push((name.to_owned(), value.to_owned()));
                }
                fields.finish()?;
            }
            Ok(StartupRequest::Startup {
                major,
                minor,
                parameters,
            })
        }
    }
}

/// Text (0) or binary (1), the two formats of a parameter or a result column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// The type's text form.
    Text,
    /// The type's binary form.
    Binary,
}

impl Format {
    fn from_code(code: i16) -> Result<Self, MalformedMessage> {
        match code {
            0 => Ok(Self::Text),
            1 => Ok(Self::Binary),
            _ => Err(malformed(format!("invalid format code {code}"))),
        }
    }

    /// The format's code in the protocol.
    pub fn code(self) -> i16 {
        match self {
            Self::Text => 0,
            Self::Binary => 1,
        }
    }
}

/// Whether a `Describe` or a `Close` is about a prepared statement or a portal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// `S`: a prepared statement.
    Statement,
    /// `P`: a portal.
    Portal,
}

/// A message from the client after startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrontendMessage {
    /// `Q`: run the statements of a query string.
    Query(String),
    /// `P`: prepare a statement.
    Parse {
        /// The statement's name; empty for the unnamed statement.
        name: String,
        /// The statement's text.
        query: String,
        /// The parameter types the client declared; 0 for unspecified.
        parameter_types: Vec<u32>,
    },
    /// `B`: bind parameters to a prepared statement, making a portal.
    Bind {
        /// The portal's name; empty for the unnamed portal.
        portal: String,
        /// The prepared statement's name.
        statement: String,
        /// The parameters' formats: none (all text), one for all, or one each.
        parameter_formats: Vec<Format>,
        /// The values, `None` for NULL.
        parameters: Vec<Option<Vec<u8>>>,
        /// The result columns' formats: none (all text), one for all, or one each.
        result_formats: Vec<Format>,
    },
    /// `D`: describe a statement or a portal.
    Describe(Target, String),
    /// `E`: run a portal, returning at most `max_rows` rows (0 for all).
    Execute {
        /// The portal's name.
        portal: String,
        /// The most rows to return, 0 for no limit.
        max_rows: i32,
    },
    /// `C`: close a statement or a portal.
    Close(Target, String),
    /// `S`: end an extended query cycle.
    Sync,
    /// `H`: send what is buffered.
    Flush,
    /// `X`: end the connection.
    Terminate,
    /// `p`: a SASL message during authentication.
    AuthenticationResponse(Vec<u8>),
    /// `d`: a piece of `COPY` data; outside a copy it is ignored, so its bytes are not kept.
    CopyData,
    /// `c`: the end of a `COPY`'s data; any body is ignored, as the message is dropped outside
    /// a copy.
    CopyDone,
    /// `f`: the client fails a `COPY ... FROM STDIN`, with its reason (read leniently, since
    /// the message is dropped outside a copy).
    CopyFail(String),
    /// A message type this server does not handle, such as a function call.
    Other(u8),
}

fn formats(fields: &mut Fields<'_>) -> Result<Vec<Format>, MalformedMessage> {
    let count = fields.count()?;
    (0..count)
        .map(|_| Format::from_code(fields.i16()?))
        .collect()
}

fn target(code: u8) -> Result<Target, MalformedMessage> {
    match code {
        b'S' => Ok(Target::Statement),
        b'P' => Ok(Target::Portal),
        _ => Err(malformed(format!(
            "invalid DESCRIBE or CLOSE message subtype {code}"
        ))),
    }
}

/// Parses a message from its type byte and its body.
pub fn parse_message(tag: u8, body: &[u8]) -> Result<FrontendMessage, MalformedMessage> {
    let mut fields = Fields::new(body);
    let message = match tag {
        b'Q' => FrontendMessage::Query(fields.string()?.to_owned()),
        b'P' => {
            let name = fields.string()?.to_owned();
            let query = fields.string()?.to_owned();
            let count = fields.count()?;
            let parameter_types = (0..count)
                .map(|_| fields.i32().map(|oid| oid as u32))
                .collect::<Result<_, _>>()?;
            FrontendMessage::Parse {
                name,
                query,
                parameter_types,
            }
        }
        b'B' => {
            let portal = fields.string()?.to_owned();
            let statement = fields.string()?.to_owned();
            let parameter_formats = formats(&mut fields)?;
            let count = fields.count()?;
            let mut parameters = Vec::with_capacity(count.min(body.len() / 4));
            for _ in 0..count {
                let length = fields.i32()?;
                parameters.push(match length {
                    -1 => None,
                    length if length < 0 => {
                        return Err(malformed(format!("invalid parameter length {length}")));
                    }
                    length => Some(fields.bytes(length as usize)?.to_vec()),
                });
            }
            let result_formats = formats(&mut fields)?;
            FrontendMessage::Bind {
                portal,
                statement,
                parameter_formats,
                parameters,
                result_formats,
            }
        }
        b'D' => {
            let target = target(fields.u8()?)?;
            FrontendMessage::Describe(target, fields.string()?.to_owned())
        }
        b'E' => FrontendMessage::Execute {
            portal: fields.string()?.to_owned(),
            max_rows: fields.i32()?,
        },
        b'C' => {
            let target = target(fields.u8()?)?;
            FrontendMessage::Close(target, fields.string()?.to_owned())
        }
        b'S' => FrontendMessage::Sync,
        b'H' => FrontendMessage::Flush,
        b'X' => FrontendMessage::Terminate,
        b'p' => return Ok(FrontendMessage::AuthenticationResponse(body.to_vec())),
        b'd' => return Ok(FrontendMessage::CopyData),
        b'c' => return Ok(FrontendMessage::CopyDone),
        b'f' => {
            let reason = body.strip_suffix(&[0]).unwrap_or(body);
            return Ok(FrontendMessage::CopyFail(
                String::from_utf8_lossy(reason).into_owned(),
            ));
        }
        other => return Ok(FrontendMessage::Other(other)),
    };
    fields.finish()?;
    Ok(message)
}

/// The body of a `SASLInitialResponse`: the mechanism the client chose and its first message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaslInitialResponse {
    /// The mechanism's name, such as `SCRAM-SHA-256`.
    pub mechanism: String,
    /// The client-first-message, empty when the client sent none.
    pub data: Vec<u8>,
}

/// Parses the body of a `p` message as a `SASLInitialResponse`.
pub fn parse_sasl_initial_response(body: &[u8]) -> Result<SaslInitialResponse, MalformedMessage> {
    let mut fields = Fields::new(body);
    let mechanism = fields.string()?.to_owned();
    let length = fields.i32()?;
    let data = match length {
        -1 => Vec::new(),
        length if length < 0 => return Err(malformed("invalid SASL response length")),
        length => fields.bytes(length as usize)?.to_vec(),
    };
    fields.finish()?;
    Ok(SaslInitialResponse { mechanism, data })
}

/// A backend message that would hold more than its count or length fields can say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TooLarge {
    /// More columns than a 16-bit count holds (32,767).
    Columns(usize),
    /// More parameters than a 16-bit count holds (32,767).
    Parameters(usize),
    /// A value longer than a 32-bit length holds.
    ValueBytes(usize),
}

impl fmt::Display for TooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Columns(count) => write!(
                f,
                "a result of {count} columns does not fit the protocol's limit of 32767"
            ),
            Self::Parameters(count) => write!(
                f,
                "a statement with {count} parameters does not fit the protocol's limit of 32767"
            ),
            Self::ValueBytes(length) => write!(
                f,
                "a value of {length} bytes does not fit the protocol's limit of 2 GiB"
            ),
        }
    }
}

fn count_field(count: usize, too_large: fn(usize) -> TooLarge) -> Result<[u8; 2], TooLarge> {
    i16::try_from(count)
        .map(i16::to_be_bytes)
        .map_err(|_| too_large(count))
}

/// Builds backend messages into one buffer, ready to send.
#[derive(Debug, Default)]
pub struct Outbox {
    bytes: Vec<u8>,
}

/// A field of an `ErrorResponse` or a `NoticeResponse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportField {
    /// `S`: the severity, possibly localized.
    Severity,
    /// `V`: the severity, never localized.
    SeverityName,
    /// `C`: the SQLSTATE code.
    Code,
    /// `M`: the primary message.
    Message,
    /// `D`: the detail.
    Detail,
    /// `H`: the hint.
    Hint,
    /// `P`: the 1-based character position in the query text.
    Position,
    /// `W`: the context, such as the line of `COPY` data an error was found on.
    Where,
}

impl ReportField {
    fn code(self) -> u8 {
        match self {
            Self::Severity => b'S',
            Self::SeverityName => b'V',
            Self::Code => b'C',
            Self::Message => b'M',
            Self::Detail => b'D',
            Self::Hint => b'H',
            Self::Position => b'P',
            Self::Where => b'W',
        }
    }
}

/// One column of a `RowDescription`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDescription {
    /// The column's name.
    pub name: String,
    /// The column's type OID.
    pub type_oid: u32,
    /// The type's size in bytes, or -1 for a variable size.
    pub type_size: i16,
    /// The type modifier, or -1.
    pub type_modifier: i32,
    /// The format its values are sent in.
    pub format: Format,
}

impl Outbox {
    /// An empty outbox.
    pub fn new() -> Self {
        Self::default()
    }

    /// The bytes built so far.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Whether nothing is waiting.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Forgets the bytes built so far, once they are sent.
    pub fn clear(&mut self) {
        self.bytes.clear();
    }

    fn message(&mut self, tag: u8, body: impl FnOnce(&mut Vec<u8>)) {
        self.bytes.push(tag);
        let length_at = self.bytes.len();
        self.bytes.extend_from_slice(&[0; 4]);
        body(&mut self.bytes);
        let length = (self.bytes.len() - length_at) as u32;
        self.bytes[length_at..length_at + 4].copy_from_slice(&length.to_be_bytes());
    }

    /// A single byte with no framing: the answer to an `SSLRequest` or a `GSSENCRequest`.
    pub fn refuse_encryption(&mut self) {
        self.bytes.push(b'N');
    }

    /// `S`: the answer to an `SSLRequest` when TLS is set up; the TLS handshake follows.
    pub fn accept_encryption(&mut self) {
        self.bytes.push(b'S');
    }

    /// `AuthenticationOk`.
    pub fn authentication_ok(&mut self) {
        self.message(b'R', |body| body.extend_from_slice(&0i32.to_be_bytes()));
    }

    /// `AuthenticationSASL` with the mechanisms offered.
    pub fn authentication_sasl(&mut self, mechanisms: &[&str]) {
        self.message(b'R', |body| {
            body.extend_from_slice(&10i32.to_be_bytes());
            for mechanism in mechanisms {
                body.extend_from_slice(mechanism.as_bytes());
                body.push(0);
            }
            body.push(0);
        });
    }

    /// `AuthenticationSASLContinue` with the server-first-message.
    pub fn authentication_sasl_continue(&mut self, data: &[u8]) {
        self.message(b'R', |body| {
            body.extend_from_slice(&11i32.to_be_bytes());
            body.extend_from_slice(data);
        });
    }

    /// `AuthenticationSASLFinal` with the server-final-message.
    pub fn authentication_sasl_final(&mut self, data: &[u8]) {
        self.message(b'R', |body| {
            body.extend_from_slice(&12i32.to_be_bytes());
            body.extend_from_slice(data);
        });
    }

    /// `NegotiateProtocolVersion`: the newest minor version supported and the protocol
    /// options not recognized.
    pub fn negotiate_protocol_version(&mut self, newest_minor: u16, unrecognized: &[String]) {
        self.message(b'v', |body| {
            body.extend_from_slice(&i32::from(newest_minor).to_be_bytes());
            body.extend_from_slice(&(unrecognized.len() as i32).to_be_bytes());
            for option in unrecognized {
                body.extend_from_slice(option.as_bytes());
                body.push(0);
            }
        });
    }

    /// `ParameterStatus`.
    pub fn parameter_status(&mut self, name: &str, value: &str) {
        self.message(b'S', |body| {
            body.extend_from_slice(name.as_bytes());
            body.push(0);
            body.extend_from_slice(value.as_bytes());
            body.push(0);
        });
    }

    /// `BackendKeyData`.
    pub fn backend_key_data(&mut self, process_id: i32, secret_key: i32) {
        self.message(b'K', |body| {
            body.extend_from_slice(&process_id.to_be_bytes());
            body.extend_from_slice(&secret_key.to_be_bytes());
        });
    }

    /// `ReadyForQuery` with the transaction status byte, `I`, `T` or `E`.
    /// `NotificationResponse`: a notification from the backend `sender` on `channel`.
    pub fn notification_response(&mut self, sender: i32, channel: &str, payload: &str) {
        self.message(b'A', |body| {
            body.extend_from_slice(&sender.to_be_bytes());
            body.extend_from_slice(channel.as_bytes());
            body.push(0);
            body.extend_from_slice(payload.as_bytes());
            body.push(0);
        });
    }

    pub fn ready_for_query(&mut self, status: u8) {
        self.message(b'Z', |body| body.push(status));
    }

    /// `RowDescription`; refused for more than 32,767 columns.
    pub fn row_description(&mut self, columns: &[ColumnDescription]) -> Result<(), TooLarge> {
        let count = count_field(columns.len(), TooLarge::Columns)?;
        self.message(b'T', |body| {
            body.extend_from_slice(&count);
            for column in columns {
                body.extend_from_slice(column.name.as_bytes());
                body.push(0);
                body.extend_from_slice(&0i32.to_be_bytes());
                body.extend_from_slice(&0i16.to_be_bytes());
                body.extend_from_slice(&column.type_oid.to_be_bytes());
                body.extend_from_slice(&column.type_size.to_be_bytes());
                body.extend_from_slice(&column.type_modifier.to_be_bytes());
                body.extend_from_slice(&column.format.code().to_be_bytes());
            }
        });
        Ok(())
    }

    /// `DataRow`, each value `None` for NULL; refused for more than 32,767 values or a value
    /// over 2 GiB.
    pub fn data_row(&mut self, values: &[Option<Vec<u8>>]) -> Result<(), TooLarge> {
        let count = count_field(values.len(), TooLarge::Columns)?;
        let mut lengths = Vec::with_capacity(values.len());
        for value in values {
            lengths.push(match value {
                None => -1i32,
                Some(bytes) => {
                    i32::try_from(bytes.len()).map_err(|_| TooLarge::ValueBytes(bytes.len()))?
                }
            });
        }
        self.message(b'D', |body| {
            body.extend_from_slice(&count);
            for (value, length) in values.iter().zip(lengths) {
                body.extend_from_slice(&length.to_be_bytes());
                if let Some(bytes) = value {
                    body.extend_from_slice(bytes);
                }
            }
        });
        Ok(())
    }

    fn copy_response(&mut self, tag: u8, columns: usize, format: Format) -> Result<(), TooLarge> {
        let count = count_field(columns, TooLarge::Columns)?;
        let code = format.code();
        self.message(tag, |body| {
            body.push(u8::from(format == Format::Binary));
            body.extend_from_slice(&count);
            for _ in 0..columns {
                body.extend_from_slice(&code.to_be_bytes());
            }
        });
        Ok(())
    }

    /// `CopyInResponse` for data in `format` (text for the text and CSV formats, binary for
    /// the binary one) with `columns` columns; refused for more than 32,767 columns.
    pub fn copy_in_response(&mut self, columns: usize, format: Format) -> Result<(), TooLarge> {
        self.copy_response(b'G', columns, format)
    }

    /// `CopyOutResponse` for data in `format` (text for the text and CSV formats, binary for
    /// the binary one) with `columns` columns; refused for more than 32,767 columns.
    pub fn copy_out_response(&mut self, columns: usize, format: Format) -> Result<(), TooLarge> {
        self.copy_response(b'H', columns, format)
    }

    /// `CopyData` with one piece of a `COPY ... TO STDOUT`: a row, or for the binary format
    /// the file header with the first row and the trailer; refused over 2 GiB.
    pub fn copy_data(&mut self, data: &[u8]) -> Result<(), TooLarge> {
        if data.len() > i32::MAX as usize - 4 {
            return Err(TooLarge::ValueBytes(data.len()));
        }
        self.message(b'd', |body| body.extend_from_slice(data));
        Ok(())
    }

    /// `CopyDone`: the rows of a `COPY ... TO STDOUT` have all been sent.
    pub fn copy_done(&mut self) {
        self.message(b'c', |_| {});
    }

    /// `CommandComplete` with its tag, such as `INSERT 0 1`.
    pub fn command_complete(&mut self, tag: &str) {
        self.message(b'C', |body| {
            body.extend_from_slice(tag.as_bytes());
            body.push(0);
        });
    }

    /// `EmptyQueryResponse`.
    pub fn empty_query_response(&mut self) {
        self.message(b'I', |_| {});
    }

    /// `ParseComplete`.
    pub fn parse_complete(&mut self) {
        self.message(b'1', |_| {});
    }

    /// `BindComplete`.
    pub fn bind_complete(&mut self) {
        self.message(b'2', |_| {});
    }

    /// `CloseComplete`.
    pub fn close_complete(&mut self) {
        self.message(b'3', |_| {});
    }

    /// `NoData`.
    pub fn no_data(&mut self) {
        self.message(b'n', |_| {});
    }

    /// `PortalSuspended`.
    pub fn portal_suspended(&mut self) {
        self.message(b's', |_| {});
    }

    /// `ParameterDescription`; refused for more than 32,767 parameters.
    pub fn parameter_description(&mut self, type_oids: &[u32]) -> Result<(), TooLarge> {
        let count = count_field(type_oids.len(), TooLarge::Parameters)?;
        self.message(b't', |body| {
            body.extend_from_slice(&count);
            for oid in type_oids {
                body.extend_from_slice(&oid.to_be_bytes());
            }
        });
        Ok(())
    }

    /// `ErrorResponse` (`notice` false) or `NoticeResponse` (`notice` true) with its fields.
    pub fn report(&mut self, notice: bool, fields: &[(ReportField, &str)]) {
        self.message(if notice { b'N' } else { b'E' }, |body| {
            for (field, value) in fields {
                body.push(field.code());
                body.extend(value.bytes().filter(|&byte| byte != 0));
                body.push(0);
            }
            body.push(0);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn startup_body(code: u32, parameters: &[(&str, &str)]) -> Vec<u8> {
        let mut body = code.to_be_bytes().to_vec();
        for (name, value) in parameters {
            body.extend_from_slice(name.as_bytes());
            body.push(0);
            body.extend_from_slice(value.as_bytes());
            body.push(0);
        }
        body.push(0);
        body
    }

    #[test]
    fn startup_packets_parse_by_their_code() {
        assert_eq!(
            parse_startup(&SSL_REQUEST_CODE.to_be_bytes()),
            Ok(StartupRequest::Ssl)
        );
        assert_eq!(
            parse_startup(&GSSENC_REQUEST_CODE.to_be_bytes()),
            Ok(StartupRequest::GssEncryption)
        );
        let mut cancel = CANCEL_REQUEST_CODE.to_be_bytes().to_vec();
        cancel.extend_from_slice(&7i32.to_be_bytes());
        cancel.extend_from_slice(&9i32.to_be_bytes());
        assert_eq!(
            parse_startup(&cancel),
            Ok(StartupRequest::Cancel {
                process_id: 7,
                secret_key: 9i32.to_be_bytes().to_vec()
            })
        );
        assert_eq!(
            parse_startup(&startup_body(
                PROTOCOL_3_0,
                &[("user", "ada"), ("database", "shop")]
            )),
            Ok(StartupRequest::Startup {
                major: 3,
                minor: 0,
                parameters: vec![
                    ("user".into(), "ada".into()),
                    ("database".into(), "shop".into())
                ]
            })
        );
        assert!(matches!(
            parse_startup(&startup_body(0x0002_0000, &[])),
            Ok(StartupRequest::Startup { major: 2, .. })
        ));
    }

    #[test]
    fn truncated_startup_packets_are_refused() {
        let body = startup_body(PROTOCOL_3_0, &[("user", "ada")]);
        for length in 0..body.len() - 1 {
            assert!(parse_startup(&body[..length]).is_err(), "{length}");
        }
        assert!(parse_startup(&[0, 0]).is_err());
        let mut extra = SSL_REQUEST_CODE.to_be_bytes().to_vec();
        extra.push(1);
        assert!(parse_startup(&extra).is_err());
        let mut bad_utf8 = PROTOCOL_3_0.to_be_bytes().to_vec();
        bad_utf8.extend_from_slice(b"user\0\xff\0\0");
        assert!(parse_startup(&bad_utf8).is_err());
    }

    fn bind_body(parameters: &[Option<&[u8]>]) -> Vec<u8> {
        let mut body = b"portal\0stmt\0".to_vec();
        body.extend_from_slice(&1i16.to_be_bytes());
        body.extend_from_slice(&1i16.to_be_bytes());
        body.extend_from_slice(&(parameters.len() as i16).to_be_bytes());
        for parameter in parameters {
            match parameter {
                None => body.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(bytes) => {
                    body.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                    body.extend_from_slice(bytes);
                }
            }
        }
        body.extend_from_slice(&0i16.to_be_bytes());
        body
    }

    #[test]
    fn every_frontend_message_parses() {
        assert_eq!(
            parse_message(b'Q', b"SELECT 1\0"),
            Ok(FrontendMessage::Query("SELECT 1".into()))
        );
        let mut parse = b"s1\0SELECT $1\0".to_vec();
        parse.extend_from_slice(&1i16.to_be_bytes());
        parse.extend_from_slice(&23i32.to_be_bytes());
        assert_eq!(
            parse_message(b'P', &parse),
            Ok(FrontendMessage::Parse {
                name: "s1".into(),
                query: "SELECT $1".into(),
                parameter_types: vec![23]
            })
        );
        assert_eq!(
            parse_message(b'B', &bind_body(&[Some(b"42"), None])),
            Ok(FrontendMessage::Bind {
                portal: "portal".into(),
                statement: "stmt".into(),
                parameter_formats: vec![Format::Binary],
                parameters: vec![Some(b"42".to_vec()), None],
                result_formats: vec![],
            })
        );
        assert_eq!(
            parse_message(b'D', b"Sname\0"),
            Ok(FrontendMessage::Describe(Target::Statement, "name".into()))
        );
        assert_eq!(
            parse_message(b'C', b"P\0"),
            Ok(FrontendMessage::Close(Target::Portal, String::new()))
        );
        let mut execute = b"\0".to_vec();
        execute.extend_from_slice(&5i32.to_be_bytes());
        assert_eq!(
            parse_message(b'E', &execute),
            Ok(FrontendMessage::Execute {
                portal: String::new(),
                max_rows: 5
            })
        );
        assert_eq!(parse_message(b'S', b""), Ok(FrontendMessage::Sync));
        assert_eq!(parse_message(b'H', b""), Ok(FrontendMessage::Flush));
        assert_eq!(parse_message(b'X', b""), Ok(FrontendMessage::Terminate));
        assert_eq!(parse_message(b'd', b"xyz"), Ok(FrontendMessage::CopyData));
        assert_eq!(parse_message(b'c', b""), Ok(FrontendMessage::CopyDone));
        assert_eq!(
            parse_message(b'f', b"gave up\0"),
            Ok(FrontendMessage::CopyFail("gave up".into()))
        );
        assert_eq!(parse_message(b'c', b"x"), Ok(FrontendMessage::CopyDone));
        assert_eq!(
            parse_message(b'f', b"no end \xff"),
            Ok(FrontendMessage::CopyFail("no end \u{fffd}".into()))
        );
        let mut sasl = b"SCRAM-SHA-256\0".to_vec();
        sasl.extend_from_slice(&3i32.to_be_bytes());
        sasl.extend_from_slice(b"n,,");
        assert_eq!(
            parse_sasl_initial_response(&sasl),
            Ok(SaslInitialResponse {
                mechanism: "SCRAM-SHA-256".into(),
                data: b"n,,".to_vec()
            })
        );
    }

    #[test]
    fn truncated_or_inconsistent_messages_are_refused() {
        let bind = bind_body(&[Some(b"42"), None]);
        for length in 0..bind.len() {
            assert!(parse_message(b'B', &bind[..length]).is_err(), "{length}");
        }
        assert!(parse_message(b'S', b"x").is_err());
        assert!(parse_message(b'Q', b"SELECT 1").is_err());
        assert!(parse_message(b'Q', b"SELECT 1\0\0").is_err());
        assert!(parse_message(b'D', b"Xname\0").is_err());
        let mut negative = b"\0\0".to_vec();
        negative.extend_from_slice(&(-2i16).to_be_bytes());
        assert!(parse_message(b'B', &negative).is_err());
        let mut huge = b"\0\0\0\0".to_vec();
        huge.extend_from_slice(&1i16.to_be_bytes());
        huge.extend_from_slice(&i32::MAX.to_be_bytes());
        assert!(parse_message(b'B', &huge).is_err());
        let mut bad_format = b"\0\0".to_vec();
        bad_format.extend_from_slice(&1i16.to_be_bytes());
        bad_format.extend_from_slice(&7i16.to_be_bytes());
        assert!(parse_message(b'B', &bad_format).is_err());
        let mut sasl = b"SCRAM-SHA-256\0".to_vec();
        sasl.extend_from_slice(&100i32.to_be_bytes());
        assert!(parse_sasl_initial_response(&sasl).is_err());
    }

    #[test]
    fn counts_beyond_a_16_bit_field_are_refused_and_nothing_is_written() {
        let mut outbox = Outbox::new();
        assert_eq!(
            outbox.parameter_description(&vec![23; 40_000]),
            Err(TooLarge::Parameters(40_000))
        );
        assert_eq!(
            outbox.data_row(&vec![None; 32_768]),
            Err(TooLarge::Columns(32_768))
        );
        let column = ColumnDescription {
            name: "c".into(),
            type_oid: 25,
            type_size: -1,
            type_modifier: -1,
            format: Format::Text,
        };
        assert_eq!(
            outbox.row_description(&vec![column; 32_768]),
            Err(TooLarge::Columns(32_768))
        );
        assert!(outbox.is_empty());
        assert!(outbox.data_row(&vec![None; 32_767]).is_ok());
    }

    #[test]
    fn backend_messages_carry_their_length() {
        let mut outbox = Outbox::new();
        outbox.ready_for_query(b'I');
        assert_eq!(outbox.bytes(), b"Z\0\0\0\x05I");
        outbox.clear();
        outbox.data_row(&[Some(b"1".to_vec()), None]).unwrap();
        assert_eq!(
            outbox.bytes(),
            b"D\0\0\0\x0f\0\x02\0\0\0\x011\xff\xff\xff\xff"
        );
        outbox.clear();
        outbox.report(
            false,
            &[(ReportField::Code, "42P01"), (ReportField::Message, "a\0b")],
        );
        assert_eq!(outbox.bytes(), b"E\0\0\0\x10C42P01\0Mab\0\0");
        outbox.clear();
        outbox.copy_in_response(2, Format::Text).unwrap();
        outbox.copy_out_response(1, Format::Text).unwrap();
        outbox.copy_data(b"1\tx\n").unwrap();
        outbox.copy_done();
        assert_eq!(
            outbox.bytes(),
            b"G\0\0\0\x0b\0\0\x02\0\0\0\0H\0\0\0\x09\0\0\x01\0\0d\0\0\0\x081\tx\nc\0\0\0\x04"
        );
        assert!(outbox.copy_in_response(40_000, Format::Text).is_err());
        outbox.clear();
        outbox.copy_in_response(2, Format::Binary).unwrap();
        assert_eq!(outbox.bytes(), b"G\0\0\0\x0b\x01\0\x02\0\x01\0\x01");
    }
}
