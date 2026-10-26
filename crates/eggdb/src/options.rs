use crate::error::{Error, ErrorKind, Result};
use eggdb_storage::PageSize;
use std::time::Duration;

const DEFAULT_CACHE_BYTES: usize = 16 << 20;
const DEFAULT_AUTO_CHECKPOINT_LOG_BYTES: u64 = 1 << 20;
const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a commit waits for the disk before it returns, and so what a power loss can take.
/// The names and behavior follow SQLite's `synchronous` setting.
///
/// A crash of the process alone never loses a commit in any mode: what a commit wrote has
/// reached the operating system. The modes differ only on a power loss or an operating system
/// crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Durability {
    /// A commit returns once it is on the disk: a power loss loses nothing that was committed.
    /// The default.
    #[default]
    Full,
    /// A commit returns once it is written, without waiting for the disk. A power loss can
    /// lose the latest commits but never corrupts the file. Commits reach the disk at the next
    /// [`Database::sync`](crate::Database::sync), checkpoint or close, and by themselves once a
    /// megabyte of log or a thousand pages wait.
    Normal,
    /// Nothing waits for the disk: a power loss can corrupt the file. For bulk loads that can
    /// be redone, and tests.
    Off,
}

impl Durability {
    pub(crate) fn to_storage(self) -> eggdb_storage::Durability {
        match self {
            Self::Full => eggdb_storage::Durability::Full,
            Self::Normal => eggdb_storage::Durability::Normal,
            Self::Off => eggdb_storage::Durability::Off,
        }
    }
}

/// How [`Database::open_with`](crate::Database::open_with) opens a database.
///
/// Each setting is a method that returns the changed options, so they chain. Values are
/// checked when the database is opened, and one EggDB does not accept fails the open with
/// [`ErrorKind::InvalidOption`].
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # let path = std::env::temp_dir().join(format!("eggdb-doc-options-{}.db", std::process::id()));
/// use eggdb::{Database, Durability, Options};
///
/// let options = Options::new()
///     .durability(Durability::Normal)
///     .cache_bytes(64 << 20);
/// let db = Database::open_with(&path, options)?;
/// db.put("greeting", "hello")?;
/// db.close()?;
/// # std::fs::remove_file(&path)?;
/// # Ok(()) }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub(crate) create_if_missing: bool,
    pub(crate) read_only: bool,
    pub(crate) durability: Durability,
    pub(crate) cache_bytes: usize,
    pub(crate) snapshot_memory_bytes: Option<usize>,
    pub(crate) auto_checkpoint_log_bytes: Option<u64>,
    pub(crate) page_size_bytes: usize,
    pub(crate) write_timeout: Duration,
}

impl Options {
    /// The defaults: create the file if it is missing, open it for writing, [`Durability::Full`],
    /// a 16 MiB page cache, snapshot memory as large as the cache, a checkpoint after every
    /// megabyte of log, 4 KiB pages for a new file, and writers that wait up to five seconds
    /// for the one before them.
    pub fn new() -> Self {
        Self {
            create_if_missing: true,
            read_only: false,
            durability: Durability::Full,
            cache_bytes: DEFAULT_CACHE_BYTES,
            snapshot_memory_bytes: None,
            auto_checkpoint_log_bytes: Some(DEFAULT_AUTO_CHECKPOINT_LOG_BYTES),
            page_size_bytes: PageSize::DEFAULT.bytes(),
            write_timeout: DEFAULT_WRITE_TIMEOUT,
        }
    }

    /// Whether opening a path with no file creates an empty database there (the default), or
    /// fails with [`ErrorKind::NotFound`]. A read-only open never creates a file.
    pub fn create_if_missing(mut self, create: bool) -> Self {
        self.create_if_missing = create;
        self
    }

    /// Whether to open the database for reading only. A read-only database takes a shared
    /// lock on its file, so other read-only opens work alongside it while a writer is refused;
    /// every change fails with [`ErrorKind::ReadOnly`]. The default is `false`.
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// How commits wait for the disk; [`Durability::Full`] by default.
    pub fn durability(mut self, durability: Durability) -> Self {
        self.durability = durability;
        self
    }

    /// The page cache's memory, in bytes, rounded down to whole pages; 16 MiB by default.
    ///
    /// It is reserved when the database opens and never grows, and the operating system
    /// gives it real memory only as pages are used. Every page a write transaction changes
    /// stays in the cache until the commit, so this also bounds the size of one write
    /// transaction: a larger one fails with [`ErrorKind::CacheFull`]. Less than one page fails
    /// the open with [`ErrorKind::InvalidOption`].
    pub fn cache_bytes(mut self, bytes: usize) -> Self {
        self.cache_bytes = bytes;
        self
    }

    /// How much memory, in bytes, may keep the pages read transactions' snapshots still need
    /// after the writer changed them; as much as the page cache by default. When the writer
    /// needs more, the oldest snapshots expire, and their transactions fail with
    /// [`ErrorKind::SnapshotTooOld`], so a forgotten reader never holds the writer back.
    pub fn snapshot_memory_bytes(mut self, bytes: usize) -> Self {
        self.snapshot_memory_bytes = Some(bytes);
        self
    }

    /// How much write-ahead log, in bytes, a commit may leave before it writes a checkpoint by
    /// itself; 1 MiB by default. A checkpoint bounds the log that the next open replays.
    /// `None` leaves checkpoints to [`Database::checkpoint`](crate::Database::checkpoint) and
    /// [`Database::close`](crate::Database::close), except when the log would outgrow the
    /// segments it reuses.
    pub fn auto_checkpoint_log_bytes(mut self, bytes: Option<u64>) -> Self {
        self.auto_checkpoint_log_bytes = bytes;
        self
    }

    /// The page size of a new database file, in bytes: a power of two from 4,096 (the
    /// default) to 65,536. It is fixed for the file's life and ignored when the file exists.
    /// Larger pages allow longer keys and values (see
    /// [`Database::max_entry_bytes`](crate::Database::max_entry_bytes)).
    pub fn page_size_bytes(mut self, bytes: usize) -> Self {
        self.page_size_bytes = bytes;
        self
    }

    /// How long [`Database::begin_write`](crate::Database::begin_write) waits for the write
    /// transaction before it to end; five seconds by default. [`Duration::ZERO`] makes it fail
    /// at once, as [`Database::try_begin_write`](crate::Database::try_begin_write) does.
    pub fn write_timeout(mut self, timeout: Duration) -> Self {
        self.write_timeout = timeout;
        self
    }

    pub(crate) fn creates_if_missing(&self) -> bool {
        self.create_if_missing && !self.read_only
    }

    pub(crate) fn snapshot_memory(&self) -> usize {
        self.snapshot_memory_bytes.unwrap_or(self.cache_bytes)
    }

    pub(crate) fn page_size(&self) -> Result<PageSize> {
        let bytes = self.page_size_bytes;
        let log2 = u8::try_from(bytes.trailing_zeros()).ok();
        let page_size = match log2.filter(|_| bytes.is_power_of_two()) {
            Some(log2) => PageSize::from_log2(log2).map_err(|_| invalid_page_size(bytes))?,
            None => return Err(invalid_page_size(bytes)),
        };
        if self.cache_bytes < page_size.bytes() {
            return Err(Error::new(
                ErrorKind::InvalidOption,
                format!(
                    "a page cache of {} bytes holds no page of {} bytes; give it at least one page",
                    self.cache_bytes,
                    page_size.bytes()
                ),
            ));
        }
        Ok(page_size)
    }
}

fn invalid_page_size(bytes: usize) -> Error {
    Error::new(
        ErrorKind::InvalidOption,
        format!(
            "a page size of {bytes} bytes is not allowed: it must be a power of two from {} to {} bytes",
            1usize << PageSize::MIN_LOG2,
            1usize << PageSize::MAX_LOG2
        ),
    )
}

impl Default for Options {
    fn default() -> Self {
        Self::new()
    }
}
