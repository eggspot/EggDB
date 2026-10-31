pub(crate) const MAX_HEADER_BYTES: usize = 64 * 1024;
pub(crate) const MAX_HEADERS: usize = 128;
pub(crate) const MAX_BODY_BYTES: usize = 16 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Frame {
    pub(crate) command: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Frame {
    pub(crate) fn new(command: &str) -> Self {
        Self { command: command.to_owned(), headers: Vec::new(), body: Vec::new() }
    }

    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    pub(crate) fn with(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    pub(crate) fn with_body(mut self, body: &[u8]) -> Self {
        self.body = body.to_vec();
        self
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let escaped = !matches!(self.command.as_str(), "CONNECTED");
        let mut out = Vec::with_capacity(self.body.len() + 64);
        out.extend_from_slice(self.command.as_bytes());
        out.push(b'\n');
        for (name, value) in &self.headers {
            out.extend_from_slice(escape(name, escaped).as_bytes());
            out.push(b':');
            out.extend_from_slice(escape(value, escaped).as_bytes());
            out.push(b'\n');
        }
        if !self.body.is_empty() {
            out.extend_from_slice(format!("content-length:{}\n", self.body.len()).as_bytes());
        }
        out.push(b'\n');
        out.extend_from_slice(&self.body);
        out.push(0);
        out
    }
}

fn escape(text: &str, escaped: bool) -> String {
    if !escaped {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            ':' => out.push_str("\\c"),
            other => out.push(other),
        }
    }
    out
}

fn unescape(text: &str) -> Result<String, &'static str> {
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next() {
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('c') => out.push(':'),
            _ => return Err("a header holds an undefined escape sequence"),
        }
    }
    Ok(out)
}

/// The next whole frame at the start of `buffer` and how many bytes it used (heart-beat end of
/// lines before it included), `None` when more bytes are needed, or why the input is refused.
pub(crate) fn next_frame(buffer: &[u8]) -> Result<Option<(Frame, usize)>, &'static str> {
    let mut at = 0;
    while let Some(byte) = buffer.get(at) {
        match byte {
            b'\n' | b'\r' => at += 1,
            _ => break,
        }
    }
    if at == buffer.len() {
        return Ok(None);
    }
    let mut lines = Vec::new();
    let mut line_start = at;
    let body_start = loop {
        let Some(offset) = buffer[line_start..].iter().position(|&byte| byte == b'\n') else {
            return if buffer.len() - at > MAX_HEADER_BYTES { Err("the frame's headers are too long") } else { Ok(None) };
        };
        let newline = line_start + offset;
        let line = buffer[line_start..newline].strip_suffix(b"\r").unwrap_or(&buffer[line_start..newline]);
        if newline - at > MAX_HEADER_BYTES {
            return Err("the frame's headers are too long");
        }
        if line.is_empty() {
            break newline + 1;
        }
        lines.push(std::str::from_utf8(line).map_err(|_| "a frame header is not UTF-8")?);
        if lines.len() > MAX_HEADERS + 1 {
            return Err("the frame has too many headers");
        }
        line_start = newline + 1;
    };
    let Some((command, header_lines)) = lines.split_first() else {
        return Err("a frame has no command");
    };
    let escaped = !matches!(*command, "CONNECT" | "STOMP");
    let mut headers = Vec::with_capacity(header_lines.len());
    for line in header_lines {
        let (name, value) = line.split_once(':').ok_or("a frame header has no colon")?;
        let (name, value) = if escaped { (unescape(name)?, unescape(value)?) } else { (name.to_owned(), value.to_owned()) };
        headers.push((name, value));
    }
    let length = headers.iter().find(|(name, _)| name == "content-length").map(|(_, value)| value.trim().parse::<usize>());
    let (body_end, used) = match length {
        Some(Ok(length)) => {
            if length > MAX_BODY_BYTES {
                return Err("the frame's body is larger than 16 MiB");
            }
            let Some(&terminator) = buffer.get(body_start + length) else {
                return Ok(None);
            };
            if terminator != 0 {
                return Err("the frame's body is not followed by a NULL octet");
            }
            (body_start + length, body_start + length + 1)
        }
        Some(Err(_)) => return Err("the content-length header is not a number"),
        None => match buffer[body_start..].iter().position(|&byte| byte == 0) {
            Some(offset) => (body_start + offset, body_start + offset + 1),
            None if buffer.len() - body_start > MAX_BODY_BYTES => return Err("the frame's body is larger than 16 MiB"),
            None => return Ok(None),
        },
    };
    let frame = Frame { command: (*command).to_owned(), headers, body: buffer[body_start..body_end].to_vec() };
    Ok(Some((frame, used)))
}
