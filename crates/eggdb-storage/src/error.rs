use std::fmt;

/// Why bytes read from a database file were refused.
///
/// Every check of the file format returns one of these as a value; nothing a file contains can
/// make the engine panic (D9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FormatError {
    /// The buffer is not exactly one page of the expected size.
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
    /// Neither header page is valid, so the file cannot be opened.
    NoValidHeader,
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::WrongLength { expected, actual } => {
                write!(f, "expected a page of {expected} bytes, got {actual} bytes")
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
                "neither header page is valid; the file is damaged or not an EggDB database",
            ),
        }
    }
}

impl std::error::Error for FormatError {}
