use super::Session;
use crate::cli::args::CommandName;
use crate::cli::failure::{Exit, Failure};
use crate::cli::render;
use eggdb_parquet::{Cell, Column, ColumnType};
use std::path::Path;

const MAX_DBF_BYTES: u64 = 2 << 30;
const HEADER_LENGTH: usize = 32;
const DESCRIPTOR_LENGTH: usize = 32;
const NAME_LENGTH: usize = 11;
const DESCRIPTORS_END: u8 = 0x0D;
const DELETED: u8 = b'*';
const MAX_COLUMNS: usize = 1600;
const JULIAN_DAY_OF_1970: i64 = 2_440_588;
const MICROS_PER_DAY: i64 = 86_400_000_000;
const DBASE_MEMO_BLOCK: usize = 512;
const DBASE4_MEMO_START: [u8; 4] = [0xFF, 0xFF, 0x08, 0x00];
const DBASE4_MEMO_HEADER: usize = 8;
const MEMO_END: u8 = 0x1A;
const NULL_FLAGS_FIELD: u8 = b'0';
const VISUAL_FOXPRO_VERSIONS: [u8; 3] = [0x30, 0x31, 0x32];
const DBASE7_VERSION_BITS: u8 = 0x07;
const DBASE7_VERSION: u8 = 0x04;
const CURRENCY_PRECISION: u32 = 19;
const CURRENCY_SCALE: u32 = 4;
const MAX_INTEGER_DIGITS: usize = 18;
const MAX_DECIMAL_PRECISION: u32 = 38;
const ASCII_MEMO_POINTER: usize = 10;
const BINARY_MEMO_POINTER: usize = 4;
const MEMO_EXTENSIONS: [(&str, MemoStyle); 4] = [
    ("dbt", MemoStyle::DBase),
    ("DBT", MemoStyle::DBase),
    ("fpt", MemoStyle::FoxPro),
    ("FPT", MemoStyle::FoxPro),
];
const CP1252_HIGH: [char; 32] = [
    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}',
    '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum MemoStyle {
    DBase,
    FoxPro,
}

/// A `.dbt` or `.fpt` memo file beside a table.
pub(super) struct Memo {
    bytes: Vec<u8>,
    style: MemoStyle,
    block: usize,
}

impl Memo {
    fn new(bytes: Vec<u8>, style: MemoStyle) -> Self {
        let block = match style {
            MemoStyle::FoxPro => bytes
                .get(6..8)
                .map_or(0, |size| usize::from(u16::from_be_bytes([size[0], size[1]]))),
            MemoStyle::DBase => bytes
                .get(20..22)
                .map_or(0, |size| usize::from(u16::from_le_bytes([size[0], size[1]]))),
        };
        let block = match (style, block) {
            (MemoStyle::DBase, 0) => DBASE_MEMO_BLOCK,
            (MemoStyle::FoxPro, 0) => 1,
            (_, block) => block,
        };
        Self { bytes, style, block }
    }

    fn entry(&self, block: usize) -> Result<Vec<u8>, String> {
        let past_end = || "a memo points past the end of its memo file".to_owned();
        let start = block
            .checked_mul(self.block)
            .filter(|start| *start < self.bytes.len())
            .ok_or_else(past_end)?;
        let data = &self.bytes[start..];
        let length_at = |range: std::ops::Range<usize>, big_endian: bool| {
            data.get(range).map(|bytes| {
                let bytes = [bytes[0], bytes[1], bytes[2], bytes[3]];
                if big_endian {
                    u32::from_be_bytes(bytes) as usize
                } else {
                    u32::from_le_bytes(bytes) as usize
                }
            })
        };
        let found = match self.style {
            MemoStyle::FoxPro => length_at(4..8, true)
                .and_then(|length| data.get(8..8usize.saturating_add(length))),
            MemoStyle::DBase if data.starts_with(&DBASE4_MEMO_START) => length_at(4..8, false)
                .and_then(|length| data.get(DBASE4_MEMO_HEADER..length.max(DBASE4_MEMO_HEADER))),
            MemoStyle::DBase => {
                let end = data.iter().position(|byte| *byte == MEMO_END).unwrap_or(data.len());
                Some(&data[..end])
            }
        };
        found.map(<[u8]>::to_vec).ok_or_else(past_end)
    }
}

struct Field {
    name: String,
    kind: u8,
    offset: usize,
    length: usize,
    decimals: u32,
}

struct Layout {
    version: u8,
    records: usize,
    header_length: usize,
    record_length: usize,
    fields: Vec<Field>,
}

impl Layout {
    fn visual_foxpro(&self) -> bool {
        VISUAL_FOXPRO_VERSIONS.contains(&self.version)
    }

    fn dbase7(&self) -> bool {
        self.version & DBASE7_VERSION_BITS == DBASE7_VERSION
    }
}

fn u16_le(bytes: &[u8]) -> usize {
    usize::from(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn text_of(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => bytes
            .iter()
            .map(|&byte| match byte {
                0x80..=0x9F => CP1252_HIGH[usize::from(byte - 0x80)],
                other => char::from(other),
            })
            .collect(),
    }
}

fn trimmed(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .rposition(|byte| !matches!(byte, b' ' | 0))
        .map_or(0, |last| last + 1);
    &bytes[..end]
}

fn layout_of(file: &[u8]) -> Result<Layout, String> {
    let header = file
        .get(..HEADER_LENGTH)
        .ok_or("it is shorter than a dBase header")?;
    let version = header[0];
    let records = u32_le(&header[4..8]) as usize;
    let header_length = u16_le(&header[8..10]);
    let record_length = u16_le(&header[10..12]);
    let mut fields = Vec::new();
    let mut at = HEADER_LENGTH;
    let mut offset = 1;
    while let Some(&first) = file.get(at) {
        if first == DESCRIPTORS_END || at + DESCRIPTOR_LENGTH > header_length {
            break;
        }
        let descriptor = file
            .get(at..at + DESCRIPTOR_LENGTH)
            .ok_or("a field descriptor ends early")?;
        let name_end = descriptor[..NAME_LENGTH]
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(NAME_LENGTH);
        let kind = descriptor[11].to_ascii_uppercase();
        let (length, decimals) = if kind == b'C' {
            (usize::from(descriptor[16]) + usize::from(descriptor[17]) * 256, 0)
        } else {
            (usize::from(descriptor[16]), u32::from(descriptor[17]))
        };
        fields.push(Field {
            name: text_of(&descriptor[..name_end]).trim().to_ascii_lowercase(),
            kind,
            offset,
            length,
            decimals,
        });
        offset += length;
        at += DESCRIPTOR_LENGTH;
    }
    if fields.is_empty() {
        return Err("it declares no fields".to_owned());
    }
    if offset > record_length {
        return Err("its fields are longer than its records".to_owned());
    }
    let end = records
        .checked_mul(record_length)
        .and_then(|size| size.checked_add(header_length));
    if end.is_none_or(|end| end > file.len()) {
        return Err("it ends before its last record".to_owned());
    }
    Ok(Layout {
        version,
        records,
        header_length,
        record_length,
        fields,
    })
}

fn column_type(field: &Field, layout: &Layout) -> Option<ColumnType> {
    Some(match field.kind {
        NULL_FLAGS_FIELD => return None,
        b'N' | b'F' if field.decimals == 0 && field.length <= MAX_INTEGER_DIGITS => ColumnType::Int64,
        b'N' | b'F' => ColumnType::Decimal {
            precision: (field.length as u32).clamp(field.decimals.max(1), MAX_DECIMAL_PRECISION),
            scale: field.decimals.min(MAX_DECIMAL_PRECISION),
        },
        b'D' => ColumnType::Date,
        b'L' => ColumnType::Boolean,
        b'I' | b'+' => ColumnType::Int64,
        b'B' if layout.visual_foxpro() => ColumnType::Double,
        b'O' if layout.dbase7() => ColumnType::Double,
        b'Y' => ColumnType::Decimal {
            precision: CURRENCY_PRECISION,
            scale: CURRENCY_SCALE,
        },
        b'T' => ColumnType::Timestamp { utc: false },
        b'G' | b'P' | b'W' | b'Q' | b'B' => ColumnType::Binary,
        _ => ColumnType::Text,
    })
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn date_of(text: &str) -> Option<i64> {
    if text.len() != 8 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let year: i64 = text[..4].parse().ok()?;
    let month: i64 = text[4..6].parse().ok()?;
    let day: i64 = text[6..].parse().ok()?;
    ((1..=12).contains(&month) && (1..=31).contains(&day) && year > 0)
        .then(|| days_from_civil(year, month, day))
}

fn decimal_of(text: &str, scale: u32) -> Option<i128> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    if !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || whole.len() + fraction.len() == 0
    {
        return None;
    }
    let scale = scale as usize;
    let mut fraction: String = fraction.chars().take(scale).collect();
    while fraction.len() < scale {
        fraction.push('0');
    }
    let joined = format!("{whole}{fraction}");
    let value: i128 = if joined.is_empty() { 0 } else { joined.parse().ok()? };
    Some(if negative { -value } else { value })
}

fn dbase7_bits(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0u64, |held, byte| (held << 8) | u64::from(*byte))
}

fn dbase7_integer(bytes: &[u8]) -> i64 {
    i64::from((dbase7_bits(bytes) as u32 ^ 0x8000_0000) as i32)
}

fn dbase7_double(bytes: &[u8]) -> f64 {
    let bits = dbase7_bits(bytes);
    if bits & (1 << 63) != 0 {
        f64::from_bits(bits & !(1 << 63))
    } else {
        f64::from_bits(!bits)
    }
}

fn memo_pointer(raw: &[u8]) -> Option<usize> {
    match raw.len() {
        BINARY_MEMO_POINTER => Some(u32_le(raw) as usize).filter(|block| *block != 0),
        _ => std::str::from_utf8(raw)
            .ok()?
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|block| *block != 0),
    }
}

fn memo_cell(raw: &[u8], memo: Option<&Memo>, as_text: bool) -> Result<Cell, String> {
    if !matches!(raw.len(), ASCII_MEMO_POINTER | BINARY_MEMO_POINTER) {
        return Ok(Cell::Bytes(raw.to_vec()));
    }
    let Some(block) = memo_pointer(raw) else {
        return Ok(Cell::Null);
    };
    let memo = memo.ok_or("it has memo values but no .dbt or .fpt file beside it")?;
    let entry = memo.entry(block)?;
    Ok(Cell::Bytes(if as_text {
        text_of(&entry).into_bytes()
    } else {
        entry
    }))
}

fn cell_of(field: &Field, column_type: ColumnType, raw: &[u8], layout: &Layout, memo: Option<&Memo>) -> Result<Cell, String> {
    let text = || text_of(trimmed(raw)).trim().to_owned();
    let not_a = |what: &str, value: &str| format!("field {} holds {value:?}, not {what}", field.name);
    Ok(match (field.kind, column_type) {
        (b'M', _) => memo_cell(raw, memo, true)?,
        (b'G' | b'P' | b'W', _) | (b'B', ColumnType::Binary) => memo_cell(raw, memo, false)?,
        (b'N' | b'F', ColumnType::Int64) => {
            let value = text();
            if value.is_empty() || value.starts_with('*') {
                Cell::Null
            } else {
                Cell::Int(value.parse().map_err(|_| not_a("a number", &value))?)
            }
        }
        (b'N' | b'F', ColumnType::Decimal { scale, .. }) => {
            let value = text();
            if value.is_empty() || value.starts_with('*') {
                Cell::Null
            } else {
                Cell::Decimal(decimal_of(&value, scale).ok_or_else(|| not_a("a number", &value))?)
            }
        }
        (b'D', _) => {
            let value = text();
            if value.is_empty() || value.bytes().all(|byte| byte == b'0') {
                Cell::Null
            } else {
                Cell::Int(date_of(&value).ok_or_else(|| not_a("a date", &value))?)
            }
        }
        (b'L', _) => match raw.first() {
            Some(b'T' | b't' | b'Y' | b'y') => Cell::Boolean(true),
            Some(b'F' | b'f' | b'N' | b'n') => Cell::Boolean(false),
            _ => Cell::Null,
        },
        (b'I' | b'+', _) if raw.len() == 4 => {
            if layout.dbase7() {
                Cell::Int(dbase7_integer(raw))
            } else {
                Cell::Int(i64::from(u32_le(raw) as i32))
            }
        }
        (b'B', ColumnType::Double) if raw.len() == 8 => {
            Cell::Float(f64::from_le_bytes(raw.try_into().map_err(|_| not_a("a double", ""))?))
        }
        (b'O', ColumnType::Double) if raw.len() == 8 => Cell::Float(dbase7_double(raw)),
        (b'Y', _) if raw.len() == 8 => Cell::Decimal(i128::from(i64::from_le_bytes(
            raw.try_into().map_err(|_| not_a("a currency", ""))?,
        ))),
        (b'T', _) if raw.len() == 8 => {
            let day = i64::from(u32_le(&raw[..4]));
            let millis = i64::from(u32_le(&raw[4..]));
            if day == 0 && millis == 0 {
                Cell::Null
            } else {
                Cell::Int((day - JULIAN_DAY_OF_1970) * MICROS_PER_DAY + millis * 1000)
            }
        }
        (_, ColumnType::Binary) => Cell::Bytes(raw.to_vec()),
        (_, ColumnType::Text) => Cell::Bytes(text_of(trimmed(raw)).into_bytes()),
        _ => Cell::Null,
    })
}

/// The live records of a dBase table (dBase III to 7, FoxPro and Visual FoxPro `.dbf`), one
/// column per field with names lowercased: character fields as text (UTF-8, else Windows-1252),
/// numbers as integers or decimals, dates, logicals, Visual FoxPro integers, doubles, currency and
/// datetimes, and memo fields read from `memo`. Deleted records are skipped.
pub(super) fn columns_of(file: &[u8], memo: Option<&Memo>) -> Result<Vec<Column>, String> {
    let layout = layout_of(file)?;
    let kept: Vec<(&Field, ColumnType)> = layout
        .fields
        .iter()
        .filter_map(|field| column_type(field, &layout).map(|kind| (field, kind)))
        .collect();
    if kept.len() > MAX_COLUMNS {
        return Err(format!("it has {} fields; a table holds at most {MAX_COLUMNS}", kept.len()));
    }
    let mut columns: Vec<Column> = kept
        .iter()
        .map(|(field, column_type)| Column {
            name: field.name.clone(),
            column_type: *column_type,
            nullable: true,
            cells: Vec::new(),
        })
        .collect();
    for record in 0..layout.records {
        let start = layout.header_length + record * layout.record_length;
        let bytes = &file[start..start + layout.record_length];
        if bytes[0] == DELETED {
            continue;
        }
        for ((field, column_type), column) in kept.iter().zip(columns.iter_mut()) {
            let raw = &bytes[field.offset..field.offset + field.length];
            let cell = cell_of(field, *column_type, raw, &layout, memo)
                .map_err(|error| format!("record {}: {error}", record + 1))?;
            column.cells.push(cell);
        }
    }
    Ok(columns)
}

fn memo_beside(source: &Path) -> Result<Option<Memo>, String> {
    for (extension, style) in MEMO_EXTENSIONS {
        let path = source.with_extension(extension);
        if path.is_file() {
            let bytes = std::fs::read(&path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            return Ok(Some(Memo::new(bytes, style)));
        }
    }
    Ok(None)
}

impl Session<'_> {
    pub(super) fn import_dbf(&mut self, source: &Path) -> Result<Exit, Failure> {
        let usage = |message: String| Failure::usage(message, Some(CommandName::Import));
        let shown = render::text(source.display().to_string().as_bytes());
        let table = self.request.csv.table.clone().unwrap_or_default();
        let size = std::fs::metadata(source)
            .map_err(|error| usage(format!("cannot read {shown}: {error}")))?
            .len();
        if size > MAX_DBF_BYTES {
            return Err(usage(format!(
                "{shown} is larger than 2 GiB, the most eggdb import --dbf reads at once"
            )));
        }
        let file = std::fs::read(source)
            .map_err(|error| usage(format!("cannot read {shown}: {error}")))?;
        let failed = |error: String| {
            Failure::database(format!("cannot import {shown}: {error}"), Exit::DatabaseError)
        };
        let memo = memo_beside(source).map_err(failed)?;
        let columns = columns_of(&file, memo.as_ref()).map_err(failed)?;
        self.import_columns(&shown, &table, &columns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(fields: &[(&str, u8, u8, u8)], records: &[Vec<u8>]) -> Vec<u8> {
        let record_length = 1 + fields.iter().map(|field| usize::from(field.2)).sum::<usize>();
        let header_length = HEADER_LENGTH + DESCRIPTOR_LENGTH * fields.len() + 1;
        let mut file = vec![0u8; HEADER_LENGTH];
        file[0] = 0x03;
        file[4..8].copy_from_slice(&(records.len() as u32).to_le_bytes());
        file[8..10].copy_from_slice(&(header_length as u16).to_le_bytes());
        file[10..12].copy_from_slice(&(record_length as u16).to_le_bytes());
        for (name, kind, length, decimals) in fields {
            let mut descriptor = [0u8; DESCRIPTOR_LENGTH];
            descriptor[..name.len()].copy_from_slice(name.as_bytes());
            descriptor[11] = *kind;
            descriptor[16] = *length;
            descriptor[17] = *decimals;
            file.extend_from_slice(&descriptor);
        }
        file.push(DESCRIPTORS_END);
        for record in records {
            file.extend_from_slice(record);
        }
        file.push(MEMO_END);
        file
    }

    #[test]
    fn dbase3_records_become_columns() {
        let file = table(
            &[("NAME", b'C', 5, 0), ("QTY", b'N', 4, 0), ("PRICE", b'N', 6, 2), ("DAY", b'D', 8, 0), ("OK", b'L', 1, 0)],
            &[
                &[&b" "[..], b"caf\xe9 ", b"  12", b"  1.50", b"20240102", b"T"].concat(),
                &[&b"*"[..], b"gone ", b"   1", b"  0.00", b"19700101", b"F"].concat(),
                &[&b" "[..], b"x    ", b"   *", b"      ", b"00000000", b"?"].concat(),
            ],
        );
        let columns = columns_of(&file, None).unwrap();
        let names: Vec<&str> = columns.iter().map(|column| column.name.as_str()).collect();
        assert_eq!(names, vec!["name", "qty", "price", "day", "ok"]);
        assert_eq!(columns[0].cells, vec![Cell::Bytes("café".as_bytes().to_vec()), Cell::Bytes(b"x".to_vec())]);
        assert_eq!(columns[1].cells, vec![Cell::Int(12), Cell::Null]);
        assert_eq!(columns[2].cells, vec![Cell::Decimal(150), Cell::Null]);
        assert_eq!(columns[3].cells, vec![Cell::Int(19_724), Cell::Null]);
        assert_eq!(columns[4].cells, vec![Cell::Boolean(true), Cell::Null]);
    }

    #[test]
    fn dbase7_numbers_and_dates_decode() {
        assert_eq!(dbase7_integer(&[0x80, 0, 0, 5]), 5);
        assert_eq!(dbase7_integer(&[0x7F, 0xFF, 0xFF, 0xFF]), -1);
        assert_eq!(dbase7_double(&(1.5f64.to_bits() | (1 << 63)).to_be_bytes()), 1.5);
        assert_eq!(dbase7_double(&(!(-2.0f64).to_bits()).to_be_bytes()), -2.0);
        assert_eq!(date_of("19700101"), Some(0));
        assert_eq!(decimal_of("-0.5", 2), Some(-50));
    }
}
