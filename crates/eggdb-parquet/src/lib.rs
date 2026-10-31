//! Parquet files for EggDB (D202), written from the public Apache Parquet format
//! specification and Apache Thrift's compact protocol: [`read`] turns a flat file (any number
//! of row groups; plain and dictionary pages, versions 1 and 2; uncompressed or Snappy) into
//! columns of [`Cell`]s, and [`write`] writes columns as an uncompressed file other tools read.
//! Damaged or hostile files are errors, never panics.

mod read;
mod snappy;
mod thrift;
mod write;

pub use read::read;
pub use write::write;

/// The most values a file may hold across its columns, to bound memory.
pub const MAX_VALUES: u64 = 1 << 28;

/// Why a Parquet file could not be read or written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParquetError(pub String);

impl std::fmt::Display for ParquetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ParquetError {}

pub(crate) fn damaged(message: &str) -> ParquetError {
    ParquetError(format!("the Parquet file is damaged: {message}"))
}

pub(crate) fn unsupported(message: &str) -> ParquetError {
    ParquetError(format!("{message} is not supported yet"))
}

/// A column's type, as the file's physical and logical types give it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    /// `BOOLEAN`.
    Boolean,
    /// `INT32`, and the smaller integer logical types.
    Int32,
    /// `INT64`.
    Int64,
    /// `FLOAT`.
    Float,
    /// `DOUBLE`.
    Double,
    /// `BYTE_ARRAY` holding UTF-8 text (`STRING`, `UTF8`, `ENUM`, `JSON`).
    Text,
    /// Other `BYTE_ARRAY` and `FIXED_LEN_BYTE_ARRAY` values.
    Binary,
    /// `DATE`: days since 1970-01-01.
    Date,
    /// `TIMESTAMP`, as microseconds since 1970-01-01 (milliseconds and nanoseconds are
    /// converted, as are legacy `INT96` values); `utc` when the file marks it adjusted to UTC.
    Timestamp {
        /// Whether the values are instants in UTC.
        utc: bool,
    },
    /// `DECIMAL(precision, scale)`, as unscaled integers.
    Decimal {
        /// Total digits.
        precision: u32,
        /// Digits after the point.
        scale: u32,
    },
}

/// One value.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    /// No value.
    Null,
    /// A boolean.
    Boolean(bool),
    /// An integer, date (days) or timestamp (microseconds).
    Int(i64),
    /// A floating point number.
    Float(f64),
    /// Text or binary bytes.
    Bytes(Vec<u8>),
    /// A decimal's unscaled value.
    Decimal(i128),
}

/// A column: its name, type, whether it may be null, and its values in row order.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    /// The name.
    pub name: String,
    /// The type.
    pub column_type: ColumnType,
    /// Whether the column may hold nulls.
    pub nullable: bool,
    /// The values.
    pub cells: Vec<Cell>,
}

#[cfg(test)]
mod tests;
