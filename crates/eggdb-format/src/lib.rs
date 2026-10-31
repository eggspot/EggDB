//! EggDB's file format building blocks (D20, D223), as `docs/format.md` specifies them, in a crate
//! that builds without the standard library (`default-features = false`, needing only `alloc`)
//! so a microcontroller reads and writes the same format as a server:
//! - [`crc32c`], the checksum of every page and log record;
//! - the [`Superblock`] kept in two alternating header slots at the start of the file, and the
//!   [`Features`] flags that decide whether a release may open a file for an [`Access`];
//! - the [`PageHeader`] at the start of every other page, written and verified through
//!   [`PageChecksum`];
//! - [`FormatError`], how every check of bytes read from a file fails, as a value and never a
//!   panic (D9);
//! - [`Flash`], the driver interface of raw flash memory, and [`FlashPages`], EggDB pages kept
//!   directly on it without a file system or any allocation.
//!
//! `eggdb-storage` builds the engine on these and re-exports them.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod bytes;
pub mod crc32c;
mod error;
mod flash;
pub mod format;
#[cfg(test)]
mod test_rng;

pub use crc32c::{crc32c, crc32c_append};
pub use error::FormatError;
pub use flash::{Flash, FlashPageError, FlashPages};
pub use format::{
    Access, FORMAT_VERSION, Features, FileId, HEADER_SLOT_SIZE, HeaderChoice, MAGIC, MAX_LOG_SEGMENT_BYTES, NO_PAGE, PAGE_ENCRYPTED,
    PAGE_HEADER_SIZE, PAGE_VOLUME_MASK, PageChecksum, PageHeader, PageSize, PageType, RECYCLED_SEGMENTS, SUPERBLOCK_SIZE, Superblock,
};
