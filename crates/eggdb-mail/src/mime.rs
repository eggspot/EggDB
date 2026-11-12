//! MIME (RFC 2045, RFC 2046): a message's tree of parts with their content types, transfer
//! encodings and dispositions, multipart bodies split at their boundaries and `message/rfc822`
//! parts read as the messages they hold, IMAP's part numbers (RFC 3501 section 6.4.5), and part
//! bodies decoded from base64 and quoted-printable.

use crate::message::{body_start, headers, text_in};
use std::ops::Range;

const MOST_DEPTH: usize = 32;
const MOST_PARTS: usize = 1000;

/// One MIME entity: a whole message, a part of a multipart, or a message inside a part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// Where its header section lies in the message, the empty line ending it included.
    pub header: Range<usize>,
    /// Where its body lies in the message.
    pub body: Range<usize>,
    /// The media type, lower case (`text`, `multipart`, `message`, `image`, ...).
    pub major: String,
    /// The subtype, lower case.
    pub minor: String,
    /// The content type's parameters, names lower case.
    pub parameters: Vec<(String, String)>,
    /// The transfer encoding, lower case: `7bit` when none is given.
    pub encoding: String,
    /// `Content-ID`.
    pub id: Option<String>,
    /// `Content-Description`.
    pub description: Option<String>,
    /// `Content-Disposition`'s kind, lower case, and its parameters.
    pub disposition: Option<(String, Vec<(String, String)>)>,
    /// A multipart's parts, or the one message a `message/rfc822` part holds.
    pub children: Vec<Part>,
}

impl Part {
    /// Whether it is `multipart/*`.
    pub fn is_multipart(&self) -> bool {
        self.major == "multipart"
    }

    /// Whether it is a `message/rfc822` (or `message/global`) part holding a message.
    pub fn is_message(&self) -> bool {
        self.major == "message" && matches!(self.minor.as_str(), "rfc822" | "global") && !self.children.is_empty()
    }

    /// A content type parameter, by name in any case.
    pub fn parameter(&self, name: &str) -> Option<&str> {
        self.parameters.iter().find(|(found, _)| found.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }

    /// The file name from `Content-Disposition`'s `filename` or the content type's `name`.
    pub fn file_name(&self) -> Option<&str> {
        self.disposition
            .as_ref()
            .and_then(|(_, parameters)| parameters.iter().find(|(name, _)| name == "filename").map(|(_, value)| value.as_str()))
            .or_else(|| self.parameter("name"))
    }

    /// Whether a mail client shows it as an attachment rather than as the message's text.
    pub fn is_attachment(&self) -> bool {
        match &self.disposition {
            Some((kind, _)) if kind == "attachment" => true,
            Some((kind, _)) if kind == "inline" => !matches!(self.major.as_str(), "text") && self.file_name().is_some(),
            _ => !self.is_multipart() && self.major != "text" && !self.is_message() || self.file_name().is_some(),
        }
    }
}

fn parameters_of(text: &str) -> (String, Vec<(String, String)>) {
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for character in text.chars() {
        match character {
            _ if escaped => {
                current.push(character);
                escaped = false;
            }
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ';' if !quoted => pieces.push(std::mem::take(&mut current)),
            _ => current.push(character),
        }
    }
    pieces.push(current);
    let mut pieces = pieces.into_iter();
    let value = pieces.next().unwrap_or_default().trim().to_ascii_lowercase();
    let parameters = pieces
        .filter_map(|piece| {
            let (name, value) = piece.split_once('=')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        })
        .collect();
    (value, parameters)
}

fn header_value(fields: &[(String, String)], name: &str) -> Option<String> {
    fields.iter().find(|(found, _)| found.eq_ignore_ascii_case(name)).map(|(_, value)| value.clone())
}

fn line_starts(body: &[u8]) -> impl Iterator<Item = usize> + '_ {
    std::iter::once(0).chain(body.iter().enumerate().filter(|(_, byte)| **byte == b'\n').map(|(at, _)| at + 1))
}

fn split_multipart(body: &[u8], boundary: &str) -> Vec<Range<usize>> {
    let delimiter = format!("--{boundary}");
    let mut parts = Vec::new();
    let mut open: Option<usize> = None;
    for start in line_starts(body) {
        let line = &body[start..];
        if !line.starts_with(delimiter.as_bytes()) {
            continue;
        }
        let after = &line[delimiter.len()..];
        let closing = after.starts_with(b"--");
        let rest = if closing { &after[2..] } else { after };
        let line_end = rest.iter().position(|byte| *byte == b'\n').map_or(body.len(), |at| start + delimiter.len() + usize::from(closing) * 2 + at + 1);
        let only_space = rest.iter().take_while(|byte| **byte != b'\n').all(|byte| matches!(byte, b' ' | b'\t' | b'\r'));
        if !only_space {
            continue;
        }
        if let Some(from) = open {
            let mut end = start;
            if end > from && body[end - 1] == b'\n' {
                end -= 1;
                if end > from && body[end - 1] == b'\r' {
                    end -= 1;
                }
            }
            parts.push(from..end.max(from));
        }
        if closing {
            return parts;
        }
        open = Some(line_end.min(body.len()));
    }
    if let Some(from) = open {
        parts.push(from..body.len());
    }
    parts
}

fn entity(raw: &[u8], range: Range<usize>, digest: bool, depth: usize, count: &mut usize) -> Part {
    *count += 1;
    let slice = &raw[range.clone()];
    let header_length = if slice.starts_with(b"\r\n") {
        2
    } else if slice.starts_with(b"\n") {
        1
    } else {
        body_start(slice).min(slice.len())
    };
    let header_end = range.start + header_length;
    let fields = headers(slice);
    let (kind, parameters) = match header_value(&fields, "Content-Type") {
        Some(value) => parameters_of(&value),
        None if digest => ("message/rfc822".to_owned(), Vec::new()),
        None => ("text/plain".to_owned(), Vec::new()),
    };
    let (major, minor) = kind.split_once('/').map_or_else(
        || ("text".to_owned(), "plain".to_owned()),
        |(major, minor)| (major.trim().to_owned(), minor.trim().to_owned()),
    );
    let mut part = Part {
        header: range.start..header_end,
        body: header_end..range.end,
        major,
        minor,
        parameters,
        encoding: header_value(&fields, "Content-Transfer-Encoding").map_or_else(|| "7bit".to_owned(), |value| value.trim().to_ascii_lowercase()),
        id: header_value(&fields, "Content-ID"),
        description: header_value(&fields, "Content-Description"),
        disposition: header_value(&fields, "Content-Disposition").map(|value| parameters_of(&value)),
        children: Vec::new(),
    };
    if depth >= MOST_DEPTH {
        return part;
    }
    if part.is_multipart()
        && let Some(boundary) = part.parameter("boundary").map(str::to_owned)
        && !boundary.is_empty()
    {
        let body = part.body.clone();
        let digest_parts = part.minor == "digest";
        for inner in split_multipart(&raw[body.clone()], &boundary) {
            if *count >= MOST_PARTS {
                break;
            }
            let absolute = body.start + inner.start..body.start + inner.end;
            part.children.push(entity(raw, absolute, digest_parts, depth + 1, count));
        }
    } else if part.major == "message" && matches!(part.minor.as_str(), "rfc822" | "global") && *count < MOST_PARTS {
        let body = part.body.clone();
        part.children.push(entity(raw, body, false, depth + 1, count));
    }
    part
}

/// The part tree of the message `raw`.
pub fn parse(raw: &[u8]) -> Part {
    let mut count = 0;
    entity(raw, 0..raw.len(), false, 0, &mut count)
}

/// The part IMAP numbers `numbers` (`[1, 2]` for `1.2`) name in `root`: a multipart's parts
/// count from 1, the body of a message that is not multipart is its part 1, and the numbers
/// after a `message/rfc822` part address the message it holds.
pub fn part_at<'p>(root: &'p Part, numbers: &[usize]) -> Option<&'p Part> {
    let mut message = root;
    let mut selected = root;
    for (at, number) in numbers.iter().enumerate() {
        let index = number.checked_sub(1)?;
        selected = if message.is_multipart() {
            message.children.get(index)?
        } else if index == 0 {
            message
        } else {
            return None;
        };
        if at + 1 < numbers.len() {
            message = if selected.is_message() { selected.children.first()? } else { selected };
        }
    }
    Some(selected)
}

/// Every leaf part (not multipart) with its IMAP part number, in order; a `message/rfc822`
/// part is a leaf here.
pub fn leaves(root: &Part) -> Vec<(String, &Part)> {
    let mut out = Vec::new();
    collect_leaves(root, "", true, &mut out);
    out
}

fn collect_leaves<'p>(part: &'p Part, number: &str, top: bool, out: &mut Vec<(String, &'p Part)>) {
    if part.is_multipart() {
        for (index, child) in part.children.iter().enumerate() {
            let child_number = if number.is_empty() { (index + 1).to_string() } else { format!("{number}.{}", index + 1) };
            collect_leaves(child, &child_number, false, out);
        }
    } else {
        out.push((if top && number.is_empty() { "1".to_owned() } else { number.to_owned() }, part));
    }
}

fn base64_body(bytes: &[u8]) -> Vec<u8> {
    let compact: Vec<u8> = bytes.iter().copied().filter(|byte| !byte.is_ascii_whitespace()).collect();
    eggdb_crypto::base64_decode(&compact).unwrap_or_default()
}

fn quoted_printable_body(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'=' {
            out.push(bytes[at]);
            at += 1;
            continue;
        }
        match bytes.get(at + 1..at + 3) {
            Some(b"\r\n") => at += 3,
            Some([b'\n', _]) => at += 2,
            Some(pair) => match std::str::from_utf8(pair).ok().and_then(|pair| u8::from_str_radix(pair, 16).ok()) {
                Some(byte) => {
                    out.push(byte);
                    at += 3;
                }
                None => {
                    out.push(b'=');
                    at += 1;
                }
            },
            None if bytes.get(at + 1) == Some(&b'\n') => at += 2,
            None => {
                out.push(b'=');
                at += 1;
            }
        }
    }
    out
}

/// The body of `part` with its transfer encoding undone.
pub fn decoded_body(raw: &[u8], part: &Part) -> Vec<u8> {
    let body = &raw[part.body.clone()];
    match part.encoding.as_str() {
        "base64" => base64_body(body),
        "quoted-printable" => quoted_printable_body(body),
        _ => body.to_vec(),
    }
}

/// The text of a `text/*` part: decoded, then read in its charset (UTF-8, US-ASCII and
/// ISO-8859-1 and its Windows variant; other charsets as UTF-8 with replacement characters),
/// with whether its charset was unknown.
pub fn text_of(raw: &[u8], part: &Part) -> (String, bool) {
    let bytes = decoded_body(raw, part);
    let charset = part.parameter("charset").unwrap_or("us-ascii");
    match text_in(charset, &bytes) {
        Some(text) => (text, false),
        None => (String::from_utf8_lossy(&bytes).into_owned(), true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIXED: &[u8] = b"From: a@example.org\r\nContent-Type: multipart/mixed; boundary=\"outer\"\r\n\r\npreamble\r\n--outer\r\nContent-Type: multipart/alternative; boundary=inner\r\n\r\n--inner\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nCaf=C3=A9 =\r\nopen\r\n--inner\r\nContent-Type: text/html\r\n\r\n<p>Hi</p>\r\n--inner--\r\n--outer\r\nContent-Type: application/pdf; name=\"menu.pdf\"\r\nContent-Disposition: attachment; filename=\"menu.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\nJVBERi0x\r\n--outer\r\nContent-Type: message/rfc822\r\n\r\nSubject: Inner\r\n\r\nInner body\r\n--outer--\r\nepilogue\r\n";

    #[test]
    fn multiparts_split_into_their_tree() {
        let root = parse(MIXED);
        assert!(root.is_multipart());
        assert_eq!(root.children.len(), 3);
        let text = part_at(&root, &[1, 1]).unwrap();
        assert_eq!(text_of(MIXED, text), ("Café open".to_owned(), false));
        let html = part_at(&root, &[1, 2]).unwrap();
        assert_eq!(&MIXED[html.body.clone()], b"<p>Hi</p>");
        let pdf = part_at(&root, &[2]).unwrap();
        assert!(pdf.is_attachment());
        assert_eq!(pdf.file_name(), Some("menu.pdf"));
        assert_eq!(decoded_body(MIXED, pdf), b"%PDF-1");
        let inner = part_at(&root, &[3]).unwrap();
        assert!(inner.is_message());
        assert_eq!(&MIXED[part_at(&root, &[3, 1]).unwrap().body.clone()], b"Inner body");
        let numbers: Vec<String> = leaves(&root).into_iter().map(|(number, _)| number).collect();
        assert_eq!(numbers, ["1.1", "1.2", "2", "3"]);
        assert_eq!(part_at(&root, &[4]), None);
    }

    #[test]
    fn a_plain_message_is_its_own_part_one() {
        let raw = b"Subject: x\r\n\r\nhello\r\n";
        let root = parse(raw);
        assert_eq!(part_at(&root, &[1]), Some(&root));
        assert_eq!(leaves(&root)[0].0, "1");
        assert_eq!(&raw[root.body.clone()], b"hello\r\n");
    }
}
