use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(2);
const MOST_REPLY_BYTES: usize = 64 << 20;

/// A RESP reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reply {
    Simple(String),
    Error(String),
    Integer(i64),
    Bulk(Option<Vec<u8>>),
    Array(Vec<Reply>),
}

/// A RESP connection to one server, signed in.
#[derive(Debug)]
pub(crate) struct RespClient {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl RespClient {
    pub(crate) fn connect(address: &str, user: &str, password: &str) -> Result<Self, String> {
        let stream = TcpStream::connect(address).map_err(|error| error.to_string())?;
        stream.set_read_timeout(Some(TIMEOUT)).map_err(|error| error.to_string())?;
        stream.set_write_timeout(Some(TIMEOUT)).map_err(|error| error.to_string())?;
        let writer = stream.try_clone().map_err(|error| error.to_string())?;
        let mut client = Self { reader: BufReader::new(stream), writer };
        match client.call(&["AUTH", user, password])? {
            Reply::Error(message) => Err(message),
            _ => Ok(client),
        }
    }

    /// Sends one command and reads its reply; `Err` when the connection failed, so the
    /// command's outcome is unknown.
    pub(crate) fn call(&mut self, words: &[&str]) -> Result<Reply, String> {
        let mut request = format!("*{}\r\n", words.len());
        for word in words {
            request.push_str(&format!("${}\r\n{word}\r\n", word.len()));
        }
        self.writer.write_all(request.as_bytes()).map_err(|error| error.to_string())?;
        self.reply(0)
    }

    fn line(&mut self) -> Result<String, String> {
        let mut line = String::new();
        let read = self.reader.read_line(&mut line).map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("the server closed the connection".into());
        }
        Ok(line.trim_end_matches(['\r', '\n']).to_owned())
    }

    fn reply(&mut self, depth: usize) -> Result<Reply, String> {
        if depth > 8 {
            return Err("a reply nests too deeply".into());
        }
        let line = self.line()?;
        let (kind, rest) = line.split_at(line.len().min(1));
        match kind {
            "+" => Ok(Reply::Simple(rest.to_owned())),
            "-" => Ok(Reply::Error(rest.to_owned())),
            ":" => rest.parse().map(Reply::Integer).map_err(|_| format!("a malformed integer: {rest}")),
            "$" => {
                let length: i64 = rest.parse().map_err(|_| format!("a malformed length: {rest}"))?;
                if length < 0 {
                    return Ok(Reply::Bulk(None));
                }
                let length = usize::try_from(length).ok().filter(|length| *length <= MOST_REPLY_BYTES).ok_or("a reply is too large")?;
                let mut bytes = vec![0u8; length + 2];
                std::io::Read::read_exact(&mut self.reader, &mut bytes).map_err(|error| error.to_string())?;
                bytes.truncate(length);
                Ok(Reply::Bulk(Some(bytes)))
            }
            "*" | "~" => {
                let count: i64 = rest.parse().map_err(|_| format!("a malformed count: {rest}"))?;
                let mut items = Vec::new();
                for _ in 0..count.max(0) {
                    items.push(self.reply(depth + 1)?);
                }
                Ok(Reply::Array(items))
            }
            other => Err(format!("an unknown reply type {other}")),
        }
    }
}
