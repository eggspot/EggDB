pub(crate) const MAX_CONTROL_LINE: usize = 4096;
pub(crate) const MAX_PAYLOAD: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Frame {
    Connect(String),
    Publish { subject: String, reply: Option<String>, headers: Vec<u8>, payload: Vec<u8> },
    Subscribe { subject: String, queue: Option<String>, sid: String },
    Unsubscribe { sid: String, max: Option<u64> },
    Ping,
    Pong,
}

pub(crate) const UNKNOWN_OPERATION: &str = "Unknown Protocol Operation";
pub(crate) const CONTROL_LINE_TOO_LONG: &str = "Maximum Control Line Exceeded";
pub(crate) const PAYLOAD_TOO_LARGE: &str = "Maximum Payload Violation";

fn size(text: &str) -> Result<usize, &'static str> {
    text.parse::<usize>().map_err(|_| UNKNOWN_OPERATION)
}

fn line_end(buffer: &[u8]) -> Option<(usize, usize)> {
    let newline = buffer.iter().position(|&byte| byte == b'\n')?;
    let end = if newline > 0 && buffer[newline - 1] == b'\r' { newline - 1 } else { newline };
    Some((end, newline + 1))
}

/// The next whole frame at the start of `buffer` and how many bytes it used, `None` when more
/// bytes are needed, or the `-ERR` text when the input breaks the protocol.
pub(crate) fn next_frame(buffer: &[u8]) -> Result<Option<(Frame, usize)>, &'static str> {
    let Some((end, after)) = line_end(buffer) else {
        return if buffer.len() > MAX_CONTROL_LINE { Err(CONTROL_LINE_TOO_LONG) } else { Ok(None) };
    };
    if end > MAX_CONTROL_LINE {
        return Err(CONTROL_LINE_TOO_LONG);
    }
    let line = std::str::from_utf8(&buffer[..end]).map_err(|_| UNKNOWN_OPERATION)?;
    let trimmed = line.trim_start();
    let (operation, rest) = trimmed.split_once([' ', '\t']).unwrap_or((trimmed, ""));
    let arguments: Vec<&str> = rest.split([' ', '\t']).filter(|argument| !argument.is_empty()).collect();
    let owned = |text: &str| text.to_owned();
    match operation.to_ascii_uppercase().as_str() {
        "PING" => Ok(Some((Frame::Ping, after))),
        "PONG" => Ok(Some((Frame::Pong, after))),
        "CONNECT" => Ok(Some((Frame::Connect(rest.trim().to_owned()), after))),
        "SUB" => {
            let frame = match arguments.as_slice() {
                [subject, sid] => Frame::Subscribe { subject: owned(subject), queue: None, sid: owned(sid) },
                [subject, queue, sid] => Frame::Subscribe { subject: owned(subject), queue: Some(owned(queue)), sid: owned(sid) },
                _ => return Err(UNKNOWN_OPERATION),
            };
            Ok(Some((frame, after)))
        }
        "UNSUB" => {
            let frame = match arguments.as_slice() {
                [sid] => Frame::Unsubscribe { sid: owned(sid), max: None },
                [sid, max] => Frame::Unsubscribe { sid: owned(sid), max: Some(max.parse().map_err(|_| UNKNOWN_OPERATION)?) },
                _ => return Err(UNKNOWN_OPERATION),
            };
            Ok(Some((frame, after)))
        }
        "PUB" => {
            let (subject, reply, total) = match arguments.as_slice() {
                [subject, total] => (*subject, None, size(total)?),
                [subject, reply, total] => (*subject, Some(*reply), size(total)?),
                _ => return Err(UNKNOWN_OPERATION),
            };
            body(buffer, after, subject, reply, 0, total)
        }
        "HPUB" => {
            let (subject, reply, header_size, total) = match arguments.as_slice() {
                [subject, header_size, total] => (*subject, None, size(header_size)?, size(total)?),
                [subject, reply, header_size, total] => (*subject, Some(*reply), size(header_size)?, size(total)?),
                _ => return Err(UNKNOWN_OPERATION),
            };
            if header_size > total {
                return Err(UNKNOWN_OPERATION);
            }
            body(buffer, after, subject, reply, header_size, total)
        }
        _ => Err(UNKNOWN_OPERATION),
    }
}

fn body(buffer: &[u8], start: usize, subject: &str, reply: Option<&str>, header_size: usize, total: usize) -> Result<Option<(Frame, usize)>, &'static str> {
    if total > MAX_PAYLOAD {
        return Err(PAYLOAD_TOO_LARGE);
    }
    let Some(rest) = buffer.get(start..) else {
        return Ok(None);
    };
    if rest.len() < total + 1 {
        return Ok(None);
    }
    let used = match &rest[total..] {
        [b'\r', b'\n', ..] => total + 2,
        [b'\n', ..] => total + 1,
        [b'\r'] => return Ok(None),
        _ => return Err(UNKNOWN_OPERATION),
    };
    let frame = Frame::Publish {
        subject: subject.to_owned(),
        reply: reply.map(str::to_owned),
        headers: rest[..header_size].to_vec(),
        payload: rest[header_size..total].to_vec(),
    };
    Ok(Some((frame, start + used)))
}

/// Whether `subject` is a valid subject, or a valid subscription filter when `wildcards` is
/// true: dot-separated non-empty tokens without white space, where `*` stands for one token and
/// a final `>` for one or more.
pub(crate) fn valid_subject(subject: &str, wildcards: bool) -> bool {
    if subject.is_empty() || subject.len() > MAX_CONTROL_LINE {
        return false;
    }
    let count = subject.split('.').count();
    subject.split('.').enumerate().all(|(at, token)| {
        !token.is_empty()
            && !token.bytes().any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
            && match token {
                "*" => wildcards,
                ">" => wildcards && at + 1 == count,
                _ => true,
            }
    })
}

/// Whether a subscription to `filter` receives messages published to `subject`.
pub(crate) fn subject_matches(filter: &str, subject: &str) -> bool {
    let mut filter_tokens = filter.split('.');
    let mut subject_tokens = subject.split('.');
    loop {
        match (filter_tokens.next(), subject_tokens.next()) {
            (Some(">"), Some(_)) | (None, None) => return true,
            (Some("*"), Some(_)) => {}
            (Some(wanted), Some(token)) if wanted == token => {}
            _ => return false,
        }
    }
}

/// A delivered message: `HMSG` when it has headers and the subscriber asked for them, else `MSG`.
pub(crate) fn message(subject: &str, sid: &str, reply: Option<&str>, headers: &[u8], payload: &[u8], with_headers: bool) -> Vec<u8> {
    let reply = reply.map(|reply| format!(" {reply}")).unwrap_or_default();
    let mut out = if with_headers && !headers.is_empty() {
        let mut out = format!("HMSG {subject} {sid}{reply} {} {}\r\n", headers.len(), headers.len() + payload.len()).into_bytes();
        out.extend_from_slice(headers);
        out
    } else {
        format!("MSG {subject} {sid}{reply} {}\r\n", payload.len()).into_bytes()
    };
    out.extend_from_slice(payload);
    out.extend_from_slice(b"\r\n");
    out
}
