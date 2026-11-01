use core::fmt;

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
    /// The write-ahead log is damaged where it had already been made durable: a record that
    /// fails its checks is followed by records proving it was synced, or a record names
    /// something the file can't hold. A crash never causes this, so the file is refused
    /// rather than silently losing commits.
    CorruptLog {
        /// The first page of the log segment holding the damage.
        segment: u64,
        /// The damaged record's byte offset from the start of that segment.
        offset: u64,
    },
    /// A page of a B+tree breaks the tree's rules: its layout is impossible, its cells overlap
    /// or are out of order, or it does not fit where its parent puts it.
    CorruptTree {
        /// The page where the damage was found.
        page: u64,
        /// The rule the page breaks.
        problem: &'static str,
    },
    /// An LSM tree's manifest, or one of its entries, breaks its rules.
    CorruptLsm {
        /// What is wrong.
        problem: &'static str,
    },
    /// A page of a large value's chain of overflow pages breaks the chain's rules: it is of
    /// another type, belongs to another value or another place in the chain, ends the chain too
    /// early or too late, or is reached twice.
    CorruptOverflow {
        /// The page where the damage was found.
        page: u64,
        /// The rule the page breaks.
        problem: &'static str,
    },
    /// A page has two uses at once, such as a page of the tree that is also on the free list,
    /// so the file's structures overlap.
    PageUsedTwice {
        /// The page.
        page: u64,
        /// One of its uses.
        first_use: &'static str,
        /// Another of its uses.
        second_use: &'static str,
    },
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
            Self::CorruptLog { segment, offset } => write!(
                f,
                "the write-ahead log is damaged in the segment at page {segment}, at byte {offset}, after it was made durable; restore from a backup or run `eggdb check`"
            ),
            Self::CorruptTree { page, problem } => write!(
                f,
                "the B+tree page {page} is damaged: {problem}; restore from a backup or run `eggdb check`"
            ),
            Self::CorruptLsm { problem } => write!(
                f,
                "an LSM tree is damaged: {problem}; restore from a backup or run `eggdb check`"
            ),
            Self::CorruptOverflow { page, problem } => write!(
                f,
                "the overflow page {page} of a large value is damaged: {problem}; restore from a backup or run `eggdb check`"
            ),
            Self::PageUsedTwice {
                page,
                first_use,
                second_use,
            } => write!(
                f,
                "page {page} is both {first_use} and {second_use}, so the file's structures overlap; restore from a backup"
            ),
        }
    }
}

impl core::error::Error for FormatError {}
