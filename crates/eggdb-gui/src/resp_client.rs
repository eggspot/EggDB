use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// How long connecting to a RESP server may take before it fails.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one read or write on an open connection may wait.
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest status, error or length line a reply may send.
pub const MAX_LINE_BYTES: usize = 64 * 1024;
/// The most bytes one reply may carry in its strings, all nested values together.
pub const MAX_REPLY_BYTES: usize = 64 * 1024 * 1024;
/// The most values one reply may hold, all nested arrays and maps together.
pub const MAX_REPLY_VALUES: usize = 1 << 20;
/// How deeply arrays and maps may nest in one reply.
pub const MAX_REPLY_DEPTH: usize = 32;

/// One reply a RESP server sent, RESP2 and RESP3 read alike: RESP3's doubles and big numbers
/// as their text, booleans as `1` and `0`, sets and pushes as arrays, blob errors as errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RespReply {
    /// A status line, `+OK`.
    Simple(Vec<u8>),
    /// An error line, `-ERR …`, or a RESP3 blob error.
    Error(Vec<u8>),
    /// An integer.
    Integer(i64),
    /// A bulk string.
    Bulk(Vec<u8>),
    /// A null bulk string, a null array or RESP3's null.
    Null,
    /// An array or a RESP3 set.
    Array(Vec<RespReply>),
    /// A RESP3 map, its pairs in the order sent.
    Map(Vec<(RespReply, RespReply)>),
}

impl RespReply {
    /// The reply's bytes when it is a string: a bulk string, a status line or a RESP3 number's
    /// text.
    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Simple(bytes) | Self::Bulk(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// The reply as an integer: an integer, or a string that spells one.
    pub fn integer(&self) -> Option<i64> {
        match self {
            Self::Integer(value) => Some(*value),
            other => std::str::from_utf8(other.bytes()?).ok()?.parse().ok(),
        }
    }

    /// The error line when the reply is an error, as the server wrote it.
    pub fn error_line(&self) -> Option<String> {
        match self {
            Self::Error(line) => Some(String::from_utf8_lossy(line).into_owned()),
            _ => None,
        }
    }
}

/// Why talking to a RESP server failed.
#[derive(Debug)]
pub enum RespError {
    /// The connection failed, timed out or closed.
    Io(io::Error),
    /// The server sent something that is not a RESP reply, or a reply past the client's
    /// limits.
    Protocol(String),
}

impl std::fmt::Display for RespError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Protocol(message) => write!(formatter, "{message}"),
        }
    }
}

impl From<io::Error> for RespError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// A command as RESP sends it: an array of bulk strings, one per part.
pub fn encode_command<P: AsRef<[u8]>>(parts: &[P]) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        16 + parts
            .iter()
            .map(|part| part.as_ref().len() + 16)
            .sum::<usize>(),
    );
    out.extend_from_slice(format!("*{}\r\n", parts.len()).as_bytes());
    for part in parts {
        let part = part.as_ref();
        out.extend_from_slice(format!("${}\r\n", part.len()).as_bytes());
        out.extend_from_slice(part);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// The `MATCH` pattern that lists the keys starting with `prefix`: each `*`, `?`, `[`, `]`
/// and `\` escaped with `\`, then `*`; the other bytes as they are.
pub fn prefix_pattern(prefix: &[u8]) -> Vec<u8> {
    let mut pattern = Vec::with_capacity(prefix.len() + 1);
    for &byte in prefix {
        if matches!(byte, b'*' | b'?' | b'[' | b']' | b'\\') {
            pattern.push(b'\\');
        }
        pattern.push(byte);
    }
    pattern.push(b'*');
    pattern
}

/// Reads one reply from `reader` within the client's limits; RESP3 attributes are read and
/// dropped, and so are pushes that come before the reply.
pub fn read_reply(reader: &mut impl BufRead) -> Result<RespReply, RespError> {
    let mut budget = Budget {
        bytes: MAX_REPLY_BYTES,
        values: MAX_REPLY_VALUES,
    };
    loop {
        let (reply, pushed) = read_value(reader, &mut budget, 0)?;
        if !pushed {
            return Ok(reply);
        }
    }
}

struct Budget {
    bytes: usize,
    values: usize,
}

impl Budget {
    fn take_bytes(&mut self, count: usize) -> Result<(), RespError> {
        self.bytes = self.bytes.checked_sub(count).ok_or_else(|| {
            RespError::Protocol("the reply is larger than the client reads".to_owned())
        })?;
        Ok(())
    }

    fn take_values(&mut self, count: usize) -> Result<(), RespError> {
        self.check_values(count)?;
        self.values -= count;
        Ok(())
    }

    fn check_values(&self, count: usize) -> Result<(), RespError> {
        if count > self.values {
            return Err(RespError::Protocol(
                "the reply holds more values than the client reads".to_owned(),
            ));
        }
        Ok(())
    }
}

fn read_line(reader: &mut impl BufRead, budget: &mut Budget) -> Result<Vec<u8>, RespError> {
    let mut line = Vec::new();
    let limit = u64::try_from(MAX_LINE_BYTES + 2).unwrap_or(u64::MAX);
    (&mut *reader).take(limit).read_until(b'\n', &mut line)?;
    if line.last() != Some(&b'\n') {
        return Err(if line.len() > MAX_LINE_BYTES {
            RespError::Protocol("a reply line is longer than the client reads".to_owned())
        } else {
            RespError::Io(io::Error::from(io::ErrorKind::UnexpectedEof))
        });
    }
    line.pop();
    if line.pop() != Some(b'\r') {
        return Err(RespError::Protocol(
            "a reply line does not end in CRLF".to_owned(),
        ));
    }
    budget.take_bytes(line.len())?;
    Ok(line)
}

fn parse_integer(text: &[u8]) -> Result<i64, RespError> {
    std::str::from_utf8(text)
        .ok()
        .and_then(|text| text.parse::<i64>().ok())
        .ok_or_else(|| RespError::Protocol("a reply's number does not parse".to_owned()))
}

fn parse_length(text: &[u8]) -> Result<Option<usize>, RespError> {
    match parse_integer(text)? {
        -1 => Ok(None),
        length => usize::try_from(length)
            .map(Some)
            .map_err(|_| RespError::Protocol("a reply's length is negative".to_owned())),
    }
}

fn read_blob(
    reader: &mut impl BufRead,
    length: usize,
    budget: &mut Budget,
) -> Result<Vec<u8>, RespError> {
    budget.take_bytes(length)?;
    let mut blob = Vec::with_capacity(length.min(64 * 1024));
    let wanted = u64::try_from(length).unwrap_or(u64::MAX);
    let read = (&mut *reader).take(wanted).read_to_end(&mut blob)?;
    if read != length {
        return Err(RespError::Io(io::Error::from(io::ErrorKind::UnexpectedEof)));
    }
    let mut ending = [0_u8; 2];
    reader.read_exact(&mut ending)?;
    if &ending != b"\r\n" {
        return Err(RespError::Protocol(
            "a bulk string does not end in CRLF".to_owned(),
        ));
    }
    Ok(blob)
}

fn read_items(
    reader: &mut impl BufRead,
    count: usize,
    budget: &mut Budget,
    depth: usize,
) -> Result<Vec<RespReply>, RespError> {
    budget.check_values(count)?;
    let mut items = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        items.push(read_nested(reader, budget, depth)?);
    }
    Ok(items)
}

fn read_nested(
    reader: &mut impl BufRead,
    budget: &mut Budget,
    depth: usize,
) -> Result<RespReply, RespError> {
    loop {
        let (reply, pushed) = read_value(reader, budget, depth)?;
        if !pushed {
            return Ok(reply);
        }
    }
}

fn read_value(
    reader: &mut impl BufRead,
    budget: &mut Budget,
    depth: usize,
) -> Result<(RespReply, bool), RespError> {
    if depth > MAX_REPLY_DEPTH {
        return Err(RespError::Protocol(
            "a reply nests deeper than the client reads".to_owned(),
        ));
    }
    budget.take_values(1)?;
    let line = read_line(reader, budget)?;
    let Some((&kind, rest)) = line.split_first() else {
        return Err(RespError::Protocol("a reply line is empty".to_owned()));
    };
    let reply = match kind {
        b'+' | b',' | b'(' => RespReply::Simple(rest.to_vec()),
        b'-' => RespReply::Error(rest.to_vec()),
        b':' => RespReply::Integer(parse_integer(rest)?),
        b'_' => RespReply::Null,
        b'#' => match rest {
            b"t" => RespReply::Integer(1),
            b"f" => RespReply::Integer(0),
            _ => {
                return Err(RespError::Protocol(
                    "a boolean is neither t nor f".to_owned(),
                ));
            }
        },
        b'$' => match parse_length(rest)? {
            None => RespReply::Null,
            Some(length) => RespReply::Bulk(read_blob(reader, length, budget)?),
        },
        b'!' => match parse_length(rest)? {
            None => RespReply::Null,
            Some(length) => RespReply::Error(read_blob(reader, length, budget)?),
        },
        b'=' => match parse_length(rest)? {
            None => RespReply::Null,
            Some(length) => {
                let text = read_blob(reader, length, budget)?;
                RespReply::Bulk(text.get(4..).map_or_else(Vec::new, <[u8]>::to_vec))
            }
        },
        b'*' | b'~' => match parse_length(rest)? {
            None => RespReply::Null,
            Some(count) => RespReply::Array(read_items(reader, count, budget, depth + 1)?),
        },
        b'>' => match parse_length(rest)? {
            None => RespReply::Null,
            Some(count) => {
                read_items(reader, count, budget, depth + 1)?;
                return Ok((RespReply::Null, true));
            }
        },
        b'%' | b'|' => {
            let count = parse_length(rest)?.unwrap_or(0);
            let doubled = count.checked_mul(2).ok_or_else(|| {
                RespError::Protocol("a map is larger than the client reads".to_owned())
            })?;
            let mut items = read_items(reader, doubled, budget, depth + 1)?.into_iter();
            let mut pairs = Vec::with_capacity(count.min(1024));
            while let (Some(key), Some(value)) = (items.next(), items.next()) {
                pairs.push((key, value));
            }
            if kind == b'|' {
                return Ok((RespReply::Null, true));
            }
            RespReply::Map(pairs)
        }
        _ => {
            return Err(RespError::Protocol(format!(
                "the server sent a reply of unknown type 0x{kind:02x}"
            )));
        }
    };
    Ok((reply, false))
}

/// Where and how to reach a RESP server.
#[derive(Clone, PartialEq, Eq)]
pub struct RespOptions {
    /// The host name or address.
    pub host: String,
    /// The TCP port.
    pub port: u16,
    /// The ACL user `AUTH` sends with the password; `None` for the server's default user.
    pub user: Option<String>,
    /// The password; `None` sends no `AUTH`.
    pub password: Option<String>,
}

impl std::fmt::Debug for RespOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RespOptions")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field(
                "password",
                &self.password.as_ref().map(|_| crate::connection::REDACTED),
            )
            .finish()
    }
}

impl RespOptions {
    /// `host:port`, as error lines name the server.
    pub fn place(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Why a RESP connection could not be opened.
#[derive(Debug)]
pub enum RespConnectError {
    /// Reaching the server failed.
    Io(io::Error),
    /// The server's handshake reply was not RESP.
    Protocol(String),
    /// The server refused the handshake with this error line (`WRONGPASS …`).
    Refused(String),
}

impl From<RespError> for RespConnectError {
    fn from(error: RespError) -> Self {
        match error {
            RespError::Io(error) => Self::Io(error),
            RespError::Protocol(message) => Self::Protocol(message),
        }
    }
}

/// A connection to a RESP server: the public RESP2/RESP3 wire format over TCP, with `HELLO 3`
/// and a fall back to RESP2, and `AUTH` from the password.
#[derive(Debug)]
pub struct RespClient {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    resp3: bool,
}

impl RespClient {
    /// Connects to `options` within [`CONNECT_TIMEOUT`] and says hello: `HELLO 3` (with
    /// `AUTH` when there is a password); a server that answers it with an error other than
    /// `NOAUTH` or `WRONGPASS` speaks RESP2 and gets `AUTH` on its own.
    pub fn connect(options: &RespOptions) -> Result<Self, RespConnectError> {
        let stream = open_stream(options).map_err(RespConnectError::Io)?;
        let mut client = Self {
            reader: BufReader::new(stream.try_clone().map_err(RespConnectError::Io)?),
            writer: stream,
            resp3: false,
        };
        client.hello(options)?;
        Ok(client)
    }

    fn hello(&mut self, options: &RespOptions) -> Result<(), RespConnectError> {
        let user = options.user.as_deref().unwrap_or("default");
        let mut hello: Vec<&[u8]> = vec![b"HELLO".as_slice(), b"3".as_slice()];
        if let Some(password) = &options.password {
            hello.push(b"AUTH".as_slice());
            hello.push(user.as_bytes());
            hello.push(password.as_bytes());
        }
        match self.command(&hello)? {
            RespReply::Error(line) if refuses_credentials(&line) => Err(RespConnectError::Refused(
                String::from_utf8_lossy(&line).into_owned(),
            )),
            RespReply::Error(_) => {
                self.resp3 = false;
                let Some(password) = &options.password else {
                    return Ok(());
                };
                let mut auth: Vec<&[u8]> = vec![b"AUTH".as_slice()];
                if let Some(user) = &options.user {
                    auth.push(user.as_bytes());
                }
                auth.push(password.as_bytes());
                match self.command(&auth)? {
                    RespReply::Error(line) => Err(RespConnectError::Refused(
                        String::from_utf8_lossy(&line).into_owned(),
                    )),
                    _ => Ok(()),
                }
            }
            _ => {
                self.resp3 = true;
                Ok(())
            }
        }
    }

    /// Whether the server took `HELLO 3` and answers in RESP3.
    pub fn speaks_resp3(&self) -> bool {
        self.resp3
    }

    /// A second handle on the socket, whose shutdown from another thread stops a running
    /// exchange.
    pub fn socket(&self) -> io::Result<TcpStream> {
        self.writer.try_clone()
    }

    /// Sends one command and reads its reply; an error reply is a reply, not an `Err`.
    pub fn command<P: AsRef<[u8]>>(&mut self, parts: &[P]) -> Result<RespReply, RespError> {
        self.writer.write_all(&encode_command(parts))?;
        self.writer.flush()?;
        read_reply(&mut self.reader)
    }

    /// Sends every command in one write and reads their replies in order.
    pub fn pipeline<P: AsRef<[u8]>>(
        &mut self,
        commands: &[Vec<P>],
    ) -> Result<Vec<RespReply>, RespError> {
        let mut out = Vec::new();
        for command in commands {
            out.extend_from_slice(&encode_command(command));
        }
        self.writer.write_all(&out)?;
        self.writer.flush()?;
        let mut replies = Vec::with_capacity(commands.len());
        for _ in commands {
            replies.push(read_reply(&mut self.reader)?);
        }
        Ok(replies)
    }

    /// Closes the connection at once, both ways.
    pub fn close(self) {
        let _ = self.writer.shutdown(Shutdown::Both);
    }
}

fn refuses_credentials(line: &[u8]) -> bool {
    line.starts_with(b"NOAUTH") || line.starts_with(b"WRONGPASS")
}

fn open_stream(options: &RespOptions) -> io::Result<TcpStream> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, "the host name has no address");
    for socket in (options.host.as_str(), options.port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&socket, CONNECT_TIMEOUT) {
            Ok(stream) => {
                stream.set_read_timeout(Some(IO_TIMEOUT))?;
                stream.set_write_timeout(Some(IO_TIMEOUT))?;
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Err(error) => last = error,
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(input: &[u8]) -> Result<RespReply, RespError> {
        read_reply(&mut &input[..])
    }

    fn bulk(text: &str) -> RespReply {
        RespReply::Bulk(text.as_bytes().to_vec())
    }

    #[test]
    fn a_command_is_an_array_of_bulk_strings() {
        assert_eq!(
            encode_command(&["SCAN", "0", "MATCH", "user:*"]),
            b"*4\r\n$4\r\nSCAN\r\n$1\r\n0\r\n$5\r\nMATCH\r\n$6\r\nuser:*\r\n".to_vec()
        );
        assert_eq!(
            encode_command(&[b"GET".as_slice(), b"a\r\nb".as_slice()]),
            b"*2\r\n$3\r\nGET\r\n$4\r\na\r\nb\r\n".to_vec()
        );
    }

    #[test]
    fn the_prefix_pattern_escapes_every_glob_character() {
        assert_eq!(prefix_pattern(b"user:"), b"user:*".to_vec());
        assert_eq!(
            prefix_pattern(b"a*b?c[d]e\\f"),
            b"a\\*b\\?c\\[d\\]e\\\\f*".to_vec()
        );
        assert_eq!(prefix_pattern(&[0x00, 0xFF]), vec![0x00, 0xFF, b'*']);
        assert_eq!(prefix_pattern(b""), b"*".to_vec());
    }

    #[test]
    fn resp2_replies_read_as_the_spec_writes_them() {
        assert_eq!(read(b"+OK\r\n").unwrap(), RespReply::Simple(b"OK".to_vec()));
        assert_eq!(
            read(b"-NOAUTH Authentication required.\r\n")
                .unwrap()
                .error_line()
                .as_deref(),
            Some("NOAUTH Authentication required.")
        );
        assert_eq!(read(b":-42\r\n").unwrap(), RespReply::Integer(-42));
        assert_eq!(read(b"$5\r\nhello\r\n").unwrap(), bulk("hello"));
        assert_eq!(read(b"$0\r\n\r\n").unwrap(), bulk(""));
        assert_eq!(read(b"$-1\r\n").unwrap(), RespReply::Null);
        assert_eq!(read(b"*-1\r\n").unwrap(), RespReply::Null);
        assert_eq!(
            read(b"*2\r\n$4\r\n1532\r\n*2\r\n$6\r\nuser:1\r\n$6\r\nuser:2\r\n").unwrap(),
            RespReply::Array(vec![
                bulk("1532"),
                RespReply::Array(vec![bulk("user:1"), bulk("user:2")]),
            ])
        );
        assert_eq!(read(b"$4\r\n1532\r\n").unwrap().integer(), Some(1532));
    }

    #[test]
    fn resp3_replies_read_into_the_same_shapes() {
        assert_eq!(
            read(b"%2\r\n+server\r\n+eggdb\r\n+proto\r\n:3\r\n").unwrap(),
            RespReply::Map(vec![
                (
                    RespReply::Simple(b"server".to_vec()),
                    RespReply::Simple(b"eggdb".to_vec())
                ),
                (RespReply::Simple(b"proto".to_vec()), RespReply::Integer(3)),
            ])
        );
        assert_eq!(
            read(b"~1\r\n$1\r\na\r\n").unwrap(),
            RespReply::Array(vec![bulk("a")])
        );
        assert_eq!(read(b"_\r\n").unwrap(), RespReply::Null);
        assert_eq!(read(b"#t\r\n").unwrap(), RespReply::Integer(1));
        assert_eq!(
            read(b",1.5\r\n").unwrap(),
            RespReply::Simple(b"1.5".to_vec())
        );
        assert_eq!(
            read(b"(12345678901234567890\r\n").unwrap().bytes(),
            Some(&b"12345678901234567890"[..])
        );
        assert_eq!(read(b"=8\r\ntxt:abcd\r\n").unwrap(), bulk("abcd"));
        assert_eq!(
            read(b"!5\r\nERR x\r\n").unwrap(),
            RespReply::Error(b"ERR x".to_vec())
        );
        assert_eq!(
            read(b"|1\r\n+ttl\r\n:3\r\n:7\r\n").unwrap(),
            RespReply::Integer(7)
        );
        assert_eq!(
            read(b">2\r\n+message\r\n+hi\r\n:7\r\n").unwrap(),
            RespReply::Integer(7)
        );
    }

    #[test]
    fn hostile_replies_are_errors_never_panics_or_huge_allocations() {
        let hostile = [
            b"".to_vec(),
            b"\r\n".to_vec(),
            b"+OK".to_vec(),
            b"+OK\n".to_vec(),
            b"?what\r\n".to_vec(),
            b":12x\r\n".to_vec(),
            b":99999999999999999999999\r\n".to_vec(),
            b"$-2\r\n".to_vec(),
            b"$5\r\nab\r\n".to_vec(),
            b"$2\r\nabcd".to_vec(),
            format!("${}\r\n", MAX_REPLY_BYTES + 1).into_bytes(),
            b"$9223372036854775807\r\n".to_vec(),
            format!("*{}\r\n", MAX_REPLY_VALUES + 1).into_bytes(),
            b"*9223372036854775807\r\n".to_vec(),
            b"%9223372036854775807\r\n".to_vec(),
            b"*3\r\n:1\r\n".to_vec(),
            b"#x\r\n".to_vec(),
            b"*-5\r\n".to_vec(),
            ["*1\r\n".repeat(MAX_REPLY_DEPTH + 2).as_bytes(), b":1\r\n"].concat(),
            [b"+".as_slice(), "a".repeat(MAX_LINE_BYTES + 10).as_bytes()].concat(),
        ];
        for input in hostile {
            assert!(
                read(&input).is_err(),
                "{:?}",
                String::from_utf8_lossy(&input)
            );
        }
        let mut many_pushes = Vec::new();
        for _ in 0..(MAX_REPLY_VALUES / 3 + 1) {
            many_pushes.extend_from_slice(b">2\r\n:1\r\n:1\r\n");
        }
        many_pushes.extend_from_slice(b"+OK\r\n");
        assert!(read(&many_pushes).is_err());
    }

    #[test]
    fn the_options_hide_their_password_in_debug_output() {
        let options = RespOptions {
            host: "cache".to_owned(),
            port: 6379,
            user: None,
            password: Some("hunter2".to_owned()),
        };
        let shown = format!("{options:?}");
        assert!(!shown.contains("hunter2"));
        assert!(shown.contains("***"));
        assert_eq!(options.place(), "cache:6379");
    }
}
