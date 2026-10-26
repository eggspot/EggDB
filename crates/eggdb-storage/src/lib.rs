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
//! - [`PageFile`], the file as checksummed pages: commits through a write-ahead log inside the
//!   file that survive a crash at any point, with a choice of [`Durability`], checkpoints that
//!   bound recovery and recycle the log's segments, reported in [`LogStats`], page reads and
//!   writes, growth, a free list, and the lock that keeps a second writer out, with
//!   [`StorageError`] for what can go wrong;
//! - [`PageCache`], the buffer manager over a [`PageFile`]: a fixed number of page frames
//!   allocated once, clock eviction, pages pinned by [`PageRef`] and [`PageMut`] handles,
//!   changes kept in memory until their commit, and [`CacheStats`];
//! - [`BTree`], a B+tree of byte-string keys and values in the cache's pages, with fence keys,
//!   prefix truncation, splits up to the root, deletes that merge under-filled pages, values
//!   too large for a leaf on chains of overflow pages, a [`Cursor`] for range, prefix and
//!   reverse scans in a [`ScanOrder`], and [`BTree::check`], which checks a whole tree and
//!   returns a [`TreeReport`];
//! - [`Store`], transactions over the cache: one [`WriteTransaction`] at a time, committed
//!   atomically or rolled back, any number of [`ReadTransaction`]s reading snapshots kept
//!   consistent by before-images of the pages the writer changes, a hybrid-clock
//!   [`CommitStamp`] for every commit, and [`Store::check`], which checks the last commit and
//!   accounts for every page of the file in a [`FileReport`].

mod before_images;
mod btree;
#[cfg(test)]
mod btree_delete_tests;
#[cfg(test)]
mod btree_tests;
mod bytes;
mod cells;
#[cfg(test)]
mod checkpoint_tests;
#[cfg(test)]
mod crash_sweep_tests;
mod crc32c;
mod cursor;
mod error;
mod format;
mod frames;
mod hybrid_clock;
mod log;
#[cfg(test)]
mod model_tests;
mod overflow;
#[cfg(test)]
mod overflow_mutation_tests;
#[cfg(test)]
mod overflow_tests;
mod page_cache;
mod page_file;
mod page_set;
mod page_table;
mod store;
#[cfg(test)]
mod store_tests;
#[cfg(test)]
mod test_rng;
mod tree_page;

pub use btree::{BTree, TreeReport};
pub use crc32c::{crc32c, crc32c_append};
pub use cursor::{Cursor, ScanOrder};
pub use error::{FormatError, StorageError};
pub use format::{
    Access, FORMAT_VERSION, Features, FileId, HEADER_SLOT_SIZE, HeaderChoice, MAGIC,
    MAX_LOG_SEGMENT_BYTES, NO_PAGE, PAGE_HEADER_SIZE, PAGE_VOLUME_MASK, PageChecksum, PageHeader,
    PageSize, PageType, RECYCLED_SEGMENTS, SUPERBLOCK_SIZE, Superblock,
};
pub use hybrid_clock::CommitStamp;
pub use log::{Durability, LogStats};
pub use page_cache::{CacheSize, CacheStats, PageCache, PageMut, PageRef};
pub use page_file::{CreateOptions, PageFile};
pub use store::{FileReport, ReadTransaction, Store, WriteTransaction};
