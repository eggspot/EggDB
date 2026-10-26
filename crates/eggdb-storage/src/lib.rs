//! EggDB's storage engine (roadmap M2, D20, D77).
//!
//! This first part holds the file format's building blocks, as `docs/format.md` specifies
//! them:
//! - [`crc32c`], the checksum of every page and log record;
//! - the [`Superblock`] kept in two alternating header slots at the start of the file;
//! - the [`PageHeader`] at the start of every other page, written and verified through
//!   [`PageChecksum`];
//! - [`FormatError`], how every check of bytes read from a file fails, as a value and never a
//!   panic (D9).

mod bytes;
mod crc32c;
mod error;
mod format;
#[cfg(test)]
mod test_rng;

pub use crc32c::{crc32c, crc32c_append};
pub use error::FormatError;
pub use format::{
    FORMAT_VERSION, Features, FileId, HEADER_SLOT_SIZE, MAGIC, NO_PAGE, PAGE_HEADER_SIZE,
    PAGE_VOLUME_MASK, PageChecksum, PageHeader, PageSize, PageType, SUPERBLOCK_SIZE, Superblock,
};
