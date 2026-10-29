use super::BulkRows;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::Value;
use std::io::Read;

const READ_CHUNK_BYTES: usize = 64 * 1024;
const MAX_ROW_BYTES: usize = 1 << 30;
const UNEXPECTED_END_OF_FILE: u16 = 4832;
const CONVERSION_ERROR: u16 = 4864;
const FIELD_TOO_LONG: u16 = 4866;

/// The terminators of a file in bcp's character format (`bcp ... -c`): what ends each field
/// but the last of a row (`-t`, a tab by default) and what ends each row (`-r`, a newline by
/// default). Each is one or more bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BcpTerminators {
    field: Vec<u8>,
    row: Vec<u8>,
}

impl Default for BcpTerminators {
    fn default() -> Self {
        Self {
            field: b"\t".to_vec(),
            row: b"\n".to_vec(),
        }
    }
}

fn written_terminator(text: &str, option: &str) -> SqlResult<Vec<u8>> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut characters = text.bytes();
    while let Some(byte) = characters.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        bytes.push(match characters.next() {
            Some(b't') => b'\t',
            Some(b'n') => b'\n',
            Some(b'r') => b'\r',
            Some(b'0') => 0,
            Some(b'\\') => b'\\',
            Some(other) => {
                bytes.push(b'\\');
                other
            }
            None => b'\\',
        });
    }
    if bytes.is_empty() {
        return Err(SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("the {option} terminator is empty"),
        ));
    }
    Ok(bytes)
}

impl BcpTerminators {
    /// The terminators as bcp's `-t` and `-r` options write them, with bcp's escapes: `\t`
    /// (tab), `\n` (newline), `\r` (carriage return), `\0` (the NUL byte) and `\\`; any other
    /// character stands for itself. `None` keeps the default. An empty terminator is an error.
    pub fn written(field: Option<&str>, row: Option<&str>) -> SqlResult<Self> {
        let default = Self::default();
        Ok(Self {
            field: match field {
                Some(text) => written_terminator(text, "field")?,
                None => default.field,
            },
            row: match row {
                Some(text) => written_terminator(text, "row")?,
                None => default.row,
            },
        })
    }
}

/// Reads a data file in bcp's character format (`bcp ... out -c`, the format `bcp ... in -c`
/// loads) as [`BulkRows`]: each row holds one field per column, every field but the last
/// ended by the field terminator and the last by the row terminator, with no quoting or
/// escaping. An empty field is NULL (its column's default unless the load keeps NULLs, as
/// `bcp -k`), and a field that is only the NUL byte is an empty string, as bcp writes them.
/// With the default newline row terminator, a carriage return before it is dropped too, so
/// files written with Windows line ends load. The text is UTF-8 (`bcp -C 65001`); the last
/// row may end without its row terminator; a row may hold up to 1 GiB.
pub struct BcpCharacterReader<'r> {
    input: &'r mut dyn Read,
    terminators: BcpTerminators,
    buffer: Vec<u8>,
    start: usize,
    ended: bool,
    rows_read: u64,
    rows_to_skip: u64,
}

impl std::fmt::Debug for BcpCharacterReader<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BcpCharacterReader")
            .field("terminators", &self.terminators)
            .field("rows_read", &self.rows_read)
            .finish()
    }
}

impl<'r> BcpCharacterReader<'r> {
    /// A reader of `input`, a whole data file, with these terminators.
    pub fn new(input: &'r mut dyn Read, terminators: BcpTerminators) -> Self {
        Self {
            input,
            terminators,
            buffer: Vec::new(),
            start: 0,
            ended: false,
            rows_read: 0,
            rows_to_skip: 0,
        }
    }

    /// Skips the file's first `rows` rows, such as a header line, as bcp's `-F` option does
    /// (`-F 2` skips one).
    pub fn skipping(mut self, rows: u64) -> Self {
        self.rows_to_skip = rows;
        self
    }

    /// How many rows have been read so far, skipped rows included.
    pub fn rows_read(&self) -> u64 {
        self.rows_read
    }

    fn fill(&mut self) -> SqlResult<bool> {
        if self.ended {
            return Ok(false);
        }
        let at = self.buffer.len();
        self.buffer.resize(at + READ_CHUNK_BYTES, 0);
        loop {
            match self.input.read(&mut self.buffer[at..]) {
                Ok(count) => {
                    self.buffer.truncate(at + count);
                    if count == 0 {
                        self.ended = true;
                    }
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

    fn find(&mut self, row_start: usize, terminator: &[u8]) -> SqlResult<Option<usize>> {
        let mut from = self.start;
        loop {
            if let Some(offset) = self
                .buffer
                .get(from..)
                .unwrap_or_default()
                .windows(terminator.len())
                .position(|window| window == terminator)
            {
                return Ok(Some(from + offset));
            }
            if self.buffer.len() - row_start > MAX_ROW_BYTES {
                return Err(SqlError::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    format!(
                        "The bulk load failed. The column is too long in the data file for row {}. Verify that the field terminator and row terminator are specified correctly.",
                        self.rows_read + 1
                    ),
                )
                .with_sqlserver_code(FIELD_TOO_LONG));
            }
            from = self
                .buffer
                .len()
                .saturating_sub(terminator.len() - 1)
                .max(self.start);
            if !self.fill()? {
                return Ok(None);
            }
        }
    }

    fn value(&self, field: std::ops::Range<usize>, column: usize, last: bool) -> SqlResult<Value> {
        let mut bytes = self.buffer.get(field).unwrap_or_default();
        if last
            && self.terminators.row == b"\n"
            && let Some(without) = bytes.strip_suffix(b"\r")
        {
            bytes = without;
        }
        match bytes {
            [] => Ok(Value::Null),
            [0] => Ok(Value::Text(String::new())),
            text => match std::str::from_utf8(text) {
                Ok(text) => Ok(Value::Text(text.to_owned())),
                Err(_) => Err(SqlError::new(
                    SqlState::CHARACTER_NOT_IN_REPERTOIRE,
                    format!(
                        "Bulk load data conversion error (type mismatch or invalid character for the specified codepage) for row {}, column {}.",
                        self.rows_read + 1,
                        column + 1
                    ),
                )
                .with_sqlserver_code(CONVERSION_ERROR)
                .with_hint("The data file must be UTF-8: write it with bcp ... -c -C 65001.")),
            },
        }
    }

    fn read_row(&mut self, columns: usize, row: &mut Vec<Value>) -> SqlResult<bool> {
        row.clear();
        if columns == 0 || self.at_end()? {
            return Ok(false);
        }
        if self.start >= READ_CHUNK_BYTES {
            self.buffer.drain(..self.start);
            self.start = 0;
        }
        let row_start = self.start;
        for column in 0..columns {
            let last = column + 1 == columns;
            let terminator = if last {
                self.terminators.row.clone()
            } else {
                self.terminators.field.clone()
            };
            let (end, next) = match self.find(row_start, &terminator)? {
                Some(end) => (end, end + terminator.len()),
                None if last => (self.buffer.len(), self.buffer.len()),
                None => {
                    return Err(SqlError::new(
                        SqlState::BAD_COPY_FILE_FORMAT,
                        "Bulk load: An unexpected end of file was encountered in the data file.",
                    )
                    .with_sqlserver_code(UNEXPECTED_END_OF_FILE)
                    .with_context(format!(
                        "bcp data file, row {}, column {}",
                        self.rows_read + 1,
                        column + 1
                    )));
                }
            };
            row.push(self.value(self.start..end, column, last)?);
            self.start = next;
        }
        self.rows_read += 1;
        Ok(true)
    }
}

impl BulkRows for BcpCharacterReader<'_> {
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

#[cfg(test)]
mod tests {
    use super::{BcpCharacterReader, BcpTerminators};
    use crate::bulk_load::BulkRows;
    use crate::types::Value;

    fn rows(data: &[u8], terminators: BcpTerminators, columns: usize) -> Vec<Vec<Value>> {
        let mut input = data;
        let mut reader = BcpCharacterReader::new(&mut input, terminators);
        let mut all = Vec::new();
        let mut row = Vec::new();
        while reader.next_row(columns, &mut row).unwrap() {
            all.push(row.clone());
        }
        all
    }

    fn text(value: &str) -> Value {
        Value::Text(value.to_owned())
    }

    #[test]
    fn fields_end_at_tabs_and_rows_at_newlines_with_empty_as_null_and_nul_as_empty() {
        assert_eq!(
            rows(
                b"1\tbread\t12.50\r\n2\t\t\0\n3\tpie\t",
                BcpTerminators::default(),
                3
            ),
            vec![
                vec![text("1"), text("bread"), text("12.50")],
                vec![text("2"), Value::Null, text("")],
                vec![text("3"), text("pie"), Value::Null],
            ]
        );
        assert_eq!(
            rows(b"a\tb\nc\td\n", BcpTerminators::default(), 2).len(),
            2
        );
        assert!(rows(b"", BcpTerminators::default(), 2).is_empty());
    }

    #[test]
    fn terminators_take_bcps_escapes_and_may_be_longer_than_one_byte() {
        let terminators = BcpTerminators::written(Some("|~|"), Some("\\r\\n")).unwrap();
        assert_eq!(
            rows(b"x|~|y\r\nwith\ttab|~|z\r\n", terminators, 2),
            vec![
                vec![text("x"), text("y")],
                vec![text("with\ttab"), text("z")]
            ]
        );
        let comma = BcpTerminators::written(Some(","), None).unwrap();
        assert_eq!(
            rows(b"1,a,b\n", comma, 2),
            vec![vec![text("1"), text("a,b")]]
        );
        assert!(BcpTerminators::written(Some(""), None).is_err());
    }

    #[test]
    fn a_row_cut_short_is_an_error_and_a_header_can_be_skipped() {
        let mut input: &[u8] = b"1\tx\n2";
        let mut reader = BcpCharacterReader::new(&mut input, BcpTerminators::default());
        let mut row = Vec::new();
        assert!(reader.next_row(3, &mut row).is_err());
        let mut input: &[u8] = b"id\tname\n7\tseven\n";
        let mut reader =
            BcpCharacterReader::new(&mut input, BcpTerminators::default()).skipping(1);
        assert!(reader.next_row(2, &mut row).unwrap());
        assert_eq!(row, vec![text("7"), text("seven")]);
        assert!(!reader.next_row(2, &mut row).unwrap());
        assert_eq!(reader.rows_read(), 2);
        let mut invalid: &[u8] = b"\xff\n";
        let mut reader = BcpCharacterReader::new(&mut invalid, BcpTerminators::default());
        assert_eq!(
            reader.next_row(1, &mut row).unwrap_err().vendor_code(),
            Some(4864)
        );
    }

    #[test]
    fn rows_longer_than_a_read_chunk_load_whole() {
        let long = "x".repeat(200_000);
        let data = format!("1\t{long}\n2\tshort\n");
        let read = rows(data.as_bytes(), BcpTerminators::default(), 2);
        assert_eq!(read.len(), 2);
        assert_eq!(read[0][1], Value::Text(long));
        assert_eq!(read[1][1], text("short"));
    }
}
