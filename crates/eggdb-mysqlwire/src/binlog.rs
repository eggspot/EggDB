//! MySQL's binary log as a replica reads it (D13): event headers, the format description,
//! rotations, `QUERY` and `XID` events, table maps and row-based `WRITE`, `UPDATE` and
//! `DELETE` rows events (versions 1 and 2), with their column values decoded, MySQL's binary
//! `JSON` included. Written from MySQL's public replication protocol documentation; every
//! length is checked, so a damaged event is an error, never a panic.

use std::collections::HashMap;

const HEADER_BYTES: usize = 19;
const CHECKSUM_BYTES: usize = 4;
const QUERY: u8 = 2;
const ROTATE: u8 = 4;
const FORMAT_DESCRIPTION: u8 = 15;
const XID: u8 = 16;
const TABLE_MAP: u8 = 19;
const WRITE_ROWS_V1: u8 = 23;
const UPDATE_ROWS_V1: u8 = 24;
const DELETE_ROWS_V1: u8 = 25;
const HEARTBEAT: u8 = 27;
const WRITE_ROWS: u8 = 30;
const UPDATE_ROWS: u8 = 31;
const DELETE_ROWS: u8 = 32;
const MOST_COLUMNS: u64 = 4096;
const MOST_JSON_DEPTH: usize = 100;

const TYPE_DECIMAL: u8 = 0;
const TYPE_TINY: u8 = 1;
const TYPE_SHORT: u8 = 2;
const TYPE_LONG: u8 = 3;
const TYPE_FLOAT: u8 = 4;
const TYPE_DOUBLE: u8 = 5;
const TYPE_NULL: u8 = 6;
const TYPE_TIMESTAMP: u8 = 7;
const TYPE_LONGLONG: u8 = 8;
const TYPE_INT24: u8 = 9;
const TYPE_DATE: u8 = 10;
const TYPE_TIME: u8 = 11;
const TYPE_DATETIME: u8 = 12;
const TYPE_YEAR: u8 = 13;
const TYPE_VARCHAR: u8 = 15;
const TYPE_BIT: u8 = 16;
const TYPE_TIMESTAMP2: u8 = 17;
const TYPE_DATETIME2: u8 = 18;
const TYPE_TIME2: u8 = 19;
const TYPE_JSON: u8 = 245;
const TYPE_NEWDECIMAL: u8 = 246;
const TYPE_ENUM: u8 = 247;
const TYPE_SET: u8 = 248;
const TYPE_TINY_BLOB: u8 = 249;
const TYPE_MEDIUM_BLOB: u8 = 250;
const TYPE_LONG_BLOB: u8 = 251;
const TYPE_BLOB: u8 = 252;
const TYPE_VAR_STRING: u8 = 253;
const TYPE_STRING: u8 = 254;
const TYPE_GEOMETRY: u8 = 255;

/// One column value of a row image.
#[derive(Debug, Clone, PartialEq)]
pub enum BinlogValue {
    /// `NULL`.
    Null,
    /// An integer as stored, with its width in bytes, so an `UNSIGNED` column can read it
    /// unsigned.
    Integer {
        /// The value read as signed.
        value: i64,
        /// The width in bytes (1, 2, 3, 4 or 8).
        bytes: u8,
    },
    /// A `FLOAT` or `DOUBLE`.
    Float(f64),
    /// A `DECIMAL`, as its exact text.
    Decimal(String),
    /// A date, time, date-time or time stamp, as MySQL writes it (`2026-11-13 10:00:00.5`).
    Temporal(String),
    /// A `YEAR`.
    Year(u16),
    /// The bytes of a string, binary string, blob or geometry.
    Bytes(Vec<u8>),
    /// An `ENUM` member's number, from 1.
    Enum(u16),
    /// A `SET`'s member bits.
    Set(u64),
    /// A `BIT` value.
    Bits(u64),
    /// A `JSON` document, as JSON text.
    Json(String),
}

/// What a rows event did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowsKind {
    /// New rows.
    Write,
    /// Rows changed: each change is a before image and an after image.
    Update,
    /// Rows removed.
    Delete,
}

/// A table map: the table a rows event changes, and its column types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableMap {
    /// The database.
    pub schema: String,
    /// The table.
    pub table: String,
    columns: Vec<(u8, u16)>,
}

/// One event of the binary log.
#[derive(Debug, Clone, PartialEq)]
pub enum BinlogEvent {
    /// The log continues in another file.
    Rotate {
        /// The next file.
        file: String,
        /// The position in it.
        position: u64,
    },
    /// A statement: `BEGIN`, DDL, or a statement logged as text.
    Query {
        /// The default database.
        schema: String,
        /// The statement.
        sql: String,
    },
    /// A transaction committed.
    Commit,
    /// Rows changed in a table.
    Rows {
        /// What happened.
        kind: RowsKind,
        /// The table's database.
        schema: String,
        /// The table.
        table: String,
        /// The row images: one per row, or a before and after image per row for updates.
        rows: Vec<Vec<BinlogValue>>,
    },
    /// A heartbeat or an event a follower needs nothing from.
    Other,
}

struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

fn damaged(what: &str) -> String {
    format!("a damaged binary log event: {what}")
}

impl<'b> Reader<'b> {
    fn take(&mut self, count: usize) -> Result<&'b [u8], String> {
        let end = self.at.checked_add(count).ok_or_else(|| damaged("a length overflows"))?;
        let taken = self.bytes.get(self.at..end).ok_or_else(|| damaged("it ends early"))?;
        self.at = end;
        Ok(taken)
    }

    fn byte(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn little(&mut self, count: usize) -> Result<u64, String> {
        Ok(self.take(count)?.iter().rev().fold(0u64, |value, byte| value << 8 | u64::from(*byte)))
    }

    fn big(&mut self, count: usize) -> Result<u64, String> {
        Ok(self.take(count)?.iter().fold(0u64, |value, byte| value << 8 | u64::from(*byte)))
    }

    fn packed(&mut self) -> Result<u64, String> {
        match self.byte()? {
            0xFC => self.little(2),
            0xFD => self.little(3),
            0xFE => self.little(8),
            small => Ok(u64::from(small)),
        }
    }

    fn rest(&self) -> &'b [u8] {
        self.bytes.get(self.at..).unwrap_or_default()
    }
}

/// Reads a binary log's events in order, keeping what later events need: the table maps and
/// whether events end with a checksum.
#[derive(Debug, Default)]
pub struct BinlogDecoder {
    tables: HashMap<u64, TableMap>,
    checksum: bool,
}

fn text_of(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn fraction(reader: &mut Reader<'_>, digits: u16) -> Result<String, String> {
    let digits = digits.min(6);
    let bytes = usize::from(digits.div_ceil(2));
    if bytes == 0 {
        return Ok(String::new());
    }
    let raw = reader.big(bytes)?;
    let micros = raw * 10u64.pow(u32::from(6 - (bytes as u16) * 2).min(6));
    let shown = format!("{micros:06}");
    Ok(format!(".{}", shown.get(..usize::from(digits)).unwrap_or_default()))
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let of_era = shifted.rem_euclid(146_097);
    let year_of_era = (of_era - of_era / 1460 + of_era / 36_524 - of_era / 146_096) / 365;
    let day_of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

const DIGITS_PER_WORD: usize = 9;
const BYTES_OF_DIGITS: [usize; 10] = [0, 1, 1, 2, 2, 3, 3, 4, 4, 4];

fn decimal(reader: &mut Reader<'_>, precision: u8, scale: u8) -> Result<String, String> {
    let (precision, scale) = (usize::from(precision), usize::from(scale));
    if scale > precision || precision > 65 {
        return Err(damaged("a DECIMAL's precision and scale"));
    }
    let integral = precision - scale;
    let size = integral / DIGITS_PER_WORD * 4
        + BYTES_OF_DIGITS[integral % DIGITS_PER_WORD]
        + scale / DIGITS_PER_WORD * 4
        + BYTES_OF_DIGITS[scale % DIGITS_PER_WORD];
    let mut bytes = reader.take(size)?.to_vec();
    let Some(first) = bytes.first_mut() else {
        return Ok("0".to_owned());
    };
    let negative = *first & 0x80 == 0;
    *first ^= 0x80;
    if negative {
        for byte in &mut bytes {
            *byte = !*byte;
        }
    }
    let mut digits = Reader { bytes: &bytes, at: 0 };
    let mut whole = String::new();
    let leading = integral % DIGITS_PER_WORD;
    if leading > 0 {
        whole.push_str(&digits.big(BYTES_OF_DIGITS[leading])?.to_string());
    }
    for _ in 0..integral / DIGITS_PER_WORD {
        whole.push_str(&format!("{:09}", digits.big(4)?));
    }
    let whole = whole.trim_start_matches('0');
    let mut fractional = String::new();
    for _ in 0..scale / DIGITS_PER_WORD {
        fractional.push_str(&format!("{:09}", digits.big(4)?));
    }
    let trailing = scale % DIGITS_PER_WORD;
    if trailing > 0 {
        let value = digits.big(BYTES_OF_DIGITS[trailing])?;
        fractional.push_str(&format!("{value:0width$}", width = trailing));
    }
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(if whole.is_empty() { "0" } else { whole });
    if !fractional.is_empty() {
        out.push('.');
        out.push_str(&fractional);
    }
    Ok(out)
}

fn json_scalar(kind: u8, reader: &mut Reader<'_>, depth: usize) -> Result<String, String> {
    match kind {
        0x00 | 0x01 | 0x02 | 0x03 => json_container(kind, reader.rest(), depth + 1),
        0x04 => Ok(match reader.byte()? {
            0 => "null",
            1 => "true",
            _ => "false",
        }
        .to_owned()),
        0x05 => Ok((reader.little(2)? as u16 as i16).to_string()),
        0x06 => Ok(reader.little(2)?.to_string()),
        0x07 => Ok((reader.little(4)? as u32 as i32).to_string()),
        0x08 => Ok(reader.little(4)?.to_string()),
        0x09 => Ok((reader.little(8)? as i64).to_string()),
        0x0A => Ok(reader.little(8)?.to_string()),
        0x0B => Ok(f64::from_bits(reader.little(8)?).to_string()),
        0x0C => {
            let length = json_length(reader)?;
            Ok(json_string(&text_of(reader.take(length)?)))
        }
        0x0F => {
            reader.byte()?;
            let length = json_length(reader)?;
            Ok(json_string(&text_of(reader.take(length)?)))
        }
        other => Err(damaged(&format!("JSON value type {other}"))),
    }
}

fn json_length(reader: &mut Reader<'_>) -> Result<usize, String> {
    let mut length = 0usize;
    for shift in (0..35).step_by(7) {
        let byte = reader.byte()?;
        length |= usize::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Ok(length);
        }
    }
    Err(damaged("a JSON length"))
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if u32::from(control) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(control))),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn json_container(kind: u8, bytes: &[u8], depth: usize) -> Result<String, String> {
    if depth > MOST_JSON_DEPTH {
        return Err(damaged("JSON nested too deeply"));
    }
    let large = kind == 0x01 || kind == 0x03;
    let object = kind == 0x00 || kind == 0x01;
    let width = if large { 4 } else { 2 };
    let mut header = Reader { bytes, at: 0 };
    let count = header.little(width)? as usize;
    header.little(width)?;
    let mut keys = Vec::new();
    if object {
        for _ in 0..count {
            let offset = header.little(width)? as usize;
            let length = header.little(2)? as usize;
            let key = bytes.get(offset..offset + length).ok_or_else(|| damaged("a JSON key"))?;
            keys.push(text_of(key));
        }
    }
    let mut values = Vec::with_capacity(count.min(1 << 16));
    for _ in 0..count {
        let value_kind = header.byte()?;
        let inline = matches!(value_kind, 0x04 | 0x05 | 0x06) || (large && matches!(value_kind, 0x07 | 0x08));
        if inline {
            let mut inline_reader = Reader { bytes: header.take(width)?, at: 0 };
            values.push(json_scalar(value_kind, &mut inline_reader, depth)?);
        } else {
            let offset = header.little(width)? as usize;
            let mut at = Reader { bytes: bytes.get(offset..).ok_or_else(|| damaged("a JSON offset"))?, at: 0 };
            values.push(json_scalar(value_kind, &mut at, depth)?);
        }
    }
    Ok(if object {
        let members: Vec<String> = keys
            .iter()
            .zip(&values)
            .map(|(key, value)| format!("{}: {value}", json_string(key)))
            .collect();
        format!("{{{}}}", members.join(", "))
    } else {
        format!("[{}]", values.join(", "))
    })
}

fn json_document(bytes: &[u8]) -> Result<String, String> {
    if bytes.is_empty() {
        return Ok("null".to_owned());
    }
    let mut reader = Reader { bytes, at: 0 };
    let kind = reader.byte()?;
    match kind {
        0x00..=0x03 => json_container(kind, reader.rest(), 0),
        other => json_scalar(other, &mut reader, 0),
    }
}

fn value(reader: &mut Reader<'_>, kind: u8, metadata: u16) -> Result<BinlogValue, String> {
    let integer = |reader: &mut Reader<'_>, bytes: u8| -> Result<BinlogValue, String> {
        let raw = reader.little(usize::from(bytes))?;
        let shift = 64 - u32::from(bytes) * 8;
        Ok(BinlogValue::Integer { value: ((raw << shift) as i64) >> shift, bytes })
    };
    Ok(match kind {
        TYPE_TINY => integer(reader, 1)?,
        TYPE_SHORT => integer(reader, 2)?,
        TYPE_INT24 => integer(reader, 3)?,
        TYPE_LONG => integer(reader, 4)?,
        TYPE_LONGLONG => integer(reader, 8)?,
        TYPE_FLOAT => BinlogValue::Float(f64::from(f32::from_bits(reader.little(4)? as u32))),
        TYPE_DOUBLE => BinlogValue::Float(f64::from_bits(reader.little(8)?)),
        TYPE_NEWDECIMAL => BinlogValue::Decimal(decimal(reader, (metadata >> 8) as u8, metadata as u8)?),
        TYPE_YEAR => {
            let year = reader.byte()?;
            BinlogValue::Year(if year == 0 { 0 } else { 1900 + u16::from(year) })
        }
        TYPE_DATE => {
            let packed = reader.little(3)?;
            BinlogValue::Temporal(format!("{:04}-{:02}-{:02}", packed >> 9, (packed >> 5) & 15, packed & 31))
        }
        TYPE_TIMESTAMP2 => {
            let seconds = reader.big(4)? as i64;
            let fraction = fraction(reader, metadata)?;
            let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
            let of_day = seconds.rem_euclid(86_400);
            BinlogValue::Temporal(format!(
                "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}{fraction}",
                of_day / 3600,
                of_day / 60 % 60,
                of_day % 60
            ))
        }
        TYPE_DATETIME2 => {
            let packed = reader.big(5)?.wrapping_sub(0x80_0000_0000);
            let fraction = fraction(reader, metadata)?;
            let year_month = (packed >> 22) & 0x1_FFFF;
            BinlogValue::Temporal(format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}{fraction}",
                year_month / 13,
                year_month % 13,
                (packed >> 17) & 31,
                (packed >> 12) & 31,
                (packed >> 6) & 63,
                packed & 63
            ))
        }
        TYPE_TIME2 => {
            let raw = reader.big(3)? as i64 - 0x80_0000;
            let fraction = fraction(reader, metadata)?;
            let (sign, magnitude) = if raw < 0 { ("-", -raw) } else { ("", raw) };
            BinlogValue::Temporal(format!(
                "{sign}{:02}:{:02}:{:02}{fraction}",
                (magnitude >> 12) & 0x3FF,
                (magnitude >> 6) & 63,
                magnitude & 63
            ))
        }
        TYPE_VARCHAR | TYPE_VAR_STRING => {
            let length = reader.little(if metadata > 255 { 2 } else { 1 })? as usize;
            BinlogValue::Bytes(reader.take(length)?.to_vec())
        }
        TYPE_STRING => {
            let real = (metadata >> 8) as u8;
            match real {
                TYPE_ENUM => BinlogValue::Enum(reader.little(usize::from(metadata & 0xFF))? as u16),
                TYPE_SET => BinlogValue::Set(reader.little(usize::from(metadata & 0xFF))?),
                _ => {
                    let longest = ((u32::from(metadata >> 4) & 0x300) ^ 0x300) + u32::from(metadata & 0xFF);
                    let length = reader.little(if longest > 255 { 2 } else { 1 })? as usize;
                    BinlogValue::Bytes(reader.take(length)?.to_vec())
                }
            }
        }
        TYPE_BLOB | TYPE_TINY_BLOB | TYPE_MEDIUM_BLOB | TYPE_LONG_BLOB | TYPE_GEOMETRY => {
            let length = reader.little(usize::from(metadata.clamp(1, 4)))? as usize;
            BinlogValue::Bytes(reader.take(length)?.to_vec())
        }
        TYPE_JSON => {
            let length = reader.little(usize::from(metadata.clamp(1, 4)))? as usize;
            BinlogValue::Json(json_document(reader.take(length)?)?)
        }
        TYPE_BIT => {
            let bytes = usize::from(metadata >> 8) + usize::from((metadata & 0xFF) > 0);
            BinlogValue::Bits(reader.big(bytes.min(8))?)
        }
        TYPE_ENUM => BinlogValue::Enum(reader.little(usize::from(metadata & 0xFF))? as u16),
        TYPE_SET => BinlogValue::Set(reader.little(usize::from(metadata & 0xFF))?),
        TYPE_NULL => BinlogValue::Null,
        TYPE_DECIMAL | TYPE_TIMESTAMP | TYPE_TIME | TYPE_DATETIME => {
            return Err(format!(
                "the binary log holds a column of the pre-5.6 type {kind}; run ALTER TABLE ... FORCE on the source to upgrade it"
            ));
        }
        other => return Err(format!("the binary log holds a column of type {other}, which EggDB cannot read yet")),
    })
}

fn metadata_of(kind: u8, reader: &mut Reader<'_>) -> Result<u16, String> {
    Ok(match kind {
        TYPE_FLOAT | TYPE_DOUBLE | TYPE_BLOB | TYPE_TINY_BLOB | TYPE_MEDIUM_BLOB | TYPE_LONG_BLOB | TYPE_JSON
        | TYPE_GEOMETRY | TYPE_TIMESTAMP2 | TYPE_DATETIME2 | TYPE_TIME2 => u16::from(reader.byte()?),
        TYPE_VARCHAR | TYPE_VAR_STRING => reader.little(2)? as u16,
        TYPE_BIT => reader.little(2)? as u16,
        TYPE_NEWDECIMAL | TYPE_STRING | TYPE_ENUM | TYPE_SET => {
            let high = u16::from(reader.byte()?);
            let low = u16::from(reader.byte()?);
            high << 8 | low
        }
        _ => 0,
    })
}

fn bitmap(reader: &mut Reader<'_>, columns: usize) -> Result<Vec<bool>, String> {
    let bytes = reader.take(columns.div_ceil(8))?;
    Ok((0..columns)
        .map(|at| bytes.get(at / 8).is_some_and(|byte| byte & (1 << (at % 8)) != 0))
        .collect())
}

impl BinlogDecoder {
    /// A decoder for a log whose format description has not been read yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads one event (without the packet's leading OK byte).
    pub fn decode(&mut self, event: &[u8]) -> Result<(u32, BinlogEvent), String> {
        let mut header = Reader { bytes: event, at: 0 };
        header.take(4)?;
        let kind = header.byte()?;
        header.take(4)?;
        let size = header.little(4)? as usize;
        let next_position = header.little(4)? as u32;
        header.take(2)?;
        let end = size.min(event.len());
        let trimmed = if self.checksum && kind != FORMAT_DESCRIPTION {
            end.saturating_sub(CHECKSUM_BYTES)
        } else {
            end
        };
        let body = event.get(HEADER_BYTES..trimmed).ok_or_else(|| damaged("it is shorter than its header"))?;
        let mut reader = Reader { bytes: body, at: 0 };
        let decoded = match kind {
            FORMAT_DESCRIPTION => {
                reader.take(2 + 50 + 4)?;
                let header_length = usize::from(reader.byte()?);
                if header_length != HEADER_BYTES {
                    return Err(damaged("an event header that is not 19 bytes"));
                }
                let tail = reader.rest();
                self.checksum = tail
                    .len()
                    .checked_sub(1 + CHECKSUM_BYTES)
                    .and_then(|at| tail.get(at))
                    == Some(&1);
                BinlogEvent::Other
            }
            ROTATE => {
                let position = reader.little(8)?;
                BinlogEvent::Rotate { file: text_of(reader.rest()), position }
            }
            QUERY => {
                reader.take(4 + 4)?;
                let schema_length = usize::from(reader.byte()?);
                reader.take(2)?;
                let status_length = reader.little(2)? as usize;
                reader.take(status_length)?;
                let schema = text_of(reader.take(schema_length)?);
                reader.take(1)?;
                BinlogEvent::Query { schema, sql: text_of(reader.rest()) }
            }
            XID => BinlogEvent::Commit,
            TABLE_MAP => {
                let table_id = reader.little(6)?;
                reader.take(2)?;
                let schema_length = usize::from(reader.byte()?);
                let schema = text_of(reader.take(schema_length)?);
                reader.take(1)?;
                let table_length = usize::from(reader.byte()?);
                let table = text_of(reader.take(table_length)?);
                reader.take(1)?;
                let count = reader.packed()?;
                if count > MOST_COLUMNS {
                    return Err(damaged("too many columns"));
                }
                let kinds = reader.take(count as usize)?.to_vec();
                reader.packed()?;
                let mut columns = Vec::with_capacity(kinds.len());
                for kind in kinds {
                    columns.push((kind, metadata_of(kind, &mut reader)?));
                }
                self.tables.insert(table_id, TableMap { schema, table, columns });
                BinlogEvent::Other
            }
            WRITE_ROWS_V1 | UPDATE_ROWS_V1 | DELETE_ROWS_V1 | WRITE_ROWS | UPDATE_ROWS | DELETE_ROWS => {
                self.rows(kind, &mut reader)?
            }
            HEARTBEAT => BinlogEvent::Other,
            _ => BinlogEvent::Other,
        };
        Ok((next_position, decoded))
    }

    fn rows(&self, kind: u8, reader: &mut Reader<'_>) -> Result<BinlogEvent, String> {
        let table_id = reader.little(6)?;
        reader.take(2)?;
        if kind >= WRITE_ROWS {
            let extra = reader.little(2)? as usize;
            reader.take(extra.saturating_sub(2))?;
        }
        let map = self
            .tables
            .get(&table_id)
            .ok_or_else(|| damaged("a rows event for a table it never mapped"))?;
        let count = reader.packed()? as usize;
        if count != map.columns.len() {
            return Err(damaged("a rows event whose column count differs from its table map"));
        }
        let rows_kind = match kind {
            WRITE_ROWS_V1 | WRITE_ROWS => RowsKind::Write,
            UPDATE_ROWS_V1 | UPDATE_ROWS => RowsKind::Update,
            _ => RowsKind::Delete,
        };
        let present = bitmap(reader, count)?;
        let present_after = if rows_kind == RowsKind::Update { bitmap(reader, count)? } else { present.clone() };
        let mut rows = Vec::new();
        while !reader.rest().is_empty() {
            rows.push(Self::image(map, &present, reader)?);
            if rows_kind == RowsKind::Update {
                rows.push(Self::image(map, &present_after, reader)?);
            }
        }
        Ok(BinlogEvent::Rows {
            kind: rows_kind,
            schema: map.schema.clone(),
            table: map.table.clone(),
            rows,
        })
    }

    fn image(map: &TableMap, present: &[bool], reader: &mut Reader<'_>) -> Result<Vec<BinlogValue>, String> {
        let shown = present.iter().filter(|present| **present).count();
        let nulls = bitmap(reader, shown)?;
        let mut values = Vec::with_capacity(present.len());
        let mut shown_at = 0;
        for (&(kind, metadata), &is_present) in map.columns.iter().zip(present) {
            if !is_present {
                values.push(BinlogValue::Null);
                continue;
            }
            let is_null = nulls.get(shown_at).copied().unwrap_or(true);
            shown_at += 1;
            values.push(if is_null { BinlogValue::Null } else { value(reader, kind, metadata)? });
        }
        Ok(values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_dates_and_json_decode_as_mysql_writes_them() {
        let mut positive = Reader { bytes: &[0x81, 0x0D, 0xFB, 0x38, 0xD2, 0x04, 0xD2], at: 0 };
        assert_eq!(decimal(&mut positive, 14, 4).unwrap(), "1234567890.1234");
        let mut negative = Reader { bytes: &[0x7E, 0xF2, 0x04, 0xC7, 0x2D, 0xFB, 0x2D], at: 0 };
        assert_eq!(decimal(&mut negative, 14, 4).unwrap(), "-1234567890.1234");
        assert_eq!(civil_from_days(20_405), (2025, 11, 13));
        let mut date = Reader { bytes: &[0x6D, 0xD3, 0x0F], at: 0 };
        assert_eq!(value(&mut date, TYPE_DATE, 0).unwrap(), BinlogValue::Temporal("2025-11-13".to_owned()));
        assert_eq!(json_document(&[0x04, 0x01]).unwrap(), "true");
        assert_eq!(json_document(&[0x0C, 0x02, b'h', b'i']).unwrap(), "\"hi\"");
        let mut tiny = Reader { bytes: &[0xFF], at: 0 };
        assert_eq!(value(&mut tiny, TYPE_TINY, 0).unwrap(), BinlogValue::Integer { value: -1, bytes: 1 });
        assert!(BinlogDecoder::new().decode(&[1, 2, 3]).is_err());
    }
}
