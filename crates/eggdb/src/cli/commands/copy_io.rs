use crate::cli::output::Output;
use eggdb_sql::{CopyChannel, CopyStart, SqlError, SqlResult, SqlState};
use std::cell::{Cell, RefCell};
use std::io::Read;

const READ_CHUNK_BYTES: usize = 64 * 1024;
const DEFAULT_CACHE_BYTES: usize = 16 << 20;
const BULK_CACHE_BYTES: usize = 256 << 20;
const LARGEST_BULK_CACHE_BYTES: usize = 1 << 30;

pub(super) fn bulk_cache(input_bytes: u64) -> usize {
    let bytes = usize::try_from(input_bytes).unwrap_or(usize::MAX);
    bytes
        .saturating_mul(4)
        .saturating_add(DEFAULT_CACHE_BYTES)
        .clamp(BULK_CACHE_BYTES, LARGEST_BULK_CACHE_BYTES)
}

pub(super) fn script_cache(text: &str) -> usize {
    let mentions_copy = text
        .as_bytes()
        .windows(4)
        .any(|window| window.eq_ignore_ascii_case(b"copy"));
    if mentions_copy {
        bulk_cache(text.len() as u64)
    } else {
        DEFAULT_CACHE_BYTES
    }
}

pub(super) fn read_chunk(input: &mut dyn Read, data: &mut Vec<u8>) -> SqlResult<bool> {
    let start = data.len();
    data.resize(start + READ_CHUNK_BYTES, 0);
    loop {
        match input.read(&mut data[start..]) {
            Ok(count) => {
                data.truncate(start + count);
                return Ok(count > 0);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                data.truncate(start);
                return Err(SqlError::new(
                    SqlState::IO_ERROR,
                    format!("cannot read the COPY data: {error}"),
                ));
            }
        }
    }
}

pub(super) fn output_closed() -> SqlError {
    SqlError::new(
        SqlState::IO_ERROR,
        "cannot write the COPY data: the output was closed",
    )
}

fn no_input() -> SqlError {
    SqlError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        "COPY FROM STDIN has no data to read here",
    )
    .with_hint("In a script the data follows the COPY statement, up to a line \\.; with -c it comes from stdin.")
}

pub(super) struct StreamCopy<'r, 'o, 'a> {
    pub(super) input: Option<&'r mut dyn Read>,
    pub(super) output: Option<&'r RefCell<&'o mut Output<'a>>>,
    pub(super) copied_out: &'r Cell<bool>,
}

impl CopyChannel for StreamCopy<'_, '_, '_> {
    fn start_copy_in(&mut self, _start: &CopyStart) -> SqlResult<()> {
        match self.input {
            Some(_) => Ok(()),
            None => Err(no_input()),
        }
    }

    fn read_copy_data(&mut self, data: &mut Vec<u8>) -> SqlResult<bool> {
        match &mut self.input {
            Some(input) => read_chunk(&mut **input, data),
            None => Err(no_input()),
        }
    }

    fn start_copy_out(&mut self, _start: &CopyStart) -> SqlResult<()> {
        self.copied_out.set(true);
        Ok(())
    }

    fn write_copy_data(&mut self, data: &[u8]) -> SqlResult<()> {
        let Some(output) = self.output else {
            return Ok(());
        };
        let mut output = output.borrow_mut();
        output.bytes(data);
        output.line_done().map_err(|_| output_closed())
    }

    fn finish_copy_out(&mut self) -> SqlResult<()> {
        Ok(())
    }
}
