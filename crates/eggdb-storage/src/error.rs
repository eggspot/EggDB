use std::fmt;

/// Why bytes read from a database file were refused.
///
/// Every check of the file format returns one of these as a value; nothing a file contains can
/// make the engine panic (D9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FormatError {
    /// The buffer is not exactly one page, or one header slot, of the expected size.
    WrongLength {
        /// The length the format requires.
        expected: usize,
        /// The length that was given.
        actual: usize,
    },
    /// The file does not start with EggDB's magic bytes, so it is not an EggDB database.
    NotADatabase,
    /// The stored checksum does not match the bytes: the page was damaged, torn by a crash, or
    /// written for another page or another file.
    ChecksumMismatch {
        /// The page whose checksum failed.
        page: u64,
    },
    /// A header slot's stored checksum does not match its bytes: it is damaged or was torn by
    /// a crash while being written.
    HeaderChecksumMismatch {
        /// The slot, 0 or 1.
        slot: usize,
    },
    /// The page type byte names no type this release knows.
    UnknownPageType {
        /// The page that was read.
        page: u64,
        /// The stored type byte.
        found: u8,
    },
    /// The page size is not a power of two between the smallest and largest the format allows.
    InvalidPageSize {
        /// The stored size, as a base-two logarithm.
        log2: u8,
    },
    /// The file was written by a newer major format version that this release cannot read.
    UnsupportedVersion {
        /// The major version stored in the file.
        major: u16,
    },
    /// The file uses features that this release does not understand and so must not open it at
    /// all.
    UnsupportedFeatures {
        /// The unknown bits of the incompatible feature set.
        bits: u64,
    },
    /// The file uses features that this release does not understand and so may only read: it
    /// was opened for writing.
    ReadOnlyFeatures {
        /// The unknown bits of the read-only-compatible feature set.
        bits: u64,
    },
    /// A field holds a value the format forbids.
    InvalidField {
        /// The field's name, as in `docs/format.md`.
        field: &'static str,
    },
    /// Neither header slot is valid, so the file cannot be opened.
    NoValidHeader,
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::WrongLength { expected, actual } => {
                write!(f, "expected {expected} bytes, got {actual} bytes")
            }
            Self::NotADatabase => f.write_str("not an EggDB database: the magic bytes are missing"),
            Self::ChecksumMismatch { page } => write!(
                f,
                "page {page} failed its checksum: it is damaged, torn by a crash, or belongs elsewhere"
            ),
            Self::HeaderChecksumMismatch { slot } => write!(
                f,
                "header slot {slot} failed its checksum: it is damaged or was torn by a crash"
            ),
            Self::UnknownPageType { page, found } => {
                write!(f, "page {page} has unknown page type {found}")
            }
            Self::InvalidPageSize { log2 } => {
                write!(
                    f,
                    "page size 2^{log2} is outside the sizes the format allows"
                )
            }
            Self::UnsupportedVersion { major } => write!(
                f,
                "the file has format version {major}, newer than this release can read; upgrade EggDB"
            ),
            Self::UnsupportedFeatures { bits } => write!(
                f,
                "the file uses features this release cannot read (incompatible bits {bits:#x}); upgrade EggDB"
            ),
            Self::ReadOnlyFeatures { bits } => write!(
                f,
                "the file uses features this release can only read (bits {bits:#x}); open it read-only or upgrade EggDB"
            ),
            Self::InvalidField { field } => {
                write!(f, "the field `{field}` holds a forbidden value")
            }
            Self::NoValidHeader => f.write_str(
                "neither header slot is valid; the file is damaged or not an EggDB database",
            ),
        }
    }
}

impl std::error::Error for FormatError {}

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
    /// A page that is free, or freed since the last commit, was written.
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
    /// twice, or disagrees with the superblock's count.
    CorruptFreeList {
        /// The free-list page where the damage was found.
        page: u64,
    },
    /// The file was opened read-only.
    ReadOnly,
    /// An earlier write or sync failed, so the file accepts no more changes until it is opened
    /// again and recovered from its durable state (D59).
    Failed,
    /// The file holds as many pages as page numbers allow.
    FileFull,
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
                "the database is open elsewhere: close the other writer, or open it read-only",
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
            Self::Failed => f.write_str(
                "an earlier write or sync failed; reopen the database to recover its last durable state",
            ),
            Self::FileFull => f.write_str("the file holds as many pages as page numbers allow"),
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
