use super::Session;
use super::ndjson_io::json_string;
use super::parquet_io::{date_text, decimal_text, timestamp_text};
use crate::cli::args::CommandName;
use crate::cli::failure::{Exit, Failure};
use crate::cli::render;
use eggdb_content::{crc32, inflate};
use eggdb_parquet::{Cell, Column, ColumnType};
use eggdb_sql::json::{Json, parse_document};
use std::path::Path;

const MAGIC: &[u8] = b"Obj\x01";
const SYNC_LENGTH: usize = 16;
const MAX_AVRO_BYTES: u64 = 2 << 30;
const MAX_INFLATED_BYTES: usize = 2 << 30;
const MAX_VALUES: usize = 100_000_000;
const MAX_SCHEMA_DEPTH: usize = 64;
const MAX_COLUMNS: usize = 1600;
const MAX_DECIMAL_BYTES: usize = 16;
const MAX_DECIMAL_PRECISION: u32 = 38;
const MAX_VARINT_BYTES: usize = 10;
const ROWS_PER_BLOCK: usize = 10_000;
const SCHEMA_KEY: &str = "avro.schema";
const CODEC_KEY: &str = "avro.codec";

#[derive(Debug, Clone, PartialEq)]
enum Schema {
    Null,
    Boolean,
    Int,
    Long,
    Float,
    Double,
    Bytes,
    Text,
    Date,
    Timestamp { millis: bool, utc: bool },
    Decimal { precision: u32, scale: u32, size: Option<usize> },
    Fixed(usize),
    Enum(Vec<String>),
    Array(Box<Schema>),
    Map(Box<Schema>),
    Record(Vec<(String, Schema)>),
    Union(Vec<Schema>),
}

fn member<'j>(members: &'j [(String, eggdb_sql::json::Node)], name: &str) -> Option<&'j Json> {
    members
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, node)| &node.value)
}

fn number_member(members: &[(String, eggdb_sql::json::Node)], name: &str) -> Option<u64> {
    match member(members, name)? {
        Json::Number(text) => text.parse().ok(),
        _ => None,
    }
}

fn primitive(name: &str) -> Option<Schema> {
    Some(match name {
        "null" => Schema::Null,
        "boolean" => Schema::Boolean,
        "int" => Schema::Int,
        "long" => Schema::Long,
        "float" => Schema::Float,
        "double" => Schema::Double,
        "bytes" => Schema::Bytes,
        "string" => Schema::Text,
        _ => return None,
    })
}

fn short_name(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

struct Names(Vec<(String, Schema)>);

impl Names {
    fn define(&mut self, members: &[(String, eggdb_sql::json::Node)], schema: &Schema) {
        if let Some(Json::String(name)) = member(members, "name") {
            self.0.push((short_name(name).to_owned(), schema.clone()));
        }
    }

    fn find(&self, name: &str) -> Option<Schema> {
        let wanted = short_name(name);
        self.0
            .iter()
            .rev()
            .find(|(known, _)| known == wanted)
            .map(|(_, schema)| schema.clone())
    }
}

fn logical(members: &[(String, eggdb_sql::json::Node)], base: Schema) -> Schema {
    let Some(Json::String(kind)) = member(members, "logicalType") else {
        return base;
    };
    match (kind.as_str(), &base) {
        ("date", Schema::Int) => Schema::Date,
        ("timestamp-millis", Schema::Long) => Schema::Timestamp { millis: true, utc: true },
        ("timestamp-micros", Schema::Long) => Schema::Timestamp { millis: false, utc: true },
        ("local-timestamp-millis", Schema::Long) => Schema::Timestamp { millis: true, utc: false },
        ("local-timestamp-micros", Schema::Long) => Schema::Timestamp { millis: false, utc: false },
        ("decimal", Schema::Bytes | Schema::Fixed(_)) => {
            let precision = number_member(members, "precision").unwrap_or(0) as u32;
            let scale = number_member(members, "scale").unwrap_or(0) as u32;
            if precision == 0 || precision > MAX_DECIMAL_PRECISION || scale > precision {
                return base;
            }
            let size = match base {
                Schema::Fixed(size) => Some(size),
                _ => None,
            };
            Schema::Decimal { precision, scale, size }
        }
        _ => base,
    }
}

fn schema_of(json: &Json, names: &mut Names, depth: usize) -> Result<Schema, String> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err("its schema nests too deeply".to_owned());
    }
    match json {
        Json::String(name) => primitive(name)
            .or_else(|| names.find(name))
            .ok_or_else(|| format!("its schema names an unknown type {name:?}")),
        Json::Array(branches) => branches
            .iter()
            .map(|branch| schema_of(&branch.value, names, depth + 1))
            .collect::<Result<Vec<_>, _>>()
            .map(Schema::Union),
        Json::Object(members) => {
            let kind = member(members, "type").ok_or("a schema object has no \"type\"")?;
            let Json::String(kind_name) = kind else {
                return schema_of(kind, names, depth + 1);
            };
            let schema = match kind_name.as_str() {
                "record" | "error" => {
                    let Some(Json::Array(fields)) = member(members, "fields") else {
                        return Err("a record schema has no fields".to_owned());
                    };
                    let mut found = Vec::with_capacity(fields.len());
                    for field in fields {
                        let Json::Object(field) = &field.value else {
                            return Err("a record field is not an object".to_owned());
                        };
                        let Some(Json::String(name)) = member(field, "name") else {
                            return Err("a record field has no name".to_owned());
                        };
                        let kind = member(field, "type").ok_or("a record field has no type")?;
                        found.push((name.clone(), schema_of(kind, names, depth + 1)?));
                    }
                    Schema::Record(found)
                }
                "enum" => {
                    let Some(Json::Array(symbols)) = member(members, "symbols") else {
                        return Err("an enum schema has no symbols".to_owned());
                    };
                    Schema::Enum(
                        symbols
                            .iter()
                            .map(|symbol| match &symbol.value {
                                Json::String(text) => text.clone(),
                                _ => String::new(),
                            })
                            .collect(),
                    )
                }
                "array" => Schema::Array(Box::new(schema_of(
                    member(members, "items").ok_or("an array schema has no items")?,
                    names,
                    depth + 1,
                )?)),
                "map" => Schema::Map(Box::new(schema_of(
                    member(members, "values").ok_or("a map schema has no values")?,
                    names,
                    depth + 1,
                )?)),
                "fixed" => Schema::Fixed(
                    number_member(members, "size").ok_or("a fixed schema has no size")? as usize,
                ),
                other => primitive(other)
                    .or_else(|| names.find(other))
                    .ok_or_else(|| format!("its schema names an unknown type {other:?}"))?,
            };
            let schema = logical(members, schema);
            if matches!(kind_name.as_str(), "record" | "error" | "enum" | "fixed") {
                names.define(members, &schema);
            }
            Ok(schema)
        }
        _ => Err("its schema is not a JSON type".to_owned()),
    }
}

struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Reader<'b> {
    fn take(&mut self, length: usize) -> Result<&'b [u8], String> {
        let end = self
            .at
            .checked_add(length)
            .filter(|end| *end <= self.bytes.len())
            .ok_or("a value runs past the end of its block")?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn long(&mut self) -> Result<i64, String> {
        let mut value = 0u64;
        for index in 0..MAX_VARINT_BYTES {
            let byte = *self.take(1)?.first().unwrap_or(&0);
            value |= u64::from(byte & 0x7F) << (7 * index);
            if byte & 0x80 == 0 {
                return Ok(((value >> 1) as i64) ^ -((value & 1) as i64));
            }
        }
        Err("a variable-length integer is too long".to_owned())
    }

    fn length(&mut self) -> Result<usize, String> {
        usize::try_from(self.long()?).map_err(|_| "a length is negative".to_owned())
    }

    fn sized(&mut self) -> Result<&'b [u8], String> {
        let length = self.length()?;
        self.take(length)
    }

    fn block_count(&mut self) -> Result<usize, String> {
        let count = self.long()?;
        if count < 0 {
            self.long()?;
        }
        usize::try_from(count.unsigned_abs()).map_err(|_| "a block count is too large".to_owned())
    }
}

fn decimal_of(bytes: &[u8]) -> Result<i128, String> {
    if bytes.len() > MAX_DECIMAL_BYTES {
        return Err("a decimal is wider than 16 bytes".to_owned());
    }
    let negative = bytes.first().is_some_and(|byte| byte & 0x80 != 0);
    let mut value: i128 = if negative { -1 } else { 0 };
    for byte in bytes {
        value = (value << 8) | i128::from(*byte);
    }
    Ok(value)
}

fn text_of(bytes: &[u8]) -> Result<String, String> {
    String::from_utf8(bytes.to_vec()).map_err(|_| "a string is not UTF-8".to_owned())
}

fn micros_of(value: i64, millis: bool) -> i64 {
    if millis { value.saturating_mul(1000) } else { value }
}

fn decimal_bytes<'b>(reader: &mut Reader<'b>, size: Option<usize>) -> Result<&'b [u8], String> {
    match size {
        Some(size) => reader.take(size),
        None => reader.sized(),
    }
}

fn json_value(reader: &mut Reader<'_>, schema: &Schema, out: &mut String) -> Result<(), String> {
    match schema {
        Schema::Null => out.push_str("null"),
        Schema::Boolean => out.push_str(if reader.take(1)?[0] != 0 { "true" } else { "false" }),
        Schema::Int | Schema::Long => out.push_str(&reader.long()?.to_string()),
        Schema::Float | Schema::Double => {
            let value = float_of(reader, schema)?;
            if value.is_finite() {
                out.push_str(&value.to_string());
            } else {
                out.push_str(&json_string(&value.to_string()));
            }
        }
        Schema::Text => out.push_str(&json_string(&text_of(reader.sized()?)?)),
        Schema::Bytes | Schema::Fixed(_) => {
            let bytes = match schema {
                Schema::Fixed(size) => reader.take(*size)?,
                _ => reader.sized()?,
            };
            out.push_str(&json_string(&format!(
                "\\x{}",
                bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>()
            )));
        }
        Schema::Enum(symbols) => {
            let index = reader.length()?;
            let symbol = symbols.get(index).ok_or("an enum index is out of range")?;
            out.push_str(&json_string(symbol));
        }
        Schema::Date => out.push_str(&json_string(&date_text(reader.long()?))),
        Schema::Timestamp { millis, utc } => {
            out.push_str(&json_string(&timestamp_text(micros_of(reader.long()?, *millis), *utc)));
        }
        Schema::Decimal { scale, size, .. } => {
            let value = decimal_of(decimal_bytes(reader, *size)?)?;
            out.push_str(&decimal_text(value, *scale));
        }
        Schema::Array(items) => {
            out.push('[');
            let mut first = true;
            loop {
                let count = reader.block_count()?;
                if count == 0 {
                    break;
                }
                for _ in 0..count {
                    if !first {
                        out.push(',');
                    }
                    first = false;
                    json_value(reader, items, out)?;
                }
            }
            out.push(']');
        }
        Schema::Map(values) => {
            out.push('{');
            let mut first = true;
            loop {
                let count = reader.block_count()?;
                if count == 0 {
                    break;
                }
                for _ in 0..count {
                    if !first {
                        out.push(',');
                    }
                    first = false;
                    out.push_str(&json_string(&text_of(reader.sized()?)?));
                    out.push(':');
                    json_value(reader, values, out)?;
                }
            }
            out.push('}');
        }
        Schema::Record(fields) => {
            out.push('{');
            for (index, (name, field)) in fields.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&json_string(name));
                out.push(':');
                json_value(reader, field, out)?;
            }
            out.push('}');
        }
        Schema::Union(branches) => {
            let branch = branches.get(reader.length()?).ok_or("a union index is out of range")?;
            json_value(reader, branch, out)?;
        }
    }
    Ok(())
}

fn float_of(reader: &mut Reader<'_>, schema: &Schema) -> Result<f64, String> {
    Ok(match schema {
        Schema::Float => {
            let bytes = reader.take(4)?;
            f64::from(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        }
        _ => {
            let bytes = reader.take(8)?;
            let mut array = [0u8; 8];
            array.copy_from_slice(bytes);
            f64::from_le_bytes(array)
        }
    })
}

fn plain(schema: &Schema) -> Option<&Schema> {
    match schema {
        Schema::Union(branches) => {
            let mut kept = branches.iter().filter(|branch| **branch != Schema::Null);
            let only = kept.next()?;
            (kept.next().is_none()).then_some(only).and_then(plain)
        }
        Schema::Array(_) | Schema::Map(_) | Schema::Record(_) | Schema::Null => None,
        other => Some(other),
    }
}

fn column_type_of(schema: &Schema) -> ColumnType {
    match plain(schema) {
        Some(Schema::Boolean) => ColumnType::Boolean,
        Some(Schema::Int | Schema::Long) => ColumnType::Int64,
        Some(Schema::Float | Schema::Double) => ColumnType::Double,
        Some(Schema::Bytes | Schema::Fixed(_)) => ColumnType::Binary,
        Some(Schema::Date) => ColumnType::Date,
        Some(Schema::Timestamp { utc, .. }) => ColumnType::Timestamp { utc: *utc },
        Some(Schema::Decimal { precision, scale, .. }) => ColumnType::Decimal {
            precision: *precision,
            scale: *scale,
        },
        _ => ColumnType::Text,
    }
}

fn cell_of(reader: &mut Reader<'_>, schema: &Schema) -> Result<Cell, String> {
    if plain(schema).is_none() {
        let mut out = String::new();
        json_value(reader, schema, &mut out)?;
        return Ok(if out == "null" { Cell::Null } else { Cell::Bytes(out.into_bytes()) });
    }
    Ok(match schema {
        Schema::Union(branches) => {
            let branch = branches.get(reader.length()?).ok_or("a union index is out of range")?;
            return cell_of(reader, branch);
        }
        Schema::Null => Cell::Null,
        Schema::Boolean => Cell::Boolean(reader.take(1)?[0] != 0),
        Schema::Int | Schema::Long | Schema::Date => Cell::Int(reader.long()?),
        Schema::Timestamp { millis, .. } => Cell::Int(micros_of(reader.long()?, *millis)),
        Schema::Float | Schema::Double => Cell::Float(float_of(reader, schema)?),
        Schema::Text => Cell::Bytes(text_of(reader.sized()?)?.into_bytes()),
        Schema::Bytes => Cell::Bytes(reader.sized()?.to_vec()),
        Schema::Fixed(size) => Cell::Bytes(reader.take(*size)?.to_vec()),
        Schema::Enum(symbols) => {
            let index = reader.length()?;
            Cell::Bytes(symbols.get(index).ok_or("an enum index is out of range")?.clone().into_bytes())
        }
        Schema::Decimal { size, .. } => Cell::Decimal(decimal_of(decimal_bytes(reader, *size)?)?),
        Schema::Array(_) | Schema::Map(_) | Schema::Record(_) => Cell::Null,
    })
}

fn metadata(reader: &mut Reader<'_>) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut found = Vec::new();
    loop {
        let count = reader.block_count()?;
        if count == 0 {
            return Ok(found);
        }
        for _ in 0..count {
            let key = text_of(reader.sized()?)?;
            found.push((key, reader.sized()?.to_vec()));
        }
    }
}

/// The records of an Avro object container file as table columns, one per field of its record
/// schema: primitives, enums, dates, timestamps and decimals as their own types, a union of null
/// and one type as that type, and records, arrays, maps and wider unions as JSON text. Reads the
/// `null` and `deflate` codecs.
pub(super) fn columns_of(file: &[u8]) -> Result<Vec<Column>, String> {
    if !file.starts_with(MAGIC) {
        return Err("it is not an Avro object container file".to_owned());
    }
    let mut reader = Reader {
        bytes: file,
        at: MAGIC.len(),
    };
    let entries = metadata(&mut reader)?;
    let sync = reader.take(SYNC_LENGTH)?.to_vec();
    let find = |key: &str| entries.iter().find(|(name, _)| name == key).map(|(_, value)| value);
    let schema_text = find(SCHEMA_KEY).ok_or("it has no avro.schema")?;
    let schema_text = std::str::from_utf8(schema_text).map_err(|_| "its schema is not UTF-8")?;
    let document = parse_document(schema_text).map_err(|error| format!("its schema: {}", error.message))?;
    let schema = schema_of(&document.value, &mut Names(Vec::new()), 0)?;
    let Schema::Record(fields) = schema else {
        return Err("its schema is not a record, so it has no columns".to_owned());
    };
    if fields.len() > MAX_COLUMNS {
        return Err(format!("it has {} fields; a table holds at most {MAX_COLUMNS}", fields.len()));
    }
    let deflated = match find(CODEC_KEY).map(|codec| String::from_utf8_lossy(codec).into_owned()) {
        None => false,
        Some(codec) if codec == "null" => false,
        Some(codec) if codec == "deflate" => true,
        Some(codec) => return Err(format!("its codec {codec:?} is not supported; null and deflate are")),
    };
    let mut columns: Vec<Column> = fields
        .iter()
        .map(|(name, field)| Column {
            name: name.clone(),
            column_type: column_type_of(field),
            nullable: true,
            cells: Vec::new(),
        })
        .collect();
    let mut inflated_left = MAX_INFLATED_BYTES;
    let mut values = 0usize;
    while reader.at < file.len() {
        let count = reader.length()?;
        let data = reader.sized()?;
        if reader.take(SYNC_LENGTH)? != sync.as_slice() {
            return Err("a block's sync marker does not match the header's".to_owned());
        }
        values = values.saturating_add(count.saturating_mul(fields.len().max(1)));
        if values > MAX_VALUES {
            return Err(format!("it holds more than {MAX_VALUES} values, the most one import reads"));
        }
        let owned;
        let data = if deflated {
            owned = inflate(data, inflated_left).map_err(|error| error.0)?;
            inflated_left = inflated_left.saturating_sub(owned.len());
            owned.as_slice()
        } else {
            data
        };
        let mut block = Reader { bytes: data, at: 0 };
        for _ in 0..count {
            for ((_, field), column) in fields.iter().zip(columns.iter_mut()) {
                column.cells.push(cell_of(&mut block, field)?);
            }
        }
    }
    Ok(columns)
}

fn avro_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|character| if character.is_ascii_alphanumeric() { character } else { '_' })
        .collect();
    if out.is_empty() || out.starts_with(|character: char| character.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

fn field_schema(column_type: ColumnType) -> String {
    let base = match column_type {
        ColumnType::Boolean => "\"boolean\"".to_owned(),
        ColumnType::Int32 => "\"int\"".to_owned(),
        ColumnType::Int64 => "\"long\"".to_owned(),
        ColumnType::Float => "\"float\"".to_owned(),
        ColumnType::Double => "\"double\"".to_owned(),
        ColumnType::Text => "\"string\"".to_owned(),
        ColumnType::Binary => "\"bytes\"".to_owned(),
        ColumnType::Date => "{\"type\":\"int\",\"logicalType\":\"date\"}".to_owned(),
        ColumnType::Timestamp { utc: true } => {
            "{\"type\":\"long\",\"logicalType\":\"timestamp-micros\"}".to_owned()
        }
        ColumnType::Timestamp { utc: false } => {
            "{\"type\":\"long\",\"logicalType\":\"local-timestamp-micros\"}".to_owned()
        }
        ColumnType::Decimal { precision, scale } => format!(
            "{{\"type\":\"bytes\",\"logicalType\":\"decimal\",\"precision\":{precision},\"scale\":{scale}}}"
        ),
    };
    format!("[\"null\",{base}]")
}

fn put_long(out: &mut Vec<u8>, value: i64) {
    let mut zigzag = ((value << 1) ^ (value >> 63)) as u64;
    loop {
        let byte = (zigzag & 0x7F) as u8;
        zigzag >>= 7;
        if zigzag == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn put_sized(out: &mut Vec<u8>, bytes: &[u8]) {
    put_long(out, bytes.len() as i64);
    out.extend_from_slice(bytes);
}

fn decimal_bytes_of(value: i128) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let mut start = 0;
    while start < MAX_DECIMAL_BYTES - 1
        && ((bytes[start] == 0 && bytes[start + 1] & 0x80 == 0)
            || (bytes[start] == 0xFF && bytes[start + 1] & 0x80 != 0))
    {
        start += 1;
    }
    bytes[start..].to_vec()
}

fn put_cell(out: &mut Vec<u8>, column_type: ColumnType, cell: &Cell) {
    if *cell == Cell::Null {
        put_long(out, 0);
        return;
    }
    put_long(out, 1);
    match (column_type, cell) {
        (_, Cell::Boolean(flag)) => out.push(u8::from(*flag)),
        (_, Cell::Int(value)) => put_long(out, *value),
        (ColumnType::Float, Cell::Float(value)) => out.extend_from_slice(&(*value as f32).to_le_bytes()),
        (_, Cell::Float(value)) => out.extend_from_slice(&value.to_le_bytes()),
        (_, Cell::Bytes(bytes)) => put_sized(out, bytes),
        (_, Cell::Decimal(value)) => put_sized(out, &decimal_bytes_of(*value)),
        (_, Cell::Null) => {}
    }
}

/// An Avro object container file (uncompressed) of `columns`, its record schema named after
/// `table`, every field a union of null and the column's type.
pub(super) fn avro_of(table: &str, columns: &[Column]) -> Vec<u8> {
    let mut used: Vec<String> = Vec::new();
    let mut fields = Vec::with_capacity(columns.len());
    for column in columns {
        let base = avro_name(&column.name);
        let mut name = base.clone();
        let mut suffix = 2;
        while used.contains(&name) {
            name = format!("{base}_{suffix}");
            suffix += 1;
        }
        used.push(name.clone());
        fields.push(format!(
            "{{\"name\":{},\"type\":{},\"default\":null}}",
            json_string(&name),
            field_schema(column.column_type)
        ));
    }
    let schema = format!(
        "{{\"type\":\"record\",\"name\":{},\"fields\":[{}]}}",
        json_string(&avro_name(table)),
        fields.join(",")
    );
    let mut sync = Vec::with_capacity(SYNC_LENGTH);
    for round in 0u8..4 {
        let mut seed = schema.as_bytes().to_vec();
        seed.push(round);
        sync.extend_from_slice(&crc32(&seed).to_le_bytes());
    }
    let mut out = MAGIC.to_vec();
    put_long(&mut out, 2);
    put_sized(&mut out, SCHEMA_KEY.as_bytes());
    put_sized(&mut out, schema.as_bytes());
    put_sized(&mut out, CODEC_KEY.as_bytes());
    put_sized(&mut out, b"null");
    put_long(&mut out, 0);
    out.extend_from_slice(&sync);
    let rows = columns.first().map_or(0, |column| column.cells.len());
    let mut start = 0;
    while start < rows {
        let end = (start + ROWS_PER_BLOCK).min(rows);
        let mut block = Vec::new();
        for row in start..end {
            for column in columns {
                put_cell(&mut block, column.column_type, &column.cells[row]);
            }
        }
        put_long(&mut out, (end - start) as i64);
        put_sized(&mut out, &block);
        out.extend_from_slice(&sync);
        start = end;
    }
    out
}

impl Session<'_> {
    pub(super) fn import_avro(&mut self, source: &Path) -> Result<Exit, Failure> {
        let usage = |message: String| Failure::usage(message, Some(CommandName::Import));
        let shown = render::text(source.display().to_string().as_bytes());
        let table = self.request.csv.table.clone().unwrap_or_default();
        let size = std::fs::metadata(source)
            .map_err(|error| usage(format!("cannot read {shown}: {error}")))?
            .len();
        if size > MAX_AVRO_BYTES {
            return Err(usage(format!(
                "{shown} is larger than 2 GiB, the most eggdb import --avro reads at once"
            )));
        }
        let file = std::fs::read(source)
            .map_err(|error| usage(format!("cannot read {shown}: {error}")))?;
        let columns = columns_of(&file).map_err(|error| {
            Failure::database(format!("cannot import {shown}: {error}"), Exit::DatabaseError)
        })?;
        self.import_columns(&shown, &table, &columns)
    }

    pub(super) fn export_avro(&mut self, target: &Path) -> Result<Exit, Failure> {
        let shown = render::text(target.display().to_string().as_bytes());
        let (table, columns, rows) = match self.table_columns()? {
            Ok(found) => found,
            Err(exit) => return Ok(exit),
        };
        std::fs::write(target, avro_of(&table, &columns)).map_err(|error| {
            Failure::database(format!("cannot write {shown}: {error}"), Exit::DatabaseError)
        })?;
        Ok(self.written(&table, &shown, rows))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avro_round_trips() {
        let columns = vec![
            Column {
                name: "id".into(),
                column_type: ColumnType::Int64,
                nullable: true,
                cells: vec![Cell::Int(-3), Cell::Null],
            },
            Column {
                name: "price".into(),
                column_type: ColumnType::Decimal { precision: 10, scale: 2 },
                nullable: true,
                cells: vec![Cell::Decimal(-12_345), Cell::Decimal(128)],
            },
            Column {
                name: "name tag".into(),
                column_type: ColumnType::Text,
                nullable: true,
                cells: vec![Cell::Bytes(b"a".to_vec()), Cell::Bytes(Vec::new())],
            },
        ];
        let file = avro_of("my table", &columns);
        let read = columns_of(&file).unwrap();
        assert_eq!(read[0].cells, columns[0].cells);
        assert_eq!(read[1].column_type, columns[1].column_type);
        assert_eq!(read[1].cells, columns[1].cells);
        assert_eq!(read[2].name, "name_tag");
        assert_eq!(read[2].cells, columns[2].cells);
    }

    #[test]
    fn nested_values_become_json() {
        let mut names = Names(Vec::new());
        let document = parse_document(
            "{\"type\":\"record\",\"name\":\"r\",\"fields\":[{\"name\":\"tags\",\"type\":{\"type\":\"array\",\"items\":\"string\"}}]}",
        )
        .unwrap();
        let Schema::Record(fields) = schema_of(&document.value, &mut names, 0).unwrap() else {
            panic!("not a record");
        };
        let mut data = Vec::new();
        put_long(&mut data, 2);
        put_sized(&mut data, b"x");
        put_sized(&mut data, b"y");
        put_long(&mut data, 0);
        let mut reader = Reader { bytes: &data, at: 0 };
        assert_eq!(
            cell_of(&mut reader, &fields[0].1).unwrap(),
            Cell::Bytes(b"[\"x\",\"y\"]".to_vec())
        );
    }
}
