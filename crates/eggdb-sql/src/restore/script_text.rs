use crate::error::{SqlError, SqlResult, SqlState};
use std::io::BufRead;

const MAX_LINE_BYTES: usize = 1 << 30;
const UTF8_BYTE_ORDER_MARK: &[u8] = b"\xEF\xBB\xBF";
const UTF16LE_BYTE_ORDER_MARK: &[u8] = b"\xFF\xFE";
const UTF16BE_BYTE_ORDER_MARK: &[u8] = b"\xFE\xFF";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScriptEncoding {
    Utf8,
    Utf16Le,
}

pub(crate) struct DecodedLines<'r> {
    input: Box<dyn BufRead + 'r>,
    encoding: Option<ScriptEncoding>,
    pending: Vec<u8>,
    pending_at: usize,
    high_surrogate: Option<u16>,
    line_number: u64,
    max_line: usize,
}

impl<'r> DecodedLines<'r> {
    pub(crate) fn new(input: &'r mut dyn BufRead) -> Self {
        Self::owning(Box::new(input))
    }

    pub(crate) fn owning(input: Box<dyn BufRead + 'r>) -> Self {
        Self::with_max_line(input, MAX_LINE_BYTES)
    }

    pub(crate) fn with_max_line(input: Box<dyn BufRead + 'r>, max_line: usize) -> Self {
        Self {
            input,
            encoding: None,
            pending: Vec::new(),
            pending_at: 0,
            high_surrogate: None,
            line_number: 0,
            max_line,
        }
    }

    pub(crate) fn line_number(&self) -> u64 {
        self.line_number
    }

    pub(crate) fn encoding(&mut self) -> SqlResult<ScriptEncoding> {
        if let Some(encoding) = self.encoding {
            return Ok(encoding);
        }
        let mut start = Vec::with_capacity(3);
        while start.len() < 3 {
            match self.input_byte()? {
                Some(byte) => start.push(byte),
                None => break,
            }
        }
        let (encoding, mark) = if start.starts_with(UTF8_BYTE_ORDER_MARK) {
            (ScriptEncoding::Utf8, UTF8_BYTE_ORDER_MARK.len())
        } else if start.starts_with(UTF16LE_BYTE_ORDER_MARK) {
            (ScriptEncoding::Utf16Le, UTF16LE_BYTE_ORDER_MARK.len())
        } else if start.starts_with(UTF16BE_BYTE_ORDER_MARK) {
            return Err(SqlError::new(
                SqlState::CHARACTER_NOT_IN_REPERTOIRE,
                "the script is UTF-16 big-endian text, which eggdb restore does not read yet",
            )
            .with_hint(
                "Save it again as Unicode (UTF-16 little-endian) or UTF-8, or convert it with iconv -f UTF-16BE -t UTF-8.",
            ));
        } else {
            (ScriptEncoding::Utf8, 0)
        };
        self.pending = start.split_off(mark);
        self.pending_at = 0;
        self.encoding = Some(encoding);
        Ok(encoding)
    }

    pub(crate) fn next_line(&mut self, line: &mut String) -> SqlResult<bool> {
        line.clear();
        let read = match self.encoding()? {
            ScriptEncoding::Utf8 => self.utf8_line(line)?,
            ScriptEncoding::Utf16Le => self.utf16_line(line)?,
        };
        if read {
            self.line_number += 1;
        }
        Ok(read)
    }

    fn input_byte(&mut self) -> SqlResult<Option<u8>> {
        loop {
            let available = match self.input.fill_buf() {
                Ok(available) => available,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(read_failure(&error)),
            };
            let Some(&byte) = available.first() else {
                return Ok(None);
            };
            self.input.consume(1);
            return Ok(Some(byte));
        }
    }

    fn next_byte(&mut self) -> SqlResult<Option<u8>> {
        if let Some(&byte) = self.pending.get(self.pending_at) {
            self.pending_at += 1;
            return Ok(Some(byte));
        }
        self.input_byte()
    }

    fn utf8_line(&mut self, line: &mut String) -> SqlResult<bool> {
        let mut bytes = std::mem::take(line).into_bytes();
        let number = self.line_number + 1;
        let mut done = false;
        while let Some(&byte) = self.pending.get(self.pending_at) {
            self.pending_at += 1;
            bytes.push(byte);
            if byte == b'\n' {
                done = true;
                break;
            }
        }
        while !done {
            let available = match self.input.fill_buf() {
                Ok(available) => available,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(read_failure(&error)),
            };
            if available.is_empty() {
                break;
            }
            let (take, found) = match available.iter().position(|&byte| byte == b'\n') {
                Some(index) => (index + 1, true),
                None => (available.len(), false),
            };
            if bytes.len() + take > self.max_line {
                return Err(line_too_long(number));
            }
            bytes.extend_from_slice(&available[..take]);
            self.input.consume(take);
            done = found;
        }
        if bytes.len() > self.max_line {
            return Err(line_too_long(number));
        }
        if bytes.is_empty() {
            return Ok(false);
        }
        match String::from_utf8(bytes) {
            Ok(text) => {
                *line = text;
                Ok(true)
            }
            Err(error) => Err(SqlError::new(
                SqlState::CHARACTER_NOT_IN_REPERTOIRE,
                format!(
                    "line {number} of the script is not UTF-8 text (invalid byte at column {})",
                    error.utf8_error().valid_up_to() + 1
                ),
            )
            .with_hint(
                "Save the script as UTF-8 or as Unicode (UTF-16 with its byte-order mark), which SSMS writes by default.",
            )),
        }
    }

    fn next_unit(&mut self, number: u64) -> SqlResult<Option<u16>> {
        let Some(low) = self.next_byte()? else {
            return Ok(None);
        };
        let Some(high) = self.next_byte()? else {
            return Err(SqlError::new(
                SqlState::CHARACTER_NOT_IN_REPERTOIRE,
                format!("the UTF-16 script ends inside a character on line {number} (an odd number of bytes)"),
            )
            .with_hint("The script is cut short or damaged; save it again."));
        };
        Ok(Some(u16::from_le_bytes([low, high])))
    }

    fn utf16_line(&mut self, line: &mut String) -> SqlResult<bool> {
        let number = self.line_number + 1;
        let mut any = false;
        loop {
            let Some(unit) = self.next_unit(number)? else {
                if self.high_surrogate.take().is_some() {
                    return Err(lone_surrogate(number));
                }
                return Ok(any);
            };
            any = true;
            let code = match self.high_surrogate.take() {
                Some(high) if (0xDC00..=0xDFFF).contains(&unit) => {
                    0x10000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(unit) - 0xDC00)
                }
                Some(_) => return Err(lone_surrogate(number)),
                None if (0xD800..=0xDBFF).contains(&unit) => {
                    self.high_surrogate = Some(unit);
                    continue;
                }
                None if (0xDC00..=0xDFFF).contains(&unit) => return Err(lone_surrogate(number)),
                None => u32::from(unit),
            };
            let Some(character) = char::from_u32(code) else {
                return Err(lone_surrogate(number));
            };
            if line.len() + character.len_utf8() > self.max_line {
                return Err(line_too_long(number));
            }
            line.push(character);
            if character == '\n' {
                return Ok(true);
            }
        }
    }
}

fn read_failure(error: &std::io::Error) -> SqlError {
    SqlError::new(
        SqlState::IO_ERROR,
        format!("cannot read the script: {error}"),
    )
}

fn line_too_long(number: u64) -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("line {number} of the script is longer than 1 GiB"),
    )
}

fn lone_surrogate(number: u64) -> SqlError {
    SqlError::new(
        SqlState::CHARACTER_NOT_IN_REPERTOIRE,
        format!("line {number} of the UTF-16 script holds a broken character (a lone surrogate)"),
    )
    .with_hint("The script is damaged; save it again from the tool that wrote it.")
}
