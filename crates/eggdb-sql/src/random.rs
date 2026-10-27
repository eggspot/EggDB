use crate::error::{SqlError, SqlResult, SqlState};
use std::sync::OnceLock;

/// A function that fills the whole of its buffer with unpredictable bytes from a
/// cryptographically secure generator, or fails; it must never leave part of the buffer
/// unfilled and succeed.
pub type RandomSource = fn(&mut [u8]) -> std::io::Result<()>;

static SOURCE: OnceLock<RandomSource> = OnceLock::new();

/// Sets the process's source of random bytes. The first registration wins for the life of
/// the process: later calls change nothing, whichever source they pass. Until one is set,
/// `gen_random_uuid()` answers SQLSTATE `0A000`.
pub fn set_random_source(source: RandomSource) {
    let _ = SOURCE.set(source);
}

pub(crate) fn fill(bytes: &mut [u8]) -> SqlResult<()> {
    let Some(source) = SOURCE.get() else {
        return Err(SqlError::not_supported(
            "this program has no source of random bytes for gen_random_uuid()",
        ));
    };
    source(bytes).map_err(|error| {
        SqlError::new(
            SqlState::IO_ERROR,
            format!("could not generate random values: {error}"),
        )
    })
}
