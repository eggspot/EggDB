use crate::{HttpError, MAX_HEAD_BYTES, MAX_HEADERS, Request};
use eggdb_platform::connection_limits::Reservation;
use std::io::{self, Read};

const MAX_CHUNK_LINE_BYTES: usize = 1024;
const MAX_CHUNK_SIZE_DIGITS: usize = 16;
const CHUNK_FRAMING_ALLOWANCE: usize = 64 * 1024;
const READ_BYTES: usize = 8192;

pub(crate) trait RequestSource: Read {
    fn head_started(&mut self) {}
    fn head_ended(&mut self) {}
}

pub(crate) struct PlainSource<R>(pub(crate) R);

impl<R: Read> Read for PlainSource<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer)
    }
}

impl<R: Read> RequestSource for PlainSource<R> {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Version {
    Http10,
    Http11,
}

impl Version {
    fn parse(text: &str) -> Option<Self> {
        let minor = text.strip_prefix("HTTP/1.")?;
        match minor.as_bytes() {
            [b'0'] => Some(Self::Http10),
            [digit] if digit.is_ascii_digit() => Some(Self::Http11),
            _ => None,
        }
    }

    pub(crate) fn wants_persistence(self, request: &Request) -> bool {
        let mut close = false;
        let mut keep_alive = false;
        for option in request
            .headers
            .iter()
            .filter(|(name, _)| name == "connection")
            .flat_map(|(_, value)| value.split(','))
            .map(|option| option.trim_matches([' ', '\t']))
        {
            close |= option.eq_ignore_ascii_case("close");
            keep_alive |= option.eq_ignore_ascii_case("keep-alive");
        }
        !close
            && match self {
                Self::Http11 => true,
                Self::Http10 => keep_alive,
            }
    }
}

enum Framing {
    Empty,
    Length(usize),
    Chunked,
}

pub(crate) struct RequestReader<S> {
    pub(crate) source: S,
    buffered: Vec<u8>,
}

impl<S: RequestSource> RequestReader<S> {
    pub(crate) fn new(source: S) -> Self {
        Self {
            source,
            buffered: Vec::new(),
        }
    }

    fn fill(&mut self) -> Result<(), HttpError> {
        let mut chunk = [0u8; READ_BYTES];
        let count = self
            .source
            .read(&mut chunk)
            .map_err(|_| HttpError::Closed)?;
        if count == 0 {
            return Err(HttpError::Closed);
        }
        self.buffered
            .extend_from_slice(chunk.get(..count).unwrap_or_default());
        Ok(())
    }

    fn skip_empty_lines(&mut self) {
        let blank = self
            .buffered
            .chunks(2)
            .take_while(|pair| *pair == b"\r\n")
            .count();
        if blank > 0 {
            self.buffered.drain(..blank * 2);
        }
    }

    fn read_head(&mut self) -> Result<Vec<u8>, HttpError> {
        if self.buffered.is_empty() {
            self.fill()?;
        }
        self.source.head_started();
        let mut searched = 0;
        loop {
            self.skip_empty_lines();
            let from = searched.min(self.buffered.len()).saturating_sub(3);
            if let Some(at) = self
                .buffered
                .get(from..)
                .unwrap_or_default()
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
            {
                let end = from + at + 4;
                if end > MAX_HEAD_BYTES {
                    return Err(HttpError::TooLarge);
                }
                self.source.head_ended();
                return Ok(self.buffered.drain(..end).collect());
            }
            if self.buffered.len() >= MAX_HEAD_BYTES {
                return Err(HttpError::TooLarge);
            }
            searched = self.buffered.len();
            self.fill()?;
        }
    }

    fn take_into(&mut self, body: &mut Vec<u8>, count: usize) -> Result<(), HttpError> {
        let from_buffer = count.min(self.buffered.len());
        body.extend(self.buffered.drain(..from_buffer));
        let start = body.len();
        let end = start + (count - from_buffer);
        body.resize(end, 0);
        let mut at = start;
        while at < end {
            let read = self
                .source
                .read(body.get_mut(at..end).unwrap_or_default())
                .map_err(|_| HttpError::Closed)?;
            if read == 0 {
                return Err(HttpError::Closed);
            }
            at += read;
        }
        Ok(())
    }

    fn take_line(&mut self, max_bytes: usize) -> Result<Vec<u8>, HttpError> {
        loop {
            if let Some(newline) = self.buffered.iter().position(|&byte| byte == b'\n') {
                let line_end = newline.checked_sub(1).ok_or(HttpError::Malformed)?;
                if line_end > max_bytes {
                    return Err(HttpError::Malformed);
                }
                let line = self.buffered.get(..line_end).unwrap_or_default();
                if self.buffered.get(line_end) != Some(&b'\r') || line.contains(&b'\r') {
                    return Err(HttpError::Malformed);
                }
                let line = line.to_vec();
                self.buffered.drain(..=newline);
                return Ok(line);
            }
            if self.buffered.len() > max_bytes {
                return Err(HttpError::Malformed);
            }
            self.fill()?;
        }
    }

    fn take_crlf(&mut self) -> Result<(), HttpError> {
        while self.buffered.len() < 2 {
            self.fill()?;
        }
        if self.buffered.get(..2) != Some(b"\r\n") {
            return Err(HttpError::Malformed);
        }
        self.buffered.drain(..2);
        Ok(())
    }

    fn read_chunked_body(
        &mut self,
        max_body_bytes: usize,
        held: &mut Reservation<'_>,
    ) -> Result<Vec<u8>, HttpError> {
        let mut body = Vec::new();
        let mut framing = 0usize;
        loop {
            let line = self.take_line(MAX_CHUNK_LINE_BYTES)?;
            framing = framing.saturating_add(line.len() + 4);
            if framing > body.len().saturating_add(CHUNK_FRAMING_ALLOWANCE) {
                return Err(HttpError::Malformed);
            }
            let size = chunk_size(&line)?;
            if size == 0 {
                break;
            }
            let total = body
                .len()
                .checked_add(size)
                .ok_or(HttpError::BodyTooLarge)?;
            if total > max_body_bytes {
                return Err(HttpError::BodyTooLarge);
            }
            if !held.hold_if_large(total) {
                return Err(HttpError::OverMemoryBudget);
            }
            body.reserve_exact(size);
            self.take_into(&mut body, size)?;
            self.take_crlf()?;
        }
        let mut trailer_bytes = 0usize;
        let mut trailers = 0usize;
        loop {
            let line = self.take_line(MAX_HEAD_BYTES)?;
            if line.is_empty() {
                return Ok(body);
            }
            trailer_bytes += line.len() + 2;
            trailers += 1;
            if trailer_bytes > MAX_HEAD_BYTES || trailers > MAX_HEADERS {
                return Err(HttpError::TooLarge);
            }
            let text = std::str::from_utf8(&line).map_err(|_| HttpError::Malformed)?;
            parse_field_line(text)?;
        }
    }

    pub(crate) fn next_request(
        &mut self,
        max_body_bytes: usize,
        held: &mut Reservation<'_>,
        on_continue: &mut dyn FnMut(),
    ) -> Result<(Request, Version), HttpError> {
        let head = self.read_head()?;
        let (mut request, version) = parse_head(&head)?;
        check_host(&request, version)?;
        let framing = framing(&request, version)?;
        let declared = match framing {
            Framing::Empty => 0,
            Framing::Length(length) => length,
            Framing::Chunked => 0,
        };
        if declared > max_body_bytes {
            return Err(HttpError::BodyTooLarge);
        }
        if !held.hold_if_large(declared) {
            return Err(HttpError::OverMemoryBudget);
        }
        let expects_body = !matches!(framing, Framing::Empty | Framing::Length(0));
        if expects_body
            && version == Version::Http11
            && self.buffered.is_empty()
            && request
                .header("expect")
                .is_some_and(|expect| expect.eq_ignore_ascii_case("100-continue"))
        {
            on_continue();
        }
        request.body = match framing {
            Framing::Empty => Vec::new(),
            Framing::Length(length) => {
                let mut body = Vec::new();
                self.take_into(&mut body, length)?;
                body
            }
            Framing::Chunked => self.read_chunked_body(max_body_bytes, held)?,
        };
        Ok((request, version))
    }
}

fn is_token(text: &str) -> bool {
    !text.is_empty()
        && text.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn parse_field_line(line: &str) -> Result<(String, String), HttpError> {
    let (name, value) = line.split_once(':').ok_or(HttpError::Malformed)?;
    if !is_token(name)
        || value
            .bytes()
            .any(|byte| byte == 0 || byte == b'\r' || byte == b'\n')
    {
        return Err(HttpError::Malformed);
    }
    Ok((
        name.to_ascii_lowercase(),
        value.trim_matches([' ', '\t']).to_owned(),
    ))
}

fn parse_head(head: &[u8]) -> Result<(Request, Version), HttpError> {
    let text = std::str::from_utf8(head).map_err(|_| HttpError::Malformed)?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    if request_line.contains(['\r', '\n', '\0']) {
        return Err(HttpError::Malformed);
    }
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(HttpError::Malformed);
    };
    let version = Version::parse(version).ok_or(HttpError::Malformed)?;
    if method.is_empty()
        || !method.bytes().all(|byte| byte.is_ascii_uppercase())
        || !target.starts_with('/')
    {
        return Err(HttpError::Malformed);
    }
    let mut headers = Vec::new();
    for line in lines.filter(|line| !line.is_empty()) {
        if headers.len() >= MAX_HEADERS {
            return Err(HttpError::TooLarge);
        }
        headers.push(parse_field_line(line)?);
    }
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(query.to_owned())),
        None => (target, None),
    };
    Ok((
        Request {
            method: method.to_owned(),
            path: path.to_owned(),
            query,
            headers,
            body: Vec::new(),
        },
        version,
    ))
}

fn check_host(request: &Request, version: Version) -> Result<(), HttpError> {
    let hosts = request
        .headers
        .iter()
        .filter(|(name, _)| name == "host")
        .count();
    match (hosts, version) {
        (1, _) | (0, Version::Http10) => Ok(()),
        _ => Err(HttpError::InvalidHost),
    }
}

fn framing(request: &Request, version: Version) -> Result<Framing, HttpError> {
    let mut encoded = false;
    let mut codings = Vec::new();
    for (_, value) in request
        .headers
        .iter()
        .filter(|(name, _)| name == "transfer-encoding")
    {
        encoded = true;
        codings.extend(
            value
                .split(',')
                .map(|coding| coding.trim_matches([' ', '\t']))
                .filter(|coding| !coding.is_empty()),
        );
    }
    let lengths: Vec<&str> = request
        .headers
        .iter()
        .filter(|(name, _)| name == "content-length")
        .map(|(_, value)| value.as_str())
        .collect();
    if encoded {
        if !lengths.is_empty() || version == Version::Http10 {
            return Err(HttpError::Malformed);
        }
        let chunked = |coding: &str| coding.eq_ignore_ascii_case("chunked");
        let chunked_count = codings.iter().filter(|coding| chunked(coding)).count();
        return match codings.last() {
            Some(last) if chunked(last) && chunked_count == 1 => {
                if codings.len() == 1 {
                    Ok(Framing::Chunked)
                } else {
                    Err(HttpError::UnsupportedTransferCoding)
                }
            }
            _ => Err(HttpError::Malformed),
        };
    }
    match lengths.as_slice() {
        [] => Ok(Framing::Empty),
        [length] => {
            if length.is_empty() || !length.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(HttpError::Malformed);
            }
            length
                .parse::<usize>()
                .map(Framing::Length)
                .map_err(|_| HttpError::BodyTooLarge)
        }
        _ => Err(HttpError::Malformed),
    }
}

fn chunk_size(line: &[u8]) -> Result<usize, HttpError> {
    let digits = line
        .iter()
        .take_while(|byte| byte.is_ascii_hexdigit())
        .count();
    if digits == 0 {
        return Err(HttpError::Malformed);
    }
    let extension = line
        .get(digits..)
        .unwrap_or_default()
        .iter()
        .skip_while(|&&byte| byte == b' ' || byte == b'\t')
        .copied()
        .collect::<Vec<u8>>();
    if extension.first().is_some_and(|&byte| byte != b';')
        || extension
            .iter()
            .any(|&byte| (byte < 0x20 && byte != b'\t') || byte == 0x7f)
    {
        return Err(HttpError::Malformed);
    }
    let significant = line
        .get(..digits)
        .unwrap_or_default()
        .iter()
        .skip_while(|&&byte| byte == b'0')
        .copied()
        .collect::<Vec<u8>>();
    if significant.len() > MAX_CHUNK_SIZE_DIGITS {
        return Err(HttpError::BodyTooLarge);
    }
    let mut size = 0usize;
    for byte in significant {
        let digit = char::from(byte)
            .to_digit(16)
            .and_then(|digit| usize::try_from(digit).ok())
            .ok_or(HttpError::Malformed)?;
        size = size
            .checked_mul(16)
            .and_then(|size| size.checked_add(digit))
            .ok_or(HttpError::BodyTooLarge)?;
    }
    Ok(size)
}
