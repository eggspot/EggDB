use super::{BcpTerminators, BulkColumn, BulkRows};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::numeric::Numeric;
use crate::types::{SqlType, Value};
use std::io::{Read, Write};
use std::ops::Range;

const READ_CHUNK_BYTES: usize = 64 * 1024;
const MAX_ROW_BYTES: usize = 1 << 30;
const UNEXPECTED_END_OF_FILE: u16 = 4832;
const CONVERSION_ERROR: u16 = 4864;
const FIELD_TOO_LONG: u16 = 4866;
const INVALID_FORMAT_FILE: u16 = 4823;
const DAYS_FROM_YEAR_ONE_TO_2000: i64 = 730_119;
const DAYS_FROM_1900_TO_2000: i64 = 36_524;
const MICROS_PER_DAY: i64 = 86_400_000_000;
const MICROS_PER_MINUTE: i64 = 60_000_000;
const DEFAULT_TIME_SCALE: u8 = 7;
const LARGEST_DECIMAL_PRECISION: u8 = 38;
const LARGEST_SHORT_CHARACTER_LENGTH: u32 = 4000;
const FORMAT_FILE_VERSION: &str = "14.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeType {
    TinyInt,
    SmallInt,
    Int,
    BigInt,
    Bit,
    Real,
    Float,
    Decimal,
    Numeric,
    Money,
    SmallMoney,
    DateTime,
    SmallDateTime,
    Date,
    Time,
    DateTime2,
    DateTimeOffset,
    UniqueId,
}

impl NativeType {
    fn fixed_bytes(self) -> u32 {
        match self {
            Self::TinyInt | Self::Bit => 1,
            Self::SmallInt => 2,
            Self::Int | Self::Real | Self::SmallMoney | Self::SmallDateTime => 4,
            Self::BigInt | Self::Float | Self::Money | Self::DateTime | Self::DateTime2 => 8,
            Self::Decimal | Self::Numeric => 19,
            Self::Date => 3,
            Self::Time => 5,
            Self::DateTimeOffset => 10,
            Self::UniqueId => 16,
        }
    }

    fn host_name(self) -> &'static str {
        match self {
            Self::TinyInt => "SQLTINYINT",
            Self::SmallInt => "SQLSMALLINT",
            Self::Int => "SQLINT",
            Self::BigInt => "SQLBIGINT",
            Self::Bit => "SQLBIT",
            Self::Real => "SQLFLT4",
            Self::Float => "SQLFLT8",
            Self::Decimal => "SQLDECIMAL",
            Self::Numeric => "SQLNUMERIC",
            Self::Money => "SQLMONEY",
            Self::SmallMoney => "SQLMONEY4",
            Self::DateTime => "SQLDATETIME",
            Self::SmallDateTime => "SQLDATETIM4",
            Self::Date => "SQLDATE",
            Self::Time => "SQLTIME",
            Self::DateTime2 => "SQLDATETIME2",
            Self::DateTimeOffset => "SQLDATETIMEOFFSET",
            Self::UniqueId => "SQLUNIQUEID",
        }
    }

    fn always_prefixed(self) -> bool {
        matches!(
            self,
            Self::Decimal
                | Self::Numeric
                | Self::Date
                | Self::Time
                | Self::DateTime2
                | Self::DateTimeOffset
        )
    }

    fn has_time_scale(self) -> bool {
        matches!(self, Self::Time | Self::DateTime2 | Self::DateTimeOffset)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldData {
    Char,
    NChar,
    Binary,
    Native(NativeType),
}

impl FieldData {
    fn host_name(self) -> &'static str {
        match self {
            Self::Char => "SQLCHAR",
            Self::NChar => "SQLNCHAR",
            Self::Binary => "SQLBINARY",
            Self::Native(native) => native.host_name(),
        }
    }

    fn named(name: &str) -> Option<Self> {
        let native = |native| Some(Self::Native(native));
        match name.to_ascii_uppercase().as_str() {
            "SQLCHAR" | "SQLVARYCHAR" | "SQLTEXT" => Some(Self::Char),
            "SQLNCHAR" | "SQLNVARCHAR" | "SQLNTEXT" | "SQLXML" => Some(Self::NChar),
            "SQLBINARY" | "SQLVARYBIN" | "SQLIMAGE" | "SQLUDT" => Some(Self::Binary),
            "SQLTINYINT" => native(NativeType::TinyInt),
            "SQLSMALLINT" => native(NativeType::SmallInt),
            "SQLINT" => native(NativeType::Int),
            "SQLBIGINT" => native(NativeType::BigInt),
            "SQLBIT" => native(NativeType::Bit),
            "SQLFLT4" => native(NativeType::Real),
            "SQLFLT8" => native(NativeType::Float),
            "SQLDECIMAL" => native(NativeType::Decimal),
            "SQLNUMERIC" => native(NativeType::Numeric),
            "SQLMONEY" => native(NativeType::Money),
            "SQLMONEY4" => native(NativeType::SmallMoney),
            "SQLDATETIME" => native(NativeType::DateTime),
            "SQLDATETIM4" => native(NativeType::SmallDateTime),
            "SQLDATE" => native(NativeType::Date),
            "SQLTIME" => native(NativeType::Time),
            "SQLDATETIME2" => native(NativeType::DateTime2),
            "SQLDATETIMEOFFSET" => native(NativeType::DateTimeOffset),
            "SQLUNIQUEID" => native(NativeType::UniqueId),
            _ => None,
        }
    }

    fn is_text(self) -> bool {
        matches!(self, Self::Char | Self::NChar)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BcpField {
    data: FieldData,
    prefix: u8,
    length: u32,
    terminator: Vec<u8>,
    column: Option<usize>,
    name: String,
    precision: u8,
    scale: Option<u8>,
}

impl BcpField {
    fn new(data: FieldData, column: Option<usize>, name: &str) -> Self {
        Self {
            data,
            prefix: 0,
            length: match data {
                FieldData::Native(native) => native.fixed_bytes(),
                _ => 0,
            },
            terminator: Vec::new(),
            column,
            name: name.to_owned(),
            precision: LARGEST_DECIMAL_PRECISION,
            scale: None,
        }
    }

    fn fixed_length(&self) -> u32 {
        match (self.length, self.data) {
            (0, FieldData::Native(native)) => native.fixed_bytes(),
            (length, _) => length,
        }
    }

    fn time_scale(&self) -> u8 {
        self.scale.unwrap_or(DEFAULT_TIME_SCALE).min(DEFAULT_TIME_SCALE)
    }

    fn learn_column(&mut self, column: &BulkColumn) {
        match column.sql_type {
            SqlType::Time(precision)
            | SqlType::Timestamp(precision)
            | SqlType::TimestampTz(precision)
                if matches!(self.data, FieldData::Native(native) if native.has_time_scale()) =>
            {
                self.scale = Some(wire_time_scale(precision));
            }
            SqlType::Numeric(Some(modifier)) => {
                self.precision = u8::try_from(modifier.precision)
                    .unwrap_or(LARGEST_DECIMAL_PRECISION)
                    .min(LARGEST_DECIMAL_PRECISION);
                self.scale = u8::try_from(modifier.scale.max(0)).ok();
            }
            _ => {}
        }
    }
}

fn wire_time_scale(precision: Option<u8>) -> u8 {
    match precision {
        Some(scale) if scale < 6 => scale,
        _ => DEFAULT_TIME_SCALE,
    }
}

fn format_error(message: impl Into<String>) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, message)
        .with_sqlserver_code(INVALID_FORMAT_FILE)
}

/// The layout of a bcp data file: one field after another in each row, each a SQL Server
/// file storage type (`SQLCHAR`, `SQLNCHAR`, `SQLINT`, `SQLDECIMAL`, ...) with its length
/// prefix, length and terminator, and the column of the table it loads (or none, skipped).
/// It comes from a format file ([`BcpFormat::parse`], what `bcp ... -f` takes), from the
/// table's columns for bcp's native format ([`BcpFormat::native`], `bcp ... -n`), or from the
/// character formats ([`BcpFormat::character`], `-c` and `-w`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BcpFormat {
    fields: Vec<BcpField>,
}

impl BcpFormat {
    /// Reads a bcp format file, either kind: a non-XML one (a version line, the number of
    /// fields, then one line per field with its order, storage type, prefix length, length,
    /// quoted terminator, server column order, column name and collation) or an XML one
    /// (`<BCPFORMAT>` with a `<RECORD>` of `<FIELD>`s and a `<ROW>` of `<COLUMN>`s). A server
    /// column order of 0, or a field no `<COLUMN>` names, is skipped. An error names what is
    /// wrong (SQL Server's 4823).
    pub fn parse(text: &str) -> SqlResult<Self> {
        let text = text.trim_start_matches('\u{feff}');
        if text.trim_start().starts_with('<') {
            parse_xml(text)
        } else {
            parse_non_xml(text)
        }
    }

    /// The layout of bcp's native format (`bcp ... -n`) for these columns, as `bcp format
    /// ... -n` describes it: integers, `bit`, floats, `money`, `datetime` and
    /// `uniqueidentifier` in their fixed sizes (with a one-byte length prefix when the column
    /// allows NULL), `decimal` and the date and time types always with a one-byte prefix,
    /// text as UTF-16 (`SQLNCHAR`) with a two-byte prefix (an eight-byte one for `max`), and
    /// binary data with an eight-byte prefix. A length prefix of all `0xFF` bytes is NULL.
    pub fn native(columns: &[BulkColumn]) -> Self {
        Self {
            fields: columns
                .iter()
                .enumerate()
                .map(|(index, column)| native_field(column, index))
                .collect(),
        }
    }

    /// The layout of bcp's character formats for these columns: `SQLCHAR` (UTF-8, `-c`) or,
    /// with `unicode`, `SQLNCHAR` (UTF-16LE, `-w`) fields ended by the field terminator and
    /// the last by the row terminator.
    pub fn character(columns: &[BulkColumn], terminators: &BcpTerminators, unicode: bool) -> Self {
        let data = if unicode {
            FieldData::NChar
        } else {
            FieldData::Char
        };
        let encode = |bytes: &[u8]| -> Vec<u8> {
            if unicode {
                bytes.iter().flat_map(|byte| [*byte, 0]).collect()
            } else {
                bytes.to_vec()
            }
        };
        let last = columns.len().saturating_sub(1);
        Self {
            fields: columns
                .iter()
                .enumerate()
                .map(|(index, column)| {
                    let mut field = BcpField::new(data, Some(index), &column.name);
                    field.length = 8000;
                    field.terminator = if index == last {
                        encode(terminators.row())
                    } else {
                        encode(terminators.field())
                    };
                    field
                })
                .collect(),
        }
    }

    /// How many fields each row of the file holds.
    pub fn field_count(&self) -> usize {
        self.fields.len()
    }

    /// The layout as a non-XML format file (version 14.0), which `bcp ... -f` and
    /// [`BcpFormat::parse`] read back.
    pub fn to_format_file(&self) -> String {
        let mut text = format!("{FORMAT_FILE_VERSION}\n{}\n", self.fields.len());
        for (index, field) in self.fields.iter().enumerate() {
            let collation = if field.data.is_text() {
                "SQL_Latin1_General_CP1_CI_AS"
            } else {
                "\"\""
            };
            let name = if field.name.is_empty() || field.name.contains(char::is_whitespace) {
                format!("column{}", index + 1)
            } else {
                field.name.clone()
            };
            text.push_str(&format!(
                "{}\t{}\t{}\t{}\t\"{}\"\t{}\t{}\t{}\n",
                index + 1,
                field.data.host_name(),
                field.prefix,
                field.length,
                escaped_terminator(&field.terminator, field.data),
                field.column.map_or(0, |column| column + 1),
                name,
                collation
            ));
        }
        text
    }

    fn learn_columns(&mut self, columns: &[BulkColumn]) {
        for field in &mut self.fields {
            if let Some(column) = field.column.and_then(|index| columns.get(index))
                && matches!(field.data, FieldData::Native(_))
            {
                field.learn_column(column);
            }
        }
    }
}

fn escaped_terminator(terminator: &[u8], data: FieldData) -> String {
    let characters: Vec<u8> = if data == FieldData::NChar {
        terminator.iter().step_by(2).copied().collect()
    } else {
        terminator.to_vec()
    };
    let mut text = String::new();
    for byte in characters {
        match byte {
            b'\t' => text.push_str("\\t"),
            b'\n' => text.push_str("\\n"),
            b'\r' => text.push_str("\\r"),
            0 => text.push_str("\\0"),
            b'\\' => text.push_str("\\\\"),
            b'"' => text.push_str("\\\""),
            other => text.push(char::from(other)),
        }
    }
    text
}

fn native_field(column: &BulkColumn, index: usize) -> BcpField {
    let native = |native: NativeType| {
        let mut field = BcpField::new(FieldData::Native(native), Some(index), &column.name);
        field.prefix = u8::from(column.nullable || native.always_prefixed());
        field.learn_column(column);
        field
    };
    let text = |length: Option<u32>| {
        let mut field = BcpField::new(FieldData::NChar, Some(index), &column.name);
        match length.filter(|length| *length <= LARGEST_SHORT_CHARACTER_LENGTH) {
            Some(length) => {
                field.prefix = 2;
                field.length = length * 2;
            }
            None => field.prefix = 8,
        }
        field
    };
    match column.sql_type {
        SqlType::SmallInt => native(NativeType::SmallInt),
        SqlType::Integer => native(NativeType::Int),
        SqlType::BigInt => native(NativeType::BigInt),
        SqlType::Boolean => native(NativeType::Bit),
        SqlType::Real => native(NativeType::Real),
        SqlType::Double => native(NativeType::Float),
        SqlType::Numeric(_) => native(NativeType::Decimal),
        SqlType::Date => native(NativeType::Date),
        SqlType::Time(_) => native(NativeType::Time),
        SqlType::Timestamp(Some(3)) => native(NativeType::DateTime),
        SqlType::Timestamp(_) => native(NativeType::DateTime2),
        SqlType::TimestampTz(_) => native(NativeType::DateTimeOffset),
        SqlType::Uuid => native(NativeType::UniqueId),
        SqlType::Bytea => {
            let mut field = BcpField::new(FieldData::Binary, Some(index), &column.name);
            field.prefix = 8;
            field
        }
        SqlType::Varchar(length) | SqlType::Bpchar(length) => text(length),
        _ => text(None),
    }
}

fn format_file_terminator(text: &str) -> SqlResult<Vec<u8>> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            let mut buffer = [0u8; 4];
            bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
            continue;
        }
        match characters.next() {
            Some('t') => bytes.push(b'\t'),
            Some('n') => bytes.push(b'\n'),
            Some('r') => bytes.push(b'\r'),
            Some('0') => bytes.push(0),
            Some('\\') => bytes.push(b'\\'),
            Some('"') => bytes.push(b'"'),
            Some(other) => {
                bytes.push(b'\\');
                let mut buffer = [0u8; 4];
                bytes.extend_from_slice(other.encode_utf8(&mut buffer).as_bytes());
            }
            None => bytes.push(b'\\'),
        }
    }
    Ok(bytes)
}

fn unicode_terminator(bytes: Vec<u8>, data: FieldData) -> Vec<u8> {
    if data != FieldData::NChar {
        return bytes;
    }
    match std::str::from_utf8(&bytes) {
        Ok(text) => text.encode_utf16().flat_map(u16::to_le_bytes).collect(),
        Err(_) => bytes,
    }
}

fn line_tokens(line: &str) -> SqlResult<Vec<String>> {
    let mut tokens = Vec::new();
    let mut characters = line.chars().peekable();
    while let Some(&character) = characters.peek() {
        if character.is_whitespace() {
            characters.next();
            continue;
        }
        let mut token = String::new();
        if character == '"' {
            characters.next();
            let mut closed = false;
            while let Some(character) = characters.next() {
                match character {
                    '\\' => {
                        token.push('\\');
                        if let Some(next) = characters.next() {
                            token.push(next);
                        }
                    }
                    '"' => {
                        closed = true;
                        break;
                    }
                    other => token.push(other),
                }
            }
            if !closed {
                return Err(format_error(format!(
                    "the format file line '{line}' has a terminator without its closing quote"
                )));
            }
            tokens.push(token);
            continue;
        }
        while let Some(&character) = characters.peek() {
            if character.is_whitespace() {
                break;
            }
            token.push(character);
            characters.next();
        }
        tokens.push(token);
    }
    Ok(tokens)
}

fn prefix_bytes(text: &str, line: &str) -> SqlResult<u8> {
    match text.parse::<u8>() {
        Ok(prefix @ (0 | 1 | 2 | 4 | 8)) => Ok(prefix),
        _ => Err(format_error(format!(
            "the format file line '{line}' has the prefix length '{text}'; it must be 0, 1, 2, 4 or 8"
        ))),
    }
}

fn parse_non_xml(text: &str) -> SqlResult<BcpFormat> {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let version = lines
        .next()
        .ok_or_else(|| format_error("the format file is empty"))?;
    if version.parse::<f64>().is_err() {
        return Err(format_error(format!(
            "the format file starts with '{version}', not its version (such as 14.0)"
        )));
    }
    let count_line = lines
        .next()
        .ok_or_else(|| format_error("the format file has no line with its number of fields"))?;
    let count: usize = count_line.parse().map_err(|_| {
        format_error(format!(
            "the format file's second line is '{count_line}', not its number of fields"
        ))
    })?;
    let mut fields = Vec::with_capacity(count.min(4096));
    for line in lines {
        if fields.len() == count {
            break;
        }
        let tokens = line_tokens(line)?;
        let [_, host, prefix, length, terminator, column, name, ..] = tokens.as_slice() else {
            return Err(format_error(format!(
                "the format file line '{line}' needs a field order, storage type, prefix length, length, terminator, column order and column name"
            )));
        };
        let data = FieldData::named(host).ok_or_else(|| {
            format_error(format!(
                "the format file line '{line}' has the storage type '{host}', which bcp does not know"
            ))
        })?;
        let mut field = BcpField::new(data, None, name);
        field.prefix = prefix_bytes(prefix, line)?;
        field.length = length.parse().map_err(|_| {
            format_error(format!(
                "the format file line '{line}' has the length '{length}', not a number"
            ))
        })?;
        field.terminator = unicode_terminator(format_file_terminator(terminator)?, data);
        let order: usize = column.parse().map_err(|_| {
            format_error(format!(
                "the format file line '{line}' has the column order '{column}', not a number"
            ))
        })?;
        field.column = order.checked_sub(1);
        fields.push(field);
    }
    if fields.len() != count {
        return Err(format_error(format!(
            "the format file says {count} fields but describes {}",
            fields.len()
        )));
    }
    Ok(BcpFormat { fields })
}

fn xml_unescaped(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn xml_tags<'t>(text: &'t str, name: &str) -> Vec<&'t str> {
    let opening = format!("<{name}");
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(&opening) {
        let after = rest.get(at + opening.len()..).unwrap_or_default();
        let boundary = after.chars().next();
        if !matches!(boundary, Some(character) if character.is_whitespace() || character == '/' || character == '>')
        {
            rest = after;
            continue;
        }
        let mut quote: Option<char> = None;
        let mut end = after.len();
        for (offset, character) in after.char_indices() {
            match (quote, character) {
                (None, '"' | '\'') => quote = Some(character),
                (Some(open), _) if open == character => quote = None,
                (None, '>') => {
                    end = offset;
                    break;
                }
                _ => {}
            }
        }
        found.push(after.get(..end).unwrap_or_default());
        rest = after.get(end..).unwrap_or_default();
    }
    found
}

fn xml_attributes(tag: &str) -> Vec<(String, String)> {
    let mut attributes = Vec::new();
    let mut rest = tag.trim_end_matches('/');
    while let Some(equals) = rest.find('=') {
        let name = rest.get(..equals).unwrap_or_default().trim().to_owned();
        let after = rest.get(equals + 1..).unwrap_or_default().trim_start();
        let Some(quote) = after.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            break;
        };
        let body = after.get(1..).unwrap_or_default();
        let Some(close) = body.find(quote) else {
            break;
        };
        attributes.push((name, xml_unescaped(body.get(..close).unwrap_or_default())));
        rest = body.get(close + 1..).unwrap_or_default();
    }
    attributes
}

fn attribute<'a>(attributes: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attributes
        .iter()
        .find(|(key, _)| {
            key.eq_ignore_ascii_case(name)
                || key
                    .rsplit_once(':')
                    .is_some_and(|(_, local)| local.eq_ignore_ascii_case(name))
        })
        .map(|(_, value)| value.as_str())
}

fn number_attribute(attributes: &[(String, String)], name: &str) -> SqlResult<Option<u32>> {
    attribute(attributes, name)
        .map(|text| {
            text.trim().parse().map_err(|_| {
                format_error(format!(
                    "the XML format file has {name}=\"{text}\", not a number"
                ))
            })
        })
        .transpose()
}

fn parse_xml(text: &str) -> SqlResult<BcpFormat> {
    if xml_tags(text, "BCPFORMAT").is_empty() {
        return Err(format_error(
            "the XML format file has no <BCPFORMAT> element",
        ));
    }
    let mut ids = Vec::new();
    let mut fields = Vec::new();
    for tag in xml_tags(text, "FIELD") {
        let attributes = xml_attributes(tag);
        let id = attribute(&attributes, "ID")
            .ok_or_else(|| format_error("a <FIELD> of the XML format file has no ID"))?
            .to_owned();
        let kind = attribute(&attributes, "type").unwrap_or_default();
        let (data, storage) = match kind {
            "CharTerm" | "CharFixed" | "CharPrefix" => (FieldData::Char, kind.get(4..)),
            "NCharTerm" | "NCharFixed" | "NCharPrefix" => (FieldData::NChar, kind.get(5..)),
            "NativeFixed" | "NativePrefix" => (FieldData::Binary, kind.get(6..)),
            other => {
                return Err(format_error(format!(
                    "the <FIELD ID=\"{id}\"> of the XML format file has xsi:type=\"{other}\"; it must be CharTerm, CharFixed, CharPrefix, NCharTerm, NCharFixed, NCharPrefix, NativeFixed or NativePrefix"
                )));
            }
        };
        let mut field = BcpField::new(data, None, "");
        match storage {
            Some("Term") => {
                let terminator = attribute(&attributes, "TERMINATOR").ok_or_else(|| {
                    format_error(format!(
                        "the <FIELD ID=\"{id}\"> of the XML format file is {kind} without a TERMINATOR"
                    ))
                })?;
                field.terminator = unicode_terminator(format_file_terminator(terminator)?, data);
                field.length = number_attribute(&attributes, "MAX_LENGTH")?.unwrap_or(0);
            }
            Some("Prefix") => {
                let prefix = attribute(&attributes, "PREFIX_LENGTH").unwrap_or("2");
                field.prefix = prefix_bytes(prefix, tag)?;
                field.length = number_attribute(&attributes, "MAX_LENGTH")?.unwrap_or(0);
            }
            _ => {
                field.length = number_attribute(&attributes, "LENGTH")?.unwrap_or(0);
            }
        }
        ids.push(id);
        fields.push(field);
    }
    for (index, tag) in xml_tags(text, "COLUMN").into_iter().enumerate() {
        let attributes = xml_attributes(tag);
        let source = attribute(&attributes, "SOURCE").ok_or_else(|| {
            format_error("a <COLUMN> of the XML format file has no SOURCE")
        })?;
        let at = ids.iter().position(|id| id == source).ok_or_else(|| {
            format_error(format!(
                "a <COLUMN> of the XML format file names SOURCE=\"{source}\", which no <FIELD> has as its ID"
            ))
        })?;
        let Some(field) = fields.get_mut(at) else {
            continue;
        };
        field.column = Some(index);
        field.name = attribute(&attributes, "NAME").unwrap_or_default().to_owned();
        if field.data == FieldData::Binary {
            let kind = attribute(&attributes, "type").unwrap_or("SQLVARYBIN");
            field.data = FieldData::named(kind).ok_or_else(|| {
                format_error(format!(
                    "the <COLUMN SOURCE=\"{source}\"> of the XML format file has xsi:type=\"{kind}\", which bcp does not know"
                ))
            })?;
        }
        if let Some(precision) = number_attribute(&attributes, "PRECISION")? {
            field.precision = u8::try_from(precision)
                .unwrap_or(LARGEST_DECIMAL_PRECISION)
                .min(LARGEST_DECIMAL_PRECISION);
        }
        if let Some(scale) = number_attribute(&attributes, "SCALE")? {
            field.scale = u8::try_from(scale).ok();
        }
    }
    if fields.is_empty() {
        return Err(format_error(
            "the XML format file's <RECORD> has no <FIELD>",
        ));
    }
    Ok(BcpFormat { fields })
}

struct FileBytes<'r> {
    input: &'r mut dyn Read,
    buffer: Vec<u8>,
    start: usize,
    ended: bool,
}

impl FileBytes<'_> {
    fn fill(&mut self) -> SqlResult<bool> {
        if self.ended {
            return Ok(false);
        }
        let at = self.buffer.len();
        self.buffer.resize(at + READ_CHUNK_BYTES, 0);
        loop {
            match self.input.read(self.buffer.get_mut(at..).unwrap_or_default()) {
                Ok(count) => {
                    self.buffer.truncate(at + count);
                    self.ended = count == 0;
                    return Ok(count > 0);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => {
                    self.buffer.truncate(at);
                    return Err(SqlError::new(
                        SqlState::IO_ERROR,
                        format!("cannot read the bcp data file: {error}"),
                    ));
                }
            }
        }
    }

    fn at_end(&mut self) -> SqlResult<bool> {
        while self.start >= self.buffer.len() {
            if !self.fill()? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn forget_read(&mut self) {
        if self.start >= READ_CHUNK_BYTES {
            self.buffer.drain(..self.start);
            self.start = 0;
        }
    }

    fn take(&mut self, count: usize) -> SqlResult<Option<Range<usize>>> {
        while self.buffer.len() - self.start < count {
            if !self.fill()? {
                return Ok(None);
            }
        }
        let range = self.start..self.start + count;
        self.start += count;
        Ok(Some(range))
    }

    fn until(&mut self, terminator: &[u8], row_start: usize) -> SqlResult<Option<Range<usize>>> {
        let mut from = self.start;
        loop {
            if let Some(offset) = self
                .buffer
                .get(from..)
                .unwrap_or_default()
                .windows(terminator.len())
                .position(|window| window == terminator)
            {
                let range = self.start..from + offset;
                self.start = from + offset + terminator.len();
                return Ok(Some(range));
            }
            if self.buffer.len() - row_start > MAX_ROW_BYTES {
                return Err(too_long());
            }
            from = self
                .buffer
                .len()
                .saturating_sub(terminator.len().saturating_sub(1))
                .max(self.start);
            if !self.fill()? {
                return Ok(None);
            }
        }
    }

    fn rest(&mut self) -> Range<usize> {
        let range = self.start..self.buffer.len();
        self.start = self.buffer.len();
        range
    }
}

fn too_long() -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        "The bulk load failed. The column is too long in the data file. Verify that the format of the data file is the one the load expects.",
    )
    .with_sqlserver_code(FIELD_TOO_LONG)
}

fn unexpected_end() -> SqlError {
    SqlError::new(
        SqlState::BAD_COPY_FILE_FORMAT,
        "Bulk load: An unexpected end of file was encountered in the data file.",
    )
    .with_sqlserver_code(UNEXPECTED_END_OF_FILE)
}

fn conversion_error(reason: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!(
            "Bulk load data conversion error (type mismatch or invalid character for the specified codepage): {reason}."
        ),
    )
    .with_sqlserver_code(CONVERSION_ERROR)
}

enum FieldValue {
    Null,
    Bytes(Range<usize>),
}

/// Reads a bcp data file laid out by a [`BcpFormat`] as [`BulkRows`]: bcp's native format
/// (`bcp ... in -n`, laid out by the columns the load fills, which it learns from
/// [`BulkRows::columns_known`]) or any file a format file describes (`bcp ... in -f`). Text
/// fields are UTF-8 (`SQLCHAR`) or UTF-16LE (`SQLNCHAR`); with a terminator and no prefix an
/// empty field is NULL and a field of only the NUL character is the empty string; a length
/// prefix of all `0xFF` bytes is NULL. A file that ends inside a row, a length beyond 1 GiB
/// or bytes that do not fit their type are errors.
pub struct BcpFileReader<'r> {
    bytes: FileBytes<'r>,
    format: Option<BcpFormat>,
    rows_read: u64,
    rows_to_skip: u64,
}

impl std::fmt::Debug for BcpFileReader<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BcpFileReader")
            .field("format", &self.format)
            .field("rows_read", &self.rows_read)
            .finish()
    }
}

impl<'r> BcpFileReader<'r> {
    /// A reader of `input`, a whole data file in bcp's native format (`-n`), laid out by the
    /// columns of the table it loads.
    pub fn native(input: &'r mut dyn Read) -> Self {
        Self::with_format(input, None)
    }

    /// A reader of `input`, a whole data file laid out as `format` says.
    pub fn new(input: &'r mut dyn Read, format: BcpFormat) -> Self {
        Self::with_format(input, Some(format))
    }

    fn with_format(input: &'r mut dyn Read, format: Option<BcpFormat>) -> Self {
        Self {
            bytes: FileBytes {
                input,
                buffer: Vec::new(),
                start: 0,
                ended: false,
            },
            format,
            rows_read: 0,
            rows_to_skip: 0,
        }
    }

    /// Skips the file's first `rows` rows, as bcp's `-F` option does (`-F 2` skips one).
    pub fn skipping(mut self, rows: u64) -> Self {
        self.rows_to_skip = rows;
        self
    }

    /// How many rows have been read so far, skipped rows included.
    pub fn rows_read(&self) -> u64 {
        self.rows_read
    }

    fn read_field(
        bytes: &mut FileBytes<'_>,
        field: &BcpField,
        row_start: usize,
        last: bool,
    ) -> SqlResult<FieldValue> {
        if field.prefix > 0 {
            let width = usize::from(field.prefix);
            let range = bytes.take(width)?.ok_or_else(unexpected_end)?;
            let prefix = bytes.buffer.get(range).unwrap_or_default();
            if prefix.iter().all(|byte| *byte == 0xFF) {
                bytes.until_terminator(field)?;
                return Ok(FieldValue::Null);
            }
            let mut wide = [0u8; 8];
            if let Some(target) = wide.get_mut(..width) {
                target.copy_from_slice(prefix);
            }
            let length = usize::try_from(u64::from_le_bytes(wide)).unwrap_or(usize::MAX);
            if length > MAX_ROW_BYTES {
                return Err(too_long());
            }
            let range = bytes.take(length)?.ok_or_else(unexpected_end)?;
            bytes.until_terminator(field)?;
            return Ok(FieldValue::Bytes(range));
        }
        if !field.terminator.is_empty() {
            let range = match bytes.until(&field.terminator, row_start)? {
                Some(range) => range,
                None if last => bytes.rest(),
                None => return Err(unexpected_end()),
            };
            let value = bytes.buffer.get(range.clone()).unwrap_or_default();
            return Ok(match (value, field.data.is_text()) {
                ([], true) => FieldValue::Null,
                _ => FieldValue::Bytes(range),
            });
        }
        let length = usize::try_from(field.fixed_length()).unwrap_or(usize::MAX);
        let range = bytes.take(length)?.ok_or_else(unexpected_end)?;
        Ok(FieldValue::Bytes(range))
    }

    fn read_row(&mut self, columns: usize, row: &mut Vec<Value>) -> SqlResult<bool> {
        row.clear();
        let Some(format) = &self.format else {
            return Err(SqlError::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "a bcp native file is read by the columns of the table it loads, which this load did not give",
            ));
        };
        if self.bytes.at_end()? {
            return Ok(false);
        }
        self.bytes.forget_read();
        let row_start = self.bytes.start;
        row.resize(columns, Value::Null);
        let number = self.rows_read + 1;
        let count = format.fields.len();
        for (index, field) in format.fields.iter().enumerate() {
            let place = |error: SqlError| {
                error.with_context(format!("bcp data file, row {number}, field {}", index + 1))
            };
            let read = Self::read_field(&mut self.bytes, field, row_start, index + 1 == count)
                .map_err(place)?;
            let Some(column) = field.column else {
                continue;
            };
            let slot = row.get_mut(column).ok_or_else(|| {
                place(format_error(format!(
                    "the format maps a field to column {}, but the load fills {columns}",
                    column + 1
                )))
            })?;
            *slot = match read {
                FieldValue::Null => Value::Null,
                FieldValue::Bytes(range) => {
                    decode(field, self.bytes.buffer.get(range).unwrap_or_default())
                        .map_err(place)?
                }
            };
        }
        self.rows_read = number;
        Ok(true)
    }
}

impl FileBytes<'_> {
    fn until_terminator(&mut self, field: &BcpField) -> SqlResult<()> {
        if field.terminator.is_empty() {
            return Ok(());
        }
        let range = self
            .take(field.terminator.len())?
            .ok_or_else(unexpected_end)?;
        if self.buffer.get(range) == Some(field.terminator.as_slice()) {
            Ok(())
        } else {
            Err(conversion_error(
                "a prefixed field is not followed by its terminator",
            ))
        }
    }
}

impl BulkRows for BcpFileReader<'_> {
    fn columns_known(&mut self, columns: &[BulkColumn]) -> SqlResult<()> {
        match &mut self.format {
            Some(format) => format.learn_columns(columns),
            None => self.format = Some(BcpFormat::native(columns)),
        }
        Ok(())
    }

    fn next_row(&mut self, columns: usize, row: &mut Vec<Value>) -> SqlResult<bool> {
        while self.rows_to_skip > 0 {
            self.rows_to_skip -= 1;
            if !self.read_row(columns, row)? {
                return Ok(false);
            }
        }
        self.read_row(columns, row)
    }
}

fn little_endian<const N: usize>(bytes: &[u8]) -> Option<[u8; N]> {
    bytes.try_into().ok()
}

fn unsigned_little_endian(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .rev()
        .fold(0u64, |value, byte| (value << 8) | u64::from(*byte))
}

fn utf16_text(bytes: &[u8]) -> SqlResult<String> {
    if !bytes.len().is_multiple_of(2) {
        return Err(conversion_error("a UTF-16 field has an odd number of bytes"));
    }
    let units = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair.first().copied().unwrap_or(0), pair.get(1).copied().unwrap_or(0)]));
    char::decode_utf16(units)
        .collect::<Result<String, _>>()
        .map_err(|_| conversion_error("a UTF-16 field holds an unpaired surrogate"))
}

fn micros_of_units(units: u64, scale: u8) -> i64 {
    let units = i64::try_from(units).unwrap_or(i64::MAX);
    if scale >= 6 {
        let divisor = 10i64.pow(u32::from(scale - 6));
        (units + divisor / 2) / divisor
    } else {
        units.saturating_mul(10i64.pow(u32::from(6 - scale)))
    }
}

fn units_of_micros(micros: i64, scale: u8) -> u64 {
    let units = if scale >= 6 {
        micros.saturating_mul(10i64.pow(u32::from(scale - 6)))
    } else {
        let divisor = 10i64.pow(u32::from(6 - scale));
        (micros + divisor / 2) / divisor
    };
    u64::try_from(units).unwrap_or(0)
}

fn time_bytes(scale: u8) -> usize {
    match scale {
        0..=2 => 3,
        3..=4 => 4,
        _ => 5,
    }
}

fn time_scale_for(length: usize, scale: u8) -> u8 {
    if time_bytes(scale) == length {
        scale
    } else {
        match length {
            3 => 2,
            4 => 4,
            _ => DEFAULT_TIME_SCALE,
        }
    }
}

fn date_of(bytes: &[u8]) -> SqlResult<i64> {
    if bytes.len() != 3 {
        return Err(conversion_error("a date field is not 3 bytes"));
    }
    i64::try_from(unsigned_little_endian(bytes))
        .map(|days| days - DAYS_FROM_YEAR_ONE_TO_2000)
        .map_err(|_| conversion_error("a date field is out of range"))
}

fn time_and_date(bytes: &[u8], field: &BcpField) -> SqlResult<i64> {
    let split = bytes.len().saturating_sub(3);
    let (time, date) = bytes.split_at(split);
    if !(3..=5).contains(&time.len()) {
        return Err(conversion_error("a datetime2 field has the wrong length"));
    }
    let micros = micros_of_units(
        unsigned_little_endian(time),
        time_scale_for(time.len(), field.time_scale()),
    );
    date_of(date)?
        .checked_mul(MICROS_PER_DAY)
        .and_then(|days| days.checked_add(micros))
        .ok_or_else(|| conversion_error("a datetime2 field is out of range"))
}

fn uuid_from_guid(bytes: [u8; 16]) -> [u8; 16] {
    let [a, b, c, d, e, f, g, h, rest @ ..] = bytes;
    let mut uuid = [0u8; 16];
    let (head, tail) = uuid.split_at_mut(8);
    head.copy_from_slice(&[d, c, b, a, f, e, h, g]);
    tail.copy_from_slice(&rest);
    uuid
}

fn decode(field: &BcpField, bytes: &[u8]) -> SqlResult<Value> {
    let wrong = || {
        conversion_error(&format!(
            "a {} field holds {} bytes",
            field.data.host_name(),
            bytes.len()
        ))
    };
    let native = match field.data {
        FieldData::Char => {
            return match bytes {
                [0] => Ok(Value::Text(String::new())),
                text => std::str::from_utf8(text)
                    .map(|text| Value::Text(text.to_owned()))
                    .map_err(|_| conversion_error("a SQLCHAR field is not UTF-8")),
            };
        }
        FieldData::NChar => {
            return match bytes {
                [0, 0] => Ok(Value::Text(String::new())),
                text => utf16_text(text).map(Value::Text),
            };
        }
        FieldData::Binary => return Ok(Value::Bytea(bytes.to_vec())),
        FieldData::Native(native) => native,
    };
    Ok(match native {
        NativeType::TinyInt => match bytes {
            [byte] => Value::SmallInt(i16::from(*byte)),
            _ => return Err(wrong()),
        },
        NativeType::SmallInt => Value::SmallInt(i16::from_le_bytes(
            little_endian(bytes).ok_or_else(wrong)?,
        )),
        NativeType::Int => Value::Integer(i32::from_le_bytes(
            little_endian(bytes).ok_or_else(wrong)?,
        )),
        NativeType::BigInt => Value::BigInt(i64::from_le_bytes(
            little_endian(bytes).ok_or_else(wrong)?,
        )),
        NativeType::Bit => match bytes {
            [byte] => Value::Boolean(*byte != 0),
            _ => return Err(wrong()),
        },
        NativeType::Real => {
            Value::Real(f32::from_le_bytes(little_endian(bytes).ok_or_else(wrong)?))
        }
        NativeType::Float => {
            Value::Double(f64::from_le_bytes(little_endian(bytes).ok_or_else(wrong)?))
        }
        NativeType::Money => {
            let (high, low) = bytes.split_at(bytes.len().min(4));
            let high = i32::from_le_bytes(little_endian(high).ok_or_else(wrong)?);
            let low = u32::from_le_bytes(little_endian(low).ok_or_else(wrong)?);
            let units = (i64::from(high) << 32) | i64::from(low);
            Value::Numeric(Numeric::from_scaled(i128::from(units), 4))
        }
        NativeType::SmallMoney => {
            let units = i32::from_le_bytes(little_endian(bytes).ok_or_else(wrong)?);
            Value::Numeric(Numeric::from_scaled(i128::from(units), 4))
        }
        NativeType::Decimal | NativeType::Numeric => {
            let [_, scale, sign, magnitude @ ..] = bytes else {
                return Err(wrong());
            };
            let magnitude: [u8; 16] = little_endian(magnitude).ok_or_else(wrong)?;
            let units =
                i128::try_from(u128::from_le_bytes(magnitude)).map_err(|_| wrong())?;
            let units = if *sign == 0 { -units } else { units };
            Value::Numeric(Numeric::from_scaled(units, u16::from(*scale)))
        }
        NativeType::DateTime => {
            let (days, ticks) = bytes.split_at(bytes.len().min(4));
            let days = i64::from(i32::from_le_bytes(little_endian(days).ok_or_else(wrong)?));
            let ticks = i64::from(u32::from_le_bytes(little_endian(ticks).ok_or_else(wrong)?));
            (days - DAYS_FROM_1900_TO_2000)
                .checked_mul(MICROS_PER_DAY)
                .and_then(|micros| micros.checked_add((ticks * 10_000 + 1) / 3))
                .map(Value::Timestamp)
                .ok_or_else(wrong)?
        }
        NativeType::SmallDateTime => {
            let (days, minutes) = bytes.split_at(bytes.len().min(2));
            let days = i64::from(u16::from_le_bytes(little_endian(days).ok_or_else(wrong)?));
            let minutes =
                i64::from(u16::from_le_bytes(little_endian(minutes).ok_or_else(wrong)?));
            Value::Timestamp(
                (days - DAYS_FROM_1900_TO_2000) * MICROS_PER_DAY + minutes * MICROS_PER_MINUTE,
            )
        }
        NativeType::Date => {
            Value::Date(i32::try_from(date_of(bytes)?).map_err(|_| wrong())?)
        }
        NativeType::Time => {
            if !(3..=5).contains(&bytes.len()) {
                return Err(wrong());
            }
            Value::Time(micros_of_units(
                unsigned_little_endian(bytes),
                time_scale_for(bytes.len(), field.time_scale()),
            ))
        }
        NativeType::DateTime2 => Value::Timestamp(time_and_date(bytes, field)?),
        NativeType::DateTimeOffset => {
            let split = bytes.len().saturating_sub(2);
            let (instant, offset) = bytes.split_at(split);
            little_endian::<2>(offset).ok_or_else(wrong)?;
            Value::TimestampTz(time_and_date(instant, field)?)
        }
        NativeType::UniqueId => {
            Value::Uuid(uuid_from_guid(little_endian(bytes).ok_or_else(wrong)?))
        }
    })
}

fn write_error(reason: String) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, reason).with_sqlserver_code(CONVERSION_ERROR)
}

fn scaled_units(number: &Numeric, scale: u8) -> SqlResult<i128> {
    let text = Value::Numeric(number.clone()).to_text().unwrap_or_default();
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.as_str()),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    if !whole.bytes().chain(fraction.bytes()).all(|byte| byte.is_ascii_digit()) {
        return Err(write_error(format!(
            "the value {text} cannot be written to a bcp native file"
        )));
    }
    let mut units: i128 = 0;
    let overflow = || write_error(format!("the value {text} does not fit its bcp field"));
    for byte in whole.bytes().chain(
        fraction
            .bytes()
            .chain(std::iter::repeat(b'0'))
            .take(usize::from(scale)),
    ) {
        units = units
            .checked_mul(10)
            .and_then(|units| units.checked_add(i128::from(byte - b'0')))
            .ok_or_else(overflow)?;
    }
    if fraction
        .as_bytes()
        .get(usize::from(scale))
        .is_some_and(|digit| *digit >= b'5')
    {
        units = units.checked_add(1).ok_or_else(overflow)?;
    }
    Ok(if negative { -units } else { units })
}

fn integer_of(value: &Value) -> Option<i64> {
    match value {
        Value::SmallInt(number) => Some(i64::from(*number)),
        Value::Integer(number) => Some(i64::from(*number)),
        Value::BigInt(number) => Some(*number),
        Value::Boolean(flag) => Some(i64::from(*flag)),
        _ => None,
    }
}

fn time_units_bytes(micros: i64, scale: u8) -> Vec<u8> {
    let units = units_of_micros(micros, scale).to_le_bytes();
    units.get(..time_bytes(scale)).unwrap_or_default().to_vec()
}

fn date_bytes(days_since_2000: i64) -> SqlResult<Vec<u8>> {
    let days = u32::try_from(days_since_2000 + DAYS_FROM_YEAR_ONE_TO_2000)
        .ok()
        .filter(|days| *days < 1 << 24)
        .ok_or_else(|| write_error("a date before year 1 or after 9999 cannot be written".to_owned()))?;
    Ok(days.to_le_bytes().get(..3).unwrap_or_default().to_vec())
}

fn timestamp_bytes(micros: i64, scale: u8) -> SqlResult<Vec<u8>> {
    let mut bytes = time_units_bytes(micros.rem_euclid(MICROS_PER_DAY), scale);
    bytes.extend(date_bytes(micros.div_euclid(MICROS_PER_DAY))?);
    Ok(bytes)
}

fn guid_of_uuid(uuid: [u8; 16]) -> [u8; 16] {
    uuid_from_guid(uuid)
}

fn encode_native(native: NativeType, field: &BcpField, value: &Value) -> SqlResult<Vec<u8>> {
    let mismatch = || {
        write_error(format!(
            "a {} value cannot be written as {}",
            crate::sqlserver_types::value_type_name(value),
            native.host_name()
        ))
    };
    let integer = || integer_of(value).ok_or_else(mismatch);
    let out_of_range = || write_error(format!("the value does not fit {}", native.host_name()));
    Ok(match native {
        NativeType::TinyInt => vec![u8::try_from(integer()?).map_err(|_| out_of_range())?],
        NativeType::SmallInt => i16::try_from(integer()?)
            .map_err(|_| out_of_range())?
            .to_le_bytes()
            .to_vec(),
        NativeType::Int => i32::try_from(integer()?)
            .map_err(|_| out_of_range())?
            .to_le_bytes()
            .to_vec(),
        NativeType::BigInt => integer()?.to_le_bytes().to_vec(),
        NativeType::Bit => match value {
            Value::Boolean(flag) => vec![u8::from(*flag)],
            _ => vec![u8::from(integer()? != 0)],
        },
        NativeType::Real => match value {
            Value::Real(number) => number.to_le_bytes().to_vec(),
            Value::Double(number) => (*number as f32).to_le_bytes().to_vec(),
            _ => return Err(mismatch()),
        },
        NativeType::Float => match value {
            Value::Double(number) => number.to_le_bytes().to_vec(),
            Value::Real(number) => f64::from(*number).to_le_bytes().to_vec(),
            _ => return Err(mismatch()),
        },
        NativeType::Decimal | NativeType::Numeric => {
            let Value::Numeric(number) = value else {
                return Err(mismatch());
            };
            let scale = field
                .scale
                .unwrap_or_else(|| u8::try_from(number.scale()).unwrap_or(LARGEST_DECIMAL_PRECISION))
                .min(LARGEST_DECIMAL_PRECISION);
            let units = scaled_units(number, scale)?;
            let mut bytes = vec![field.precision, scale, u8::from(units >= 0)];
            bytes.extend_from_slice(&units.unsigned_abs().to_le_bytes());
            bytes
        }
        NativeType::Money | NativeType::SmallMoney => {
            let Value::Numeric(number) = value else {
                return Err(mismatch());
            };
            let units = i64::try_from(scaled_units(number, 4)?).map_err(|_| out_of_range())?;
            if native == NativeType::SmallMoney {
                i32::try_from(units)
                    .map_err(|_| out_of_range())?
                    .to_le_bytes()
                    .to_vec()
            } else {
                let mut bytes = ((units >> 32) as i32).to_le_bytes().to_vec();
                bytes.extend_from_slice(&(units as u32).to_le_bytes());
                bytes
            }
        }
        NativeType::Date => match value {
            Value::Date(days) => date_bytes(i64::from(*days))?,
            _ => return Err(mismatch()),
        },
        NativeType::Time => match value {
            Value::Time(micros) => time_units_bytes(*micros, field.time_scale()),
            _ => return Err(mismatch()),
        },
        NativeType::DateTime2 => match value {
            Value::Timestamp(micros) => timestamp_bytes(*micros, field.time_scale())?,
            _ => return Err(mismatch()),
        },
        NativeType::DateTimeOffset => match value {
            Value::TimestampTz(micros) => {
                let mut bytes = timestamp_bytes(*micros, field.time_scale())?;
                bytes.extend_from_slice(&0i16.to_le_bytes());
                bytes
            }
            _ => return Err(mismatch()),
        },
        NativeType::DateTime | NativeType::SmallDateTime => {
            let Value::Timestamp(micros) = value else {
                return Err(mismatch());
            };
            let days = micros.div_euclid(MICROS_PER_DAY) + DAYS_FROM_1900_TO_2000;
            let of_day = micros.rem_euclid(MICROS_PER_DAY);
            if native == NativeType::DateTime {
                let mut bytes = i32::try_from(days)
                    .map_err(|_| out_of_range())?
                    .to_le_bytes()
                    .to_vec();
                let ticks = u32::try_from((of_day * 3 + 5_000) / 10_000).unwrap_or(0);
                bytes.extend_from_slice(&ticks.to_le_bytes());
                bytes
            } else {
                let mut bytes = u16::try_from(days)
                    .map_err(|_| out_of_range())?
                    .to_le_bytes()
                    .to_vec();
                let minutes = u16::try_from((of_day + MICROS_PER_MINUTE / 2) / MICROS_PER_MINUTE)
                    .unwrap_or(0);
                bytes.extend_from_slice(&minutes.to_le_bytes());
                bytes
            }
        }
        NativeType::UniqueId => match value {
            Value::Uuid(uuid) => guid_of_uuid(*uuid).to_vec(),
            _ => return Err(mismatch()),
        },
    })
}

fn character_text(value: &Value) -> SqlResult<String> {
    match value {
        Value::Bytea(bytes) => Ok(crate::bytea::format_hex(bytes)
            .get(2..)
            .unwrap_or_default()
            .to_ascii_uppercase()),
        other => crate::sqlserver_conversion::text_of(other, None, None),
    }
}

fn encode(field: &BcpField, value: &Value) -> SqlResult<Vec<u8>> {
    match field.data {
        FieldData::Char => {
            let text = character_text(value)?;
            Ok(if text.is_empty() && !field.terminator.is_empty() {
                vec![0]
            } else {
                text.into_bytes()
            })
        }
        FieldData::NChar => {
            let text = character_text(value)?;
            Ok(if text.is_empty() && !field.terminator.is_empty() {
                vec![0, 0]
            } else {
                text.encode_utf16().flat_map(u16::to_le_bytes).collect()
            })
        }
        FieldData::Binary => Ok(match value {
            Value::Bytea(bytes) => bytes.clone(),
            other => character_text(other)?.into_bytes(),
        }),
        FieldData::Native(native) => encode_native(native, field, value),
    }
}

/// Writes rows as a bcp data file laid out by a [`BcpFormat`], the files `bcp ... out`
/// writes: each row one field after another, each value in its field's storage type.
pub struct BcpFileWriter<'w> {
    output: &'w mut dyn Write,
    format: BcpFormat,
    rows_written: u64,
}

impl std::fmt::Debug for BcpFileWriter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BcpFileWriter")
            .field("format", &self.format)
            .field("rows_written", &self.rows_written)
            .finish()
    }
}

impl<'w> BcpFileWriter<'w> {
    /// A writer of a data file laid out as `format` says to `output`.
    pub fn new(output: &'w mut dyn Write, format: BcpFormat) -> Self {
        Self {
            output,
            format,
            rows_written: 0,
        }
    }

    /// How many rows have been written.
    pub fn rows_written(&self) -> u64 {
        self.rows_written
    }

    /// Writes one row: `row` holds a value for each column the format's fields name, in
    /// column order. A value that does not fit its field (NULL in a fixed field with no
    /// prefix, a number out of its type's range, text longer than a fixed field) is an
    /// error, as is a failed write.
    pub fn write_row(&mut self, row: &[Value]) -> SqlResult<()> {
        let mut bytes = Vec::new();
        for (index, field) in self.format.fields.iter().enumerate() {
            let value = field
                .column
                .and_then(|column| row.get(column))
                .unwrap_or(&Value::Null);
            encode_field(field, value, &mut bytes).map_err(|error| {
                error.with_context(format!(
                    "bcp data file, row {}, field {}",
                    self.rows_written + 1,
                    index + 1
                ))
            })?;
        }
        self.output.write_all(&bytes).map_err(|error| {
            SqlError::new(
                SqlState::IO_ERROR,
                format!("cannot write the bcp data file: {error}"),
            )
        })?;
        self.rows_written += 1;
        Ok(())
    }
}

fn encode_field(field: &BcpField, value: &Value, bytes: &mut Vec<u8>) -> SqlResult<()> {
    let width = usize::from(field.prefix);
    if matches!(value, Value::Null) {
        if width > 0 {
            bytes.extend(std::iter::repeat_n(0xFF, width));
        } else if field.terminator.is_empty() {
            return Err(write_error(
                "NULL cannot be written into a fixed-length field without a length prefix"
                    .to_owned(),
            ));
        }
        bytes.extend_from_slice(&field.terminator);
        return Ok(());
    }
    let mut data = encode(field, value)?;
    if width > 0 {
        let most = if width == 8 {
            u64::MAX - 1
        } else {
            (1u64 << (width * 8)) - 2
        };
        let length = u64::try_from(data.len()).unwrap_or(u64::MAX);
        if length > most {
            return Err(write_error(format!(
                "a value of {length} bytes does not fit a {width}-byte length prefix"
            )));
        }
        bytes.extend_from_slice(length.to_le_bytes().get(..width).unwrap_or_default());
    } else if field.terminator.is_empty() {
        let length = usize::try_from(field.fixed_length()).unwrap_or(usize::MAX);
        if data.len() > length {
            return Err(write_error(format!(
                "a value of {} bytes does not fit a fixed field of {length}",
                data.len()
            )));
        }
        let padding = match field.data {
            FieldData::Char => vec![b' '; length - data.len()],
            FieldData::NChar => [b' ', 0].repeat((length - data.len()) / 2),
            _ => vec![0; length - data.len()],
        };
        data.extend(padding);
    }
    bytes.extend(data);
    bytes.extend_from_slice(&field.terminator);
    Ok(())
}

/// What [`write_bcp_table`] wrote: the number of rows and the layout of the file, which
/// [`BcpFormat::to_format_file`] writes as a format file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BcpTableWritten {
    /// The number of rows written.
    pub rows: u64,
    /// The layout of the file.
    pub format: BcpFormat,
}

/// Which of bcp's data file formats [`write_bcp_table`] writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BcpFileFormat {
    /// The character format (`-c`): UTF-8 text with these terminators.
    Character(BcpTerminators),
    /// The Unicode character format (`-w`): UTF-16LE text with these terminators.
    Unicode(BcpTerminators),
    /// The native format (`-n`), laid out by the table's columns as [`BcpFormat::native`].
    Native,
}

fn sql_text(text: &str) -> String {
    format!("N'{}'", text.replace('\'', "''"))
}

/// Writes every row of `table` (a name as Transact-SQL writes it, such as `dbo.items`) to
/// `output` as `bcp table out` does, in one of bcp's formats, through a SQL Server dialect
/// session: the columns' types and whether they allow NULL decide the native layout.
pub fn write_bcp_table(
    session: &mut crate::Session<'_>,
    table: &str,
    format: &BcpFileFormat,
    output: &mut dyn Write,
) -> SqlResult<BcpTableWritten> {
    let mut found = None;
    session.execute(&format!("SELECT * FROM {table}"), &mut |result| {
        found = Some(result);
    })?;
    let result = found.ok_or_else(|| {
        SqlError::new(SqlState::UNDEFINED_TABLE, format!("{table} returned no rows"))
    })?;
    let mut columns = Vec::new();
    for column in result.columns.unwrap_or_default() {
        let mut allows_null = true;
        session.execute(
            &format!(
                "SELECT COLUMNPROPERTY(OBJECT_ID({}), {}, 'AllowsNull')",
                sql_text(table),
                sql_text(&column.name)
            ),
            &mut |answer| {
                allows_null = !matches!(
                    answer.rows.first().and_then(|row| row.first()),
                    Some(value) if integer_of(value) == Some(0)
                );
            },
        )?;
        columns.push(BulkColumn {
            name: column.name,
            sql_type: column.sql_type.unwrap_or(SqlType::Text),
            nullable: allows_null,
        });
    }
    let layout = match format {
        BcpFileFormat::Character(terminators) => {
            BcpFormat::character(&columns, terminators, false)
        }
        BcpFileFormat::Unicode(terminators) => BcpFormat::character(&columns, terminators, true),
        BcpFileFormat::Native => BcpFormat::native(&columns),
    };
    let mut writer = BcpFileWriter::new(output, layout.clone());
    for row in &result.rows {
        writer.write_row(row)?;
    }
    Ok(BcpTableWritten {
        rows: writer.rows_written(),
        format: layout,
    })
}

#[cfg(test)]
mod tests {
    use super::{BcpFileReader, BcpFileWriter, BcpFormat};
    use crate::bulk_load::{BcpTerminators, BulkColumn, BulkRows};
    use crate::types::{NumericModifier, SqlType, Value};

    fn column(name: &str, sql_type: SqlType, nullable: bool) -> BulkColumn {
        BulkColumn {
            name: name.to_owned(),
            sql_type,
            nullable,
        }
    }

    fn read_all(reader: &mut BcpFileReader<'_>, columns: usize) -> crate::SqlResult<Vec<Vec<Value>>> {
        let mut rows = Vec::new();
        let mut row = Vec::new();
        while reader.next_row(columns, &mut row)? {
            rows.push(row.clone());
        }
        Ok(rows)
    }

    fn numeric(text: &str) -> Value {
        Value::Numeric(crate::numeric::Numeric::parse(text).unwrap())
    }

    fn table() -> Vec<BulkColumn> {
        vec![
            column("id", SqlType::Integer, false),
            column("name", SqlType::Varchar(Some(10)), true),
            column(
                "price",
                SqlType::Numeric(Some(NumericModifier {
                    precision: 10,
                    scale: 2,
                })),
                true,
            ),
            column("seen", SqlType::Boolean, true),
            column("day", SqlType::Date, true),
            column("at", SqlType::Timestamp(Some(3)), true),
            column("key", SqlType::Uuid, true),
            column("data", SqlType::Bytea, true),
        ]
    }

    const NATIVE_ROW: &[u8] = &[
        0x07, 0x00, 0x00, 0x00,
        0x04, 0x00, b'p', 0x00, b'i', 0x00,
        0x13, 10, 2, 1, 0xE2, 0x04, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0x01, 0x01,
        0x03, 0x5B, 0x95, 0x0A,
        0x08, 0xAC, 0x8E, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x10, 0x33, 0x22, 0x11, 0x00, 0x55, 0x44, 0x77, 0x66, 0x88, 0x99, 0xAA, 0xBB, 0xCC,
        0xDD, 0xEE, 0xFF,
        0x02, 0, 0, 0, 0, 0, 0, 0, 0xCA, 0xFE,
    ];

    const NATIVE_NULL_ROW: &[u8] = &[
        0x08, 0x00, 0x00, 0x00,
        0xFF, 0xFF,
        0xFF,
        0xFF,
        0xFF,
        0xFF,
        0xFF,
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    ];

    fn expected_row() -> Vec<Value> {
        vec![
            Value::Integer(7),
            Value::Text("pi".to_owned()),
            numeric("12.50"),
            Value::Boolean(true),
            Value::Date(-36_524),
            Value::Timestamp(0),
            Value::Uuid([
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC,
                0xDD, 0xEE, 0xFF,
            ]),
            Value::Bytea(vec![0xCA, 0xFE]),
        ]
    }

    #[test]
    fn native_files_read_sql_servers_encodings_with_length_prefixes_and_null_markers() {
        let mut data = NATIVE_ROW.to_vec();
        data.extend_from_slice(NATIVE_NULL_ROW);
        let mut input = data.as_slice();
        let mut reader = BcpFileReader::native(&mut input);
        reader.columns_known(&table()).unwrap();
        let rows = read_all(&mut reader, 8).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], expected_row());
        assert_eq!(rows[1][0], Value::Integer(8));
        assert!(rows[1][1..].iter().all(|value| *value == Value::Null));
    }

    #[test]
    fn native_files_write_the_bytes_they_read() {
        let mut written = Vec::new();
        let mut writer = BcpFileWriter::new(&mut written, BcpFormat::native(&table()));
        writer.write_row(&expected_row()).unwrap();
        let mut nulls = vec![Value::Integer(8)];
        nulls.extend(std::iter::repeat_n(Value::Null, 7));
        writer.write_row(&nulls).unwrap();
        assert_eq!(writer.rows_written(), 2);
        let mut expected = NATIVE_ROW.to_vec();
        expected.extend_from_slice(NATIVE_NULL_ROW);
        assert_eq!(written, expected);
        let mut no_null = Vec::new();
        let mut writer = BcpFileWriter::new(&mut no_null, BcpFormat::native(&table()));
        assert!(writer.write_row(&[Value::Null]).is_err());
    }

    #[test]
    fn native_date_and_time_types_round_trip_at_their_scales() {
        let columns = vec![
            column("t", SqlType::Time(Some(6)), true),
            column("t0", SqlType::Time(Some(0)), true),
            column("d2", SqlType::Timestamp(Some(6)), true),
            column("dto", SqlType::TimestampTz(None), true),
            column("small", SqlType::SmallInt, false),
            column("big", SqlType::BigInt, false),
            column("f", SqlType::Double, false),
            column("text", SqlType::Text, true),
        ];
        let row = vec![
            Value::Time(45_296_789_012),
            Value::Time(3_600_000_000),
            Value::Timestamp(-86_400_000_000 + 1_500_000),
            Value::TimestampTz(774_000_000_123_456),
            Value::SmallInt(-2),
            Value::BigInt(i64::MIN),
            Value::Double(0.25),
            Value::Text(String::new()),
        ];
        let mut written = Vec::new();
        BcpFileWriter::new(&mut written, BcpFormat::native(&columns))
            .write_row(&row)
            .unwrap();
        assert_eq!(&written[..6], &[5, 0xC8, 0x7C, 0xFD, 0x76, 0x69]);
        assert_eq!(&written[6..10], &[3, 0x10, 0x0E, 0x00]);
        let mut input = written.as_slice();
        let mut reader = BcpFileReader::native(&mut input);
        reader.columns_known(&columns).unwrap();
        assert_eq!(read_all(&mut reader, 8).unwrap(), vec![row]);
    }

    #[test]
    fn non_xml_format_files_describe_terminated_prefixed_and_skipped_fields() {
        let format = BcpFormat::parse(
            "14.0\n4\n1  SQLCHAR  0  12  \"|\"  1  id  \"\"\n2  SQLCHAR  0  0  \"|\"  0  skip  \"\"\n3  SQLNCHAR  2  40  \"\"  2  name  SQL_Latin1_General_CP1_CI_AS\n4  SQLINT  1  4  \"\\r\\n\"  3  qty  \"\"\n",
        )
        .unwrap();
        assert_eq!(format.field_count(), 4);
        let mut data: Vec<u8> = b"42|ignored|".to_vec();
        data.extend_from_slice(&[4, 0, b'h', 0, b'i', 0, 4, 9, 0, 0, 0, b'\r', b'\n']);
        data.extend_from_slice(b"|x|");
        data.extend_from_slice(&[0xFF, 0xFF, 0xFF, b'\r', b'\n']);
        let mut input = data.as_slice();
        let mut reader = BcpFileReader::new(&mut input, format.clone());
        let rows = read_all(&mut reader, 3).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![
                    Value::Text("42".to_owned()),
                    Value::Text("hi".to_owned()),
                    Value::Integer(9)
                ],
                vec![Value::Null, Value::Null, Value::Null],
            ]
        );
        assert_eq!(BcpFormat::parse(&format.to_format_file()).unwrap(), format);
        let mut short: &[u8] = &[b'1', b'|', b'x', b'|', 4, 0, b'h'];
        let mut reader = BcpFileReader::new(&mut short, format);
        assert_eq!(
            read_all(&mut reader, 3).unwrap_err().vendor_code(),
            Some(4832)
        );
    }

    #[test]
    fn broken_format_files_are_error_4823() {
        for text in [
            "",
            "abc\n1\n",
            "14.0\nx\n",
            "14.0\n2\n1 SQLCHAR 0 12 \"\\t\" 1 id \"\"\n",
            "14.0\n1\n1 SQLWHAT 0 12 \"\\t\" 1 id \"\"\n",
            "14.0\n1\n1 SQLCHAR 3 12 \"\\t\" 1 id \"\"\n",
            "14.0\n1\n1 SQLCHAR 0 12 \"\\t 1 id\n",
            "<BCPFORMAT><RECORD></RECORD></BCPFORMAT>",
            "<BCPFORMAT><RECORD><FIELD ID=\"1\" xsi:type=\"Odd\"/></RECORD></BCPFORMAT>",
            "<BCPFORMAT><RECORD><FIELD ID=\"1\" xsi:type=\"CharTerm\" TERMINATOR=\",\"/></RECORD><ROW><COLUMN SOURCE=\"2\" NAME=\"a\"/></ROW></BCPFORMAT>",
        ] {
            assert_eq!(
                BcpFormat::parse(text).unwrap_err().vendor_code(),
                Some(4823),
                "{text}"
            );
        }
    }

    #[test]
    fn xml_format_files_map_fields_to_columns_by_their_row() {
        let format = BcpFormat::parse(
            r#"<?xml version="1.0"?>
<BCPFORMAT xmlns="http://schemas.microsoft.com/sqlserver/2004/bulkload/format" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
 <RECORD>
  <FIELD ID="1" xsi:type="NativeFixed" LENGTH="4"/>
  <FIELD ID="2" xsi:type="CharTerm" TERMINATOR="&amp;" MAX_LENGTH="10"/>
  <FIELD ID="3" xsi:type="NCharPrefix" PREFIX_LENGTH="2" MAX_LENGTH="20" COLLATION="SQL_Latin1_General_CP1_CI_AS"/>
  <FIELD ID="4" xsi:type="NativePrefix" PREFIX_LENGTH="1"/>
  <FIELD ID="5" xsi:type="CharTerm" TERMINATOR="\r\n" MAX_LENGTH="10"/>
 </RECORD>
 <ROW>
  <COLUMN SOURCE="1" NAME="id" xsi:type="SQLINT"/>
  <COLUMN SOURCE="3" NAME="name" xsi:type="SQLNVARCHAR"/>
  <COLUMN SOURCE="4" NAME="price" xsi:type="SQLMONEY"/>
  <COLUMN SOURCE="5" NAME="note" xsi:type="SQLVARYCHAR"/>
 </ROW>
</BCPFORMAT>"#,
        )
        .unwrap();
        let mut data: Vec<u8> = vec![5, 0, 0, 0];
        data.extend_from_slice(b"skipped&");
        data.extend_from_slice(&[2, 0, b'A', 0]);
        data.extend_from_slice(&[8, 0, 0, 0, 0, 0x50, 0xC3, 0, 0]);
        data.extend_from_slice(b"\0\r\n");
        let mut input = data.as_slice();
        let mut reader = BcpFileReader::new(&mut input, format);
        assert_eq!(
            read_all(&mut reader, 4).unwrap(),
            vec![vec![
                Value::Integer(5),
                Value::Text("A".to_owned()),
                numeric("5.0000"),
                Value::Text(String::new()),
            ]]
        );
    }

    #[test]
    fn character_layouts_write_bcp_text_and_unicode_files() {
        let columns = vec![
            column("id", SqlType::Integer, false),
            column("flag", SqlType::Boolean, true),
            column("name", SqlType::Text, true),
        ];
        let row = [
            Value::Integer(1),
            Value::Boolean(true),
            Value::Text(String::new()),
        ];
        let mut written = Vec::new();
        let format = BcpFormat::character(&columns, &BcpTerminators::default(), false);
        let mut writer = BcpFileWriter::new(&mut written, format);
        writer.write_row(&row).unwrap();
        writer
            .write_row(&[Value::Integer(2), Value::Null, Value::Text("x".to_owned())])
            .unwrap();
        assert_eq!(written, b"1\t1\t\0\n2\t\tx\n");
        let mut unicode = Vec::new();
        let format = BcpFormat::character(&columns, &BcpTerminators::default(), true);
        BcpFileWriter::new(&mut unicode, format.clone())
            .write_row(&row)
            .unwrap();
        assert_eq!(unicode, [b'1', 0, b'\t', 0, b'1', 0, b'\t', 0, 0, 0, b'\n', 0]);
        let mut input = unicode.as_slice();
        let mut reader = BcpFileReader::new(&mut input, format);
        assert_eq!(
            read_all(&mut reader, 3).unwrap(),
            vec![vec![
                Value::Text("1".to_owned()),
                Value::Text("1".to_owned()),
                Value::Text(String::new())
            ]]
        );
    }

    #[test]
    fn hostile_native_files_are_errors_not_panics() {
        let columns = table();
        for data in [
            &[0x07, 0x00][..],
            &[0x07, 0, 0, 0, 0xFE, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F][..],
            &[0x07, 0, 0, 0, 0, 0, 0x02, 1, 2][..],
            &[0x07, 0, 0, 0, 0, 0, 0x13, 10, 2, 1, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF][..],
        ] {
            let mut input = data;
            let mut reader = BcpFileReader::native(&mut input);
            reader.columns_known(&columns).unwrap();
            assert!(read_all(&mut reader, 8).is_err(), "{data:?}");
        }
        let mut input: &[u8] = &[1, 0, 0, 0];
        let mut reader = BcpFileReader::native(&mut input);
        assert!(read_all(&mut reader, 1).is_err());
    }
}
