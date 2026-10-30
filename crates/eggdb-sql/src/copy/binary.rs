use super::CopyChannel;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::time_zones::TimeZone;
use crate::types::Value;
use std::ops::Range;

pub(crate) const SIGNATURE: &[u8; 11] = b"PGCOPY\n\xff\r\n\0";
const OIDS_FLAG: u32 = 1 << 16;
const CRITICAL_FLAGS: u32 = 0xFFFF_0000;
const COMPACT_ABOVE_BYTES: usize = 1 << 20;
const TRAILER: i16 = -1;

fn bad_format(message: impl Into<String>) -> SqlError {
    SqlError::new(SqlState::BAD_COPY_FILE_FORMAT, message)
}

fn unexpected_end() -> SqlError {
    bad_format("unexpected EOF in COPY data")
}

pub(crate) struct BinaryReader<'a> {
    channel: &'a mut dyn CopyChannel,
    buffer: Vec<u8>,
    start: usize,
    input_ended: bool,
    line: u64,
    max_tuple: usize,
}

impl<'a> BinaryReader<'a> {
    pub(crate) fn new(channel: &'a mut dyn CopyChannel, max_tuple: usize) -> Self {
        Self {
            channel,
            buffer: Vec::new(),
            start: 0,
            input_ended: false,
            line: 0,
            max_tuple,
        }
    }

    pub(crate) fn line(&self) -> u64 {
        self.line
    }

    pub(crate) fn bytes(&self, range: Range<usize>) -> &[u8] {
        self.buffer.get(range).unwrap_or_default()
    }

    fn available(&self) -> usize {
        self.buffer.len() - self.start
    }

    fn fill_to(&mut self, wanted: usize) -> SqlResult<bool> {
        while self.available() < wanted {
            if self.input_ended {
                return Ok(false);
            }
            if !self.channel.read_copy_data(&mut self.buffer)? {
                self.input_ended = true;
            }
        }
        Ok(true)
    }

    fn compact(&mut self) {
        if self.start >= COMPACT_ABOVE_BYTES && self.start * 2 >= self.buffer.len() {
            self.buffer.drain(..self.start);
            self.start = 0;
        }
    }

    fn take(&mut self, count: usize) -> Range<usize> {
        let range = self.start..self.start + count;
        self.start += count;
        range
    }

    fn word(&mut self) -> Option<[u8; 4]> {
        let range = self.take(4);
        let bytes = self.buffer.get(range)?;
        Some([bytes[0], bytes[1], bytes[2], bytes[3]])
    }

    pub(crate) fn header(&mut self) -> SqlResult<()> {
        if !self.fill_to(SIGNATURE.len())?
            || self.buffer.get(self.start..self.start + SIGNATURE.len()) != Some(&SIGNATURE[..])
        {
            return Err(bad_format("COPY file signature not recognized"));
        }
        self.take(SIGNATURE.len());
        if !self.fill_to(4)? {
            return Err(bad_format("invalid COPY file header (missing flags)"));
        }
        let flags = u32::from_be_bytes(self.word().unwrap_or_default());
        if flags & OIDS_FLAG != 0 {
            return Err(bad_format("invalid COPY file header (WITH OIDS)"));
        }
        if flags & CRITICAL_FLAGS != 0 {
            return Err(bad_format(
                "unrecognized critical flags in COPY file header",
            ));
        }
        if !self.fill_to(4)? {
            return Err(bad_format("invalid COPY file header (missing length)"));
        }
        let length = i32::from_be_bytes(self.word().unwrap_or_default());
        let mut left = usize::try_from(length)
            .map_err(|_| bad_format("invalid COPY file header (wrong length)"))?;
        while left > 0 {
            if !self.fill_to(1)? {
                return Err(bad_format("invalid COPY file header (wrong length)"));
            }
            let skipped = left.min(self.available());
            self.take(skipped);
            left -= skipped;
            self.compact();
        }
        Ok(())
    }

    pub(crate) fn next_tuple(
        &mut self,
        columns: usize,
        fields: &mut Vec<Option<Range<usize>>>,
    ) -> SqlResult<bool> {
        self.compact();
        fields.clear();
        if !self.fill_to(2)? {
            return if self.available() == 0 {
                Ok(false)
            } else {
                self.line += 1;
                Err(unexpected_end())
            };
        }
        self.line += 1;
        let range = self.take(2);
        let count = i16::from_be_bytes([self.buffer[range.start], self.buffer[range.start + 1]]);
        if count == TRAILER {
            if self.fill_to(1)? {
                return Err(bad_format("received copy data after EOF marker"));
            }
            return Ok(false);
        }
        if usize::try_from(count).ok() != Some(columns) {
            return Err(bad_format(format!(
                "row field count is {count}, expected {columns}"
            )));
        }
        let tuple_start = self.start;
        for _ in 0..columns {
            if !self.fill_to(4)? {
                return Err(unexpected_end());
            }
            let length = i32::from_be_bytes(self.word().unwrap_or_default());
            if length == -1 {
                fields.push(None);
                continue;
            }
            let length = usize::try_from(length).map_err(|_| bad_format("invalid field size"))?;
            if self.start - tuple_start + length > self.max_tuple {
                return Err(SqlError::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    format!(
                        "a row of COPY data is longer than {} bytes",
                        self.max_tuple
                    ),
                )
                .with_hint("Each row of COPY data must fit in 1 GiB, as in PostgreSQL."));
            }
            if !self.fill_to(length)? {
                return Err(unexpected_end());
            }
            fields.push(Some(self.take(length)));
        }
        Ok(true)
    }

    pub(crate) fn drain(&mut self) -> SqlResult<()> {
        let mut discarded = Vec::new();
        while !self.input_ended {
            discarded.clear();
            if !self.channel.read_copy_data(&mut discarded)? {
                self.input_ended = true;
            }
        }
        Ok(())
    }

    pub(crate) fn abandon(&mut self) {
        if !self.input_ended {
            self.channel.abandon_copy_in();
        }
    }
}

pub(crate) struct BinaryWriter {
    oids: Vec<u32>,
    zone: TimeZone,
    line: Vec<u8>,
    header_sent: bool,
}

fn field_too_long() -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        "a value of COPY binary data is longer than 2 GiB",
    )
}

impl BinaryWriter {
    pub(crate) fn new(oids: Vec<u32>, zone: TimeZone) -> Self {
        Self {
            oids,
            zone,
            line: Vec::new(),
            header_sent: false,
        }
    }

    fn start_piece(&mut self) {
        self.line.clear();
        if !self.header_sent {
            self.line.extend_from_slice(SIGNATURE);
            self.line.extend_from_slice(&0u32.to_be_bytes());
            self.line.extend_from_slice(&0u32.to_be_bytes());
            self.header_sent = true;
        }
    }

    pub(crate) fn row(&mut self, values: &[&Value]) -> SqlResult<&[u8]> {
        self.start_piece();
        let count = i16::try_from(values.len()).map_err(|_| {
            SqlError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                "COPY in the binary format writes at most 32,767 columns",
            )
        })?;
        self.line.extend_from_slice(&count.to_be_bytes());
        for (value, &oid) in values.iter().zip(&self.oids) {
            match crate::pg_binary::encode_pg_binary(value, oid, self.zone)? {
                None => self.line.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(bytes) => {
                    let length = i32::try_from(bytes.len()).map_err(|_| field_too_long())?;
                    self.line.extend_from_slice(&length.to_be_bytes());
                    self.line.extend_from_slice(&bytes);
                }
            }
        }
        Ok(&self.line)
    }

    pub(crate) fn trailer(&mut self) -> &[u8] {
        self.start_piece();
        self.line.extend_from_slice(&TRAILER.to_be_bytes());
        &self.line
    }
}
