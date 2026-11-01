pub use eggdb_format::FormatError;
use std::fmt;

/// Why a storage operation failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum StorageError {
    /// The file system failed.
    Io(eggdb_platform::Error),
    /// Bytes read from the file were refused.
    Format(FormatError),
    /// Another handle, in this process or another, has the file open in a way that excludes
    /// this one: one writer, or any number of readers.
    Locked,
    /// The file is shorter than its superblock says: it was cut off.
    Truncated {
        /// The length the superblock implies.
        expected_bytes: u64,
        /// The file's length.
        actual_bytes: u64,
    },
    /// The page number is a header slot or lies past the end of the file.
    PageOutOfRange {
        /// The page asked for.
        page: u64,
        /// How many pages the file holds.
        page_count: u64,
    },
    /// A page that is free, freed since the last commit, or part of the free list was written.
    PageNotInUse {
        /// The page.
        page: u64,
    },
    /// A page was freed that is already free.
    DoubleFree {
        /// The page.
        page: u64,
    },
    /// The free list is damaged: a page of it loops, points outside the file, lists a page
    /// twice or a page in use, or disagrees with the superblock's count.
    CorruptFreeList {
        /// The free-list page where the damage was found.
        page: u64,
    },
    /// The file was opened read-only.
    ReadOnly,
    /// An earlier write or sync failed, so the file accepts no more changes until it is opened
    /// again and recovered from its durable state (D59).
    NeedsReopen,
    /// The file holds as many pages as page numbers allow.
    FileFull,
    /// A checkpoint was asked for while changes since the last commit are pending; commit or
    /// roll them back first.
    UncommittedChanges,
    /// The page cache has no frame to give: every frame holds a page that is pinned by an open
    /// page handle, or changed by the uncommitted transaction, which stays in memory until its
    /// commit.
    CacheFull {
        /// How many frames the cache has.
        frames: usize,
        /// How many of them are pinned.
        pinned: usize,
        /// How many of them hold changes not yet committed.
        dirty: usize,
    },
    /// The page is held by another handle in a way that excludes this one: a page being
    /// written can't be read, written or freed through a second handle until the first is
    /// dropped, and a page being read can't be written or freed.
    PageBorrowed {
        /// The page.
        page: u64,
    },
    /// The page cache was asked for a size outside the pages it can hold.
    InvalidCacheSize {
        /// The number of pages the size works out to.
        pages: usize,
    },
    /// A key is longer than a B+tree of this page size stores (see
    /// [`BTree::max_key_bytes`](crate::BTree::max_key_bytes)).
    KeyTooLarge {
        /// The key's length.
        bytes: usize,
        /// The longest key allowed.
        max: usize,
    },
    /// A value is longer than the largest value a B+tree stores (see
    /// [`BTree::MAX_VALUE_BYTES`](crate::BTree::MAX_VALUE_BYTES)).
    ValueTooLarge {
        /// The value's length.
        bytes: usize,
        /// The longest value allowed.
        max: usize,
    },
    /// A value needs more overflow pages, with the leaf that names them, than the page cache
    /// has frames, so no write transaction could hold it: every page a write transaction
    /// changes stays in the cache until its commit.
    ValueExceedsCache {
        /// The value's length.
        bytes: usize,
        /// The pages the value needs: its overflow pages and its leaf.
        pages: u64,
        /// The frames of the page cache.
        frames: usize,
    },
    /// A write transaction was asked for while another one is open: a store has one writer at
    /// a time, and a second one fails at once rather than waiting.
    WriterActive,
    /// The write transaction failed earlier and was rolled back then, so it takes no more
    /// work; begin a new one.
    TransactionFailed,
    /// The read transaction's snapshot is older than the store of before-images can still
    /// serve, because the writer changed more pages since it began than the store holds.
    SnapshotTooOld,
    /// Every commit stamp has been handed out: the hybrid clock's high-water mark is at the
    /// largest stamp there is.
    ClockExhausted,
    /// The file's pages are encrypted (D187) and no key was given to open it.
    NeedsKey,
    /// The key given does not open this file, or is not [`crate::PAGE_KEY_BYTES`] long.
    WrongKey,
    /// A key was given for a file whose pages are not encrypted.
    NotEncrypted,
    /// A copy of a snapshot, checked after it was written, does not hold what the snapshot
    /// held, or holds pages nothing uses (see
    /// [`ReadTransaction::copy_to`](crate::ReadTransaction::copy_to)); the copy is not kept.
    CopyIncomplete {
        /// The entries of the snapshot.
        expected: u64,
        /// The entries the copy holds.
        found: u64,
        /// The copy's pages that nothing uses.
        leaked_pages: u64,
    },
}

impl From<eggdb_platform::Error> for StorageError {
    fn from(error: eggdb_platform::Error) -> Self {
        if error.kind() == eggdb_platform::ErrorKind::Locked {
            Self::Locked
        } else {
            Self::Io(error)
        }
    }
}

impl From<FormatError> for StorageError {
    fn from(error: FormatError) -> Self {
        Self::Format(error)
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "file system error: {error}"),
            Self::Format(error) => error.fmt(f),
            Self::Locked => f.write_str(
                "the database is open elsewhere: a writer excludes every other user, and readers exclude a writer",
            ),
            Self::Truncated {
                expected_bytes,
                actual_bytes,
            } => write!(
                f,
                "the file is cut off: its header says {expected_bytes} bytes, it has {actual_bytes}"
            ),
            Self::PageOutOfRange { page, page_count } => write!(
                f,
                "page {page} is not a data page of this file, which has {page_count} pages"
            ),
            Self::DoubleFree { page } => write!(f, "page {page} was freed twice"),
            Self::PageNotInUse { page } => {
                write!(f, "page {page} is free and must be allocated before it is written")
            }
            Self::CorruptFreeList { page } => {
                write!(f, "the free list is damaged at page {page}; run `eggdb check`")
            }
            Self::ReadOnly => f.write_str("the database was opened read-only"),
            Self::NeedsReopen => f.write_str(
                "an earlier write or sync failed; reopen the database to recover its last durable state",
            ),
            Self::FileFull => f.write_str("the file holds as many pages as page numbers allow"),
            Self::UncommittedChanges => f.write_str(
                "a checkpoint needs every change committed; commit or roll back first",
            ),
            Self::CacheFull {
                frames,
                pinned,
                dirty,
            } => write!(
                f,
                "all {frames} frames of the page cache are taken: {pinned} pinned by open page handles, {dirty} changed by the uncommitted transaction; drop page handles, commit smaller transactions, or open with a larger cache"
            ),
            Self::PageBorrowed { page } => write!(
                f,
                "page {page} is held by another handle: a page being written allows no other handle, and a page being read allows only readers"
            ),
            Self::InvalidCacheSize { pages } => write!(
                f,
                "a page cache of {pages} pages is outside the allowed range of 1 to {} pages",
                crate::page_cache::MAX_FRAMES
            ),
            Self::KeyTooLarge { bytes, max } => write!(
                f,
                "a key of {bytes} bytes is longer than the {max} bytes a key may have with this page size"
            ),
            Self::ValueTooLarge { bytes, max } => write!(
                f,
                "a value of {bytes} bytes is longer than the {max} bytes a value may have"
            ),
            Self::ValueExceedsCache {
                bytes,
                pages,
                frames,
            } => write!(
                f,
                "a value of {bytes} bytes needs {pages} pages, more than the {frames} frames of the page cache, which holds every page a write transaction changes until its commit; open the database with a larger cache"
            ),
            Self::WriterActive => f.write_str(
                "another write transaction is open; one writer at a time: commit or roll it back first",
            ),
            Self::TransactionFailed => f.write_str(
                "this write transaction failed earlier and was rolled back; begin a new one",
            ),
            Self::SnapshotTooOld => f.write_str(
                "this read transaction's snapshot is too old: the writer changed more pages since it began than the snapshot memory holds; begin a new read transaction, or give snapshots more memory",
            ),
            Self::ClockExhausted => f.write_str(
                "every commit stamp has been handed out: the file's clock high-water mark is at its largest value",
            ),
            Self::NeedsKey => f.write_str(
                "the database is encrypted; open it with its key (--key-file, or Options::encryption_key)",
            ),
            Self::WrongKey => f.write_str("the key does not open this database"),
            Self::NotEncrypted => f.write_str(
                "the database was created without encryption; a key cannot be added to it yet",
            ),
            Self::CopyIncomplete {
                expected,
                found,
                leaked_pages,
            } => write!(
                f,
                "the copy was checked after it was written and holds {found} entries and {leaked_pages} unused pages, where the snapshot held {expected} entries; it was not kept"
            ),
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Format(error) => Some(error),
            _ => None,
        }
    }
}
