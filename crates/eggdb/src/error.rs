use eggdb_storage::{FormatError, StorageError};
use std::fmt;
use std::path::Path;

/// The result of an EggDB operation.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// What kind of failure an EggDB operation hit, for callers that react to some failures and
/// not others without reading the message.
///
/// New kinds may be added in later releases, so a `match` needs a wildcard arm.
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # let path = std::env::temp_dir().join(format!("eggdb-doc-error-kind-{}.db", std::process::id()));
/// let missing = eggdb::Options::new().create_if_missing(false);
/// match eggdb::Database::open_with(&path, missing) {
///     Err(error) if error.kind() == eggdb::ErrorKind::NotFound => println!("no database yet"),
///     Err(error) => return Err(error.into()),
///     Ok(_) => println!("opened"),
/// }
/// # Ok(()) }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The database file does not exist, and the options did not allow creating it.
    NotFound,
    /// The operating system refused access to the file or its directory.
    PermissionDenied,
    /// The file is open elsewhere, in this process or another: a database opened for
    /// writing excludes every other open, and one opened read-only excludes writers.
    Locked,
    /// The database was opened read-only and the operation would change it.
    ReadOnly,
    /// The file is not an EggDB database.
    NotADatabase,
    /// The file was written by a newer EggDB, with a format version or features this
    /// release cannot read, or can only read.
    Incompatible,
    /// The file is damaged: a checksum, a page or a structure failed its checks.
    Corrupt,
    /// The file system failed to read, write or sync.
    Io,
    /// The memory for the page cache, or for an operation, could not be had.
    OutOfMemory,
    /// A key or a value is longer than EggDB stores (see
    /// [`Database::max_key_bytes`](crate::Database::max_key_bytes) and
    /// [`Database::max_value_bytes`](crate::Database::max_value_bytes)), or a value needs
    /// more pages than the page cache holds (see
    /// [`Options::cache_bytes`](crate::Options::cache_bytes)). Nothing changed, and the
    /// transaction stays open.
    TooLarge,
    /// The page cache has no room left: a write transaction changed more pages than the cache
    /// holds. The transaction was rolled back; commit smaller transactions, or open with a
    /// larger [`Options::cache_bytes`](crate::Options::cache_bytes).
    CacheFull,
    /// Another write transaction is open, and stayed open for the whole wait (see
    /// [`Database::begin_write`](crate::Database::begin_write)).
    WriterActive,
    /// A read transaction's snapshot expired: the writer changed more pages since it began
    /// than the snapshot memory holds. Begin a new read transaction.
    SnapshotTooOld,
    /// The write transaction failed earlier and was rolled back then; begin a new one.
    TransactionFailed,
    /// An earlier write or sync failed, so the database refuses everything until it is
    /// opened again, which recovers its last durable state.
    NeedsReopen,
    /// The database can take no more commits: its file holds as many pages as page numbers
    /// allow, or every commit stamp has been handed out. Reads still work.
    DatabaseFull,
    /// An [`Options`](crate::Options) value is outside what EggDB accepts.
    InvalidOption,
    /// Any other failure.
    Other,
}

/// Why an EggDB operation failed: its [`ErrorKind`], and a message written to be read by
/// people and by AI models alike, which says what happened and what to do about it.
///
/// [`std::error::Error::source`] gives the storage engine's own error when there is one. Its
/// type is not part of this crate's stable API: match on [`Error::kind`] instead.
pub struct Error {
    kind: ErrorKind,
    detail: Detail,
    path: Option<Box<Path>>,
}

enum Detail {
    Storage(StorageError),
    Message(Box<str>, Option<StorageError>),
}

impl Error {
    /// The kind of failure.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub(crate) fn storage(error: StorageError) -> Self {
        Self {
            kind: kind_of(&error),
            detail: Detail::Storage(error),
            path: None,
        }
    }

    pub(crate) fn new(kind: ErrorKind, message: impl Into<Box<str>>) -> Self {
        Self {
            kind,
            detail: Detail::Message(message.into(), None),
            path: None,
        }
    }

    pub(crate) fn explained(self, message: impl Into<Box<str>>) -> Self {
        let source = match self.detail {
            Detail::Storage(source) | Detail::Message(_, Some(source)) => Some(source),
            Detail::Message(_, None) => None,
        };
        Self {
            detail: Detail::Message(message.into(), source),
            ..self
        }
    }

    pub(crate) fn opening(mut self, path: &Path) -> Self {
        self.path = Some(path.into());
        self
    }

    fn storage_source(&self) -> Option<&StorageError> {
        match &self.detail {
            Detail::Storage(source) | Detail::Message(_, Some(source)) => Some(source),
            Detail::Message(_, None) => None,
        }
    }
}

fn kind_of(error: &StorageError) -> ErrorKind {
    match error {
        StorageError::Io(error) => match error.kind() {
            eggdb_platform::ErrorKind::NotFound => ErrorKind::NotFound,
            eggdb_platform::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
            eggdb_platform::ErrorKind::Locked => ErrorKind::Locked,
            eggdb_platform::ErrorKind::OutOfMemory => ErrorKind::OutOfMemory,
            _ => ErrorKind::Io,
        },
        StorageError::Format(error) => kind_of_format(error),
        StorageError::Locked => ErrorKind::Locked,
        StorageError::ReadOnly => ErrorKind::ReadOnly,
        StorageError::NeedsReopen => ErrorKind::NeedsReopen,
        StorageError::FileFull | StorageError::ClockExhausted => ErrorKind::DatabaseFull,
        StorageError::Truncated { .. }
        | StorageError::PageOutOfRange { .. }
        | StorageError::PageNotInUse { .. }
        | StorageError::DoubleFree { .. }
        | StorageError::CorruptFreeList { .. } => ErrorKind::Corrupt,
        StorageError::CacheFull { .. } => ErrorKind::CacheFull,
        StorageError::InvalidCacheSize { .. } => ErrorKind::InvalidOption,
        StorageError::KeyTooLarge { .. }
        | StorageError::ValueTooLarge { .. }
        | StorageError::ValueExceedsCache { .. } => ErrorKind::TooLarge,
        StorageError::WriterActive | StorageError::UncommittedChanges => ErrorKind::WriterActive,
        StorageError::TransactionFailed => ErrorKind::TransactionFailed,
        StorageError::SnapshotTooOld => ErrorKind::SnapshotTooOld,
        _ => ErrorKind::Other,
    }
}

fn kind_of_format(error: &FormatError) -> ErrorKind {
    match error {
        FormatError::NotADatabase => ErrorKind::NotADatabase,
        FormatError::UnsupportedVersion { .. }
        | FormatError::UnsupportedFeatures { .. }
        | FormatError::ReadOnlyFeatures { .. } => ErrorKind::Incompatible,
        _ => ErrorKind::Corrupt,
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(path) = &self.path {
            write!(f, "cannot open the database {}: ", path.display())?;
        }
        match &self.detail {
            Detail::Storage(source) => source.fmt(f),
            Detail::Message(message, _) => f.write_str(message),
        }
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("Error");
        debug.field("kind", &self.kind);
        if let Detail::Message(message, _) = &self.detail {
            debug.field("message", message);
        }
        debug
            .field("source", &self.storage_source())
            .field("path", &self.path)
            .finish()
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.storage_source()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

impl From<Error> for std::io::Error {
    fn from(error: Error) -> Self {
        let kind = match error.kind {
            ErrorKind::NotFound => std::io::ErrorKind::NotFound,
            ErrorKind::PermissionDenied => std::io::ErrorKind::PermissionDenied,
            ErrorKind::Locked | ErrorKind::WriterActive => std::io::ErrorKind::ResourceBusy,
            ErrorKind::ReadOnly => std::io::ErrorKind::ReadOnlyFilesystem,
            ErrorKind::NotADatabase | ErrorKind::Incompatible | ErrorKind::Corrupt => {
                std::io::ErrorKind::InvalidData
            }
            ErrorKind::TooLarge | ErrorKind::InvalidOption => std::io::ErrorKind::InvalidInput,
            ErrorKind::OutOfMemory => std::io::ErrorKind::OutOfMemory,
            ErrorKind::DatabaseFull => std::io::ErrorKind::StorageFull,
            _ => std::io::ErrorKind::Other,
        };
        Self::new(kind, error)
    }
}
