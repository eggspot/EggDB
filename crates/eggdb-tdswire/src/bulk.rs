//! Reading bulk load data, as [MS-TDS] 2.2.6.1 defines it: what `bcp in` and `SqlBulkCopy`
//! send in a bulk load message (packet type 0x07) after an `INSERT BULK` statement. The data
//! is a COLMETADATA token that describes its columns ([MS-TDS] 2.2.7.4), one ROW or NBCROW
//! token per row and a DONE token. It may be far larger than any other message, so
//! [`BulkReader`] reads it as it arrives, a row at a time, from a [`BulkInput`]. Each value is
//! read by its TYPE_INFO as [`crate::rpc`] reads a parameter, and anything truncated,
//! oversized or inconsistent is an error, never a panic.

use crate::messages::MalformedMessage;
use crate::rpc::{
    ENDS_EARLY, Length, Reader, RpcError, Shape, decode, read_data, read_type_info, utf16_text,
};
use eggdb_sql::Value;

const COLMETADATA: u8 = 0x81;
const ROW: u8 = 0xD1;
const NBCROW: u8 = 0xD2;
const DONE: u8 = 0xFD;
const NO_METADATA: u16 = 0xFFFF;
const ENCRYPTED_COLUMN: u16 = 0x0800;
const DONE_BYTES: usize = 12;
const TEXT_POINTER_LENGTH: u8 = 16;
const TIMESTAMP_BYTES: usize = 8;

/// The most columns a bulk load may describe, as many as a SQL Server table has.
pub const MAX_BULK_COLUMNS: usize = 4096;
/// The largest row a bulk load may send: 64 MiB, the size of the largest SQL batch EggDB
/// accepts.
pub const MAX_BULK_ROW_BYTES: usize = 64 * 1024 * 1024;

/// Why bulk load data could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkLoadError {
    /// The bytes do not follow [MS-TDS], or end before the DONE token.
    Malformed(MalformedMessage),
    /// A column has a TDS type EggDB does not read yet (`sql_variant`, `xml`, user-defined
    /// types and the types older than TDS 7.2), named by its byte.
    UnsupportedType(u8),
    /// A column is encrypted (Always Encrypted), which EggDB does not offer.
    EncryptedColumn,
    /// A row is larger than [`MAX_BULK_ROW_BYTES`].
    RowTooLarge,
}

impl From<RpcError> for BulkLoadError {
    fn from(error: RpcError) -> Self {
        match error {
            RpcError::Malformed(malformed) => Self::Malformed(malformed),
            RpcError::UnsupportedType(kind) => Self::UnsupportedType(kind),
            RpcError::EncryptedParameter => Self::EncryptedColumn,
            RpcError::TooManyParameters => Self::Malformed(MalformedMessage {
                reason: "the bulk load data describes too many columns",
            }),
        }
    }
}

fn malformed(reason: &'static str) -> RpcError {
    RpcError::Malformed(MalformedMessage { reason })
}

/// One column of bulk load data, as its COLMETADATA token describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct BulkColumn {
    name: String,
    shape: Shape,
    length: Length,
}

impl BulkColumn {
    /// The column's name, as the client sent it.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Where the rest of the bulk load data comes from: the packets of the bulk load message
/// that follow the first, as a TDS connection reads them.
pub trait BulkInput {
    /// Appends the next piece of the data to `buffer`; `false`, with nothing appended, once
    /// the data has ended or can no longer be read.
    fn read_more(&mut self, buffer: &mut Vec<u8>) -> bool;
}

impl<T: BulkInput + ?Sized> BulkInput for &mut T {
    fn read_more(&mut self, buffer: &mut Vec<u8>) -> bool {
        (**self).read_more(buffer)
    }
}

/// Reads bulk load data as it arrives: [`BulkReader::read_metadata`] once, then
/// [`BulkReader::next_row`] until it returns `false`. It holds at most about twice the largest
/// row in memory.
pub struct BulkReader<I: BulkInput> {
    input: I,
    buffer: Vec<u8>,
    at: usize,
    columns: Vec<BulkColumn>,
    input_ended: bool,
    finished: bool,
}

impl<I: BulkInput> std::fmt::Debug for BulkReader<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BulkReader")
            .field("columns", &self.columns)
            .field("buffered", &self.buffer.len().saturating_sub(self.at))
            .field("finished", &self.finished)
            .finish()
    }
}

impl<I: BulkInput> BulkReader<I> {
    /// A reader of data that begins with `first`, the bytes already received, and goes on
    /// with what `input` reads.
    pub fn new(first: Vec<u8>, input: I) -> Self {
        Self {
            input,
            buffer: first,
            at: 0,
            columns: Vec::new(),
            input_ended: false,
            finished: false,
        }
    }

    /// Reads the COLMETADATA token the data starts with and returns its columns.
    pub fn read_metadata(&mut self) -> Result<&[BulkColumn], BulkLoadError> {
        let columns = self.parse(read_metadata)?;
        self.columns = columns;
        Ok(&self.columns)
    }

    /// The columns [`BulkReader::read_metadata`] read.
    pub fn columns(&self) -> &[BulkColumn] {
        &self.columns
    }

    /// Reads the next row into `row`, one engine value per column (as [`crate::rpc`] decodes
    /// a parameter of the same TDS type); `false`, with `row` left empty, at the DONE token
    /// that ends the data.
    pub fn next_row(&mut self, row: &mut Vec<Value>) -> Result<bool, BulkLoadError> {
        row.clear();
        if self.finished {
            return Ok(false);
        }
        let columns = std::mem::take(&mut self.columns);
        let parsed = self.parse(|reader| read_row(reader, &columns));
        self.columns = columns;
        match parsed? {
            Some(values) => {
                *row = values;
                Ok(true)
            }
            None => {
                self.finished = true;
                Ok(false)
            }
        }
    }

    fn parse<T>(
        &mut self,
        read: impl Fn(&mut Reader<'_>) -> Result<T, RpcError>,
    ) -> Result<T, BulkLoadError> {
        loop {
            let outcome = {
                let mut reader = Reader {
                    bytes: self.buffer.get(self.at..).unwrap_or_default(),
                    at: 0,
                };
                read(&mut reader).map(|value| (value, reader.at))
            };
            match outcome {
                Ok((value, used)) => {
                    self.at += used;
                    return Ok(value);
                }
                Err(RpcError::Malformed(MalformedMessage { reason })) if reason == ENDS_EARLY => {
                    if !self.more()? {
                        return Err(BulkLoadError::Malformed(MalformedMessage {
                            reason: "the bulk load data ends before its DONE token",
                        }));
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn more(&mut self) -> Result<bool, BulkLoadError> {
        if self.input_ended {
            return Ok(false);
        }
        self.buffer.drain(..self.at.min(self.buffer.len()));
        self.at = 0;
        if self.buffer.len() > MAX_BULK_ROW_BYTES {
            return Err(BulkLoadError::RowTooLarge);
        }
        let before = self.buffer.len();
        let wanted = before.saturating_mul(2).max(1);
        while self.buffer.len() < wanted {
            if !self.input.read_more(&mut self.buffer) {
                self.input_ended = true;
                break;
            }
        }
        Ok(self.buffer.len() > before)
    }
}

fn read_metadata(reader: &mut Reader<'_>) -> Result<Vec<BulkColumn>, RpcError> {
    if reader.byte()? != COLMETADATA {
        return Err(malformed(
            "the bulk load data does not start with column metadata",
        ));
    }
    let count = reader.u16()?;
    if count == NO_METADATA || usize::from(count) > MAX_BULK_COLUMNS {
        return Err(malformed("the bulk load column count is out of range"));
    }
    let mut columns = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        reader.u32()?;
        let flags = reader.u16()?;
        if flags & ENCRYPTED_COLUMN != 0 {
            return Err(RpcError::EncryptedParameter);
        }
        let (shape, length) = read_type_info(reader)?;
        if length == Length::Long {
            let units = usize::from(reader.u16()?);
            reader.take(units * 2)?;
        }
        let units = usize::from(reader.byte()?);
        let name = utf16_text(
            reader.take(units * 2)?,
            "a bulk load column name is not valid UTF-16",
        )?;
        columns.push(BulkColumn {
            name,
            shape,
            length,
        });
    }
    Ok(columns)
}

fn read_value(reader: &mut Reader<'_>, column: &BulkColumn) -> Result<Value, RpcError> {
    let data = if column.length == Length::Long {
        match reader.byte()? {
            0 => None,
            TEXT_POINTER_LENGTH => {
                reader.take(usize::from(TEXT_POINTER_LENGTH) + TIMESTAMP_BYTES)?;
                read_data(reader, Length::Long)?
            }
            _ => {
                return Err(malformed(
                    "a text, ntext or image value has a text pointer of the wrong length",
                ));
            }
        }
    } else {
        read_data(reader, column.length)?
    };
    decode(column.shape, data.as_deref())
}

fn read_row(
    reader: &mut Reader<'_>,
    columns: &[BulkColumn],
) -> Result<Option<Vec<Value>>, RpcError> {
    match reader.byte()? {
        ROW => {
            let mut values = Vec::with_capacity(columns.len());
            for column in columns {
                values.push(read_value(reader, column)?);
            }
            Ok(Some(values))
        }
        NBCROW => {
            let bitmap = reader.take(columns.len().div_ceil(8))?;
            let mut values = Vec::with_capacity(columns.len());
            for (index, column) in columns.iter().enumerate() {
                let null = bitmap
                    .get(index / 8)
                    .is_some_and(|bits| bits & (1 << (index % 8)) != 0);
                values.push(if null {
                    Value::Null
                } else {
                    read_value(reader, column)?
                });
            }
            Ok(Some(values))
        }
        DONE => {
            reader.take(DONE_BYTES)?;
            Ok(None)
        }
        _ => Err(malformed(
            "the bulk load data holds a token other than ROW, NBCROW and DONE",
        )),
    }
}

struct NothingMore;

impl BulkInput for NothingMore {
    fn read_more(&mut self, _buffer: &mut Vec<u8>) -> bool {
        false
    }
}

/// Reads a whole bulk load message held in memory, as [`BulkReader`] reads one as it arrives:
/// its columns and its rows, up to its DONE token.
pub fn parse_bulk_load(body: &[u8]) -> Result<(Vec<BulkColumn>, Vec<Vec<Value>>), BulkLoadError> {
    let mut reader = BulkReader::new(body.to_vec(), NothingMore);
    let columns = reader.read_metadata()?.to_vec();
    let mut rows = Vec::new();
    let mut row = Vec::new();
    while reader.next_row(&mut row)? {
        rows.push(std::mem::take(&mut row));
    }
    Ok((columns, rows))
}

#[cfg(test)]
mod tests {
    use super::{BulkInput, BulkLoadError, BulkReader, parse_bulk_load};
    use eggdb_sql::{Numeric, Value};

    fn utf16(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn column(name: &str, type_info: &[u8]) -> Vec<u8> {
        let mut out = vec![0, 0, 0, 0, 0x09, 0x00];
        out.extend_from_slice(type_info);
        out.push(name.encode_utf16().count() as u8);
        out.extend(utf16(name));
        out
    }

    fn nvarchar_value(text: &str) -> Vec<u8> {
        let bytes = utf16(text);
        let mut out = (bytes.len() as u16).to_le_bytes().to_vec();
        out.extend(bytes);
        out
    }

    fn sample() -> Vec<u8> {
        let mut body = vec![0x81, 3, 0];
        body.extend(column("id", &[0x26, 4]));
        body.extend(column("name", &[0xE7, 80, 0, 0x09, 0x04, 0x00, 0x22, 0x00]));
        body.extend(column("price", &[0x6A, 5, 10, 2]));
        body.push(0xD1);
        body.extend_from_slice(&[4, 1, 0, 0, 0]);
        body.extend(nvarchar_value("bread"));
        body.extend_from_slice(&[5, 1, 0xE2, 0x04, 0, 0]);
        body.push(0xD2);
        body.push(0b0000_0110);
        body.extend_from_slice(&[4, 2, 0, 0, 0]);
        body.extend_from_slice(&[0xFD, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        body
    }

    #[test]
    fn metadata_rows_and_null_bitmaps_read_as_engine_values() {
        let (columns, rows) = parse_bulk_load(&sample()).unwrap();
        let names: Vec<&str> = columns.iter().map(|column| column.name()).collect();
        assert_eq!(names, ["id", "name", "price"]);
        assert_eq!(
            rows,
            vec![
                vec![
                    Value::Integer(1),
                    Value::Text("bread".into()),
                    Value::Numeric(Numeric::parse("12.50").unwrap()),
                ],
                vec![Value::Integer(2), Value::Null, Value::Null],
            ]
        );
    }

    struct Packets {
        pieces: Vec<Vec<u8>>,
    }

    impl BulkInput for Packets {
        fn read_more(&mut self, buffer: &mut Vec<u8>) -> bool {
            if self.pieces.is_empty() {
                return false;
            }
            buffer.extend(self.pieces.remove(0));
            true
        }
    }

    #[test]
    fn data_split_anywhere_reads_the_same_as_whole() {
        let whole = sample();
        let expected = parse_bulk_load(&whole).unwrap().1;
        for size in [1, 2, 3, 7, 64] {
            let pieces: Vec<Vec<u8>> = whole.chunks(size).map(<[u8]>::to_vec).collect();
            let mut reader = BulkReader::new(Vec::new(), Packets { pieces });
            reader.read_metadata().unwrap();
            let mut rows = Vec::new();
            let mut row = Vec::new();
            while reader.next_row(&mut row).unwrap() {
                rows.push(row.clone());
            }
            assert_eq!(rows, expected, "pieces of {size}");
        }
    }

    #[test]
    fn truncated_and_inconsistent_data_are_errors_never_panics() {
        let whole = sample();
        for cut in 0..whole.len() {
            assert!(parse_bulk_load(&whole[..cut]).is_err(), "cut at {cut}");
        }
        let mut variant = vec![0x81, 1, 0];
        variant.extend(column("v", &[0x62, 0, 0, 0, 0]));
        assert_eq!(
            parse_bulk_load(&variant),
            Err(BulkLoadError::UnsupportedType(0x62))
        );
        let mut encrypted = vec![0x81, 1, 0, 0, 0, 0, 0, 0x01, 0x08, 0x26, 4];
        encrypted.extend_from_slice(&[1, b'e', 0]);
        assert_eq!(
            parse_bulk_load(&encrypted),
            Err(BulkLoadError::EncryptedColumn)
        );
        assert!(parse_bulk_load(&[0xD1, 0, 0]).is_err());
        assert!(parse_bulk_load(&[0x81, 0xFF, 0xFF]).is_err());
    }
}
