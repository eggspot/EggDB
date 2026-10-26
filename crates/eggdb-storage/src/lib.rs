//! EggDB's storage engine (roadmap M2, D20, D77).
//!
//! This first part holds the file format's building blocks, as `docs/format.md` specifies
//! them:
//! - [`crc32c`], the checksum of every page and log record;
//! - the [`Superblock`] kept in two alternating header slots at the start of the file, and the
//!   [`Features`] flags that decide whether a release may open a file for an [`Access`];
//! - the [`PageHeader`] at the start of every other page, written and verified through
//!   [`PageChecksum`];
//! - [`FormatError`], how every check of bytes read from a file fails, as a value and never a
//!   panic (D9);
//! - [`PageFile`], the file as checksummed pages: header updates that survive a crash at any
//!   point, page reads and writes, growth, a free list, and the lock that keeps a second
//!   writer out, with [`StorageError`] for what can go wrong;
//! - [`PageCache`], the buffer manager over a [`PageFile`]: a fixed number of page frames
//!   allocated once, clock eviction, pages pinned by [`PageRef`] and [`PageMut`] handles,
//!   changes kept in memory until their commit, and [`CacheStats`].

mod bytes;
mod crc32c;
mod error;
mod format;
mod frames;
mod page_cache;
mod page_file;
mod page_table;
#[cfg(test)]
mod test_rng;

pub use crc32c::{crc32c, crc32c_append};
pub use error::{FormatError, StorageError};
pub use format::{
    Access, FORMAT_VERSION, Features, FileId, HEADER_SLOT_SIZE, HeaderChoice, MAGIC,
    MAX_LOG_SEGMENT_BYTES, NO_PAGE, PAGE_HEADER_SIZE, PAGE_VOLUME_MASK, PageChecksum, PageHeader,
    PageSize, PageType, RECYCLED_SEGMENTS, SUPERBLOCK_SIZE, Superblock,
};
pub use page_cache::{CacheSize, CacheStats, PageCache, PageMut, PageRef};
pub use page_file::{CreateOptions, PageFile};
