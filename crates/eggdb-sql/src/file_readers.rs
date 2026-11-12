use crate::error::{SqlError, SqlResult, SqlState};
use crate::json_functions::RecordSink;
use crate::types::{Conversion, SqlType, Value, convert};
use eggdb_parquet::{Cell, Column, ColumnType};

const MOST_FILE_BYTES: usize = 1 << 30;
const DAYS_1970_TO_2000: i64 = 10_957;
const MICROS_1970_TO_2000: i64 = 946_684_800_000_000;

fn decimal_text(unscaled: i128, scale: u32) -> String {
    let negative = unscaled < 0;
    let digits = unscaled.unsigned_abs().to_string();
    let scale = scale as usize;
    let text = if scale == 0 {
        digits
    } else if digits.len() > scale {
        format!("{}.{}", &digits[..digits.len() - scale], &digits[digits.len() - scale..])
    } else {
        format!("0.{}{digits}", "0".repeat(scale - digits.len()))
    };
    if negative { format!("-{text}") } else { text }
}

fn value_of(column: &Column, cell: &Cell) -> Value {
    match (cell, column.column_type) {
        (Cell::Null, _) => Value::Null,
        (Cell::Boolean(flag), _) => Value::Boolean(*flag),
        (Cell::Int(days), ColumnType::Date) => i32::try_from(*days - DAYS_1970_TO_2000).map_or(Value::Null, Value::Date),
        (Cell::Int(micros), ColumnType::Timestamp { utc: true }) => Value::TimestampTz(micros - MICROS_1970_TO_2000),
        (Cell::Int(micros), ColumnType::Timestamp { utc: false }) => Value::Timestamp(micros - MICROS_1970_TO_2000),
        (Cell::Int(whole), _) => Value::BigInt(*whole),
        (Cell::Float(number), _) => Value::Double(*number),
        (Cell::Bytes(bytes), ColumnType::Text) => Value::Text(String::from_utf8_lossy(bytes).into_owned()),
        (Cell::Bytes(bytes), _) => Value::Bytea(bytes.clone()),
        (Cell::Decimal(unscaled), ColumnType::Decimal { scale, .. }) => Value::Text(decimal_text(*unscaled, scale)),
        (Cell::Decimal(unscaled), _) => Value::Text(unscaled.to_string()),
    }
}

/// The rows of `read_parquet(path) AS t(name type, ...)` (`arrow` false) or
/// `read_arrow(path) AS t(...)` (an Arrow IPC file or stream): the file read through the
/// session's file reader, each declared column taken from the file's column of that name
/// (matched exactly, then without regard to case; `NULL` when the file has none) and cast to
/// its type.
pub(crate) fn rows(
    values: &[Value],
    names: &[String],
    types: &[Option<SqlType>],
    sink: &mut RecordSink<'_>,
    arrow: bool,
) -> SqlResult<()> {
    let Some(path) = values.first() else {
        return Err(crate::json_to_record::definition_list_required());
    };
    let Some(path) = path.to_text() else {
        return Ok(());
    };
    let bytes = crate::new_files::read_file(&path, MOST_FILE_BYTES)?;
    let columns = if arrow {
        eggdb_arrow::read(&bytes).map_err(|error| SqlError::new(SqlState::DATA_CORRUPTED, format!("{path}: {error}")))?
    } else {
        eggdb_parquet::read(&bytes).map_err(|error| SqlError::new(SqlState::DATA_CORRUPTED, format!("{path}: {}", error.0)))?
    };
    let chosen: Vec<Option<&Column>> = names
        .iter()
        .map(|name| {
            columns
                .iter()
                .find(|column| column.name == *name)
                .or_else(|| columns.iter().find(|column| column.name.eq_ignore_ascii_case(name)))
        })
        .collect();
    let count = columns.first().map_or(0, |column| column.cells.len());
    crate::memory::check_value_bytes(bytes.len())?;
    for row in 0..count {
        let mut record = Vec::with_capacity(names.len());
        for (at, column) in chosen.iter().enumerate() {
            let value = column
                .and_then(|column| column.cells.get(row).map(|cell| value_of(column, cell)))
                .unwrap_or(Value::Null);
            let declared = types.get(at + 1).copied().flatten();
            record.push(match declared {
                Some(sql_type) if !value.is_null() && value.sql_type() != Some(sql_type) => {
                    convert(value, sql_type, Conversion::Explicit)?
                }
                _ => value,
            });
        }
        sink(record)?;
    }
    Ok(())
}

/// Whether `name` is one of the file-reading functions.
pub(crate) fn reads_files(name: &str) -> bool {
    matches!(name, "read_parquet" | "read_arrow" | "read_csv" | "read_vcf" | "read_bam" | "read_netcdf" | "read_geotiff")
}

const GEOTIFF_COLUMNS: [(&str, SqlType); 5] = [
    ("x", SqlType::Integer),
    ("y", SqlType::Integer),
    ("values", SqlType::Array(crate::arrays::ArrayElement::Double)),
    ("map_x", SqlType::Double),
    ("map_y", SqlType::Double),
];

/// The rows of `read_geotiff(path)`: one per pixel of a TIFF's first image, with its column
/// `x` and row `y`, its band `values`, and for a GeoTIFF the map coordinates of the pixel's
/// centre.
pub(crate) fn geotiff_rows(
    values: &[Value],
    names: &[String],
    types: &[Option<SqlType>],
    sink: &mut RecordSink<'_>,
) -> SqlResult<()> {
    let Some(path) = values.first().and_then(Value::to_text) else {
        return Ok(());
    };
    let bytes = crate::new_files::read_file(&path, MOST_FILE_BYTES)?;
    crate::memory::check_value_bytes(bytes.len())?;
    let raster = crate::geotiff::raster(&bytes)?;
    let wanted: Vec<Option<usize>> = names
        .iter()
        .map(|name| GEOTIFF_COLUMNS.iter().position(|(column, _)| column.eq_ignore_ascii_case(name)))
        .collect();
    for y in 0..raster.height {
        for x in 0..raster.width {
            let at = (y * raster.width + x) * raster.bands;
            let bands = crate::arrays::ArrayValue::new(
                crate::arrays::ArrayElement::Double,
                raster.values[at..at + raster.bands].iter().map(|value| Value::Double(*value)).collect(),
            );
            let (map_x, map_y) = raster.origin.map_or((Value::Null, Value::Null), |(west, north, width, height)| {
                (Value::Double(west + (x as f64 + 0.5) * width), Value::Double(north - (y as f64 + 0.5) * height))
            });
            let record: [Value; 5] = [
                Value::Integer(i32::try_from(x).unwrap_or(i32::MAX)),
                Value::Integer(i32::try_from(y).unwrap_or(i32::MAX)),
                Value::Array(Box::new(bands)),
                map_x,
                map_y,
            ];
            let mut row = Vec::with_capacity(names.len());
            for (index, position) in wanted.iter().enumerate() {
                let value = position.map_or(Value::Null, |position| record[position].clone());
                let declared = types.get(index + 1).copied().flatten();
                row.push(match declared {
                    Some(sql_type) if !value.is_null() && value.sql_type() != Some(sql_type) => convert(value, sql_type, Conversion::Explicit)?,
                    _ => value,
                });
            }
            sink(row)?;
        }
    }
    Ok(())
}

const NETCDF_COLUMNS: [(&str, SqlType); 4] = [
    ("variable", SqlType::Text),
    ("coordinates", SqlType::Array(crate::arrays::ArrayElement::BigInt)),
    ("value", SqlType::Double),
    ("text", SqlType::Text),
];

/// The rows of `read_netcdf(path)`: one per element of every variable of a NetCDF classic file,
/// with the `variable`'s name, the element's `coordinates`, and its `value` (numbers) or `text`
/// (one string of a character variable).
pub(crate) fn netcdf_rows(
    values: &[Value],
    names: &[String],
    types: &[Option<SqlType>],
    sink: &mut RecordSink<'_>,
) -> SqlResult<()> {
    let Some(path) = values.first().and_then(Value::to_text) else {
        return Ok(());
    };
    let bytes = crate::new_files::read_file(&path, MOST_FILE_BYTES)?;
    crate::memory::check_value_bytes(bytes.len())?;
    let wanted: Vec<Option<usize>> = names
        .iter()
        .map(|name| NETCDF_COLUMNS.iter().position(|(column, _)| column.eq_ignore_ascii_case(name)))
        .collect();
    for (variable, coordinates, element) in crate::netcdf::elements(&bytes)? {
        let coordinates = crate::arrays::ArrayValue::new(
            crate::arrays::ArrayElement::BigInt,
            coordinates.into_iter().map(Value::BigInt).collect(),
        );
        let (number, text) = match element {
            crate::netcdf::Element::Number(number) => (Value::Double(number), Value::Null),
            crate::netcdf::Element::Text(text) => (Value::Null, Value::Text(text)),
        };
        let record: [Value; 4] = [Value::Text(variable), Value::Array(Box::new(coordinates)), number, text];
        let mut row = Vec::with_capacity(names.len());
        for (at, position) in wanted.iter().enumerate() {
            let value = position.map_or(Value::Null, |position| record[position].clone());
            let declared = types.get(at + 1).copied().flatten();
            row.push(match declared {
                Some(sql_type) if !value.is_null() && value.sql_type() != Some(sql_type) => convert(value, sql_type, Conversion::Explicit)?,
                _ => value,
            });
        }
        sink(row)?;
    }
    Ok(())
}

const BAM_MAGIC: &[u8] = b"BAM\x01";
const BAM_COLUMNS: [(&str, SqlType); 12] = [
    ("qname", SqlType::Text),
    ("flag", SqlType::Integer),
    ("rname", SqlType::Text),
    ("pos", SqlType::BigInt),
    ("mapq", SqlType::Integer),
    ("cigar", SqlType::Text),
    ("rnext", SqlType::Text),
    ("pnext", SqlType::BigInt),
    ("tlen", SqlType::BigInt),
    ("seq", SqlType::Text),
    ("qual", SqlType::Text),
    ("tags", SqlType::Jsonb),
];
const CIGAR_OPERATIONS: &[u8; 9] = b"MIDNSHP=X";
const SEQUENCE_CODES: &[u8; 16] = b"=ACMGRSVTWYHKDBN";

fn bam_error(path: &str, message: &str) -> SqlError {
    SqlError::new(SqlState::DATA_CORRUPTED, format!("{path}: {message}"))
}

struct BamCursor<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> BamCursor<'b> {
    fn take(&mut self, count: usize) -> Option<&'b [u8]> {
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len())?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Some(taken)
    }

    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
}

fn bam_tag_value(cursor: &mut BamCursor<'_>, kind: u8) -> Option<String> {
    Some(match kind {
        b'A' => json_text(&char::from(cursor.u8()?).to_string()),
        b'c' => (cursor.u8()? as i8).to_string(),
        b'C' => cursor.u8()?.to_string(),
        b's' => (cursor.u16()? as i16).to_string(),
        b'S' => cursor.u16()?.to_string(),
        b'i' => cursor.i32()?.to_string(),
        b'I' => cursor.u32()?.to_string(),
        b'f' => {
            let number = f32::from_le_bytes(cursor.take(4)?.try_into().ok()?);
            if number.is_finite() { number.to_string() } else { "null".to_owned() }
        }
        b'Z' | b'H' => {
            let rest = &cursor.bytes[cursor.at..];
            let end = rest.iter().position(|byte| *byte == 0)?;
            let text = String::from_utf8_lossy(&rest[..end]).into_owned();
            cursor.at += end + 1;
            json_text(&text)
        }
        b'B' => {
            let subtype = cursor.u8()?;
            let count = cursor.u32()? as usize;
            if count > cursor.bytes.len() {
                return None;
            }
            let items: Option<Vec<String>> = (0..count).map(|_| bam_tag_value(cursor, subtype)).collect();
            format!("[{}]", items?.join(","))
        }
        _ => return None,
    })
}

/// The rows of `read_bam(path)`: one per alignment of a BAM file (BGZF-compressed), as SAM shows
/// it: `qname`, `flag`, `rname` and `rnext` by reference name (`*` read as `NULL`), 1-based `pos`
/// and `pnext`, `mapq`, `cigar`, `tlen`, `seq`, `qual` as Phred+33 text, and the optional fields
/// as a JSON object of tag to value.
pub(crate) fn bam_rows(
    values: &[Value],
    names: &[String],
    types: &[Option<SqlType>],
    sink: &mut RecordSink<'_>,
) -> SqlResult<()> {
    let Some(path) = values.first().and_then(Value::to_text) else {
        return Ok(());
    };
    let compressed = crate::new_files::read_file(&path, MOST_FILE_BYTES)?;
    let bytes = eggdb_content::gunzip(&compressed, MOST_FILE_BYTES)
        .map_err(|error| SqlError::new(SqlState::DATA_CORRUPTED, format!("{path}: {error}")))?;
    crate::memory::check_value_bytes(bytes.len())?;
    let mut cursor = BamCursor { bytes: &bytes, at: 0 };
    if cursor.take(4) != Some(BAM_MAGIC) {
        return Err(bam_error(&path, "not a BAM file"));
    }
    let header_length = cursor.u32().ok_or_else(|| bam_error(&path, "the header is cut short"))? as usize;
    cursor.take(header_length).ok_or_else(|| bam_error(&path, "the header is cut short"))?;
    let reference_count = cursor.u32().ok_or_else(|| bam_error(&path, "the references are cut short"))? as usize;
    let mut references = Vec::new();
    for _ in 0..reference_count.min(bytes.len()) {
        let name_length = cursor.u32().ok_or_else(|| bam_error(&path, "a reference is cut short"))? as usize;
        let name = cursor.take(name_length).ok_or_else(|| bam_error(&path, "a reference is cut short"))?;
        cursor.u32().ok_or_else(|| bam_error(&path, "a reference is cut short"))?;
        references.push(String::from_utf8_lossy(name.strip_suffix(&[0]).unwrap_or(name)).into_owned());
    }
    let reference = |id: i32| usize::try_from(id).ok().and_then(|at| references.get(at)).map_or(Value::Null, |name| Value::Text(name.clone()));
    let wanted: Vec<Option<usize>> = names
        .iter()
        .map(|name| BAM_COLUMNS.iter().position(|(column, _)| column.eq_ignore_ascii_case(name)))
        .collect();
    while cursor.at < bytes.len() {
        let block_size = cursor.u32().ok_or_else(|| bam_error(&path, "an alignment is cut short"))? as usize;
        let block = cursor.take(block_size).ok_or_else(|| bam_error(&path, "an alignment is cut short"))?;
        let mut record = BamCursor { bytes: block, at: 0 };
        let short = || bam_error(&path, "an alignment's fields are cut short");
        let reference_id = record.i32().ok_or_else(short)?;
        let pos = record.i32().ok_or_else(short)?;
        let name_length = usize::from(record.u8().ok_or_else(short)?);
        let mapq = record.u8().ok_or_else(short)?;
        record.u16().ok_or_else(short)?;
        let cigar_count = usize::from(record.u16().ok_or_else(short)?);
        let flag = record.u16().ok_or_else(short)?;
        let sequence_length = usize::try_from(record.i32().ok_or_else(short)?).unwrap_or(0);
        let next_reference = record.i32().ok_or_else(short)?;
        let next_pos = record.i32().ok_or_else(short)?;
        let template_length = record.i32().ok_or_else(short)?;
        let read_name = record.take(name_length).ok_or_else(short)?;
        let mut cigar = String::new();
        for _ in 0..cigar_count {
            let operation = record.u32().ok_or_else(short)?;
            let code = CIGAR_OPERATIONS.get((operation & 0xf) as usize).copied().unwrap_or(b'?');
            cigar.push_str(&(operation >> 4).to_string());
            cigar.push(char::from(code));
        }
        let packed = record.take(sequence_length.div_ceil(2)).ok_or_else(short)?;
        let sequence: String = (0..sequence_length)
            .map(|at| {
                let byte = packed[at / 2];
                let code = if at % 2 == 0 { byte >> 4 } else { byte & 0xf };
                char::from(SEQUENCE_CODES[usize::from(code)])
            })
            .collect();
        let qualities = record.take(sequence_length).ok_or_else(short)?;
        let quality = if qualities.first().is_none_or(|first| *first == 0xff) {
            Value::Null
        } else {
            Value::Text(qualities.iter().map(|score| char::from(score.saturating_add(33))).collect())
        };
        let mut tags = Vec::new();
        while record.at + 3 <= block.len() {
            let tag = record.take(2).ok_or_else(short)?;
            let kind = record.u8().ok_or_else(short)?;
            let value = bam_tag_value(&mut record, kind).ok_or_else(|| bam_error(&path, "an optional field is damaged"))?;
            tags.push(format!("{}:{value}", json_text(&String::from_utf8_lossy(tag))));
        }
        let read_name = String::from_utf8_lossy(read_name.strip_suffix(&[0]).unwrap_or(read_name)).into_owned();
        let row_values: [Value; 12] = [
            if read_name == "*" { Value::Null } else { Value::Text(read_name) },
            Value::Integer(i32::from(flag)),
            reference(reference_id),
            if pos < 0 { Value::Null } else { Value::BigInt(i64::from(pos) + 1) },
            Value::Integer(i32::from(mapq)),
            if cigar.is_empty() { Value::Null } else { Value::Text(cigar) },
            reference(next_reference),
            if next_pos < 0 { Value::Null } else { Value::BigInt(i64::from(next_pos) + 1) },
            Value::BigInt(i64::from(template_length)),
            if sequence.is_empty() { Value::Null } else { Value::Text(sequence) },
            quality,
            crate::json::normalize(&format!("{{{}}}", tags.join(","))).map(Value::Jsonb)?,
        ];
        let mut row = Vec::with_capacity(names.len());
        for (at, position) in wanted.iter().enumerate() {
            let value = position.map_or(Value::Null, |position| row_values[position].clone());
            let declared = types.get(at + 1).copied().flatten();
            row.push(match declared {
                Some(sql_type) if !value.is_null() && value.sql_type() != Some(sql_type) => convert(value, sql_type, Conversion::Explicit)?,
                _ => value,
            });
        }
        sink(row)?;
    }
    Ok(())
}

const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const VCF_COLUMNS: [(&str, SqlType); 10] = [
    ("chrom", SqlType::Text),
    ("pos", SqlType::BigInt),
    ("id", SqlType::Text),
    ("ref", SqlType::Text),
    ("alt", SqlType::Text),
    ("qual", SqlType::Double),
    ("filter", SqlType::Text),
    ("info", SqlType::Jsonb),
    ("format", SqlType::Text),
    ("samples", SqlType::Jsonb),
];

/// The text of a file read through the session's reader, gunzipped when it is gzip (BGZF
/// included), as UTF-8.
fn text_file(path: &str) -> SqlResult<String> {
    let bytes = crate::new_files::read_file(path, MOST_FILE_BYTES)?;
    let bytes = if bytes.starts_with(&GZIP_MAGIC) {
        eggdb_content::gunzip(&bytes, MOST_FILE_BYTES)
            .map_err(|error| SqlError::new(SqlState::DATA_CORRUPTED, format!("{path}: {error}")))?
    } else {
        bytes
    };
    crate::memory::check_value_bytes(bytes.len())?;
    let text = String::from_utf8(bytes)
        .map_err(|_| SqlError::new(SqlState::CHARACTER_NOT_IN_REPERTOIRE, format!("{path} is not UTF-8")))?;
    Ok(text.strip_prefix('\u{feff}').map(str::to_owned).unwrap_or(text))
}

fn json_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    crate::json::write_string(text, &mut out);
    out
}

fn missing(field: &str) -> Option<&str> {
    (field != "." && !field.is_empty()).then_some(field)
}

fn vcf_info(field: &str) -> String {
    let members: Vec<String> = field
        .split(';')
        .filter(|entry| !entry.is_empty() && *entry != ".")
        .map(|entry| match entry.split_once('=') {
            Some((key, value)) => format!("{}:{}", json_text(key), json_text(value)),
            None => format!("{}:true", json_text(entry)),
        })
        .collect();
    format!("{{{}}}", members.join(","))
}

fn vcf_samples(format: Option<&str>, names: &[&str], values: &[&str]) -> String {
    let keys: Vec<&str> = format.map(|format| format.split(':').collect()).unwrap_or_default();
    let members: Vec<String> = names
        .iter()
        .zip(values)
        .map(|(name, value)| {
            let fields: Vec<String> = keys
                .iter()
                .zip(value.split(':'))
                .map(|(key, field)| format!("{}:{}", json_text(key), missing(field).map_or_else(|| "null".to_owned(), json_text)))
                .collect();
            format!("{}:{{{}}}", json_text(name), fields.join(","))
        })
        .collect();
    format!("{{{}}}", members.join(","))
}

/// The rows of `read_vcf(path)`: one per variant line of a VCF file (gzip and BGZF read too),
/// with `chrom`, `pos`, `id`, `ref`, `alt`, `qual` and `filter` (`.` read as `NULL`), `info` as a
/// JSON object (flags as `true`), `format`, and `samples` as `{sample: {key: value}}` by the
/// header's sample names.
pub(crate) fn vcf_rows(
    values: &[Value],
    names: &[String],
    types: &[Option<SqlType>],
    sink: &mut RecordSink<'_>,
) -> SqlResult<()> {
    let Some(path) = values.first().and_then(Value::to_text) else {
        return Ok(());
    };
    let text = text_file(&path)?;
    let mut sample_names: Vec<&str> = Vec::new();
    let wanted: Vec<Option<usize>> = names
        .iter()
        .map(|name| VCF_COLUMNS.iter().position(|(column, _)| column.eq_ignore_ascii_case(name)))
        .collect();
    for line in text.lines() {
        if let Some(header) = line.strip_prefix("#CHROM") {
            sample_names = header.split('\t').skip(9).collect();
            continue;
        }
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 8 {
            return Err(SqlError::new(SqlState::BAD_COPY_FILE_FORMAT, format!("{path}: a VCF line has fewer than 8 columns")));
        }
        let pos = fields[1]
            .parse::<i64>()
            .map_err(|_| SqlError::new(SqlState::BAD_COPY_FILE_FORMAT, format!("{path}: POS {} is not a number", fields[1])))?;
        let format = fields.get(8).copied().and_then(missing);
        let record: [Value; 10] = [
            Value::Text(fields[0].to_owned()),
            Value::BigInt(pos),
            missing(fields[2]).map_or(Value::Null, |id| Value::Text(id.to_owned())),
            Value::Text(fields[3].to_owned()),
            missing(fields[4]).map_or(Value::Null, |alt| Value::Text(alt.to_owned())),
            missing(fields[5]).and_then(|qual| qual.parse::<f64>().ok()).map_or(Value::Null, Value::Double),
            missing(fields[6]).map_or(Value::Null, |filter| Value::Text(filter.to_owned())),
            crate::json::normalize(&vcf_info(fields[7])).map(Value::Jsonb)?,
            format.map_or(Value::Null, |format| Value::Text(format.to_owned())),
            crate::json::normalize(&vcf_samples(format, &sample_names, fields.get(9..).unwrap_or_default()))
                .map(Value::Jsonb)?,
        ];
        let mut row = Vec::with_capacity(names.len());
        for (at, position) in wanted.iter().enumerate() {
            let value = position.map_or(Value::Null, |position| record[position].clone());
            let declared = types.get(at + 1).copied().flatten();
            row.push(match declared {
                Some(sql_type) if !value.is_null() && value.sql_type() != Some(sql_type) => convert(value, sql_type, Conversion::Explicit)?,
                _ => value,
            });
        }
        sink(row)?;
    }
    Ok(())
}

fn sql_type_of(column_type: ColumnType) -> SqlType {
    match column_type {
        ColumnType::Boolean => SqlType::Boolean,
        ColumnType::Int32 => SqlType::Integer,
        ColumnType::Int64 => SqlType::BigInt,
        ColumnType::Float => SqlType::Real,
        ColumnType::Double => SqlType::Double,
        ColumnType::Text => SqlType::Text,
        ColumnType::Binary => SqlType::Bytea,
        ColumnType::Date => SqlType::Date,
        ColumnType::Timestamp { utc: true } => SqlType::TimestampTz(None),
        ColumnType::Timestamp { utc: false } => SqlType::Timestamp(None),
        ColumnType::Decimal { precision, scale } => SqlType::Numeric(Some(crate::types::NumericModifier {
            precision: u16::try_from(precision).unwrap_or(u16::MAX),
            scale: i16::try_from(scale).unwrap_or(0),
        })),
    }
}

/// The columns of the file a `read_parquet`, `read_arrow` or `read_csv` call with no column
/// definition list reads, as its parser reads them: the file's names and types (all `text` for
/// a CSV file's header).
pub(crate) fn discovered_columns(name: &str, path: &str) -> SqlResult<Vec<(String, SqlType)>> {
    if name == "read_vcf" {
        return Ok(VCF_COLUMNS.iter().map(|(column, sql_type)| ((*column).to_owned(), *sql_type)).collect());
    }
    if name == "read_bam" {
        return Ok(BAM_COLUMNS.iter().map(|(column, sql_type)| ((*column).to_owned(), *sql_type)).collect());
    }
    if name == "read_netcdf" {
        return Ok(NETCDF_COLUMNS.iter().map(|(column, sql_type)| ((*column).to_owned(), *sql_type)).collect());
    }
    if name == "read_geotiff" {
        return Ok(GEOTIFF_COLUMNS.iter().map(|(column, sql_type)| ((*column).to_owned(), *sql_type)).collect());
    }
    if name == "read_csv" {
        let text = text_file(path)?;
        let header_line = text.lines().next().unwrap_or_default();
        let header = csv_records(header_line, delimiter_of(header_line))?.into_iter().next().unwrap_or_default();
        return Ok(header
            .into_iter()
            .map(|column| (column.unwrap_or_default().trim().to_owned(), SqlType::Text))
            .collect());
    }
    let columns = if name == "read_arrow" {
        eggdb_arrow::read(&bytes).map_err(|error| SqlError::new(SqlState::DATA_CORRUPTED, format!("{path}: {error}")))?
    } else {
        eggdb_parquet::read(&bytes).map_err(|error| SqlError::new(SqlState::DATA_CORRUPTED, format!("{path}: {}", error.0)))?
    };
    Ok(columns.iter().map(|column| (column.name.clone(), sql_type_of(column.column_type))).collect())
}

fn delimiter_of(header: &str) -> char {
    [',', ';', '\t', '|']
        .into_iter()
        .max_by_key(|candidate| header.matches(*candidate).count())
        .filter(|candidate| header.contains(*candidate))
        .unwrap_or(',')
}

fn csv_records(text: &str, delimiter: char) -> SqlResult<Vec<Vec<Option<String>>>> {
    let mut records = Vec::new();
    let mut record: Vec<Option<String>> = Vec::new();
    let mut field = String::new();
    let mut quoted_field = false;
    let mut in_quotes = false;
    let mut characters = text.chars().peekable();
    let finish_field = |record: &mut Vec<Option<String>>, field: &mut String, quoted: &mut bool| {
        let value = std::mem::take(field);
        record.push(if value.is_empty() && !*quoted { None } else { Some(value) });
        *quoted = false;
    };
    while let Some(character) = characters.next() {
        if in_quotes {
            if character == '"' {
                if characters.peek() == Some(&'"') {
                    characters.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(character);
            }
            continue;
        }
        match character {
            '"' if field.is_empty() => {
                in_quotes = true;
                quoted_field = true;
            }
            '\r' if characters.peek() == Some(&'\n') => {}
            '\n' => {
                finish_field(&mut record, &mut field, &mut quoted_field);
                records.push(std::mem::take(&mut record));
            }
            found if found == delimiter => finish_field(&mut record, &mut field, &mut quoted_field),
            other => field.push(other),
        }
    }
    if in_quotes {
        return Err(SqlError::new(SqlState::BAD_COPY_FILE_FORMAT, "unterminated CSV quoted field"));
    }
    if !field.is_empty() || quoted_field || !record.is_empty() {
        finish_field(&mut record, &mut field, &mut quoted_field);
        records.push(record);
    }
    Ok(records)
}

/// The rows of `read_csv(path) AS t(name type, ...)`: a CSV file (RFC 4180 quoting) whose first
/// line names its columns and picks its delimiter (`,`, `;`, tab or `|`); each declared column
/// comes from the file's column of that name (ignoring case), an empty unquoted field is `NULL`,
/// and values are cast from text to their declared types.
pub(crate) fn csv_rows(
    values: &[Value],
    names: &[String],
    types: &[Option<SqlType>],
    sink: &mut RecordSink<'_>,
) -> SqlResult<()> {
    let Some(path) = values.first().and_then(Value::to_text) else {
        return Ok(());
    };
    let text = text_file(&path)?;
    let header_line = text.lines().next().unwrap_or_default();
    let mut records = csv_records(&text, delimiter_of(header_line))?.into_iter();
    let header: Vec<String> = records.next().unwrap_or_default().into_iter().map(Option::unwrap_or_default).collect();
    let positions: Vec<Option<usize>> = names
        .iter()
        .map(|name| header.iter().position(|column| column.trim().eq_ignore_ascii_case(name)))
        .collect();
    for record in records {
        if record.len() == 1 && record[0].is_none() {
            continue;
        }
        let mut row = Vec::with_capacity(names.len());
        for (at, position) in positions.iter().enumerate() {
            let text = position.and_then(|position| record.get(position).cloned().flatten());
            let value = text.map_or(Value::Null, Value::Text);
            let declared = types.get(at + 1).copied().flatten();
            row.push(match declared {
                Some(sql_type) if !value.is_null() && !sql_type.is_string() => convert(value, sql_type, Conversion::Explicit)?,
                _ => value,
            });
        }
        sink(row)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::memory_store::MemoryStore;
    use crate::{Dialect, Session};

    struct MemoryFile;

    static FILE: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    static READER: MemoryFile = MemoryFile;

    impl crate::new_files::FileReader for MemoryFile {
        fn read_file(&self, path: &str, _most_bytes: usize) -> Result<Vec<u8>, String> {
            if path != "people.parquet" {
                return Err(format!("{path}: no such file"));
            }
            Ok(FILE.get_or_init(|| {
                eggdb_parquet::write(&[
                    Column {
                        name: "id".to_owned(),
                        column_type: ColumnType::Int64,
                        nullable: false,
                        cells: vec![Cell::Int(1), Cell::Int(2)],
                    },
                    Column {
                        name: "Name".to_owned(),
                        column_type: ColumnType::Text,
                        nullable: true,
                        cells: vec![Cell::Bytes(b"egg".to_vec()), Cell::Null],
                    },
                ])
                .expect("writes")
            })
            .clone())
        }
    }

    #[test]
    fn read_parquet_reads_declared_columns_through_the_session_reader() {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, Dialect::PostgreSql);
        let query = "SELECT id, name, missing FROM read_parquet('people.parquet') AS t(id int, name text, missing text) ORDER BY id";
        assert!(session.execute(query, &mut |_| {}).is_err(), "a session without a reader reads no file");
        session.set_file_reader(&READER);
        let mut rows = Vec::new();
        session.execute(query, &mut |result| rows.extend(result.rows)).expect("reads");
        let mut discovered = Vec::new();
        session
            .execute("SELECT * FROM read_parquet('people.parquet') ORDER BY 1", &mut |result| {
                discovered.extend(result.columns.into_iter().flatten().map(|column| column.name));
            })
            .expect("reads its own columns");
        assert_eq!(discovered, vec!["id".to_owned(), "Name".to_owned()]);
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Text("egg".to_owned()), Value::Null],
                vec![Value::Integer(2), Value::Null, Value::Null],
            ]
        );
    }

    #[test]
    fn csv_fields_quote_escape_and_leave_empty_ones_null() {
        let records = csv_records("a,b,c\n1,\"x, \"\"y\"\"\",\n2,,\"\"\n", ',').unwrap();
        assert_eq!(records[1], vec![Some("1".to_owned()), Some("x, \"y\"".to_owned()), None]);
        assert_eq!(records[2], vec![Some("2".to_owned()), None, Some(String::new())]);
        assert_eq!(delimiter_of("a;b;c"), ';');
        assert!(csv_records("a\n\"open", ',').is_err());
    }

    #[test]
    fn vcf_info_and_samples_become_json() {
        assert_eq!(vcf_info("DP=10;AF=0.5,0.3;DB"), "{\"DP\":\"10\",\"AF\":\"0.5,0.3\",\"DB\":true}");
        assert_eq!(
            vcf_samples(Some("GT:DP"), &["NA1"], &["0|1:."]),
            "{\"NA1\":{\"GT\":\"0|1\",\"DP\":null}}"
        );
    }

    #[test]
    fn bam_tags_read_their_types() {
        let bytes = [b'i', 0x2a, 0, 0, 0];
        let mut cursor = BamCursor { bytes: &bytes[1..], at: 0 };
        assert_eq!(bam_tag_value(&mut cursor, bytes[0]).as_deref(), Some("42"));
        let text = b"RG1\0";
        let mut cursor = BamCursor { bytes: text, at: 0 };
        assert_eq!(bam_tag_value(&mut cursor, b'Z').as_deref(), Some("\"RG1\""));
        let array = [b'C', 2, 0, 0, 0, 7, 9];
        let mut cursor = BamCursor { bytes: &array, at: 0 };
        assert_eq!(bam_tag_value(&mut cursor, b'B').as_deref(), Some("[7,9]"));
    }

    #[test]
    fn decimals_keep_their_scale() {
        assert_eq!(decimal_text(12345, 2), "123.45");
        assert_eq!(decimal_text(-5, 3), "-0.005");
        assert_eq!(decimal_text(7, 0), "7");
    }
}
