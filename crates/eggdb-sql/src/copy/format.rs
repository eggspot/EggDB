use super::{CopyChannel, CopyFormat, CopyOptions};
use crate::error::{SqlError, SqlResult, SqlState};
use std::ops::Range;

pub(crate) const MAX_RECORD_BYTES: usize = 1 << 30;
const COMPACT_ABOVE_BYTES: usize = 1 << 20;
const SHOWN_DATA_BYTES: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Layout {
    pub(crate) csv: bool,
    pub(crate) delimiter: u8,
    pub(crate) null: Vec<u8>,
    pub(crate) default: Option<Vec<u8>>,
    pub(crate) quote: u8,
    pub(crate) escape: u8,
}

impl Layout {
    pub(crate) fn of(options: &CopyOptions) -> Self {
        let csv = options.format == CopyFormat::Csv;
        let quote = options.quote.unwrap_or(b'"');
        Self {
            csv,
            delimiter: options.delimiter.unwrap_or(if csv { b',' } else { b'\t' }),
            null: options
                .null
                .clone()
                .unwrap_or_else(|| if csv { String::new() } else { "\\N".into() })
                .into_bytes(),
            default: options.default.clone().map(String::into_bytes),
            quote,
            escape: options.escape.unwrap_or(quote),
        }
    }

    #[cfg(test)]
    pub(crate) fn text() -> Self {
        Self::of(&CopyOptions::default())
    }
}

fn bad_format(message: impl Into<String>) -> SqlError {
    SqlError::new(SqlState::BAD_COPY_FILE_FORMAT, message)
}

pub(crate) fn shown_data(bytes: &[u8]) -> String {
    let cut = &bytes[..bytes.len().min(SHOWN_DATA_BYTES)];
    let valid = match std::str::from_utf8(cut) {
        Ok(text) => text,
        Err(error) => std::str::from_utf8(&cut[..error.valid_up_to()]).unwrap_or_default(),
    };
    let mut shown = valid.replace('\0', "");
    if bytes.len() > SHOWN_DATA_BYTES {
        shown.push_str("...");
    }
    shown
}

fn invalid_utf8(bytes: &[u8]) -> SqlError {
    let at = match std::str::from_utf8(bytes) {
        Ok(_) => bytes.iter().position(|&byte| byte == 0).unwrap_or(0),
        Err(error) => error.valid_up_to(),
    };
    let shown: Vec<String> = bytes[at..]
        .iter()
        .take(2)
        .map(|byte| format!("0x{byte:02x}"))
        .collect();
    SqlError::new(
        SqlState::CHARACTER_NOT_IN_REPERTOIRE,
        format!(
            "invalid byte sequence for encoding \"UTF8\": {}",
            shown.join(" ")
        ),
    )
}

pub(crate) fn utf8_text(bytes: &[u8]) -> SqlResult<&str> {
    match std::str::from_utf8(bytes) {
        Ok(text) if !bytes.contains(&0) => Ok(text),
        _ => Err(invalid_utf8(bytes)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineEnd {
    Newline,
    CarriageReturn,
    Both,
}

pub(crate) enum Input<'a> {
    Channel(&'a mut dyn CopyChannel),
    Inline(&'a [u8]),
}

enum Scan {
    Record { content_end: usize, next: usize },
    NeedMore,
}

pub(crate) struct RecordReader<'a> {
    input: Input<'a>,
    owned: Vec<u8>,
    start: usize,
    scanned: usize,
    in_quote: bool,
    line_end: Option<LineEnd>,
    input_ended: bool,
    records_ended: bool,
    csv: bool,
    quote: u8,
    escape: u8,
    max_record: usize,
    consumed_before: usize,
    line: u64,
}

impl<'a> RecordReader<'a> {
    pub(crate) fn new(input: Input<'a>, layout: &Layout, max_record: usize) -> Self {
        let input_ended = matches!(input, Input::Inline(_));
        Self {
            input,
            owned: Vec::new(),
            start: 0,
            scanned: 0,
            in_quote: false,
            line_end: None,
            input_ended,
            records_ended: false,
            csv: layout.csv,
            quote: layout.quote,
            escape: layout.escape,
            max_record,
            consumed_before: 0,
            line: 0,
        }
    }

    pub(crate) fn line(&self) -> u64 {
        self.line
    }

    pub(crate) fn consumed(&self) -> usize {
        self.consumed_before + self.start
    }

    fn data(&self) -> &[u8] {
        match &self.input {
            Input::Inline(bytes) => bytes,
            Input::Channel(_) => &self.owned,
        }
    }

    pub(crate) fn bytes(&self, range: Range<usize>) -> &[u8] {
        self.data().get(range).unwrap_or_default()
    }

    fn line_end_error(&self, found: LineEnd) -> SqlError {
        let (message, hint) = match (found, self.csv) {
            (LineEnd::Newline, false) => (
                "literal newline found in data",
                "Use \"\\n\" to represent newline.",
            ),
            (LineEnd::Newline, true) => (
                "unquoted newline found in data",
                "Use quoted CSV field to represent newline.",
            ),
            (_, false) => (
                "literal carriage return found in data",
                "Use \"\\r\" to represent carriage return.",
            ),
            (_, true) => (
                "unquoted carriage return found in data",
                "Use quoted CSV field to represent carriage return.",
            ),
        };
        bad_format(message).with_hint(hint)
    }

    fn record_end(&mut self, at: usize, found: LineEnd) -> SqlResult<Scan> {
        let expected = *self.line_end.get_or_insert(found);
        if expected != found {
            let reported = match (expected, found) {
                (_, LineEnd::Newline) | (LineEnd::CarriageReturn, LineEnd::Both) => {
                    LineEnd::Newline
                }
                _ => LineEnd::CarriageReturn,
            };
            return Err(self.line_end_error(reported));
        }
        let length = if found == LineEnd::Both { 2 } else { 1 };
        Ok(Scan::Record {
            content_end: at,
            next: at + length,
        })
    }

    fn scan(&mut self) -> SqlResult<Scan> {
        let (csv, quote, escape, input_ended) =
            (self.csv, self.quote, self.escape, self.input_ended);
        let mut in_quote = self.in_quote;
        let mut at = self.scanned;
        let found = {
            let data = self.data();
            let length = data.len();
            let mut found = None;
            while at < length {
                let byte = data[at];
                if csv && in_quote {
                    if byte == escape && escape != quote {
                        match data.get(at + 1) {
                            None if !input_ended => break,
                            Some(&next) if next == quote || next == escape => at += 2,
                            _ => at += 1,
                        }
                        continue;
                    }
                    if byte == quote {
                        in_quote = false;
                    }
                    at += 1;
                    continue;
                }
                match byte {
                    b'\n' => {
                        found = Some((at, LineEnd::Newline));
                        break;
                    }
                    b'\r' => match data.get(at + 1) {
                        None if !input_ended => break,
                        Some(b'\n') => {
                            found = Some((at, LineEnd::Both));
                            break;
                        }
                        _ => {
                            found = Some((at, LineEnd::CarriageReturn));
                            break;
                        }
                    },
                    b'\\' if !csv => {
                        if at + 1 >= length && !input_ended {
                            break;
                        }
                        at += 2;
                    }
                    byte if csv && byte == quote => {
                        in_quote = true;
                        at += 1;
                    }
                    _ => at += 1,
                }
            }
            found
        };
        self.in_quote = in_quote;
        match found {
            Some((end, line_end)) => {
                self.scanned = end;
                self.record_end(end, line_end)
            }
            None => {
                self.scanned = at.min(self.data().len());
                Ok(Scan::NeedMore)
            }
        }
    }

    fn too_long(&self) -> SqlError {
        SqlError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!(
                "a line of COPY data is longer than {} bytes",
                self.max_record
            ),
        )
        .with_hint("Each row of COPY data must fit in 1 GiB, as in PostgreSQL.")
    }

    fn fill(&mut self) -> SqlResult<bool> {
        if self.input_ended {
            return Ok(false);
        }
        if self.start >= COMPACT_ABOVE_BYTES && self.start * 2 >= self.owned.len() {
            self.owned.drain(..self.start);
            self.consumed_before += self.start;
            self.scanned -= self.start;
            self.start = 0;
        }
        let Input::Channel(channel) = &mut self.input else {
            return Ok(false);
        };
        let before = self.owned.len();
        loop {
            if !channel.read_copy_data(&mut self.owned)? {
                self.input_ended = true;
                return Ok(false);
            }
            if self.owned.len() > before {
                return Ok(true);
            }
        }
    }

    fn is_end_marker(&self, range: &Range<usize>) -> bool {
        self.bytes(range.clone()) == b"\\."
    }

    pub(crate) fn next_record(&mut self) -> SqlResult<Option<Range<usize>>> {
        if self.records_ended {
            return Ok(None);
        }
        loop {
            match self.scan()? {
                Scan::Record { content_end, next } => {
                    let range = self.start..content_end;
                    self.start = next;
                    self.scanned = next;
                    self.line += 1;
                    if range.len() > self.max_record {
                        return Err(self.too_long());
                    }
                    if self.is_end_marker(&range) {
                        self.records_ended = true;
                        return Ok(None);
                    }
                    return Ok(Some(range));
                }
                Scan::NeedMore => {
                    if self.scanned - self.start > self.max_record {
                        return Err(self.too_long());
                    }
                    if self.fill()? {
                        continue;
                    }
                    if self.input_ended && self.scanned < self.data().len() {
                        continue;
                    }
                    self.records_ended = true;
                    if self.csv && self.in_quote {
                        self.line += 1;
                        return Err(bad_format("unterminated CSV quoted field"));
                    }
                    let end = self.data().len();
                    if self.start >= end {
                        return Ok(None);
                    }
                    let range = self.start..end;
                    self.start = end;
                    self.scanned = end;
                    self.line += 1;
                    if self.is_end_marker(&range) {
                        return Ok(None);
                    }
                    return Ok(Some(range));
                }
            }
        }
    }

    pub(crate) fn drain(&mut self) -> SqlResult<()> {
        self.records_ended = true;
        if let Input::Channel(channel) = &mut self.input {
            let mut discarded = Vec::new();
            while !self.input_ended {
                discarded.clear();
                if !channel.read_copy_data(&mut discarded)? {
                    self.input_ended = true;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn abandon(&mut self) {
        if let Input::Channel(channel) = &mut self.input
            && !self.input_ended
        {
            channel.abandon_copy_in();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldKind {
    Null,
    Default,
    Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Field {
    pub(crate) kind: FieldKind,
    pub(crate) value: Range<usize>,
}

#[derive(Default)]
pub(crate) struct Fields {
    pub(crate) fields: Vec<Field>,
    pub(crate) decoded: Vec<u8>,
}

impl Fields {
    pub(crate) fn value(&self, field: &Field) -> &[u8] {
        self.decoded.get(field.value.clone()).unwrap_or_default()
    }

    fn push(&mut self, kind: FieldKind, start: usize) {
        if kind != FieldKind::Value {
            self.decoded.truncate(start);
        }
        let end = self.decoded.len();
        self.fields.push(Field {
            kind,
            value: start..end,
        });
    }

    pub(crate) fn split(
        &mut self,
        record: &[u8],
        layout: &Layout,
        force_not_null: &[bool],
        force_null: &[bool],
    ) -> SqlResult<()> {
        self.fields.clear();
        self.decoded.clear();
        if layout.csv {
            self.split_csv(record, layout, force_not_null, force_null)
        } else {
            self.split_text(record, layout);
            Ok(())
        }
    }

    fn split_text(&mut self, record: &[u8], layout: &Layout) {
        let mut at = 0;
        loop {
            let start = at;
            while at < record.len() {
                match record[at] {
                    b'\\' => at += 2,
                    byte if byte == layout.delimiter => break,
                    _ => at += 1,
                }
            }
            let end = at.min(record.len());
            let raw = &record[start..end];
            let decoded_start = self.decoded.len();
            let kind = if raw == layout.null.as_slice() {
                FieldKind::Null
            } else if layout.default.as_deref() == Some(raw) {
                FieldKind::Default
            } else {
                unescape_text(raw, &mut self.decoded);
                FieldKind::Value
            };
            self.push(kind, decoded_start);
            if end >= record.len() {
                return;
            }
            at = end + 1;
        }
    }

    fn split_csv(
        &mut self,
        record: &[u8],
        layout: &Layout,
        force_not_null: &[bool],
        force_null: &[bool],
    ) -> SqlResult<()> {
        let (delimiter, quote, escape) = (layout.delimiter, layout.quote, layout.escape);
        let mut at = 0;
        loop {
            let column = self.fields.len();
            let start = at;
            let decoded_start = self.decoded.len();
            let mut quoted = false;
            let mut in_quote = false;
            while at < record.len() {
                let byte = record[at];
                if in_quote {
                    if byte == escape
                        && let Some(&next) = record.get(at + 1)
                        && (next == quote || next == escape)
                    {
                        self.decoded.push(next);
                        at += 2;
                        continue;
                    }
                    if byte == quote {
                        in_quote = false;
                    } else {
                        self.decoded.push(byte);
                    }
                    at += 1;
                    continue;
                }
                if byte == delimiter {
                    break;
                }
                if byte == quote {
                    in_quote = true;
                    quoted = true;
                } else {
                    self.decoded.push(byte);
                }
                at += 1;
            }
            if in_quote {
                return Err(bad_format("unterminated CSV quoted field"));
            }
            let raw = &record[start..at];
            let value = &self.decoded[decoded_start..];
            let null_text = layout.null.as_slice();
            let is_null = if !quoted && raw == null_text {
                !force_not_null.get(column).copied().unwrap_or(false)
            } else {
                quoted && force_null.get(column).copied().unwrap_or(false) && value == null_text
            };
            let kind = if is_null {
                FieldKind::Null
            } else if !quoted && layout.default.as_deref() == Some(raw) {
                FieldKind::Default
            } else {
                FieldKind::Value
            };
            self.push(kind, decoded_start);
            if at >= record.len() {
                return Ok(());
            }
            at += 1;
        }
    }
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn unescape_text(raw: &[u8], out: &mut Vec<u8>) {
    let mut at = 0;
    while at < raw.len() {
        let rest = &raw[at..];
        let plain = rest
            .iter()
            .position(|&byte| byte == b'\\')
            .unwrap_or(rest.len());
        out.extend_from_slice(&rest[..plain]);
        at += plain;
        if at >= raw.len() {
            return;
        }
        let Some(&next) = raw.get(at + 1) else {
            out.push(b'\\');
            return;
        };
        at += 2;
        match next {
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0b),
            b'0'..=b'7' => {
                let mut value = u32::from(next - b'0');
                for _ in 0..2 {
                    match raw.get(at) {
                        Some(&digit @ b'0'..=b'7') => {
                            value = value * 8 + u32::from(digit - b'0');
                            at += 1;
                        }
                        _ => break,
                    }
                }
                out.push((value & 0xff) as u8);
            }
            b'x' => match raw.get(at).copied().and_then(hex_digit) {
                Some(high) => {
                    at += 1;
                    let mut value = high;
                    if let Some(low) = raw.get(at).copied().and_then(hex_digit) {
                        value = value * 16 + low;
                        at += 1;
                    }
                    out.push(value);
                }
                None => out.push(b'x'),
            },
            other => out.push(other),
        }
    }
}

pub(crate) fn escape_text(text: &[u8], delimiter: u8, out: &mut Vec<u8>) {
    for &byte in text {
        let letter = match byte {
            b'\\' => b'\\',
            0x08 => b'b',
            0x0c => b'f',
            b'\n' => b'n',
            b'\r' => b'r',
            b'\t' => b't',
            0x0b => b'v',
            byte if byte == delimiter => byte,
            byte => {
                out.push(byte);
                continue;
            }
        };
        out.push(b'\\');
        out.push(letter);
    }
}

pub(crate) struct RowWriter {
    layout: Layout,
    force_quote: Vec<bool>,
    line: Vec<u8>,
}

fn csv_field(layout: &Layout, line: &mut Vec<u8>, value: &[u8], forced: bool, lone: bool) {
    let special = |byte: &u8| {
        *byte == layout.delimiter || *byte == layout.quote || *byte == b'\n' || *byte == b'\r'
    };
    let needs_quotes = forced
        || value == layout.null.as_slice()
        || (lone && value == b"\\.")
        || value.iter().any(special);
    if !needs_quotes {
        line.extend_from_slice(value);
        return;
    }
    line.push(layout.quote);
    for &byte in value {
        if byte == layout.quote || byte == layout.escape {
            line.push(layout.escape);
        }
        line.push(byte);
    }
    line.push(layout.quote);
}

impl RowWriter {
    pub(crate) fn new(layout: Layout, force_quote: Vec<bool>) -> Self {
        Self {
            layout,
            force_quote,
            line: Vec::new(),
        }
    }

    pub(crate) fn line(&mut self, values: &[Option<String>]) -> &[u8] {
        let Self {
            layout,
            force_quote,
            line,
        } = self;
        line.clear();
        let lone = values.len() == 1;
        for (index, value) in values.iter().enumerate() {
            if index > 0 {
                line.push(layout.delimiter);
            }
            match value {
                None => line.extend_from_slice(&layout.null),
                Some(value) if layout.csv => {
                    let forced = force_quote.get(index).copied().unwrap_or(false);
                    csv_field(layout, line, value.as_bytes(), forced, lone);
                }
                Some(value) => escape_text(value.as_bytes(), layout.delimiter, line),
            }
        }
        line.push(b'\n');
        line
    }
}
