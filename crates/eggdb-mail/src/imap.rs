//! An IMAP4rev1 server (RFC 3501) over the email store (D893), with `UIDPLUS` (RFC 4315),
//! `MOVE` (RFC 6851), `LITERAL+` (RFC 7888) and `IDLE` (RFC 2177): sign-in with `LOGIN` or `AUTHENTICATE PLAIN`,
//! the mailbox commands, `APPEND`, `SELECT`/`EXAMINE`, `FETCH` with envelopes, body
//! structures, sections and partial fetches, `STORE`, `SEARCH`, `EXPUNGE`, `COPY`, `MOVE`
//! and their `UID` forms.

use crate::message::{Address, addresses, body_start, headers, imap_date, parse_rfc5322_date};
use crate::mime::{self, Part};
use crate::store::{self, INBOX, MailError, StoredMessage};
use crate::{ServerConfig, now_seconds};
use eggdb_keyspace::Keyspace;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;

const MAX_LINE_BYTES: usize = 64 * 1024;
const MAX_ITEMS: usize = 10_000;
const MAX_NESTING: usize = 16;
const SYSTEM_FLAGS: [&str; 5] = ["\\Seen", "\\Answered", "\\Flagged", "\\Deleted", "\\Draft"];
const CAPABILITIES: &str = "IMAP4rev1 AUTH=PLAIN UIDPLUS MOVE LITERAL+ ID IDLE";
const MOST_DONE_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq)]
enum Item {
    Atom(String),
    Text(Vec<u8>),
    List(Vec<Item>),
}

impl Item {
    fn text(&self) -> Option<String> {
        match self {
            Self::Atom(atom) => Some(atom.clone()),
            Self::Text(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
            Self::List(_) => None,
        }
    }

    fn is(&self, word: &str) -> bool {
        matches!(self, Self::Atom(atom) if atom.eq_ignore_ascii_case(word))
    }
}

fn bad(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

fn timed_out(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted)
}

struct Connection<'k> {
    keyspace: &'k dyn Keyspace,
    config: &'k ServerConfig,
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    stop: &'k (dyn Fn() -> bool + Sync),
    signed_in: bool,
    selected: Option<(String, bool)>,
}

fn quoted(text: &str) -> Vec<u8> {
    if text.bytes().all(|byte| (0x20..0x7f).contains(&byte)) && text.len() < 1024 {
        let mut out = b"\"".to_vec();
        for byte in text.bytes() {
            if byte == b'"' || byte == b'\\' {
                out.push(b'\\');
            }
            out.push(byte);
        }
        out.push(b'"');
        out
    } else {
        literal(text.as_bytes())
    }
}

fn literal(bytes: &[u8]) -> Vec<u8> {
    let mut out = format!("{{{}}}\r\n", bytes.len()).into_bytes();
    out.extend_from_slice(bytes);
    out
}

fn nil_or(text: Option<&str>) -> Vec<u8> {
    text.map_or_else(|| b"NIL".to_vec(), quoted)
}

fn address_list(list: &[Address]) -> Vec<u8> {
    if list.is_empty() {
        return b"NIL".to_vec();
    }
    let mut out = b"(".to_vec();
    for address in list {
        let (mailbox, host) = address.email.rsplit_once('@').unwrap_or((&address.email, ""));
        out.push(b'(');
        out.extend(nil_or(address.name.as_deref()));
        out.extend_from_slice(b" NIL ");
        out.extend(quoted(mailbox));
        out.push(b' ');
        out.extend(if host.is_empty() { b"NIL".to_vec() } else { quoted(host) });
        out.push(b')');
    }
    out.push(b')');
    out
}

fn envelope(raw: &[u8]) -> Vec<u8> {
    let fields = headers(raw);
    let value = |name: &str| {
        fields
            .iter()
            .find(|(found, _)| found.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    };
    let list = |name: &str| value(name).map(|text| addresses(&text)).unwrap_or_default();
    let from = list("From");
    let sender = if value("Sender").is_some() { list("Sender") } else { from.clone() };
    let reply_to = if value("Reply-To").is_some() { list("Reply-To") } else { from.clone() };
    let mut out = b"(".to_vec();
    out.extend(nil_or(value("Date").as_deref()));
    out.push(b' ');
    out.extend(nil_or(value("Subject").as_deref()));
    for part in [&from, &sender, &reply_to, &list("To"), &list("Cc"), &list("Bcc")] {
        out.push(b' ');
        out.extend(address_list(part));
    }
    out.push(b' ');
    out.extend(nil_or(value("In-Reply-To").as_deref()));
    out.push(b' ');
    out.extend(nil_or(value("Message-ID").as_deref()));
    out.push(b')');
    out
}

fn text(value: &str) -> String {
    String::from_utf8_lossy(&quoted(value)).into_owned()
}

fn nil_text(value: Option<&str>) -> String {
    value.map_or_else(|| "NIL".to_owned(), text)
}

fn parameter_list(parameters: &[(String, String)]) -> String {
    if parameters.is_empty() {
        return "NIL".to_owned();
    }
    let items: Vec<String> = parameters
        .iter()
        .map(|(name, value)| format!("{} {}", text(&name.to_ascii_uppercase()), text(value)))
        .collect();
    format!("({})", items.join(" "))
}

fn disposition(part: &Part) -> String {
    part.disposition.as_ref().map_or_else(
        || "NIL".to_owned(),
        |(kind, parameters)| format!("({} {})", text(&kind.to_ascii_uppercase()), parameter_list(parameters)),
    )
}

fn part_structure(raw: &[u8], part: &Part, extended: bool) -> String {
    if part.is_multipart() && !part.children.is_empty() {
        let mut out = String::from("(");
        for child in &part.children {
            out.push_str(&part_structure(raw, child, extended));
        }
        out.push(' ');
        out.push_str(&text(&part.minor.to_ascii_uppercase()));
        if extended {
            out.push_str(&format!(" {} {} NIL NIL", parameter_list(&part.parameters), disposition(part)));
        }
        out.push(')');
        return out;
    }
    let mut parameters = part.parameters.clone();
    if part.major == "text" && part.parameter("charset").is_none() {
        parameters.insert(0, ("charset".to_owned(), "us-ascii".to_owned()));
    }
    let body = &raw[part.body.clone()];
    let lines = body.iter().filter(|byte| **byte == b'\n').count();
    let mut out = format!(
        "({} {} {} {} {} {} {}",
        text(&part.major.to_ascii_uppercase()),
        text(&part.minor.to_ascii_uppercase()),
        parameter_list(&parameters),
        nil_text(part.id.as_deref()),
        nil_text(part.description.as_deref()),
        text(&part.encoding.to_ascii_uppercase()),
        body.len()
    );
    if part.is_message()
        && let Some(inner) = part.children.first()
    {
        let inner_raw = &raw[inner.header.start..inner.body.end];
        out.push_str(&format!(
            " {} {} {lines}",
            String::from_utf8_lossy(&envelope(inner_raw)),
            part_structure(raw, inner, extended)
        ));
    } else if part.major == "text" {
        out.push_str(&format!(" {lines}"));
    }
    if extended {
        out.push_str(&format!(" NIL {} NIL NIL", disposition(part)));
    }
    out.push(')');
    out
}

fn body_structure(raw: &[u8], extended: bool) -> Vec<u8> {
    part_structure(raw, &mime::parse(raw), extended).into_bytes()
}

fn header_fields(raw: &[u8], names: &[String], negated: bool) -> Vec<u8> {
    let section = &raw[..body_start(raw)];
    let text = String::from_utf8_lossy(section);
    let mut out = Vec::new();
    let mut keep = false;
    for line in text.split_inclusive('\n') {
        if line == "\r\n" || line == "\n" {
            break;
        }
        if !line.starts_with([' ', '\t']) {
            let name = line.split(':').next().unwrap_or_default().trim();
            keep = names.iter().any(|wanted| wanted.eq_ignore_ascii_case(name)) != negated;
        }
        if keep {
            out.extend_from_slice(line.as_bytes());
        }
    }
    out.extend_from_slice(b"\r\n");
    out
}

/// A sequence set (`1:5,7,9:*`) against `last`, the highest number there is.
fn in_set(set: &str, number: u32, last: u32) -> bool {
    set.split(',').any(|range| {
        let bound = |text: &str| if text == "*" { Some(last) } else { text.parse::<u32>().ok() };
        match range.split_once(':') {
            Some((from, to)) => match (bound(from), bound(to)) {
                (Some(a), Some(b)) => (a.min(b)..=a.max(b)).contains(&number),
                _ => false,
            },
            None => bound(range) == Some(number),
        }
    })
}

fn has_flag(message: &StoredMessage, flag: &str) -> bool {
    message.flags.iter().any(|found| found.eq_ignore_ascii_case(flag))
}

fn imap_day(text: &str) -> Option<u64> {
    let parts: Vec<&str> = text.split('-').collect();
    let [day, month, year] = parts.as_slice() else {
        return None;
    };
    parse_rfc5322_date(&format!("{day} {month} {year} 00:00:00 +0000"))
}

fn contains_folded(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

struct Searched<'m> {
    message: &'m StoredMessage,
    sequence: u32,
    last_sequence: u32,
    last_uid: u32,
}

fn search_key(items: &[Item], at: &mut usize, target: &Searched<'_>, depth: usize) -> io::Result<bool> {
    if depth > MAX_NESTING {
        return Err(bad("the search nests too deeply"));
    }
    let item = items.get(*at).ok_or_else(|| bad("a search key is missing"))?.clone();
    *at += 1;
    let mut argument = || -> io::Result<String> {
        let value = items.get(*at).and_then(Item::text).ok_or_else(|| bad("a search key needs an argument"))?;
        *at += 1;
        Ok(value)
    };
    let message = target.message;
    let raw_text = || String::from_utf8_lossy(&message.raw).into_owned();
    let field = |name: &str| crate::message::header(&message.raw, name).unwrap_or_default();
    Ok(match &item {
        Item::List(inner) => {
            let mut inner_at = 0;
            let mut all = true;
            while inner_at < inner.len() {
                all &= search_key(inner, &mut inner_at, target, depth + 1)?;
            }
            all
        }
        Item::Text(_) => return Err(bad("a search key is an atom")),
        Item::Atom(word) => match word.to_ascii_uppercase().as_str() {
            "ALL" => true,
            "SEEN" => has_flag(message, "\\Seen"),
            "UNSEEN" | "NEW" => !has_flag(message, "\\Seen"),
            "FLAGGED" => has_flag(message, "\\Flagged"),
            "UNFLAGGED" => !has_flag(message, "\\Flagged"),
            "DELETED" => has_flag(message, "\\Deleted"),
            "UNDELETED" => !has_flag(message, "\\Deleted"),
            "ANSWERED" => has_flag(message, "\\Answered"),
            "UNANSWERED" => !has_flag(message, "\\Answered"),
            "DRAFT" => has_flag(message, "\\Draft"),
            "UNDRAFT" => !has_flag(message, "\\Draft"),
            "RECENT" => false,
            "OLD" => true,
            "KEYWORD" => has_flag(message, &argument()?),
            "UNKEYWORD" => !has_flag(message, &argument()?),
            "FROM" | "TO" | "CC" | "BCC" | "SUBJECT" => contains_folded(&field(word), &argument()?),
            "HEADER" => {
                let name = argument()?;
                let wanted = argument()?;
                contains_folded(&field(&name), &wanted)
            }
            "BODY" => contains_folded(&String::from_utf8_lossy(&message.raw[body_start(&message.raw)..]), &argument()?),
            "TEXT" => contains_folded(&raw_text(), &argument()?),
            "LARGER" => message.raw.len() as u64 > argument()?.parse::<u64>().map_err(|_| bad("LARGER needs a number"))?,
            "SMALLER" => (message.raw.len() as u64) < argument()?.parse::<u64>().map_err(|_| bad("SMALLER needs a number"))?,
            "BEFORE" | "ON" | "SINCE" | "SENTBEFORE" | "SENTON" | "SENTSINCE" => {
                let day = imap_day(&argument()?).ok_or_else(|| bad("a date is day-Mon-year"))?;
                let when = if word.to_ascii_uppercase().starts_with("SENT") {
                    parse_rfc5322_date(&field("Date")).unwrap_or(message.internal_date)
                } else {
                    message.internal_date
                };
                let start = when / 86_400 * 86_400;
                match word.to_ascii_uppercase().trim_start_matches("SENT") {
                    "BEFORE" => start < day,
                    "ON" => start == day,
                    _ => start >= day,
                }
            }
            "UID" => in_set(&argument()?, message.uid, target.last_uid),
            "NOT" => !search_key(items, at, target, depth + 1)?,
            "OR" => {
                let left = search_key(items, at, target, depth + 1)?;
                let right = search_key(items, at, target, depth + 1)?;
                left || right
            }
            set if set.starts_with(|first: char| first.is_ascii_digit() || first == '*') => {
                in_set(set, target.sequence, target.last_sequence)
            }
            other => return Err(bad(&format!("unknown search key {other}"))),
        },
    })
}

fn parse_items(bytes: &[u8], at: &mut usize, depth: usize) -> io::Result<Vec<Item>> {
    if depth > MAX_NESTING {
        return Err(bad("the command nests too deeply"));
    }
    let mut items = Vec::new();
    while *at < bytes.len() {
        match bytes[*at] {
            b' ' => *at += 1,
            b')' => {
                *at += 1;
                return Ok(items);
            }
            b'(' => {
                *at += 1;
                items.push(Item::List(parse_items(bytes, at, depth + 1)?));
            }
            b'"' => {
                *at += 1;
                let mut text = Vec::new();
                loop {
                    let Some(&byte) = bytes.get(*at) else {
                        return Err(bad("an unterminated quoted string"));
                    };
                    *at += 1;
                    match byte {
                        b'"' => break,
                        b'\\' => {
                            text.push(*bytes.get(*at).ok_or_else(|| bad("an escape at the end"))?);
                            *at += 1;
                        }
                        other => text.push(other),
                    }
                }
                items.push(Item::Text(text));
            }
            0 => {
                *at += 1;
                let length = bytes.get(*at..*at + 8).ok_or_else(|| bad("a broken literal"))?;
                let length = u64::from_be_bytes(length.try_into().map_err(|_| bad("a broken literal"))?) as usize;
                *at += 8;
                let text = bytes.get(*at..*at + length).ok_or_else(|| bad("a broken literal"))?.to_vec();
                *at += length;
                items.push(Item::Text(text));
            }
            _ => {
                let start = *at;
                let mut brackets = 0usize;
                while let Some(&byte) = bytes.get(*at) {
                    match byte {
                        b'[' => brackets += 1,
                        b']' => brackets = brackets.saturating_sub(1),
                        b' ' | b'(' | b')' if brackets == 0 => break,
                        _ => {}
                    }
                    *at += 1;
                }
                items.push(Item::Atom(String::from_utf8_lossy(&bytes[start..*at]).into_owned()));
            }
        }
        if items.len() > MAX_ITEMS {
            return Err(bad("the command has too many parts"));
        }
    }
    Ok(items)
}

struct Fetched {
    names: Vec<(String, bool)>,
}

fn fetch_names(item: &Item) -> Fetched {
    let mut names = Vec::new();
    let mut add = |atom: &str| names.push((atom.to_owned(), false));
    match item {
        Item::Atom(word) => match word.to_ascii_uppercase().as_str() {
            "ALL" => ["FLAGS", "INTERNALDATE", "RFC822.SIZE", "ENVELOPE"].iter().for_each(|name| add(name)),
            "FAST" => ["FLAGS", "INTERNALDATE", "RFC822.SIZE"].iter().for_each(|name| add(name)),
            "FULL" => ["FLAGS", "INTERNALDATE", "RFC822.SIZE", "ENVELOPE", "BODY"].iter().for_each(|name| add(name)),
            _ => add(word),
        },
        Item::List(items) => items.iter().filter_map(Item::text).for_each(|name| add(&name)),
        Item::Text(text) => add(&String::from_utf8_lossy(text)),
    }
    Fetched { names }
}

fn section_data(raw: &[u8], section: &str) -> io::Result<Vec<u8>> {
    let root = mime::parse(raw);
    let mut numbers = Vec::new();
    let mut rest = section;
    loop {
        let length = rest.bytes().take_while(u8::is_ascii_digit).count();
        if length == 0 {
            break;
        }
        numbers.push(rest[..length].parse::<usize>().map_err(|_| bad("a part number is out of range"))?);
        rest = &rest[length..];
        match rest.strip_prefix('.') {
            Some(after) => rest = after,
            None => break,
        }
    }
    let selected = mime::part_at(&root, &numbers).ok_or_else(|| bad("no such body part"))?;
    let message = match (numbers.is_empty(), selected.is_message()) {
        (true, _) => &root,
        (false, true) => selected.children.first().unwrap_or(selected),
        (false, false) => selected,
    };
    let upper = rest.to_ascii_uppercase();
    Ok(match upper.as_str() {
        "" if numbers.is_empty() => raw.to_vec(),
        "" => raw[selected.body.clone()].to_vec(),
        "MIME" if !numbers.is_empty() => raw[selected.header.clone()].to_vec(),
        "HEADER" => raw[message.header.clone()].to_vec(),
        "TEXT" => raw[message.body.clone()].to_vec(),
        fields if fields.starts_with("HEADER.FIELDS") => {
            let negated = fields.starts_with("HEADER.FIELDS.NOT");
            let open = rest.find('(').ok_or_else(|| bad("HEADER.FIELDS needs a list"))?;
            let close = rest.rfind(')').ok_or_else(|| bad("HEADER.FIELDS needs a list"))?;
            let names: Vec<String> = rest[open + 1..close].split_whitespace().map(|name| name.trim_matches('"').to_owned()).collect();
            header_fields(&raw[message.header.start..message.body.end], &names, negated)
        }
        _ => return Err(bad("that body section is not served")),
    })
}

impl Connection<'_> {
    fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.write_all(b"\r\n")?;
        self.writer.flush()
    }

    fn read_raw_line(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut line = Vec::new();
        loop {
            if (self.stop)() {
                return Ok(None);
            }
            let room = (MAX_LINE_BYTES + 2).saturating_sub(line.len()) as u64;
            match self.reader.by_ref().take(room).read_until(b'\n', &mut line) {
                Ok(0) if line.is_empty() => return Ok(None),
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the connection ended mid-line")),
                Ok(_) if line.ends_with(b"\n") => return Ok(Some(line)),
                Ok(_) if line.len() > MAX_LINE_BYTES => return Err(bad("line too long")),
                Ok(_) => {}
                Err(error) if timed_out(&error) => {}
                Err(error) => return Err(error),
            }
        }
    }

    fn read_exact_bytes(&mut self, length: usize) -> io::Result<Vec<u8>> {
        let mut bytes = vec![0u8; length];
        let mut filled = 0;
        while filled < length {
            if (self.stop)() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "stopping"));
            }
            match self.reader.read(&mut bytes[filled..]) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the connection ended inside a literal")),
                Ok(read) => filled += read,
                Err(error) if timed_out(&error) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(bytes)
    }

    /// One command with its literals read in: literal bytes are marked with a NUL and their
    /// length, so the item reader takes them whole.
    fn read_command(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut command = Vec::new();
        loop {
            let Some(line) = self.read_raw_line()? else {
                return Ok(None);
            };
            let line = line.strip_suffix(b"\r\n").or_else(|| line.strip_suffix(b"\n")).unwrap_or(&line).to_vec();
            let literal = line.ends_with(b"}").then(|| line.iter().rposition(|byte| *byte == b'{')).flatten();
            let Some(open) = literal else {
                command.extend_from_slice(&line);
                return Ok(Some(command));
            };
            let spec = String::from_utf8_lossy(&line[open + 1..line.len() - 1]).into_owned();
            let (digits, non_synchronizing) = match spec.strip_suffix('+') {
                Some(digits) => (digits.to_owned(), true),
                None => (spec, false),
            };
            let Ok(length) = digits.parse::<usize>() else {
                command.extend_from_slice(&line);
                return Ok(Some(command));
            };
            if length > self.config.max_message_bytes {
                return Err(bad("the literal is too large"));
            }
            command.extend_from_slice(&line[..open]);
            if !non_synchronizing {
                self.send(b"+ Ready for literal")?;
            }
            let bytes = self.read_exact_bytes(length)?;
            command.push(0);
            command.extend_from_slice(&(length as u64).to_be_bytes());
            command.extend_from_slice(&bytes);
            if command.len() > self.config.max_message_bytes + MAX_LINE_BYTES {
                return Err(bad("the command is too large"));
            }
        }
    }

    fn write_store<T>(&self, body: &mut dyn FnMut(&mut dyn eggdb_keyspace::KeysWrite) -> Result<T, MailError>) -> Result<T, String> {
        let mut outcome = None;
        self.keyspace.write(0, &mut |keys| {
            let result = body(keys).map_err(|error| error.0);
            let failed = result.as_ref().err().cloned();
            outcome = Some(result);
            failed.map_or(Ok(()), Err)
        })?;
        outcome.unwrap_or_else(|| Err("the change did not run".to_owned()))
    }

    fn read_store<T>(&self, body: &mut dyn FnMut(&dyn eggdb_keyspace::KeysRead) -> Result<T, MailError>) -> Result<T, String> {
        let mut outcome = None;
        self.keyspace.read(0, &mut |keys| {
            outcome = Some(body(keys).map_err(|error| error.0));
            Ok(())
        })?;
        outcome.unwrap_or_else(|| Err("the read did not run".to_owned()))
    }

    fn selected_messages(&self) -> Result<Vec<StoredMessage>, String> {
        let Some((name, _)) = &self.selected else {
            return Ok(Vec::new());
        };
        let name = name.clone();
        self.read_store(&mut |keys| store::messages(keys, &name))
    }

    fn select(&mut self, tag: &str, name: &str, read_only: bool) -> io::Result<()> {
        let name = store::canonical(name);
        let found = if name == INBOX {
            self.write_store(&mut |keys| store::ensure_mailbox(keys, INBOX)).map(Some)
        } else {
            let wanted = name.clone();
            self.read_store(&mut |keys| store::mailbox(keys, &wanted))
        };
        let Ok(Some(mailbox)) = found else {
            self.selected = None;
            return self.send(format!("{tag} NO Mailbox does not exist").as_bytes());
        };
        self.selected = Some((mailbox.name.clone(), read_only));
        let messages = self.selected_messages().unwrap_or_default();
        self.send(b"* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)")?;
        self.send(format!("* {} EXISTS", messages.len()).as_bytes())?;
        self.send(b"* 0 RECENT")?;
        if let Some(first) = messages.iter().position(|message| !has_flag(message, "\\Seen")) {
            self.send(format!("* OK [UNSEEN {}] First unseen", first + 1).as_bytes())?;
        }
        self.send(format!("* OK [UIDVALIDITY {}] UIDs valid", mailbox.uid_validity).as_bytes())?;
        self.send(format!("* OK [UIDNEXT {}] Predicted next UID", mailbox.uid_next).as_bytes())?;
        self.send(b"* OK [PERMANENTFLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft \\*)] Flags permitted")?;
        let mode = if read_only { "READ-ONLY" } else { "READ-WRITE" };
        let verb = if read_only { "EXAMINE" } else { "SELECT" };
        self.send(format!("{tag} OK [{mode}] {verb} completed").as_bytes())
    }

    fn list(&mut self, tag: &str, arguments: &[Item], subscribed_only: bool) -> io::Result<()> {
        let reference = arguments.first().and_then(Item::text).unwrap_or_default();
        let pattern = arguments.get(1).and_then(Item::text).unwrap_or_default();
        let verb = if subscribed_only { "LSUB" } else { "LIST" };
        if pattern.is_empty() {
            self.send(format!("* {verb} (\\Noselect) \"/\" \"\"").as_bytes())?;
            return self.send(format!("{tag} OK {verb} completed").as_bytes());
        }
        let wanted = format!("{reference}{pattern}");
        let mailboxes = self.write_store(&mut |keys| {
            store::ensure_mailbox(keys, INBOX)?;
            store::mailboxes(&*keys)
        });
        let Ok(mailboxes) = mailboxes else {
            return self.send(format!("{tag} NO {verb} failed").as_bytes());
        };
        for mailbox in &mailboxes {
            if subscribed_only && !mailbox.subscribed {
                continue;
            }
            if wildcard_matches(&wanted, &mailbox.name) {
                let children = mailboxes.iter().any(|other| other.name.starts_with(&format!("{}/", mailbox.name)));
                let attribute = if children { "\\HasChildren" } else { "\\HasNoChildren" };
                let mut line = format!("* {verb} ({attribute}) \"/\" ").into_bytes();
                line.extend(quoted(&mailbox.name));
                self.send(&line)?;
            }
        }
        self.send(format!("{tag} OK {verb} completed").as_bytes())
    }

    fn status(&mut self, tag: &str, arguments: &[Item]) -> io::Result<()> {
        let Some(name) = arguments.first().and_then(Item::text) else {
            return self.send(format!("{tag} BAD STATUS needs a mailbox").as_bytes());
        };
        let wanted = store::canonical(&name);
        let found = self.read_store(&mut |keys| Ok((store::mailbox(keys, &wanted)?, store::messages(keys, &wanted)?)));
        let Ok((Some(mailbox), messages)) = found else {
            return self.send(format!("{tag} NO Mailbox does not exist").as_bytes());
        };
        let mut parts = Vec::new();
        if let Some(Item::List(items)) = arguments.get(1) {
            for item in items.iter().filter_map(Item::text) {
                let value = match item.to_ascii_uppercase().as_str() {
                    "MESSAGES" => messages.len() as u64,
                    "RECENT" => 0,
                    "UIDNEXT" => u64::from(mailbox.uid_next),
                    "UIDVALIDITY" => u64::from(mailbox.uid_validity),
                    "UNSEEN" => messages.iter().filter(|message| !has_flag(message, "\\Seen")).count() as u64,
                    _ => continue,
                };
                parts.push(format!("{} {value}", item.to_ascii_uppercase()));
            }
        }
        let mut line = b"* STATUS ".to_vec();
        line.extend(quoted(&mailbox.name));
        line.extend_from_slice(format!(" ({})", parts.join(" ")).as_bytes());
        self.send(&line)?;
        self.send(format!("{tag} OK STATUS completed").as_bytes())
    }

    fn fetch(&mut self, tag: &str, arguments: &[Item], by_uid: bool) -> io::Result<()> {
        let (Some(set), Some(what)) = (arguments.first().and_then(Item::text), arguments.get(1)) else {
            return self.send(format!("{tag} BAD FETCH needs a set and items").as_bytes());
        };
        let Ok(messages) = self.selected_messages() else {
            return self.send(format!("{tag} NO FETCH failed").as_bytes());
        };
        let mut fetched = fetch_names(what);
        if by_uid && !fetched.names.iter().any(|(name, _)| name.eq_ignore_ascii_case("UID")) {
            fetched.names.insert(0, ("UID".to_owned(), false));
        }
        let read_only = self.selected.as_ref().is_some_and(|(_, read_only)| *read_only);
        let mailbox = self.selected.as_ref().map(|(name, _)| name.clone()).unwrap_or_default();
        let last_sequence = u32::try_from(messages.len()).unwrap_or(u32::MAX);
        let last_uid = messages.last().map_or(0, |message| message.uid);
        for (index, message) in messages.iter().enumerate() {
            let sequence = index as u32 + 1;
            let wanted = if by_uid { in_set(&set, message.uid, last_uid) } else { in_set(&set, sequence, last_sequence) };
            if !wanted {
                continue;
            }
            let mut parts: Vec<Vec<u8>> = Vec::new();
            let mut mark_seen = false;
            for (name, _) in &fetched.names {
                let upper = name.to_ascii_uppercase();
                let part = match upper.as_str() {
                    "UID" => format!("UID {}", message.uid).into_bytes(),
                    "FLAGS" => format!("FLAGS ({})", message.flags.join(" ")).into_bytes(),
                    "INTERNALDATE" => format!("INTERNALDATE \"{}\"", imap_date(message.internal_date)).into_bytes(),
                    "RFC822.SIZE" => format!("RFC822.SIZE {}", message.raw.len()).into_bytes(),
                    "ENVELOPE" => [b"ENVELOPE ".to_vec(), envelope(&message.raw)].concat(),
                    "BODY" | "BODYSTRUCTURE" => [format!("{upper} ").into_bytes(), body_structure(&message.raw, upper == "BODYSTRUCTURE")].concat(),
                    "RFC822" => {
                        mark_seen = true;
                        [b"RFC822 ".to_vec(), literal(&message.raw)].concat()
                    }
                    "RFC822.HEADER" => [b"RFC822.HEADER ".to_vec(), literal(&section_data(&message.raw, "HEADER")?)].concat(),
                    "RFC822.TEXT" => {
                        mark_seen = true;
                        [b"RFC822.TEXT ".to_vec(), literal(&section_data(&message.raw, "TEXT")?)].concat()
                    }
                    body if body.starts_with("BODY[") || body.starts_with("BODY.PEEK[") => {
                        let peek = body.starts_with("BODY.PEEK[");
                        let open = name.find('[').unwrap_or(0);
                        let close = name.rfind(']').ok_or_else(|| bad("a body section needs ]"))?;
                        let section = &name[open + 1..close];
                        let mut data = section_data(&message.raw, section)?;
                        let mut label = format!("BODY[{section}]");
                        if let Some(partial) = name[close + 1..].strip_prefix('<').and_then(|rest| rest.strip_suffix('>')) {
                            let (start, count) = partial.split_once('.').unwrap_or((partial, ""));
                            let start: usize = start.parse().map_err(|_| bad("a partial needs numbers"))?;
                            let count: usize = if count.is_empty() { usize::MAX } else { count.parse().map_err(|_| bad("a partial needs numbers"))? };
                            let from = start.min(data.len());
                            let to = from.saturating_add(count).min(data.len());
                            data = data[from..to].to_vec();
                            label.push_str(&format!("<{start}>"));
                        }
                        if !peek {
                            mark_seen = true;
                        }
                        [format!("{label} ").into_bytes(), literal(&data)].concat()
                    }
                    _ => return self.send(format!("{tag} BAD Unknown fetch item {name}").as_bytes()),
                };
                parts.push(part);
            }
            if mark_seen && !read_only && !has_flag(message, "\\Seen") {
                let mut flags = message.flags.clone();
                flags.push("\\Seen".to_owned());
                let uid = message.uid;
                let _ = self.write_store(&mut |keys| store::set_flags(keys, &mailbox, uid, &flags));
                let updated = format!("FLAGS ({})", flags.join(" ")).into_bytes();
                match parts.iter().position(|part| part.starts_with(b"FLAGS (")) {
                    Some(at) => parts[at] = updated,
                    None => parts.push(updated),
                }
            }
            let mut line = format!("* {sequence} FETCH (").into_bytes();
            line.extend(parts.join(&b' '));
            line.push(b')');
            self.send(&line)?;
        }
        self.send(format!("{tag} OK FETCH completed").as_bytes())
    }

    fn store_flags(&mut self, tag: &str, arguments: &[Item], by_uid: bool) -> io::Result<()> {
        if self.selected.as_ref().is_some_and(|(_, read_only)| *read_only) {
            return self.send(format!("{tag} NO The mailbox is read-only").as_bytes());
        }
        let (Some(set), Some(action)) = (arguments.first().and_then(Item::text), arguments.get(1).and_then(Item::text)) else {
            return self.send(format!("{tag} BAD STORE needs a set, an action and flags").as_bytes());
        };
        let given: Vec<String> = match arguments.get(2) {
            Some(Item::List(items)) => items.iter().filter_map(Item::text).collect(),
            Some(item) => item.text().into_iter().collect(),
            None => Vec::new(),
        };
        let action = action.to_ascii_uppercase();
        let silent = action.ends_with(".SILENT");
        let mode = action.trim_end_matches(".SILENT").to_owned();
        if !matches!(mode.as_str(), "FLAGS" | "+FLAGS" | "-FLAGS") {
            return self.send(format!("{tag} BAD Unknown STORE action").as_bytes());
        }
        let Ok(messages) = self.selected_messages() else {
            return self.send(format!("{tag} NO STORE failed").as_bytes());
        };
        let mailbox = self.selected.as_ref().map(|(name, _)| name.clone()).unwrap_or_default();
        let last_sequence = u32::try_from(messages.len()).unwrap_or(u32::MAX);
        let last_uid = messages.last().map_or(0, |message| message.uid);
        for (index, message) in messages.iter().enumerate() {
            let sequence = index as u32 + 1;
            let wanted = if by_uid { in_set(&set, message.uid, last_uid) } else { in_set(&set, sequence, last_sequence) };
            if !wanted {
                continue;
            }
            let mut flags: Vec<String> = match mode.as_str() {
                "FLAGS" => Vec::new(),
                _ => message.flags.clone(),
            };
            for flag in &given {
                let flag = SYSTEM_FLAGS.iter().find(|known| known.eq_ignore_ascii_case(flag)).map_or_else(|| flag.clone(), |known| (*known).to_owned());
                if mode == "-FLAGS" {
                    flags.retain(|found| !found.eq_ignore_ascii_case(&flag));
                } else if !flags.iter().any(|found| found.eq_ignore_ascii_case(&flag)) {
                    flags.push(flag);
                }
            }
            let uid = message.uid;
            if self.write_store(&mut |keys| store::set_flags(keys, &mailbox, uid, &flags)).is_err() {
                return self.send(format!("{tag} NO STORE failed").as_bytes());
            }
            if !silent {
                let uid_part = if by_uid { format!("UID {uid} ") } else { String::new() };
                self.send(format!("* {sequence} FETCH ({uid_part}FLAGS ({}))", flags.join(" ")).as_bytes())?;
            }
        }
        self.send(format!("{tag} OK STORE completed").as_bytes())
    }

    fn search(&mut self, tag: &str, arguments: &[Item], by_uid: bool) -> io::Result<()> {
        let Ok(messages) = self.selected_messages() else {
            return self.send(format!("{tag} NO SEARCH failed").as_bytes());
        };
        let mut keys = arguments;
        if keys.first().is_some_and(|item| item.is("CHARSET")) {
            keys = keys.get(2..).unwrap_or_default();
        }
        let last_sequence = u32::try_from(messages.len()).unwrap_or(u32::MAX);
        let last_uid = messages.last().map_or(0, |message| message.uid);
        let mut found = Vec::new();
        for (index, message) in messages.iter().enumerate() {
            let target = Searched {
                message,
                sequence: index as u32 + 1,
                last_sequence,
                last_uid,
            };
            let mut at = 0;
            let mut all = true;
            while at < keys.len() {
                match search_key(keys, &mut at, &target, 0) {
                    Ok(matched) => all &= matched,
                    Err(error) => return self.send(format!("{tag} BAD {error}").as_bytes()),
                }
            }
            if all {
                found.push(if by_uid { message.uid } else { target.sequence });
            }
        }
        let listed: Vec<String> = found.iter().map(u32::to_string).collect();
        let line = if listed.is_empty() { "* SEARCH".to_owned() } else { format!("* SEARCH {}", listed.join(" ")) };
        self.send(line.as_bytes())?;
        self.send(format!("{tag} OK SEARCH completed").as_bytes())
    }

    fn expunge(&mut self, tag: Option<&str>, only_uids: Option<&str>) -> io::Result<()> {
        let Ok(messages) = self.selected_messages() else {
            return Ok(());
        };
        let mailbox = self.selected.as_ref().map(|(name, _)| name.clone()).unwrap_or_default();
        let last_uid = messages.last().map_or(0, |message| message.uid);
        let mut removed = 0u32;
        for (index, message) in messages.iter().enumerate() {
            let chosen = has_flag(message, "\\Deleted") && only_uids.is_none_or(|set| in_set(set, message.uid, last_uid));
            if !chosen {
                continue;
            }
            let uid = message.uid;
            if self.write_store(&mut |keys| store::remove(keys, &mailbox, uid)).is_ok() {
                let sequence = index as u32 + 1 - removed;
                if tag.is_some() {
                    self.send(format!("* {sequence} EXPUNGE").as_bytes())?;
                }
                removed += 1;
            }
        }
        if let Some(tag) = tag {
            self.send(format!("{tag} OK EXPUNGE completed").as_bytes())?;
        }
        Ok(())
    }

    fn copy(&mut self, tag: &str, arguments: &[Item], by_uid: bool, moving: bool) -> io::Result<()> {
        let verb = if moving { "MOVE" } else { "COPY" };
        if moving && self.selected.as_ref().is_some_and(|(_, read_only)| *read_only) {
            return self.send(format!("{tag} NO The mailbox is read-only").as_bytes());
        }
        let (Some(set), Some(target)) = (arguments.first().and_then(Item::text), arguments.get(1).and_then(Item::text)) else {
            return self.send(format!("{tag} BAD {verb} needs a set and a mailbox").as_bytes());
        };
        let target = store::canonical(&target);
        let exists = self.read_store(&mut |keys| store::mailbox(keys, &target)).ok().flatten();
        let Some(destination) = exists else {
            return self.send(format!("{tag} NO [TRYCREATE] Mailbox does not exist").as_bytes());
        };
        let Ok(messages) = self.selected_messages() else {
            return self.send(format!("{tag} NO {verb} failed").as_bytes());
        };
        let source = self.selected.as_ref().map(|(name, _)| name.clone()).unwrap_or_default();
        let last_sequence = u32::try_from(messages.len()).unwrap_or(u32::MAX);
        let last_uid = messages.last().map_or(0, |message| message.uid);
        let chosen: Vec<(u32, StoredMessage)> = messages
            .iter()
            .enumerate()
            .filter(|(index, message)| {
                if by_uid { in_set(&set, message.uid, last_uid) } else { in_set(&set, *index as u32 + 1, last_sequence) }
            })
            .map(|(index, message)| (index as u32 + 1, message.clone()))
            .collect();
        let copied = self.write_store(&mut |keys| {
            let mut pairs = Vec::new();
            for (_, message) in &chosen {
                let flags: Vec<String> = message.flags.iter().filter(|flag| !flag.eq_ignore_ascii_case("\\Recent")).cloned().collect();
                let uid = store::append(keys, &target, &flags, message.internal_date, &message.raw)?;
                if moving {
                    store::remove(keys, &source, message.uid)?;
                }
                pairs.push((message.uid, uid));
            }
            Ok(pairs)
        });
        let Ok(pairs) = copied else {
            return self.send(format!("{tag} NO {verb} failed").as_bytes());
        };
        let code = if pairs.is_empty() {
            String::new()
        } else {
            let from: Vec<String> = pairs.iter().map(|(from, _)| from.to_string()).collect();
            let to: Vec<String> = pairs.iter().map(|(_, to)| to.to_string()).collect();
            format!("[COPYUID {} {} {}] ", destination.uid_validity, from.join(","), to.join(","))
        };
        if moving {
            if !code.is_empty() {
                self.send(format!("* OK {code}Moved").as_bytes())?;
            }
            for (removed, (sequence, _)) in chosen.iter().enumerate() {
                self.send(format!("* {} EXPUNGE", sequence - removed as u32).as_bytes())?;
            }
            return self.send(format!("{tag} OK {verb} completed").as_bytes());
        }
        self.send(format!("{tag} OK {code}{verb} completed").as_bytes())
    }

    fn append(&mut self, tag: &str, arguments: &[Item]) -> io::Result<()> {
        let Some(name) = arguments.first().and_then(Item::text) else {
            return self.send(format!("{tag} BAD APPEND needs a mailbox").as_bytes());
        };
        let mut flags = Vec::new();
        let mut date = now_seconds();
        let mut message = None;
        for item in &arguments[1..] {
            match item {
                Item::List(items) => flags = items.iter().filter_map(Item::text).collect(),
                Item::Text(bytes) if message.is_none() && arguments.last() == Some(item) => message = Some(bytes.clone()),
                Item::Text(bytes) => {
                    let text = String::from_utf8_lossy(bytes);
                    let (day, time) = text.split_once(' ').unwrap_or((&text, "00:00:00 +0000"));
                    if let (Some(start), Some(clock)) = (imap_day(day.trim()), parse_rfc5322_date(&format!("1 Jan 1970 {time}"))) {
                        date = start + clock;
                    }
                }
                Item::Atom(_) => {}
            }
        }
        let Some(message) = message else {
            return self.send(format!("{tag} BAD APPEND needs a message literal").as_bytes());
        };
        let target = store::canonical(&name);
        if target != INBOX && self.read_store(&mut |keys| store::mailbox(keys, &target)).ok().flatten().is_none() {
            return self.send(format!("{tag} NO [TRYCREATE] Mailbox does not exist").as_bytes());
        }
        let stored = self.write_store(&mut |keys| {
            let uid = store::append(keys, &target, &flags, date, &message)?;
            let validity = store::mailbox(&*keys, &target)?.map_or(0, |mailbox| mailbox.uid_validity);
            Ok((validity, uid))
        });
        match stored {
            Ok((validity, uid)) => self.send(format!("{tag} OK [APPENDUID {validity} {uid}] APPEND completed").as_bytes()),
            Err(_) => self.send(format!("{tag} NO APPEND failed").as_bytes()),
        }
    }

    fn signed_in_as(&self, user: &str, password: &str) -> bool {
        user == self.config.user && eggdb_crypto::sha256(password.as_bytes()) == self.config.password_sha256
    }

    fn authenticate(&mut self, tag: &str, arguments: &[Item]) -> io::Result<()> {
        if !arguments.first().is_some_and(|item| item.is("PLAIN")) {
            return self.send(format!("{tag} NO Only PLAIN is offered").as_bytes());
        }
        let encoded = match arguments.get(1).and_then(Item::text) {
            Some(initial) => initial,
            None => {
                self.send(b"+ ")?;
                let Some(line) = self.read_raw_line()? else {
                    return Ok(());
                };
                String::from_utf8_lossy(&line).trim().to_owned()
            }
        };
        let decoded = eggdb_crypto::base64_decode(encoded.as_bytes()).unwrap_or_default();
        let parts: Vec<&[u8]> = decoded.split(|byte| *byte == 0).collect();
        let accepted = match parts.as_slice() {
            [_, user, password] => self.signed_in_as(&String::from_utf8_lossy(user), &String::from_utf8_lossy(password)),
            _ => false,
        };
        if accepted {
            self.signed_in = true;
            self.send(format!("{tag} OK AUTHENTICATE completed").as_bytes())
        } else {
            self.send(format!("{tag} NO [AUTHENTICATIONFAILED] Authentication failed").as_bytes())
        }
    }

    fn selected_uids(&self) -> Option<Vec<u32>> {
        self.selected.as_ref()?;
        self.selected_messages().ok().map(|messages| messages.iter().map(|message| message.uid).collect())
    }

    fn report_changes(&mut self, known: &mut Vec<u32>) -> io::Result<()> {
        let Some(now) = self.selected_uids() else {
            return Ok(());
        };
        let mut position = known.len();
        while position > 0 {
            position -= 1;
            if !now.contains(&known[position]) {
                known.remove(position);
                self.send(format!("* {} EXPUNGE", position + 1).as_bytes())?;
            }
        }
        if now.iter().any(|uid| !known.contains(uid)) {
            self.send(format!("* {} EXISTS", now.len()).as_bytes())?;
        }
        *known = now;
        Ok(())
    }

    fn idle(&mut self, tag: &str) -> io::Result<()> {
        let mut known = self.selected_uids().unwrap_or_default();
        self.send(b"+ idling")?;
        let mut line = Vec::new();
        loop {
            if (self.stop)() {
                return Ok(());
            }
            let room = MOST_DONE_BYTES.saturating_sub(line.len()) as u64 + 1;
            match self.reader.by_ref().take(room).read_until(b'\n', &mut line) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the connection ended while idling")),
                Ok(_) if line.ends_with(b"\n") => {
                    let word = String::from_utf8_lossy(&line).trim().to_ascii_uppercase();
                    return if word == "DONE" {
                        self.send(format!("{tag} OK IDLE terminated").as_bytes())
                    } else {
                        self.send(format!("{tag} BAD IDLE ends with DONE").as_bytes())
                    };
                }
                Ok(_) if line.len() > MOST_DONE_BYTES => return Err(bad("IDLE ends with DONE")),
                Ok(_) => {}
                Err(error) if timed_out(&error) => self.report_changes(&mut known)?,
                Err(error) => return Err(error),
            }
        }
    }

    fn run(&mut self, tag: &str, verb: &str, arguments: &[Item]) -> io::Result<bool> {
        let verb = verb.to_ascii_uppercase();
        match verb.as_str() {
            "CAPABILITY" => {
                self.send(format!("* CAPABILITY {CAPABILITIES}").as_bytes())?;
                self.send(format!("{tag} OK CAPABILITY completed").as_bytes())?;
            }
            "NOOP" | "CHECK" => self.send(format!("{tag} OK {verb} completed").as_bytes())?,
            "ID" => {
                self.send(b"* ID (\"name\" \"eggdb\")")?;
                self.send(format!("{tag} OK ID completed").as_bytes())?;
            }
            "LOGOUT" => {
                self.send(b"* BYE eggdb logging out")?;
                self.send(format!("{tag} OK LOGOUT completed").as_bytes())?;
                return Ok(false);
            }
            "LOGIN" => {
                let (Some(user), Some(password)) = (arguments.first().and_then(Item::text), arguments.get(1).and_then(Item::text)) else {
                    self.send(format!("{tag} BAD LOGIN needs a user and a password").as_bytes())?;
                    return Ok(true);
                };
                if self.signed_in_as(&user, &password) {
                    self.signed_in = true;
                    self.send(format!("{tag} OK [CAPABILITY {CAPABILITIES}] LOGIN completed").as_bytes())?;
                } else {
                    self.send(format!("{tag} NO [AUTHENTICATIONFAILED] Authentication failed").as_bytes())?;
                }
            }
            "AUTHENTICATE" => self.authenticate(tag, arguments)?,
            _ if !self.signed_in => self.send(format!("{tag} NO Sign in first").as_bytes())?,
            "SELECT" | "EXAMINE" => match arguments.first().and_then(Item::text) {
                Some(name) => self.select(tag, &name, verb == "EXAMINE")?,
                None => self.send(format!("{tag} BAD {verb} needs a mailbox").as_bytes())?,
            },
            "CREATE" | "DELETE" | "SUBSCRIBE" | "UNSUBSCRIBE" => {
                let Some(name) = arguments.first().and_then(Item::text) else {
                    self.send(format!("{tag} BAD {verb} needs a mailbox").as_bytes())?;
                    return Ok(true);
                };
                let outcome = self.write_store(&mut |keys| match verb.as_str() {
                    "CREATE" => store::create_mailbox(keys, &name).map(drop),
                    "DELETE" => store::delete_mailbox(keys, &name),
                    "SUBSCRIBE" => store::subscribe(keys, &name, true),
                    _ => store::subscribe(keys, &name, false),
                });
                match outcome {
                    Ok(()) => self.send(format!("{tag} OK {verb} completed").as_bytes())?,
                    Err(message) => self.send(format!("{tag} NO {message}").as_bytes())?,
                }
            }
            "RENAME" => {
                let (Some(from), Some(to)) = (arguments.first().and_then(Item::text), arguments.get(1).and_then(Item::text)) else {
                    self.send(format!("{tag} BAD RENAME needs two mailboxes").as_bytes())?;
                    return Ok(true);
                };
                match self.write_store(&mut |keys| store::rename_mailbox(keys, &from, &to)) {
                    Ok(()) => self.send(format!("{tag} OK RENAME completed").as_bytes())?,
                    Err(message) => self.send(format!("{tag} NO {message}").as_bytes())?,
                }
            }
            "LIST" => self.list(tag, arguments, false)?,
            "LSUB" => self.list(tag, arguments, true)?,
            "STATUS" => self.status(tag, arguments)?,
            "APPEND" => self.append(tag, arguments)?,
            "IDLE" => self.idle(tag)?,
            _ if self.selected.is_none() => self.send(format!("{tag} NO Select a mailbox first").as_bytes())?,
            "CLOSE" | "UNSELECT" => {
                if verb == "CLOSE" && !self.selected.as_ref().is_some_and(|(_, read_only)| *read_only) {
                    self.expunge(None, None)?;
                }
                self.selected = None;
                self.send(format!("{tag} OK {verb} completed").as_bytes())?;
            }
            "EXPUNGE" => {
                if self.selected.as_ref().is_some_and(|(_, read_only)| *read_only) {
                    self.send(format!("{tag} NO The mailbox is read-only").as_bytes())?;
                } else {
                    self.expunge(Some(tag), None)?;
                }
            }
            "FETCH" => self.fetch(tag, arguments, false)?,
            "STORE" => self.store_flags(tag, arguments, false)?,
            "SEARCH" => self.search(tag, arguments, false)?,
            "COPY" => self.copy(tag, arguments, false, false)?,
            "MOVE" => self.copy(tag, arguments, false, true)?,
            "UID" => {
                let Some(inner) = arguments.first().and_then(Item::text) else {
                    self.send(format!("{tag} BAD UID needs a command").as_bytes())?;
                    return Ok(true);
                };
                let rest = &arguments[1..];
                match inner.to_ascii_uppercase().as_str() {
                    "FETCH" => self.fetch(tag, rest, true)?,
                    "STORE" => self.store_flags(tag, rest, true)?,
                    "SEARCH" => self.search(tag, rest, true)?,
                    "COPY" => self.copy(tag, rest, true, false)?,
                    "MOVE" => self.copy(tag, rest, true, true)?,
                    "EXPUNGE" => {
                        let set = rest.first().and_then(Item::text);
                        self.expunge(Some(tag), set.as_deref())?;
                    }
                    other => self.send(format!("{tag} BAD Unknown UID command {other}").as_bytes())?,
                }
            }
            other => self.send(format!("{tag} BAD Unknown command {other}").as_bytes())?,
        }
        Ok(true)
    }
}

/// Whether mailbox `name` matches a `LIST` pattern: `*` matches anything, `%` anything but the
/// hierarchy separator `/`; `INBOX` matches in any case.
fn wildcard_matches(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = if pattern.eq_ignore_ascii_case(INBOX) { INBOX.chars().collect() } else { pattern.chars().collect() };
    let name: Vec<char> = name.chars().collect();
    let mut table = vec![vec![false; name.len() + 1]; pattern.len() + 1];
    table[0][0] = true;
    for (p, wanted) in pattern.iter().enumerate() {
        for n in 0..=name.len() {
            table[p + 1][n] = match wanted {
                '*' => table[p][n] || (n > 0 && table[p + 1][n - 1]),
                '%' => table[p][n] || (n > 0 && name[n - 1] != '/' && table[p + 1][n - 1]),
                literal => n > 0 && table[p][n - 1] && name[n - 1] == *literal,
            };
        }
    }
    table[pattern.len()][name.len()]
}

/// Serves one IMAP connection until `LOGOUT`, the end of the stream or `stop`.
pub(crate) fn serve_connection(keyspace: &dyn Keyspace, config: &ServerConfig, stream: TcpStream, stop: &(dyn Fn() -> bool + Sync)) -> io::Result<()> {
    let writer = stream.try_clone()?;
    let mut connection = Connection {
        keyspace,
        config,
        reader: BufReader::new(stream),
        writer,
        stop,
        signed_in: false,
        selected: None,
    };
    connection.send(format!("* OK [CAPABILITY {CAPABILITIES}] eggdb IMAP ready").as_bytes())?;
    loop {
        let command = match connection.read_command() {
            Ok(Some(command)) => command,
            Ok(None) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                connection.send(format!("* BAD {error}").as_bytes())?;
                continue;
            }
            Err(error) => return Err(error),
        };
        let mut at = 0;
        let items = match parse_items(&command, &mut at, 0) {
            Ok(items) => items,
            Err(error) => {
                connection.send(format!("* BAD {error}").as_bytes())?;
                continue;
            }
        };
        let (Some(Item::Atom(tag)), Some(Item::Atom(verb))) = (items.first(), items.get(1)) else {
            connection.send(b"* BAD Expected a tag and a command")?;
            continue;
        };
        let (tag, verb) = (tag.clone(), verb.clone());
        if !connection.run(&tag, &verb, &items[2..])? {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_patterns_and_items_parse() {
        assert!(in_set("1:3,7", 2, 10));
        assert!(in_set("5:*", 9, 9));
        assert!(!in_set("1:3,7", 5, 10));
        assert!(wildcard_matches("*", "Archive/2026"));
        assert!(wildcard_matches("%", "Archive"));
        assert!(!wildcard_matches("%", "Archive/2026"));
        assert!(wildcard_matches("inbox", "INBOX"));
        let mut at = 0;
        let items = parse_items(b"a1 FETCH 1:* (FLAGS BODY.PEEK[HEADER.FIELDS (FROM TO)]<0.100>) \"quoted \\\" text\"", &mut at, 0).unwrap();
        assert_eq!(items.len(), 5);
        let Item::List(fetched) = &items[3] else {
            panic!("a list");
        };
        assert_eq!(fetched[1], Item::Atom("BODY.PEEK[HEADER.FIELDS (FROM TO)]<0.100>".to_owned()));
        assert_eq!(items[4], Item::Text(b"quoted \" text".to_vec()));
    }

    #[test]
    fn envelopes_and_sections_follow_rfc_3501() {
        let raw = b"From: Ann <ann@example.org>\r\nTo: bob@example.org\r\nSubject: Hi\r\nDate: Thu, 12 Nov 2026 10:00:00 +0000\r\n\r\nBody\r\n";
        let shown = String::from_utf8_lossy(&envelope(raw)).into_owned();
        assert_eq!(
            shown,
            "(\"Thu, 12 Nov 2026 10:00:00 +0000\" \"Hi\" ((\"Ann\" NIL \"ann\" \"example.org\")) ((\"Ann\" NIL \"ann\" \"example.org\")) ((\"Ann\" NIL \"ann\" \"example.org\")) ((NIL NIL \"bob\" \"example.org\")) NIL NIL NIL NIL)"
        );
        assert_eq!(section_data(raw, "TEXT").unwrap(), b"Body\r\n");
        assert_eq!(section_data(raw, "HEADER.FIELDS (SUBJECT)").unwrap(), b"Subject: Hi\r\n\r\n");
        assert!(String::from_utf8_lossy(&body_structure(raw, true)).starts_with("(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"us-ascii\") NIL NIL \"7BIT\" 6 1"));
    }

    #[test]
    fn multipart_messages_have_structures_and_numbered_sections() {
        let raw = b"Subject: Menu\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nSee attached.\r\n--b\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=menu.pdf\r\nContent-Transfer-Encoding: base64\r\n\r\nJVBERi0x\r\n--b--\r\n";
        let structure = String::from_utf8_lossy(&body_structure(raw, true)).into_owned();
        assert_eq!(
            structure,
            "((\"TEXT\" \"PLAIN\" (\"CHARSET\" \"us-ascii\") NIL NIL \"7BIT\" 13 0 NIL NIL NIL NIL)(\"APPLICATION\" \"PDF\" NIL NIL NIL \"BASE64\" 8 NIL (\"ATTACHMENT\" (\"FILENAME\" \"menu.pdf\")) NIL NIL) \"MIXED\" (\"BOUNDARY\" \"b\") NIL NIL NIL)"
        );
        assert!(String::from_utf8_lossy(&body_structure(raw, false)).ends_with("\"BASE64\" 8) \"MIXED\")"));
        assert_eq!(section_data(raw, "1").unwrap(), b"See attached.");
        assert_eq!(section_data(raw, "2").unwrap(), b"JVBERi0x");
        assert!(section_data(raw, "2.MIME").unwrap().starts_with(b"Content-Type: application/pdf"));
        assert!(section_data(raw, "3").is_err());
    }
}
