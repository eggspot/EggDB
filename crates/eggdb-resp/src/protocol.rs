use std::io::{self, Read};

const MAX_ARGUMENTS: usize = 1 << 20;
const MAX_BULK_BYTES: usize = 512 << 20;
const MAX_LINE_BYTES: usize = 64 * 1024;

/// Why a request could not be read.
#[derive(Debug, PartialEq, Eq)]
pub enum ProtocolError {
    /// The connection closed.
    Closed,
    /// The bytes are not a RESP request, with what was wrong as the server says it.
    Invalid(String),
}

impl From<io::Error> for ProtocolError {
    fn from(_: io::Error) -> Self {
        Self::Closed
    }
}

/// A reply, written in RESP2 or RESP3 by [`Reply::encode`].
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// `+OK`-style status.
    Status(String),
    /// `-ERR ...`.
    Error(String),
    /// `:42`.
    Integer(i64),
    /// A bulk string, or the null reply.
    Bulk(Option<Vec<u8>>),
    /// An array, or the null array.
    Array(Option<Vec<Reply>>),
    /// A floating-point number: RESP3's `,`, a bulk string in RESP2.
    Double(f64),
    /// Field-value pairs: RESP3's `%`, a flat array in RESP2.
    Map(Vec<(Reply, Reply)>),
    /// Members: RESP3's `~`, an array in RESP2.
    Set(Vec<Reply>),
    /// Out-of-band data such as a published message: RESP3's `>`, an array in RESP2.
    Push(Vec<Reply>),
    /// Keyed results: a map in RESP3, an array of two-element arrays in RESP2 (as `XREAD`).
    PairArray(Vec<(Reply, Reply)>),
}

impl Reply {
    /// `+OK`.
    pub fn ok() -> Self {
        Self::Status("OK".to_owned())
    }

    /// A bulk string of `bytes`.
    pub fn bulk(bytes: impl Into<Vec<u8>>) -> Self {
        Self::Bulk(Some(bytes.into()))
    }

    /// The null bulk string.
    pub fn null() -> Self {
        Self::Bulk(None)
    }

    /// The reply in RESP3 when `resp3`, else RESP2.
    pub fn encode(&self, out: &mut Vec<u8>, resp3: bool) {
        match self {
            Self::Status(text) => {
                out.push(b'+');
                out.extend_from_slice(text.replace(['\r', '\n'], " ").as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Self::Error(text) => {
                out.push(b'-');
                out.extend_from_slice(text.replace(['\r', '\n'], " ").as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Self::Integer(number) => {
                out.extend_from_slice(format!(":{number}\r\n").as_bytes());
            }
            Self::Bulk(None) if resp3 => out.extend_from_slice(b"_\r\n"),
            Self::Bulk(None) => out.extend_from_slice(b"$-1\r\n"),
            Self::Bulk(Some(bytes)) => {
                out.extend_from_slice(format!("${}\r\n", bytes.len()).as_bytes());
                out.extend_from_slice(bytes);
                out.extend_from_slice(b"\r\n");
            }
            Self::Array(None) if resp3 => out.extend_from_slice(b"_\r\n"),
            Self::Array(None) => out.extend_from_slice(b"*-1\r\n"),
            Self::Array(Some(items)) => {
                out.extend_from_slice(format!("*{}\r\n", items.len()).as_bytes());
                for item in items {
                    item.encode(out, resp3);
                }
            }
            Self::Double(number) => {
                let text = format_double(*number);
                if resp3 {
                    out.extend_from_slice(format!(",{text}\r\n").as_bytes());
                } else {
                    Self::bulk(text).encode(out, resp3);
                }
            }
            Self::PairArray(pairs) if !resp3 => {
                out.extend_from_slice(format!("*{}\r\n", pairs.len()).as_bytes());
                for (key, value) in pairs {
                    out.extend_from_slice(b"*2\r\n");
                    key.encode(out, resp3);
                    value.encode(out, resp3);
                }
            }
            Self::Map(pairs) | Self::PairArray(pairs) => {
                if resp3 {
                    out.extend_from_slice(format!("%{}\r\n", pairs.len()).as_bytes());
                } else {
                    out.extend_from_slice(format!("*{}\r\n", pairs.len() * 2).as_bytes());
                }
                for (key, value) in pairs {
                    key.encode(out, resp3);
                    value.encode(out, resp3);
                }
            }
            Self::Set(items) | Self::Push(items) => {
                let mark = match (self, resp3) {
                    (Self::Set(_), true) => '~',
                    (Self::Push(_), true) => '>',
                    _ => '*',
                };
                out.extend_from_slice(format!("{mark}{}\r\n", items.len()).as_bytes());
                for item in items {
                    item.encode(out, resp3);
                }
            }
        }
    }
}

/// A number as the protocol prints scores: the shortest text that reads back the same, `inf`
/// and `-inf` for infinities.
pub fn format_double(number: f64) -> String {
    if number.is_infinite() {
        return if number > 0.0 { "inf".to_owned() } else { "-inf".to_owned() };
    }
    if number == number.trunc() && number.abs() < 1e17 {
        return format!("{}", number as i64);
    }
    format!("{number}")
}

/// Reads requests from a byte stream: RESP arrays of bulk strings, or inline commands
/// (space-separated words on one line), as clients and `redis-cli` send them.
#[derive(Debug)]
pub struct RequestReader<R> {
    inner: R,
    buffer: Vec<u8>,
    start: usize,
}

impl<R: Read> RequestReader<R> {
    /// A reader over `inner`.
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
            start: 0,
        }
    }

    /// Whether bytes of another request are already read.
    pub fn has_buffered(&self) -> bool {
        self.start < self.buffer.len()
    }

    fn fill(&mut self) -> Result<(), ProtocolError> {
        if self.start > 0 && self.start == self.buffer.len() {
            self.buffer.clear();
            self.start = 0;
        }
        let mut chunk = [0u8; 16 * 1024];
        let count = self.inner.read(&mut chunk)?;
        if count == 0 {
            return Err(ProtocolError::Closed);
        }
        self.buffer.extend_from_slice(&chunk[..count]);
        Ok(())
    }

    fn line(&mut self) -> Result<Vec<u8>, ProtocolError> {
        loop {
            if let Some(end) = self.buffer[self.start..].windows(2).position(|pair| pair == b"\r\n") {
                let line = self.buffer[self.start..self.start + end].to_vec();
                self.start += end + 2;
                return Ok(line);
            }
            if self.buffer.len() - self.start > MAX_LINE_BYTES {
                return Err(ProtocolError::Invalid("Protocol error: too big inline request".to_owned()));
            }
            self.fill()?;
        }
    }

    fn exact(&mut self, length: usize) -> Result<Vec<u8>, ProtocolError> {
        while self.buffer.len() - self.start < length + 2 {
            self.fill()?;
        }
        let bytes = self.buffer[self.start..self.start + length].to_vec();
        if &self.buffer[self.start + length..self.start + length + 2] != b"\r\n" {
            return Err(ProtocolError::Invalid("Protocol error: expected '\\r\\n'".to_owned()));
        }
        self.start += length + 2;
        Ok(bytes)
    }

    fn number(line: &[u8], what: &str) -> Result<i64, ProtocolError> {
        std::str::from_utf8(line)
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| ProtocolError::Invalid(format!("Protocol error: invalid {what}")))
    }

    /// The next request's arguments.
    pub fn next(&mut self) -> Result<Vec<Vec<u8>>, ProtocolError> {
        loop {
            let line = self.line()?;
            match line.first() {
                Some(b'*') => {
                    let count = Self::number(&line[1..], "multibulk length")?;
                    if count <= 0 {
                        continue;
                    }
                    if count as usize > MAX_ARGUMENTS {
                        return Err(ProtocolError::Invalid("Protocol error: invalid multibulk length".to_owned()));
                    }
                    let mut arguments = Vec::with_capacity((count as usize).min(1024));
                    for _ in 0..count {
                        let header = self.line()?;
                        if header.first() != Some(&b'$') {
                            return Err(ProtocolError::Invalid(format!(
                                "Protocol error: expected '$', got '{}'",
                                header.first().map_or('?', |byte| char::from(*byte))
                            )));
                        }
                        let length = Self::number(&header[1..], "bulk length")?;
                        if length < 0 || length as usize > MAX_BULK_BYTES {
                            return Err(ProtocolError::Invalid("Protocol error: invalid bulk length".to_owned()));
                        }
                        arguments.push(self.exact(length as usize)?);
                    }
                    return Ok(arguments);
                }
                _ => {
                    let words: Vec<Vec<u8>> = line
                        .split(|byte| *byte == b' ' || *byte == b'\t')
                        .filter(|word| !word.is_empty())
                        .map(<[u8]>::to_vec)
                        .collect();
                    if !words.is_empty() {
                        return Ok(words);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_parse_as_arrays_and_inline_lines() {
        let input = b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\nPING hello\r\n\r\n*1\r\n$4\r\nPING\r\n".to_vec();
        let mut reader = RequestReader::new(&input[..]);
        assert_eq!(reader.next().unwrap(), vec![b"GET".to_vec(), b"k".to_vec()]);
        assert_eq!(reader.next().unwrap(), vec![b"PING".to_vec(), b"hello".to_vec()]);
        assert_eq!(reader.next().unwrap(), vec![b"PING".to_vec()]);
        assert_eq!(reader.next(), Err(ProtocolError::Closed));
    }

    #[test]
    fn hostile_requests_are_refused_without_panics() {
        for input in [
            &b"*x\r\n"[..],
            b"*1\r\n:3\r\n",
            b"*1\r\n$-5\r\n",
            b"*1\r\n$3\r\nabcde\r\n",
            b"*99999999999\r\n",
        ] {
            let mut reader = RequestReader::new(input);
            assert!(matches!(reader.next(), Err(ProtocolError::Invalid(_))), "{input:?}");
        }
        let endless = vec![b'a'; MAX_LINE_BYTES + 10];
        let mut reader = RequestReader::new(&endless[..]);
        assert!(reader.next().is_err());
    }

    #[test]
    fn replies_encode_in_both_protocol_versions() {
        let reply = Reply::Map(vec![(Reply::bulk("a"), Reply::Double(1.5))]);
        let (mut two, mut three) = (Vec::new(), Vec::new());
        reply.encode(&mut two, false);
        reply.encode(&mut three, true);
        assert_eq!(two, b"*2\r\n$1\r\na\r\n$3\r\n1.5\r\n");
        assert_eq!(three, b"%1\r\n$1\r\na\r\n,1.5\r\n");
        let mut null = Vec::new();
        Reply::null().encode(&mut null, false);
        assert_eq!(null, b"$-1\r\n");
        assert_eq!(format_double(3.0), "3");
        assert_eq!(format_double(f64::NEG_INFINITY), "-inf");
    }
}
