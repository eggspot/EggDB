use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

#[derive(Debug)]
pub(crate) struct RespClient {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
    request: Vec<u8>,
    line: String,
}

impl RespClient {
    pub(crate) fn connect(address: &str) -> std::io::Result<Self> {
        let stream = TcpStream::connect(address)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        Ok(Self {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
            request: Vec::new(),
            line: String::new(),
        })
    }

    pub(crate) fn queue(&mut self, arguments: &[&[u8]]) {
        self.request
            .extend_from_slice(format!("*{}\r\n", arguments.len()).as_bytes());
        for argument in arguments {
            self.request
                .extend_from_slice(format!("${}\r\n", argument.len()).as_bytes());
            self.request.extend_from_slice(argument);
            self.request.extend_from_slice(b"\r\n");
        }
    }

    pub(crate) fn send(&mut self) -> std::io::Result<()> {
        self.writer.write_all(&self.request)?;
        self.request.clear();
        Ok(())
    }

    pub(crate) fn reply(&mut self) -> Result<Option<Vec<u8>>, String> {
        self.line.clear();
        self.reader
            .read_line(&mut self.line)
            .map_err(|error| format!("reading a reply: {error}"))?;
        let line = self.line.trim_end_matches("\r\n");
        let (kind, rest) = line.split_at_checked(1).ok_or("an empty reply")?;
        match kind {
            "+" | ":" => Ok(Some(rest.as_bytes().to_vec())),
            "-" => Err(format!("the server answered an error: {rest}")),
            "$" => {
                let length: i64 = rest.parse().map_err(|_| "a malformed bulk length")?;
                if length < 0 {
                    return Ok(None);
                }
                let mut value = vec![0u8; length as usize + 2];
                self.reader
                    .read_exact(&mut value)
                    .map_err(|error| format!("reading a bulk reply: {error}"))?;
                value.truncate(length as usize);
                Ok(Some(value))
            }
            other => Err(format!("an unexpected reply type {other}")),
        }
    }
}
