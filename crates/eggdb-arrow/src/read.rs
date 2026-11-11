use eggdb_parquet::{Cell, Column, ColumnType};

const FILE_MAGIC: &[u8] = b"ARROW1";
const FILE_MAGIC_PADDED: usize = 8;
const CONTINUATION: u32 = 0xffff_ffff;
const MAX_ROWS: u64 = eggdb_parquet::MAX_VALUES;
const MAX_FIELDS: usize = 4096;
const HEADER_SCHEMA: u8 = 1;
const HEADER_DICTIONARY_BATCH: u8 = 2;
const HEADER_RECORD_BATCH: u8 = 3;
const TYPE_INT: u8 = 2;
const TYPE_FLOATING_POINT: u8 = 3;
const TYPE_BINARY: u8 = 4;
const TYPE_UTF8: u8 = 5;
const TYPE_BOOL: u8 = 6;
const TYPE_DECIMAL: u8 = 7;
const TYPE_DATE: u8 = 8;
const TYPE_TIMESTAMP: u8 = 10;
const TYPE_LARGE_BINARY: u8 = 19;
const TYPE_LARGE_UTF8: u8 = 20;
const MILLIS_PER_DAY: i64 = 86_400_000;
const CODEC_LZ4_FRAME: u8 = 0;
const CODEC_ZSTD: u8 = 1;
const UNCOMPRESSED_BUFFER: i64 = -1;
const MOST_BUFFER_BYTES: usize = 1 << 28;

fn damaged(what: &str) -> String {
    format!("the Arrow stream is damaged: {what}")
}

#[derive(Clone, Copy)]
struct Table<'b> {
    bytes: &'b [u8],
    at: usize,
    vtable: usize,
    vtable_length: usize,
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn i64_at(bytes: &[u8], at: usize) -> Option<i64> {
    Some(i64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

impl<'b> Table<'b> {
    fn at(bytes: &'b [u8], at: usize) -> Option<Self> {
        let soffset = i32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?);
        let vtable = usize::try_from(i64::try_from(at).ok()? - i64::from(soffset)).ok()?;
        let vtable_length = usize::from(u16_at(bytes, vtable)?);
        if vtable_length < 4 || vtable + vtable_length > bytes.len() {
            return None;
        }
        Some(Self {
            bytes,
            at,
            vtable,
            vtable_length,
        })
    }

    fn root(bytes: &'b [u8]) -> Option<Self> {
        Self::at(bytes, usize::try_from(u32_at(bytes, 0)?).ok()?)
    }

    fn field(&self, index: usize) -> Option<usize> {
        let slot = 4 + index * 2;
        if slot + 2 > self.vtable_length {
            return None;
        }
        let offset = usize::from(u16_at(self.bytes, self.vtable + slot)?);
        (offset != 0).then_some(self.at + offset)
    }

    fn u8(&self, index: usize, default: u8) -> u8 {
        self.field(index)
            .and_then(|at| self.bytes.get(at).copied())
            .unwrap_or(default)
    }

    fn i16(&self, index: usize, default: i16) -> i16 {
        self.field(index)
            .and_then(|at| u16_at(self.bytes, at))
            .map_or(default, |value| value as i16)
    }

    fn i32(&self, index: usize, default: i32) -> i32 {
        self.field(index)
            .and_then(|at| u32_at(self.bytes, at))
            .map_or(default, |value| value as i32)
    }

    fn i64(&self, index: usize, default: i64) -> i64 {
        self.field(index)
            .and_then(|at| i64_at(self.bytes, at))
            .unwrap_or(default)
    }

    fn target(&self, index: usize) -> Option<usize> {
        let at = self.field(index)?;
        at.checked_add(usize::try_from(u32_at(self.bytes, at)?).ok()?)
    }

    fn table(&self, index: usize) -> Option<Table<'b>> {
        Table::at(self.bytes, self.target(index)?)
    }

    fn string(&self, index: usize) -> Option<&'b str> {
        let at = self.target(index)?;
        let length = usize::try_from(u32_at(self.bytes, at)?).ok()?;
        std::str::from_utf8(self.bytes.get(at + 4..at.checked_add(4 + length)?)?).ok()
    }

    fn vector(&self, index: usize) -> Option<(usize, usize)> {
        let at = self.target(index)?;
        Some((at + 4, usize::try_from(u32_at(self.bytes, at)?).ok()?))
    }

    fn tables(&self, index: usize) -> Option<Vec<Table<'b>>> {
        let Some((start, count)) = self.vector(index) else {
            return Some(Vec::new());
        };
        if count > MAX_FIELDS {
            return None;
        }
        (0..count)
            .map(|item| {
                let slot = start + item * 4;
                Table::at(self.bytes, slot.checked_add(usize::try_from(u32_at(self.bytes, slot)?).ok()?)?)
            })
            .collect()
    }

    fn pairs(&self, index: usize) -> Option<Vec<(i64, i64)>> {
        let Some((start, count)) = self.vector(index) else {
            return Some(Vec::new());
        };
        if count > MAX_FIELDS * 4 {
            return None;
        }
        (0..count)
            .map(|item| {
                let at = start + item * 16;
                Some((i64_at(self.bytes, at)?, i64_at(self.bytes, at + 8)?))
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Bool,
    Int { width: i32, signed: bool },
    Float { single: bool },
    Text { large: bool },
    Bytes { large: bool },
    DateDays,
    DateMillis,
    Timestamp { unit: i16, utc: bool },
    Decimal { precision: u32, scale: u32 },
}

impl Kind {
    fn column_type(self) -> ColumnType {
        match self {
            Self::Bool => ColumnType::Boolean,
            Self::Int { width, signed } if width < 32 || (width == 32 && signed) => ColumnType::Int32,
            Self::Int { .. } => ColumnType::Int64,
            Self::Float { single: true } => ColumnType::Float,
            Self::Float { single: false } => ColumnType::Double,
            Self::Text { .. } => ColumnType::Text,
            Self::Bytes { .. } => ColumnType::Binary,
            Self::DateDays | Self::DateMillis => ColumnType::Date,
            Self::Timestamp { utc, .. } => ColumnType::Timestamp { utc },
            Self::Decimal { precision, scale } => ColumnType::Decimal { precision, scale },
        }
    }

    fn buffers(self) -> usize {
        match self {
            Self::Text { .. } | Self::Bytes { .. } => 3,
            _ => 2,
        }
    }
}

fn kind_of(field: &Table<'_>) -> Result<Kind, String> {
    let type_id = field.u8(2, 0);
    let details = field.table(3);
    let detail_i16 = |index: usize, default: i16| details.map_or(default, |table| table.i16(index, default));
    let detail_i32 = |index: usize, default: i32| details.map_or(default, |table| table.i32(index, default));
    Ok(match type_id {
        TYPE_BOOL => Kind::Bool,
        TYPE_INT => {
            let width = detail_i32(0, 0);
            if !matches!(width, 8 | 16 | 32 | 64) {
                return Err(damaged("an integer type has an unknown width"));
            }
            Kind::Int {
                width,
                signed: details.is_some_and(|table| table.u8(1, 0) != 0),
            }
        }
        TYPE_FLOATING_POINT => match detail_i16(0, 0) {
            1 => Kind::Float { single: true },
            2 => Kind::Float { single: false },
            _ => return Err("half-precision floats are not supported yet".to_owned()),
        },
        TYPE_UTF8 => Kind::Text { large: false },
        TYPE_LARGE_UTF8 => Kind::Text { large: true },
        TYPE_BINARY => Kind::Bytes { large: false },
        TYPE_LARGE_BINARY => Kind::Bytes { large: true },
        TYPE_DATE => match detail_i16(0, 1) {
            0 => Kind::DateDays,
            _ => Kind::DateMillis,
        },
        TYPE_TIMESTAMP => Kind::Timestamp {
            unit: detail_i16(0, 0),
            utc: details.and_then(|table| table.string(1)).is_some_and(|zone| !zone.is_empty()),
        },
        TYPE_DECIMAL => {
            if detail_i32(2, 128) != 128 {
                return Err("256-bit decimals are not supported yet".to_owned());
            }
            Kind::Decimal {
                precision: u32::try_from(detail_i32(0, 38)).unwrap_or(38),
                scale: u32::try_from(detail_i32(1, 0)).unwrap_or(0),
            }
        }
        other => return Err(format!("Arrow type {other} is not supported yet")),
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Dictionary {
    id: i64,
    index: Kind,
}

fn dictionary_of(field: &Table<'_>) -> Result<Option<Dictionary>, String> {
    let Some(encoding) = field.table(4) else {
        return Ok(None);
    };
    let index_type = encoding.table(1);
    let width = index_type.map_or(32, |table| table.i32(0, 32));
    if !matches!(width, 8 | 16 | 32 | 64) {
        return Err(damaged("a dictionary's index type has an unknown width"));
    }
    Ok(Some(Dictionary {
        id: encoding.i64(0, 0),
        index: Kind::Int {
            width,
            signed: index_type.is_none_or(|table| table.u8(1, 0) != 0),
        },
    }))
}

fn looked_up(indices: Vec<Cell>, values: &[Cell], cells: &mut Vec<Cell>) -> Result<(), String> {
    for index in indices {
        cells.push(match index {
            Cell::Null => Cell::Null,
            Cell::Int(index) => usize::try_from(index)
                .ok()
                .and_then(|index| values.get(index))
                .cloned()
                .ok_or_else(|| damaged("a dictionary index lies outside its dictionary"))?,
            _ => return Err(damaged("a dictionary index is not an integer")),
        });
    }
    Ok(())
}

struct Message<'b> {
    header_type: u8,
    header: Table<'b>,
    body: &'b [u8],
}

fn next_message<'b>(bytes: &'b [u8], at: &mut usize) -> Result<Option<Message<'b>>, String> {
    let Some(first) = u32_at(bytes, *at) else {
        return Ok(None);
    };
    let length_at = if first == CONTINUATION { *at + 4 } else { *at };
    let length = usize::try_from(u32_at(bytes, length_at).ok_or_else(|| damaged("a message is cut short"))?)
        .map_err(|_| damaged("a message is too long"))?;
    if length == 0 {
        return Ok(None);
    }
    let metadata_start = length_at + 4;
    let metadata = bytes
        .get(metadata_start..metadata_start.checked_add(length).ok_or_else(|| damaged("a message is too long"))?)
        .ok_or_else(|| damaged("a message is cut short"))?;
    let root = Table::root(metadata).ok_or_else(|| damaged("a message's header cannot be read"))?;
    let body_length = usize::try_from(root.i64(3, 0)).map_err(|_| damaged("a body length is negative"))?;
    let body_start = metadata_start + length;
    let body = bytes
        .get(body_start..body_start.checked_add(body_length).ok_or_else(|| damaged("a body is too long"))?)
        .ok_or_else(|| damaged("a body is cut short"))?;
    *at = body_start + body_length;
    let header = root.table(2).ok_or_else(|| damaged("a message has no header"))?;
    Ok(Some(Message {
        header_type: root.u8(1, 0),
        header,
        body,
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Codec {
    Lz4Frame,
    Zstd,
}

fn codec_of(batch: &Table<'_>) -> Result<Option<Codec>, String> {
    let Some(compression) = batch.table(3) else {
        return Ok(None);
    };
    if compression.u8(1, 0) != 0 {
        return Err(damaged("a batch's compression method is not per buffer"));
    }
    match compression.u8(0, CODEC_LZ4_FRAME) {
        CODEC_LZ4_FRAME => Ok(Some(Codec::Lz4Frame)),
        CODEC_ZSTD => Ok(Some(Codec::Zstd)),
        _ => Err(damaged("a batch names an unknown compression codec")),
    }
}

struct Batch<'b> {
    nodes: Vec<(i64, i64)>,
    buffers: Vec<(i64, i64)>,
    body: &'b [u8],
    next_node: usize,
    next_buffer: usize,
    codec: Option<Codec>,
}

impl<'b> Batch<'b> {
    fn buffer(&mut self) -> Result<std::borrow::Cow<'b, [u8]>, String> {
        let stored = self.stored_buffer()?;
        let Some(codec) = self.codec else {
            return Ok(std::borrow::Cow::Borrowed(stored));
        };
        if stored.is_empty() {
            return Ok(std::borrow::Cow::Borrowed(stored));
        }
        let length = i64_at(stored, 0).ok_or_else(|| damaged("a compressed buffer has no length"))?;
        let rest = &stored[8..];
        if length == UNCOMPRESSED_BUFFER {
            return Ok(std::borrow::Cow::Borrowed(rest));
        }
        let length = usize::try_from(length)
            .ok()
            .filter(|length| *length <= MOST_BUFFER_BYTES)
            .ok_or_else(|| damaged("a compressed buffer's length is out of range"))?;
        let bytes = match codec {
            Codec::Lz4Frame => eggdb_storage::decompress_lz4_frame(rest, length),
            Codec::Zstd => eggdb_storage::decompress_zstd(rest, length),
        }
        .map_err(damaged)?;
        Ok(std::borrow::Cow::Owned(bytes))
    }

    fn stored_buffer(&mut self) -> Result<&'b [u8], String> {
        let (offset, length) = *self
            .buffers
            .get(self.next_buffer)
            .ok_or_else(|| damaged("a batch has too few buffers"))?;
        self.next_buffer += 1;
        let (offset, length) = (
            usize::try_from(offset).map_err(|_| damaged("a buffer offset is negative"))?,
            usize::try_from(length).map_err(|_| damaged("a buffer length is negative"))?,
        );
        self.body
            .get(offset..offset.checked_add(length).ok_or_else(|| damaged("a buffer is too long"))?)
            .ok_or_else(|| damaged("a buffer lies outside its body"))
    }
}

fn bit(bytes: &[u8], index: usize) -> Option<bool> {
    bytes.get(index / 8).map(|byte| byte & (1 << (index % 8)) != 0)
}

fn fixed<const N: usize>(data: &[u8], index: usize) -> Result<[u8; N], String> {
    data.get(index * N..index * N + N)
        .and_then(|slice| slice.try_into().ok())
        .ok_or_else(|| damaged("a value lies outside its buffer"))
}

fn offset_at(offsets: &[u8], index: usize, large: bool) -> Result<usize, String> {
    let value = if large {
        i64::from_le_bytes(fixed::<8>(offsets, index)?)
    } else {
        i64::from(i32::from_le_bytes(fixed::<4>(offsets, index)?))
    };
    usize::try_from(value).map_err(|_| damaged("an offset is negative"))
}

fn timestamp_micros(value: i64, unit: i16) -> Result<i64, String> {
    match unit {
        0 => value.checked_mul(1_000_000),
        1 => value.checked_mul(1_000),
        2 => Some(value),
        _ => Some(value.div_euclid(1_000)),
    }
    .ok_or_else(|| "a timestamp is out of range".to_owned())
}

fn decode(kind: Kind, batch: &mut Batch<'_>, rows: usize, cells: &mut Vec<Cell>) -> Result<(), String> {
    let (length, nulls) = *batch
        .nodes
        .get(batch.next_node)
        .ok_or_else(|| damaged("a batch has too few field nodes"))?;
    batch.next_node += 1;
    if usize::try_from(length).ok() != Some(rows) {
        return Err(damaged("a column's length differs from its batch's"));
    }
    let validity_buffer = batch.buffer()?;
    let validity: &[u8] = &validity_buffer;
    let present = |index: usize| nulls == 0 || validity.is_empty() || bit(validity, index).unwrap_or(false);
    let data_buffer = batch.buffer()?;
    let data: &[u8] = &data_buffer;
    let values_buffer = if kind.buffers() == 3 { Some(batch.buffer()?) } else { None };
    let values: Option<&[u8]> = values_buffer.as_deref();
    for index in 0..rows {
        if !present(index) {
            cells.push(Cell::Null);
            continue;
        }
        cells.push(match kind {
            Kind::Bool => Cell::Boolean(bit(data, index).ok_or_else(|| damaged("a value lies outside its buffer"))?),
            Kind::Int { width, signed } => Cell::Int(match (width, signed) {
                (8, true) => i64::from(i8::from_le_bytes(fixed::<1>(data, index)?)),
                (8, false) => i64::from(u8::from_le_bytes(fixed::<1>(data, index)?)),
                (16, true) => i64::from(i16::from_le_bytes(fixed::<2>(data, index)?)),
                (16, false) => i64::from(u16::from_le_bytes(fixed::<2>(data, index)?)),
                (32, true) => i64::from(i32::from_le_bytes(fixed::<4>(data, index)?)),
                (32, false) => i64::from(u32::from_le_bytes(fixed::<4>(data, index)?)),
                (_, true) => i64::from_le_bytes(fixed::<8>(data, index)?),
                (_, false) => i64::try_from(u64::from_le_bytes(fixed::<8>(data, index)?))
                    .map_err(|_| "an unsigned 64-bit value is past the largest bigint".to_owned())?,
            }),
            Kind::Float { single: true } => Cell::Float(f64::from(f32::from_le_bytes(fixed::<4>(data, index)?))),
            Kind::Float { single: false } => Cell::Float(f64::from_le_bytes(fixed::<8>(data, index)?)),
            Kind::Text { large } | Kind::Bytes { large } => {
                let values = values.unwrap_or_default();
                let start = offset_at(data, index, large)?;
                let end = offset_at(data, index + 1, large)?;
                let bytes = values
                    .get(start..end)
                    .ok_or_else(|| damaged("a value lies outside its buffer"))?;
                if matches!(kind, Kind::Text { .. }) && std::str::from_utf8(bytes).is_err() {
                    return Err(damaged("a utf8 value is not UTF-8"));
                }
                Cell::Bytes(bytes.to_vec())
            }
            Kind::DateDays => Cell::Int(i64::from(i32::from_le_bytes(fixed::<4>(data, index)?))),
            Kind::DateMillis => Cell::Int(i64::from_le_bytes(fixed::<8>(data, index)?).div_euclid(MILLIS_PER_DAY)),
            Kind::Timestamp { unit, .. } => {
                Cell::Int(timestamp_micros(i64::from_le_bytes(fixed::<8>(data, index)?), unit)?)
            }
            Kind::Decimal { .. } => Cell::Decimal(i128::from_le_bytes(fixed::<16>(data, index)?)),
        });
    }
    Ok(())
}

/// Reads an Arrow IPC stream, or an Arrow IPC file (`.arrow`, Feather 2), into columns: its
/// schema's flat fields and every record batch's rows in order, dictionary-encoded columns read
/// through their dictionary batches (deltas appended), and batches whose buffers are compressed
/// with `LZ4_FRAME` or `ZSTD` decompressed. Damaged or hostile input is an error, never a panic;
/// nested types are refused by name.
pub fn read(bytes: &[u8]) -> Result<Vec<Column>, String> {
    let mut at = if bytes.starts_with(FILE_MAGIC) {
        FILE_MAGIC_PADDED
    } else {
        0
    };
    let mut columns: Vec<(Kind, Option<Dictionary>, Column)> = Vec::new();
    let mut dictionaries: std::collections::HashMap<i64, Vec<Cell>> = std::collections::HashMap::new();
    let mut dictionary_cells: u64 = 0;
    let mut have_schema = false;
    let mut total_rows: u64 = 0;
    while let Some(message) = next_message(bytes, &mut at)? {
        match message.header_type {
            HEADER_SCHEMA if !have_schema => {
                for field in message.header.tables(1).ok_or_else(|| damaged("the schema's fields cannot be read"))? {
                    if field.tables(5).is_some_and(|children| !children.is_empty()) {
                        return Err("nested Arrow types are not supported yet".to_owned());
                    }
                    let kind = kind_of(&field)?;
                    columns.push((
                        kind,
                        dictionary_of(&field)?,
                        Column {
                            name: field.string(0).unwrap_or_default().to_owned(),
                            column_type: kind.column_type(),
                            nullable: field.u8(1, 0) != 0,
                            cells: Vec::new(),
                        },
                    ));
                }
                have_schema = true;
            }
            HEADER_RECORD_BATCH if have_schema => {
                let codec = codec_of(&message.header)?;
                let rows = u64::try_from(message.header.i64(0, 0)).map_err(|_| damaged("a batch's length is negative"))?;
                total_rows = total_rows.saturating_add(rows);
                if total_rows > MAX_ROWS {
                    return Err(format!("the stream holds more than {MAX_ROWS} rows"));
                }
                let mut batch = Batch {
                    nodes: message.header.pairs(1).ok_or_else(|| damaged("a batch's nodes cannot be read"))?,
                    buffers: message.header.pairs(2).ok_or_else(|| damaged("a batch's buffers cannot be read"))?,
                    body: message.body,
                    next_node: 0,
                    next_buffer: 0,
                    codec,
                };
                let rows = usize::try_from(rows).map_err(|_| damaged("a batch is too long"))?;
                for (kind, dictionary, column) in &mut columns {
                    match dictionary {
                        None => decode(*kind, &mut batch, rows, &mut column.cells)?,
                        Some(dictionary) => {
                            let mut indices = Vec::with_capacity(rows);
                            decode(dictionary.index, &mut batch, rows, &mut indices)?;
                            let values = dictionaries
                                .get(&dictionary.id)
                                .ok_or_else(|| damaged("a batch uses a dictionary that was never sent"))?;
                            looked_up(indices, values, &mut column.cells)?;
                        }
                    }
                }
            }
            HEADER_DICTIONARY_BATCH if have_schema => {
                let id = message.header.i64(0, 0);
                let data = message
                    .header
                    .table(1)
                    .ok_or_else(|| damaged("a dictionary batch has no data"))?;
                let codec = codec_of(&data)?;
                let kind = columns
                    .iter()
                    .find(|(_, dictionary, _)| dictionary.is_some_and(|dictionary| dictionary.id == id))
                    .map(|(kind, _, _)| *kind)
                    .ok_or_else(|| damaged("a dictionary batch belongs to no column"))?;
                let rows = u64::try_from(data.i64(0, 0)).map_err(|_| damaged("a dictionary's length is negative"))?;
                dictionary_cells = dictionary_cells.saturating_add(rows);
                if dictionary_cells > MAX_ROWS {
                    return Err(format!("the stream's dictionaries hold more than {MAX_ROWS} values"));
                }
                let mut batch = Batch {
                    nodes: data.pairs(1).ok_or_else(|| damaged("a dictionary's nodes cannot be read"))?,
                    buffers: data.pairs(2).ok_or_else(|| damaged("a dictionary's buffers cannot be read"))?,
                    body: message.body,
                    next_node: 0,
                    next_buffer: 0,
                    codec,
                };
                let rows = usize::try_from(rows).map_err(|_| damaged("a dictionary is too long"))?;
                let mut values = Vec::with_capacity(rows);
                decode(kind, &mut batch, rows, &mut values)?;
                if message.header.u8(2, 0) != 0 {
                    dictionaries.entry(id).or_default().extend(values);
                } else {
                    dictionaries.insert(id, values);
                }
            }
            HEADER_SCHEMA => return Err(damaged("a second schema follows the first")),
            _ => return Err(damaged("a message comes before the schema or has an unknown kind")),
        }
    }
    if !have_schema {
        return Err(damaged("there is no schema"));
    }
    Ok(columns.into_iter().map(|(_, _, column)| column).collect())
}
