use super::render::{self, JsonObject, LineBreaks};
use eggdb_storage::{BTree, PageSize};
use std::io::{self, Read, Write};

const FORMAT_NAME: &str = "eggdb-jsonl";
const FORMAT_VERSION: u64 = 1;
const COLLECTIONS_VERSION: u64 = 2;
const READ_BUFFER_BYTES: usize = 64 * 1024;
const HEX_CHUNK_BYTES: usize = 8 * 1024;
const LINE_SLACK_BYTES: u64 = 64 * 1024;
const HEADER_TEXT_BYTES: usize = 256;
const FIELD_NAME_BYTES: usize = 16;
const COLLECTION_NAME_BYTES: usize = 128;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Header {
    pub(crate) page_size: Option<usize>,
    pub(crate) eggdb: Option<String>,
    pub(crate) exported_at: Option<String>,
    pub(crate) collections: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Item {
    Entry,
    Collection,
    End,
}

pub(crate) fn largest_key_bytes() -> usize {
    PageSize::from_log2(PageSize::MAX_LOG2).map_or(
        BTree::max_key_bytes(PageSize::DEFAULT),
        BTree::max_key_bytes,
    )
}

fn longest_line_bytes() -> u64 {
    let entry = largest_key_bytes().saturating_add(BTree::MAX_VALUE_BYTES) as u64;
    entry.saturating_mul(2).saturating_add(LINE_SLACK_BYTES)
}

fn is_page_size(bytes: u64) -> bool {
    bytes.is_power_of_two()
        && ((1u64 << PageSize::MIN_LOG2)..=(1u64 << PageSize::MAX_LOG2)).contains(&bytes)
}

pub(crate) fn write_header(out: &mut dyn Write, header: &Header) -> io::Result<()> {
    let mut object = JsonObject::new()
        .string("format", FORMAT_NAME)
        .number(
            "version",
            if header.collections {
                COLLECTIONS_VERSION
            } else {
                FORMAT_VERSION
            },
        );
    if let Some(eggdb) = &header.eggdb {
        object = object.string("eggdb", eggdb);
    }
    if let Some(page_size) = header.page_size {
        object = object.number("page_size", page_size as u64);
    }
    if let Some(exported_at) = &header.exported_at {
        object = object.string("exported_at", exported_at);
    }
    out.write_all(object.line().as_bytes())
}

pub(crate) fn write_entry(out: &mut dyn Write, key: &[u8], value: &[u8]) -> io::Result<()> {
    write_entry_in(out, None, key, value)
}

pub(crate) fn write_entry_in(
    out: &mut dyn Write,
    collection: Option<&str>,
    key: &[u8],
    value: &[u8],
) -> io::Result<()> {
    out.write_all(b"{")?;
    if let Some(collection) = collection {
        out.write_all(b"\"collection\":\"")?;
        write_escaped(out, collection.as_bytes())?;
        out.write_all(b"\",")?;
    }
    write_bytes_field(out, "key", key)?;
    out.write_all(b",")?;
    write_bytes_field(out, "value", value)?;
    out.write_all(b"}\n")
}

pub(crate) fn write_collection_line(out: &mut dyn Write, collection: &str) -> io::Result<()> {
    out.write_all(b"{\"collection\":\"")?;
    write_escaped(out, collection.as_bytes())?;
    out.write_all(b"\"}\n")
}

pub(crate) fn write_trailer(out: &mut dyn Write, entries: u64) -> io::Result<()> {
    out.write_all(
        JsonObject::new()
            .number("entries", entries)
            .line()
            .as_bytes(),
    )
}

fn write_bytes_field(out: &mut dyn Write, name: &str, bytes: &[u8]) -> io::Result<()> {
    match render::as_plain_text(bytes, LineBreaks::Allowed) {
        Some(text) => {
            write!(out, "\"{name}\":\"")?;
            write_escaped(out, text.as_bytes())?;
        }
        None => {
            write!(out, "\"{name}_hex\":\"")?;
            write_hex(out, bytes)?;
        }
    }
    out.write_all(b"\"")
}

fn write_escaped(out: &mut dyn Write, mut text: &[u8]) -> io::Result<()> {
    while let Some(at) = text
        .iter()
        .position(|byte| matches!(byte, b'"' | b'\\' | b'\n' | b'\r' | b'\t'))
    {
        let (plain, rest) = text.split_at(at);
        out.write_all(plain)?;
        let Some((byte, rest)) = rest.split_first() else {
            return Ok(());
        };
        out.write_all(match byte {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            _ => b"\\t",
        })?;
        text = rest;
    }
    out.write_all(text)
}

fn write_hex(out: &mut dyn Write, bytes: &[u8]) -> io::Result<()> {
    let mut digits = String::with_capacity(2 * HEX_CHUNK_BYTES);
    for chunk in bytes.chunks(HEX_CHUNK_BYTES) {
        digits.clear();
        render::push_hex(&mut digits, chunk);
        out.write_all(digits.as_bytes())?;
    }
    Ok(())
}

pub(crate) type Entry<'e> = (&'e [u8], &'e [u8]);

#[derive(Debug)]
pub(crate) enum ReadError {
    Input(io::Error),
    Invalid { line: u64, problem: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Format,
    Version,
    Eggdb,
    PageSize,
    ExportedAt,
    Key,
    KeyHex,
    Value,
    ValueHex,
    Entries,
    Collection,
}

const HEADER_FIELDS: [Field; 5] = [
    Field::Format,
    Field::Version,
    Field::Eggdb,
    Field::PageSize,
    Field::ExportedAt,
];
const ENTRY_FIELDS: [Field; 5] = [
    Field::Key,
    Field::KeyHex,
    Field::Value,
    Field::ValueHex,
    Field::Collection,
];
const ALL_FIELDS: [Field; 11] = [
    Field::Format,
    Field::Version,
    Field::Eggdb,
    Field::PageSize,
    Field::ExportedAt,
    Field::Key,
    Field::KeyHex,
    Field::Value,
    Field::ValueHex,
    Field::Entries,
    Field::Collection,
];

impl Field {
    fn from_name(name: &[u8]) -> Option<Self> {
        ALL_FIELDS
            .into_iter()
            .find(|field| field.name().as_bytes() == name)
    }

    fn name(self) -> &'static str {
        match self {
            Self::Format => "format",
            Self::Version => "version",
            Self::Eggdb => "eggdb",
            Self::PageSize => "page_size",
            Self::ExportedAt => "exported_at",
            Self::Key => "key",
            Self::KeyHex => "key_hex",
            Self::Value => "value",
            Self::ValueHex => "value_hex",
            Self::Entries => "entries",
            Self::Collection => "collection",
        }
    }

    fn bit(self) -> u16 {
        1 << self as u16
    }

    fn other_spelling(self) -> Option<Self> {
        match self {
            Self::Key => Some(Self::KeyHex),
            Self::KeyHex => Some(Self::Key),
            Self::Value => Some(Self::ValueHex),
            Self::ValueHex => Some(Self::Value),
            _ => None,
        }
    }
}

#[derive(Debug, Default)]
struct Fields {
    seen: u16,
    version: u64,
    page_size: u64,
    entries: u64,
    format: Vec<u8>,
    eggdb: Vec<u8>,
    exported_at: Vec<u8>,
}

impl Fields {
    fn has(&self, field: Field) -> bool {
        self.seen & field.bit() != 0
    }

    fn first_outside(&self, allowed: &[Field]) -> Option<Field> {
        ALL_FIELDS
            .into_iter()
            .find(|field| self.has(*field) && !allowed.contains(field))
    }
}

enum Decoded {
    Done,
    TooLong,
    NotUtf8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Header,
    Entries,
    Finished,
}

pub(crate) struct ExportReader<'r> {
    input: &'r mut dyn Read,
    buffer: Vec<u8>,
    start: usize,
    end: usize,
    input_ended: bool,
    line: u64,
    line_bytes: u64,
    longest_line: u64,
    stage: Stage,
    version: u64,
    entries: u64,
    key: Vec<u8>,
    value: Vec<u8>,
    collection: Vec<u8>,
    field_name: Vec<u8>,
    fields: Fields,
}

impl<'r> ExportReader<'r> {
    pub(crate) fn new(input: &'r mut dyn Read) -> Self {
        Self::with_longest_line(input, longest_line_bytes())
    }

    fn with_longest_line(input: &'r mut dyn Read, longest_line: u64) -> Self {
        Self {
            input,
            buffer: vec![0; READ_BUFFER_BYTES],
            start: 0,
            end: 0,
            input_ended: false,
            line: 0,
            line_bytes: 0,
            longest_line,
            stage: Stage::Header,
            version: FORMAT_VERSION,
            entries: 0,
            key: Vec::new(),
            value: Vec::new(),
            collection: Vec::new(),
            field_name: Vec::new(),
            fields: Fields::default(),
        }
    }

    pub(crate) fn line(&self) -> u64 {
        self.line
    }

    pub(crate) fn entries(&self) -> u64 {
        self.entries
    }

    pub(crate) fn current_entry(&self) -> (&[u8], &[u8]) {
        (&self.key, &self.value)
    }

    pub(crate) fn current_collection(&self) -> Option<&str> {
        if !self.fields.has(Field::Collection) {
            return None;
        }
        std::str::from_utf8(&self.collection).ok()
    }

    pub(crate) fn read_header(&mut self) -> Result<Header, ReadError> {
        if self.stage != Stage::Header {
            return Err(self.invalid("the header was already read"));
        }
        if !self.read_line()? {
            self.line = 1;
            return Err(self.invalid(
                "the file is empty; an export starts with a header line such as {\"format\":\"eggdb-jsonl\",\"version\":1}",
            ));
        }
        if !self.fields.has(Field::Format) {
            return Err(self.invalid(
                "this is not an eggdb-jsonl export: its first line has no \"format\" field",
            ));
        }
        if self.fields.format != FORMAT_NAME.as_bytes() {
            return Err(self.invalid(format!(
                "this is not an eggdb-jsonl export: its format is \"{}\"",
                render::text(&self.fields.format)
            )));
        }
        if let Some(field) = self.fields.first_outside(&HEADER_FIELDS) {
            return Err(self.invalid(format!(
                "\"{}\" does not belong in the header",
                field.name()
            )));
        }
        if !self.fields.has(Field::Version) {
            return Err(self.invalid("the header has no \"version\""));
        }
        match self.fields.version {
            FORMAT_VERSION | COLLECTIONS_VERSION => self.version = self.fields.version,
            0 => return Err(self.invalid("version 0 is not a version of eggdb-jsonl")),
            newer => {
                return Err(self.invalid(format!(
                    "this export is version {newer} of eggdb-jsonl, from a newer EggDB; this eggdb reads versions {FORMAT_VERSION} and {COLLECTIONS_VERSION}. Upgrade eggdb from https://github.com/eggspot/EggDB/releases"
                )));
            }
        }
        let page_size = if self.fields.has(Field::PageSize) {
            if !is_page_size(self.fields.page_size) {
                return Err(self.invalid(format!(
                    "\"page_size\" must be a power of two from 4096 to 65536, not {}",
                    self.fields.page_size
                )));
            }
            usize::try_from(self.fields.page_size).ok()
        } else {
            None
        };
        let text = |field: Field, bytes: &[u8]| {
            self.fields
                .has(field)
                .then(|| String::from_utf8_lossy(bytes).into_owned())
        };
        let header = Header {
            page_size,
            eggdb: text(Field::Eggdb, &self.fields.eggdb),
            exported_at: text(Field::ExportedAt, &self.fields.exported_at),
            collections: self.version == COLLECTIONS_VERSION,
        };
        self.stage = Stage::Entries;
        Ok(header)
    }

    pub(crate) fn read_entry(&mut self) -> Result<bool, ReadError> {
        loop {
            match self.read_item()? {
                Item::Entry => return Ok(true),
                Item::Collection => {}
                Item::End => return Ok(false),
            }
        }
    }

    pub(crate) fn next_entry(&mut self) -> Result<Option<Entry<'_>>, ReadError> {
        if self.read_entry()? {
            Ok(Some((&self.key, &self.value)))
        } else {
            Ok(None)
        }
    }

    pub(crate) fn read_item(&mut self) -> Result<Item, ReadError> {
        match self.stage {
            Stage::Header => {
                self.read_header()?;
            }
            Stage::Entries => {}
            Stage::Finished => return Ok(Item::End),
        }
        if !self.read_line()? {
            return Err(self.invalid(
                "the file ends after this line without the trailer line {\"entries\":…}: it is cut short",
            ));
        }
        if self.fields.has(Field::Format) {
            return Err(self.invalid("a second header; an export has one, on its first line"));
        }
        if self.fields.has(Field::Entries) {
            self.finish()?;
            return Ok(Item::End);
        }
        if let Some(field) = self.fields.first_outside(&ENTRY_FIELDS) {
            return Err(self.invalid(format!("\"{}\" does not belong in an entry", field.name())));
        }
        let has_key = self.fields.has(Field::Key) || self.fields.has(Field::KeyHex);
        let has_value = self.fields.has(Field::Value) || self.fields.has(Field::ValueHex);
        if self.fields.has(Field::Collection) {
            if self.version != COLLECTIONS_VERSION {
                return Err(self.invalid(format!(
                    "\"collection\" belongs in version {COLLECTIONS_VERSION} exports, and this one says version {}",
                    self.version
                )));
            }
            if !has_key && !has_value {
                return Ok(Item::Collection);
            }
        }
        if !has_key {
            return Err(self.invalid("an entry needs \"key\" or \"key_hex\""));
        }
        if !has_value {
            return Err(self.invalid("an entry needs \"value\" or \"value_hex\""));
        }
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or_else(|| self.invalid("more entries than a count can hold"))?;
        Ok(Item::Entry)
    }

    fn finish(&mut self) -> Result<(), ReadError> {
        if let Some(field) = self.fields.first_outside(&[Field::Entries]) {
            return Err(self.invalid(format!(
                "\"{}\" does not belong in the trailer line",
                field.name()
            )));
        }
        if self.fields.entries != self.entries {
            return Err(self.invalid(format!(
                "the trailer says the export holds {}, but the file has {}: lines are missing or added",
                render::entries(self.fields.entries),
                render::entries(self.entries)
            )));
        }
        if self.peek()?.is_some() {
            self.line += 1;
            return Err(self.invalid("nothing may follow the trailer line"));
        }
        self.stage = Stage::Finished;
        Ok(())
    }

    fn invalid(&self, problem: impl Into<String>) -> ReadError {
        ReadError::Invalid {
            line: self.line,
            problem: problem.into(),
        }
    }

    fn fill(&mut self) -> Result<bool, ReadError> {
        if self.line_bytes > self.longest_line {
            return Err(self.too_long());
        }
        if self.input_ended {
            return Ok(false);
        }
        loop {
            match self.input.read(&mut self.buffer) {
                Ok(0) => {
                    self.input_ended = true;
                    return Ok(false);
                }
                Ok(read) => {
                    self.start = 0;
                    self.end = read.min(self.buffer.len());
                    return Ok(true);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(ReadError::Input(error)),
            }
        }
    }

    fn too_long(&self) -> ReadError {
        self.invalid(format!(
            "the line is longer than {} bytes, more than any entry needs",
            render::count(self.longest_line)
        ))
    }

    fn peek(&mut self) -> Result<Option<u8>, ReadError> {
        if self.start >= self.end && !self.fill()? {
            return Ok(None);
        }
        Ok(self.buffer.get(self.start).copied())
    }

    fn advance(&mut self, bytes: usize) {
        self.start = self.start.saturating_add(bytes).min(self.end);
        self.line_bytes = self.line_bytes.saturating_add(bytes as u64);
    }

    fn available(&self) -> &[u8] {
        self.buffer.get(self.start..self.end).unwrap_or_default()
    }

    fn found(&mut self) -> Result<String, ReadError> {
        Ok(match self.peek()? {
            None => "the end of the file".to_owned(),
            Some(b'\n') => "the end of the line".to_owned(),
            Some(byte @ 0x21..=0x7e) => format!("'{}'", char::from(byte)),
            Some(byte) => format!("byte 0x{byte:02x}"),
        })
    }

    fn expect(&mut self, wanted: u8, what: &str) -> Result<(), ReadError> {
        if self.peek()? == Some(wanted) {
            self.advance(1);
            return Ok(());
        }
        let found = self.found()?;
        Err(self.invalid(format!("expected {what}, found {found}")))
    }

    fn skip_whitespace(&mut self) -> Result<(), ReadError> {
        while let Some(b' ' | b'\t' | b'\r') = self.peek()? {
            self.advance(1);
        }
        Ok(())
    }

    fn read_line(&mut self) -> Result<bool, ReadError> {
        self.fields.seen = 0;
        self.line_bytes = 0;
        if self.peek()?.is_none() {
            return Ok(false);
        }
        self.line += 1;
        if self.peek()? == Some(b'\n') {
            return Err(self.invalid("the line is empty"));
        }
        self.skip_whitespace()?;
        self.expect(b'{', "'{' to start the line's object")?;
        self.skip_whitespace()?;
        if self.peek()? == Some(b'}') {
            self.advance(1);
        } else {
            self.read_members()?;
        }
        self.skip_whitespace()?;
        match self.peek()? {
            None => {}
            Some(b'\n') => self.advance(1),
            Some(_) => {
                let found = self.found()?;
                return Err(self.invalid(format!(
                    "expected the end of the line after '}}', found {found}"
                )));
            }
        }
        if self.line_bytes > self.longest_line {
            return Err(self.too_long());
        }
        Ok(true)
    }

    fn read_members(&mut self) -> Result<(), ReadError> {
        loop {
            self.skip_whitespace()?;
            let field = self.read_field_name()?;
            self.skip_whitespace()?;
            self.expect(b':', "':' after a field name")?;
            self.skip_whitespace()?;
            self.read_field_value(field)?;
            self.fields.seen |= field.bit();
            self.skip_whitespace()?;
            match self.peek()? {
                Some(b',') => self.advance(1),
                Some(b'}') => {
                    self.advance(1);
                    return Ok(());
                }
                _ => {
                    let found = self.found()?;
                    return Err(self.invalid(format!("expected ',' or '}}', found {found}")));
                }
            }
        }
    }

    fn read_field_name(&mut self) -> Result<Field, ReadError> {
        if self.peek()? != Some(b'"') {
            let found = self.found()?;
            return Err(self.invalid(format!(
                "expected a field name in double quotes, found {found}"
            )));
        }
        let mut name = std::mem::take(&mut self.field_name);
        name.clear();
        let decoded = self.read_text(&mut name, FIELD_NAME_BYTES);
        let field = match decoded {
            Ok(Decoded::Done) => Field::from_name(&name),
            Ok(Decoded::TooLong | Decoded::NotUtf8) => None,
            Err(error) => {
                self.field_name = name;
                return Err(error);
            }
        };
        let shown = render::text(&name);
        self.field_name = name;
        let Some(field) = field else {
            return Err(self.invalid(match decoded {
                Ok(Decoded::TooLong) => format!("unknown field \"{shown}…\""),
                _ => format!("unknown field \"{shown}\""),
            }));
        };
        if self.fields.has(field) {
            return Err(self.invalid(format!("\"{}\" appears twice", field.name())));
        }
        if let Some(other) = field
            .other_spelling()
            .filter(|other| self.fields.has(*other))
        {
            return Err(self.invalid(format!(
                "\"{}\" and \"{}\" cannot both appear",
                other.name(),
                field.name()
            )));
        }
        Ok(field)
    }

    fn read_field_value(&mut self, field: Field) -> Result<(), ReadError> {
        match field {
            Field::Version | Field::PageSize | Field::Entries => {
                let number = self.read_number(field)?;
                match field {
                    Field::Version => self.fields.version = number,
                    Field::PageSize => self.fields.page_size = number,
                    _ => self.fields.entries = number,
                }
                Ok(())
            }
            Field::Format | Field::Eggdb | Field::ExportedAt => {
                let mut text = std::mem::take(match field {
                    Field::Format => &mut self.fields.format,
                    Field::Eggdb => &mut self.fields.eggdb,
                    _ => &mut self.fields.exported_at,
                });
                text.clear();
                let decoded = self.read_string(field, &mut text, HEADER_TEXT_BYTES);
                *match field {
                    Field::Format => &mut self.fields.format,
                    Field::Eggdb => &mut self.fields.eggdb,
                    _ => &mut self.fields.exported_at,
                } = text;
                match decoded? {
                    Decoded::Done => Ok(()),
                    Decoded::TooLong => Err(self.invalid(format!(
                        "\"{}\" is longer than {HEADER_TEXT_BYTES} bytes",
                        field.name()
                    ))),
                    Decoded::NotUtf8 => Err(self.not_utf8(field)),
                }
            }
            Field::Key | Field::KeyHex => {
                let mut key = std::mem::take(&mut self.key);
                key.clear();
                let limit = largest_key_bytes();
                let decoded = self.read_string(field, &mut key, limit);
                self.key = key;
                match decoded? {
                    Decoded::Done => Ok(()),
                    Decoded::TooLong => Err(self.invalid(format!(
                        "the key is longer than {} bytes, the longest key any EggDB database stores",
                        render::count(limit as u64)
                    ))),
                    Decoded::NotUtf8 => Err(self.not_utf8(field)),
                }
            }
            Field::Collection => {
                let mut name = std::mem::take(&mut self.collection);
                name.clear();
                let decoded = self.read_string(field, &mut name, COLLECTION_NAME_BYTES);
                self.collection = name;
                match decoded? {
                    Decoded::Done if self.collection.is_empty() => {
                        Err(self.invalid("\"collection\" is empty; leave it out for the default key space"))
                    }
                    Decoded::Done => Ok(()),
                    Decoded::TooLong => Err(self.invalid(format!(
                        "\"collection\" is longer than {COLLECTION_NAME_BYTES} bytes, the longest collection name"
                    ))),
                    Decoded::NotUtf8 => Err(self.not_utf8(field)),
                }
            }
            Field::Value | Field::ValueHex => {
                let mut value = std::mem::take(&mut self.value);
                value.clear();
                let decoded = self.read_string(field, &mut value, BTree::MAX_VALUE_BYTES);
                self.value = value;
                match decoded? {
                    Decoded::Done => Ok(()),
                    Decoded::TooLong => Err(self
                        .invalid("the value is longer than 1 GiB, the longest value EggDB stores")),
                    Decoded::NotUtf8 => Err(self.not_utf8(field)),
                }
            }
        }
    }

    fn not_utf8(&self, field: Field) -> ReadError {
        match field.other_spelling() {
            Some(hex) => self.invalid(format!(
                "\"{}\" is not valid UTF-8; bytes that are not text go in \"{}\" as hex",
                field.name(),
                hex.name()
            )),
            None => self.invalid(format!("\"{}\" is not valid UTF-8", field.name())),
        }
    }

    fn read_string(
        &mut self,
        field: Field,
        out: &mut Vec<u8>,
        limit: usize,
    ) -> Result<Decoded, ReadError> {
        if self.peek()? != Some(b'"') {
            let found = self.found()?;
            return Err(self.invalid(format!(
                "\"{}\" must be a string in double quotes, found {found}",
                field.name()
            )));
        }
        match field {
            Field::KeyHex | Field::ValueHex => self.read_hex(field, out, limit),
            _ => self.read_text(out, limit),
        }
    }

    fn read_text(&mut self, out: &mut Vec<u8>, limit: usize) -> Result<Decoded, ReadError> {
        self.advance(1);
        loop {
            if self.start >= self.end && !self.fill()? {
                return Err(self.invalid("the file ends inside a string"));
            }
            let available = self.available();
            let run = available
                .iter()
                .position(|byte| matches!(byte, b'"' | b'\\' | 0..=0x1f))
                .unwrap_or(available.len());
            if out.len().saturating_add(run) > limit {
                let room = limit.saturating_sub(out.len());
                out.extend_from_slice(available.get(..room).unwrap_or_default());
                return Ok(Decoded::TooLong);
            }
            out.extend_from_slice(available.get(..run).unwrap_or_default());
            self.advance(run);
            let Some(byte) = self.peek()? else {
                continue;
            };
            match byte {
                0x20.. if byte != b'"' && byte != b'\\' => {}
                b'"' => {
                    self.advance(1);
                    break;
                }
                b'\\' => {
                    self.advance(1);
                    self.read_escape(out)?;
                    if out.len() > limit {
                        return Ok(Decoded::TooLong);
                    }
                }
                b'\n' => return Err(self.invalid("the line ends inside a string")),
                control => {
                    return Err(self.invalid(format!(
                        "a control character (byte 0x{control:02x}) inside a string; JSON spells it \\u{control:04x}"
                    )));
                }
            }
        }
        Ok(match std::str::from_utf8(out) {
            Ok(_) => Decoded::Done,
            Err(_) => Decoded::NotUtf8,
        })
    }

    fn read_escape(&mut self, out: &mut Vec<u8>) -> Result<(), ReadError> {
        let Some(escape) = self.peek()? else {
            return Err(self.invalid("the file ends inside a string"));
        };
        self.advance(1);
        let byte = match escape {
            b'"' => b'"',
            b'\\' => b'\\',
            b'/' => b'/',
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'u' => {
                let character = self.read_unicode_escape()?;
                let mut encoded = [0; 4];
                out.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
                return Ok(());
            }
            other => {
                let shown = match other {
                    0x21..=0x7e => char::from(other).to_string(),
                    _ => format!("byte 0x{other:02x}"),
                };
                return Err(self.invalid(format!(
                    "unknown escape \\{shown}; JSON knows \\\" \\\\ \\/ \\b \\f \\n \\r \\t and \\uXXXX"
                )));
            }
        };
        out.push(byte);
        Ok(())
    }

    fn read_unicode_escape(&mut self) -> Result<char, ReadError> {
        let first = self.read_four_hex_digits()?;
        let code = match first {
            0xd800..=0xdbff => {
                if self.peek()? != Some(b'\\') {
                    return Err(self.lone_surrogate(first));
                }
                self.advance(1);
                if self.peek()? != Some(b'u') {
                    return Err(self.lone_surrogate(first));
                }
                self.advance(1);
                let second = self.read_four_hex_digits()?;
                if !(0xdc00..=0xdfff).contains(&second) {
                    return Err(self.lone_surrogate(first));
                }
                0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00)
            }
            0xdc00..=0xdfff => return Err(self.lone_surrogate(first)),
            scalar => scalar,
        };
        char::from_u32(code).ok_or_else(|| self.lone_surrogate(first))
    }

    fn lone_surrogate(&self, code: u32) -> ReadError {
        self.invalid(format!(
            "\\u{code:04x} is half of a UTF-16 surrogate pair without its other half"
        ))
    }

    fn read_four_hex_digits(&mut self) -> Result<u32, ReadError> {
        let mut code = 0;
        for _ in 0..4 {
            let digit = self.peek()?.and_then(hex_value);
            let Some(digit) = digit else {
                return Err(self.invalid("\\u needs four hex digits"));
            };
            self.advance(1);
            code = code << 4 | u32::from(digit);
        }
        Ok(code)
    }

    fn read_hex(
        &mut self,
        field: Field,
        out: &mut Vec<u8>,
        limit: usize,
    ) -> Result<Decoded, ReadError> {
        self.advance(1);
        let mut high: Option<u8> = None;
        loop {
            if self.start >= self.end && !self.fill()? {
                return Err(self.invalid("the file ends inside a string"));
            }
            let available = self.available();
            let run = available
                .iter()
                .position(|digit| hex_value(*digit).is_none())
                .unwrap_or(available.len());
            let digits = available.get(..run).unwrap_or_default();
            if out.len().saturating_add(run / 2) > limit {
                return Ok(Decoded::TooLong);
            }
            for value in digits.iter().filter_map(|digit| hex_value(*digit)) {
                match high.take() {
                    None => high = Some(value),
                    Some(first) => out.push(first << 4 | value),
                }
            }
            if out.len() > limit {
                return Ok(Decoded::TooLong);
            }
            self.advance(run);
            match self.peek()? {
                None => {}
                Some(digit) if hex_value(digit).is_some() => {}
                Some(b'"') => {
                    self.advance(1);
                    if high.is_some() {
                        return Err(self.invalid(format!(
                            "\"{}\" needs an even number of hex digits",
                            field.name()
                        )));
                    }
                    return Ok(Decoded::Done);
                }
                Some(b'\n') => return Err(self.invalid("the line ends inside a string")),
                Some(_) => {
                    let found = self.found()?;
                    return Err(self.invalid(format!(
                        "\"{}\" holds {found}, which is not a hex digit",
                        field.name()
                    )));
                }
            }
        }
    }

    fn read_number(&mut self, field: Field) -> Result<u64, ReadError> {
        let mut number: u64 = 0;
        let mut digits = 0usize;
        while let Some(digit @ b'0'..=b'9') = self.peek()? {
            if digits == 1 && number == 0 {
                return Err(self.invalid(format!(
                    "\"{}\" has a leading zero, which JSON does not allow",
                    field.name()
                )));
            }
            number = number
                .checked_mul(10)
                .and_then(|number| number.checked_add(u64::from(digit - b'0')))
                .ok_or_else(|| self.invalid(format!("\"{}\" is too large", field.name())))?;
            digits += 1;
            self.advance(1);
        }
        let fraction_or_sign = matches!(self.peek()?, Some(b'.' | b'e' | b'E' | b'-' | b'+'));
        if digits == 0 || fraction_or_sign {
            return Err(self.invalid(format!(
                "\"{}\" must be a whole number such as 1",
                field.name()
            )));
        }
        Ok(number)
    }
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
