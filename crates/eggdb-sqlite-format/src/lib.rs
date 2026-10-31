//! Reads database files in SQLite's file format, version 3, as SQLite's published "Database File
//! Format" description lays them out: the 100-byte header, table b-tree pages, records and their
//! serial types, overflow pages, the `sqlite_schema` table, and the pages of a write-ahead log up
//! to its last commit. Every length, offset and page number read from the file is checked
//! before it is used, and every walk is bounded, so a hostile file is an error, never a panic or
//! a hang. Writing the format is not part of this crate yet.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;

const MAGIC: &[u8; 16] = b"SQLite format 3\0";
const HEADER_BYTES: usize = 100;
const WAL_MAGIC_LITTLE: u32 = 0x377f_0682;
const WAL_MAGIC_BIG: u32 = 0x377f_0683;
const WAL_HEADER_BYTES: usize = 32;
const WAL_FRAME_HEADER_BYTES: usize = 24;
const TABLE_LEAF: u8 = 13;
const TABLE_INTERIOR: u8 = 5;
const INDEX_LEAF: u8 = 10;
const INDEX_INTERIOR: u8 = 2;
const MOST_DEPTH: usize = 64;
const MOST_FILE_BYTES: u64 = 1 << 40;

/// Why a file could not be read as an SQLite database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqliteFormatError {
    /// The file could not be read.
    Io(String),
    /// The file does not start with SQLite's header.
    NotSqlite,
    /// The file uses a part of the format this reader does not read yet.
    Unsupported(String),
    /// The file breaks the format.
    Damaged(String),
}

impl fmt::Display for SqliteFormatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) => write!(formatter, "{message}"),
            Self::NotSqlite => formatter.write_str("the file is not an SQLite database"),
            Self::Unsupported(what) => write!(formatter, "{what} is not read yet"),
            Self::Damaged(problem) => write!(formatter, "the SQLite file is damaged: {problem}"),
        }
    }
}

impl std::error::Error for SqliteFormatError {}

type Result<T> = std::result::Result<T, SqliteFormatError>;

fn damaged(problem: impl Into<String>) -> SqliteFormatError {
    SqliteFormatError::Damaged(problem.into())
}

/// A value of an SQLite record.
#[derive(Debug, Clone, PartialEq)]
pub enum SqliteValue {
    /// NULL.
    Null,
    /// An integer.
    Integer(i64),
    /// A floating-point number.
    Real(f64),
    /// Text, decoded as UTF-8.
    Text(String),
    /// A blob.
    Blob(Vec<u8>),
}

/// One row of the `sqlite_schema` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaEntry {
    /// `table`, `index`, `view` or `trigger`.
    pub kind: String,
    /// The object's name.
    pub name: String,
    /// The table it belongs to.
    pub table: String,
    /// Its root page, 0 for views and triggers.
    pub root_page: u32,
    /// The `CREATE` statement, absent for indexes SQLite makes by itself.
    pub sql: Option<String>,
}

/// One row of a table: its rowid and its record's values.
#[derive(Debug, Clone, PartialEq)]
pub struct SqliteRow {
    /// The rowid.
    pub rowid: i64,
    /// The record's values, in column order.
    pub values: Vec<SqliteValue>,
}

/// An SQLite database file read whole into memory, with its write-ahead log's committed pages
/// laid over it.
#[derive(Debug)]
pub struct SqliteFile {
    bytes: Vec<u8>,
    page_size: usize,
    usable: usize,
    page_count: u32,
    wal_pages: HashMap<u32, Vec<u8>>,
}

/// Whether `bytes` start with SQLite's header.
pub fn is_sqlite(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

fn u16_at(bytes: &[u8], at: usize) -> Result<u16> {
    let slice = bytes.get(at..at + 2).ok_or_else(|| damaged("a page is cut short"))?;
    Ok(u16::from_be_bytes([slice[0], slice[1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32> {
    let slice = bytes.get(at..at + 4).ok_or_else(|| damaged("a page is cut short"))?;
    Ok(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn varint(bytes: &[u8], at: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    for index in 0..9 {
        let byte = *bytes
            .get(*at)
            .ok_or_else(|| damaged("a varint runs past its page"))?;
        *at += 1;
        if index == 8 {
            return Ok((value << 8) | u64::from(byte));
        }
        value = (value << 7) | u64::from(byte & 0x7f);
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Ok(value)
}

fn wal_checksum(words: &[u8], big_endian: bool, mut sums: (u32, u32)) -> (u32, u32) {
    for pair in words.chunks_exact(8) {
        let word = |bytes: &[u8]| {
            let array = [bytes[0], bytes[1], bytes[2], bytes[3]];
            if big_endian {
                u32::from_be_bytes(array)
            } else {
                u32::from_le_bytes(array)
            }
        };
        sums.0 = sums.0.wrapping_add(word(&pair[..4]).wrapping_add(sums.1));
        sums.1 = sums.1.wrapping_add(word(&pair[4..]).wrapping_add(sums.0));
    }
    sums
}

fn read_wal(bytes: &[u8], page_size: usize) -> Result<HashMap<u32, Vec<u8>>> {
    let mut pages = HashMap::new();
    if bytes.len() < WAL_HEADER_BYTES {
        return Ok(pages);
    }
    let magic = u32_at(bytes, 0)?;
    let big_endian = match magic {
        WAL_MAGIC_BIG => true,
        WAL_MAGIC_LITTLE => false,
        _ => return Ok(pages),
    };
    let wal_page_size = match u32_at(bytes, 8)? {
        1 => 65_536,
        size => size as usize,
    };
    if wal_page_size != page_size {
        return Ok(pages);
    }
    let salts = (u32_at(bytes, 16)?, u32_at(bytes, 20)?);
    let mut sums = wal_checksum(&bytes[..24], big_endian, (0, 0));
    if sums != (u32_at(bytes, 24)?, u32_at(bytes, 28)?) {
        return Ok(pages);
    }
    let frame_bytes = WAL_FRAME_HEADER_BYTES + page_size;
    let mut pending: Vec<(u32, &[u8])> = Vec::new();
    let mut at = WAL_HEADER_BYTES;
    while at + frame_bytes <= bytes.len() {
        let header = &bytes[at..at + WAL_FRAME_HEADER_BYTES];
        let page = &bytes[at + WAL_FRAME_HEADER_BYTES..at + frame_bytes];
        if (u32_at(header, 8)?, u32_at(header, 12)?) != salts {
            break;
        }
        sums = wal_checksum(&header[..8], big_endian, sums);
        sums = wal_checksum(page, big_endian, sums);
        if sums != (u32_at(header, 16)?, u32_at(header, 20)?) {
            break;
        }
        let page_number = u32_at(header, 0)?;
        if page_number == 0 {
            break;
        }
        pending.push((page_number, page));
        if u32_at(header, 4)? != 0 {
            for (number, page) in pending.drain(..) {
                pages.insert(number, page.to_vec());
            }
        }
        at += frame_bytes;
    }
    Ok(pages)
}

impl SqliteFile {
    /// Reads the file at `path`, and its write-ahead log `<path>-wal` when there is one.
    pub fn open(path: &Path) -> Result<Self> {
        let size = std::fs::metadata(path)
            .map_err(|error| SqliteFormatError::Io(error.to_string()))?
            .len();
        if size > MOST_FILE_BYTES {
            return Err(SqliteFormatError::Unsupported(
                "a file larger than 1 TiB".to_owned(),
            ));
        }
        let bytes = std::fs::read(path).map_err(|error| SqliteFormatError::Io(error.to_string()))?;
        let mut wal_path = path.as_os_str().to_owned();
        wal_path.push("-wal");
        let wal = std::fs::read(Path::new(&wal_path)).unwrap_or_default();
        Self::from_bytes(bytes, &wal)
    }

    /// Reads a database from its bytes and its write-ahead log's bytes (empty without one).
    pub fn from_bytes(bytes: Vec<u8>, wal: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_BYTES || !is_sqlite(&bytes) {
            return Err(SqliteFormatError::NotSqlite);
        }
        let page_size = match u16_at(&bytes, 16)? {
            1 => 65_536,
            size if size >= 512 && size.is_power_of_two() => usize::from(size),
            _ => return Err(damaged("the page size is not a power of two from 512 to 65536")),
        };
        let reserved = usize::from(bytes[20]);
        let usable = page_size
            .checked_sub(reserved)
            .filter(|usable| *usable >= 480)
            .ok_or_else(|| damaged("too much reserved space at the end of each page"))?;
        match u32_at(&bytes, 56)? {
            0 | 1 => {}
            2 | 3 => {
                return Err(SqliteFormatError::Unsupported(
                    "a database in UTF-16".to_owned(),
                ));
            }
            _ => return Err(damaged("an unknown text encoding")),
        }
        let pages_in_file = u32::try_from(bytes.len() / page_size).unwrap_or(u32::MAX);
        let counter = u32_at(&bytes, 24)?;
        let valid_for = u32_at(&bytes, 92)?;
        let declared = u32_at(&bytes, 28)?;
        let mut page_count = if declared != 0 && counter == valid_for {
            declared.min(pages_in_file)
        } else {
            pages_in_file
        };
        let wal_pages = read_wal(wal, page_size)?;
        if let Some(&highest) = wal_pages.keys().max() {
            page_count = page_count.max(highest);
        }
        Ok(Self {
            bytes,
            page_size,
            usable,
            page_count,
            wal_pages,
        })
    }

    /// The page size in bytes.
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    fn page(&self, number: u32) -> Result<&[u8]> {
        if number == 0 || number > self.page_count {
            return Err(damaged(format!("page {number} lies outside the file")));
        }
        if let Some(page) = self.wal_pages.get(&number) {
            return Ok(page);
        }
        let past_end = || damaged(format!("page {number} lies past the end of the file"));
        let start = (number as usize - 1)
            .checked_mul(self.page_size)
            .ok_or_else(past_end)?;
        self.bytes
            .get(start..start + self.page_size)
            .ok_or_else(past_end)
    }

    /// The rows of `sqlite_schema`.
    pub fn schema(&self) -> Result<Vec<SchemaEntry>> {
        let mut entries = Vec::new();
        for row in self.rows(1)? {
            let text = |index: usize| match row.values.get(index) {
                Some(SqliteValue::Text(text)) => Some(text.clone()),
                _ => None,
            };
            let root_page = match row.values.get(3) {
                Some(SqliteValue::Integer(page)) => u32::try_from(*page).unwrap_or(0),
                _ => 0,
            };
            entries.push(SchemaEntry {
                kind: text(0).unwrap_or_default(),
                name: text(1).unwrap_or_default(),
                table: text(2).unwrap_or_default(),
                root_page,
                sql: text(4),
            });
        }
        Ok(entries)
    }

    /// Every row of the table whose b-tree's root is `root_page`, in rowid order.
    pub fn rows(&self, root_page: u32) -> Result<Vec<SqliteRow>> {
        let mut rows = Vec::new();
        let mut visited = HashSet::new();
        self.walk_table(root_page, 0, &mut visited, &mut rows)?;
        Ok(rows)
    }

    fn walk_table(
        &self,
        number: u32,
        depth: usize,
        visited: &mut HashSet<u32>,
        rows: &mut Vec<SqliteRow>,
    ) -> Result<()> {
        if depth > MOST_DEPTH || !visited.insert(number) {
            return Err(damaged("a b-tree loops or is too deep"));
        }
        let page = self.page(number)?;
        let start = if number == 1 { HEADER_BYTES } else { 0 };
        let kind = *page.get(start).ok_or_else(|| damaged("an empty page"))?;
        let cells = usize::from(u16_at(page, start + 3)?);
        let (header, interior) = match kind {
            TABLE_LEAF => (8, false),
            TABLE_INTERIOR => (12, true),
            INDEX_LEAF | INDEX_INTERIOR => {
                return Err(SqliteFormatError::Unsupported(
                    "a WITHOUT ROWID table".to_owned(),
                ));
            }
            _ => return Err(damaged(format!("page {number} has an unknown b-tree type {kind}"))),
        };
        for cell in 0..cells {
            let offset = usize::from(u16_at(page, start + header + cell * 2)?);
            if offset >= self.usable {
                return Err(damaged("a cell pointer lies past the usable page"));
            }
            if interior {
                let child = u32_at(page, offset)?;
                self.walk_table(child, depth + 1, visited, rows)?;
            } else {
                rows.push(self.table_leaf_cell(page, offset)?);
            }
        }
        if interior {
            let right = u32_at(page, start + 8)?;
            self.walk_table(right, depth + 1, visited, rows)?;
        }
        Ok(())
    }

    fn table_leaf_cell(&self, page: &[u8], offset: usize) -> Result<SqliteRow> {
        let mut at = offset;
        let payload_bytes = varint(page, &mut at)?;
        let rowid = varint(page, &mut at)? as i64;
        let payload_bytes = usize::try_from(payload_bytes)
            .ok()
            .filter(|bytes| *bytes <= (1 << 30) + 1024)
            .ok_or_else(|| damaged("a record is larger than any SQLite record can be"))?;
        let payload = self.payload(page, at, payload_bytes)?;
        Ok(SqliteRow {
            rowid,
            values: record(&payload)?,
        })
    }

    fn payload(&self, page: &[u8], at: usize, total: usize) -> Result<Vec<u8>> {
        let usable = self.usable;
        let inline_most = usable - 35;
        let local = if total <= inline_most {
            total
        } else {
            let least = ((usable - 12) * 32 / 255) - 23;
            let candidate = least + ((total - least) % (usable - 4));
            if candidate <= inline_most { candidate } else { least }
        };
        let mut payload = Vec::with_capacity(total.min(1 << 24));
        payload.extend_from_slice(
            page.get(at..at + local)
                .ok_or_else(|| damaged("a record runs past its page"))?,
        );
        if local == total {
            return Ok(payload);
        }
        let mut next = u32_at(page, at + local)?;
        let mut seen = HashSet::new();
        while payload.len() < total {
            if !seen.insert(next) {
                return Err(damaged("an overflow chain loops"));
            }
            let overflow = self.page(next)?;
            let take = (total - payload.len()).min(usable - 4);
            payload.extend_from_slice(
                overflow
                    .get(4..4 + take)
                    .ok_or_else(|| damaged("an overflow page is cut short"))?,
            );
            next = u32_at(overflow, 0)?;
        }
        Ok(payload)
    }
}

fn record(payload: &[u8]) -> Result<Vec<SqliteValue>> {
    let mut at = 0;
    let header_bytes = usize::try_from(varint(payload, &mut at)?)
        .ok()
        .filter(|bytes| *bytes <= payload.len())
        .ok_or_else(|| damaged("a record's header is longer than the record"))?;
    let mut types = Vec::new();
    while at < header_bytes {
        types.push(varint(payload, &mut at)?);
    }
    let mut body = header_bytes;
    let mut values = Vec::with_capacity(types.len());
    for serial in types {
        let (value, width) = value_of(serial, payload.get(body..).unwrap_or_default())?;
        body += width;
        values.push(value);
    }
    Ok(values)
}

fn signed(bytes: &[u8]) -> i64 {
    let mut value: i64 = if bytes.first().is_some_and(|byte| byte & 0x80 != 0) { -1 } else { 0 };
    for byte in bytes {
        value = (value << 8) | i64::from(*byte);
    }
    value
}

fn value_of(serial: u64, rest: &[u8]) -> Result<(SqliteValue, usize)> {
    let take = |width: usize| -> Result<&[u8]> {
        rest.get(..width)
            .ok_or_else(|| damaged("a record's value runs past the record"))
    };
    Ok(match serial {
        0 => (SqliteValue::Null, 0),
        1..=4 => {
            let width = serial as usize;
            (SqliteValue::Integer(signed(take(width)?)), width)
        }
        5 => (SqliteValue::Integer(signed(take(6)?)), 6),
        6 => (SqliteValue::Integer(signed(take(8)?)), 8),
        7 => {
            let bytes = take(8)?;
            let mut array = [0u8; 8];
            array.copy_from_slice(bytes);
            (SqliteValue::Real(f64::from_be_bytes(array)), 8)
        }
        8 => (SqliteValue::Integer(0), 0),
        9 => (SqliteValue::Integer(1), 0),
        10 | 11 => return Err(damaged("a record uses a reserved serial type")),
        even if even % 2 == 0 => {
            let width = usize::try_from((even - 12) / 2)
                .map_err(|_| damaged("a blob is too long"))?;
            (SqliteValue::Blob(take(width)?.to_vec()), width)
        }
        odd => {
            let width = usize::try_from((odd - 13) / 2)
                .map_err(|_| damaged("a text is too long"))?;
            let text = String::from_utf8_lossy(take(width)?).into_owned();
            (SqliteValue::Text(text), width)
        }
    })
}

#[cfg(test)]
mod tests;
