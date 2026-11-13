use crate::error::{Error, ErrorKind, Result};
use eggdb_platform::{Clock, SystemClock};
use eggdb_storage::PageSize;
use std::sync::Arc;
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
    pub(crate) busy_timeout: Duration,
    pub(crate) clock: ClockChoice,
    pub(crate) lock_cache: bool,
    pub(crate) cache_numa_node: Option<u32>,
    pub(crate) value_compression: eggdb_storage::ValueCompression,
    pub(crate) encryption_key: Option<EncryptionKey>,
    pub(crate) change_archive: Option<std::path::PathBuf>,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct EncryptionKey(pub(crate) [u8; eggdb_storage::PAGE_KEY_BYTES]);

impl std::fmt::Debug for EncryptionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EncryptionKey(..)")
    }
}

#[derive(Clone, Default)]
pub(crate) struct ClockChoice(Option<Arc<dyn Clock>>);

impl ClockChoice {
    pub(crate) fn clock(&self) -> Arc<dyn Clock> {
        match &self.0 {
            Some(clock) => Arc::clone(clock),
            None => Arc::new(SystemClock::new()),
        }
    }
}

impl PartialEq for ClockChoice {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (None, None) => true,
            (Some(mine), Some(theirs)) => Arc::ptr_eq(mine, theirs),
            _ => false,
        }
    }
}

impl Eq for ClockChoice {}

impl std::fmt::Debug for ClockChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() { "Given" } else { "System" })
    }
}

impl Options {
    /// The defaults: create the file if it is missing, open it for writing, [`Durability::Full`],
    /// a 16 MiB page cache, snapshot memory as large as the cache, a checkpoint after every
    /// megabyte of log, 4 KiB pages for a new file, writers that wait up to five seconds
    /// for the one before them, and an open that fails at once when another program has the
    /// file.
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
            busy_timeout: Duration::ZERO,
            clock: ClockChoice::default(),
            lock_cache: false,
            cache_numa_node: None,
            value_compression: eggdb_storage::ValueCompression::Off,
            encryption_key: None,
            change_archive: None,
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
    /// transaction: a larger one fails with [`ErrorKind::CacheFull`]. A large value takes
    /// about its size in pages, so it also bounds a value: with the default, a value of more
    /// than about 15.8 MiB fails with [`ErrorKind::TooLarge`]. Less than one page fails the
    /// open with [`ErrorKind::InvalidOption`].
    pub fn cache_bytes(mut self, bytes: usize) -> Self {
        self.cache_bytes = bytes;
        self
    }

    /// Keeps the page cache in RAM, never swapped out (`mlock` on Unix systems, `VirtualLock` on
    /// Windows), so a busy machine never pages the database's hot data to disk. Off by default.
    /// When the process may not lock that much memory, the open fails with
    /// [`ErrorKind::OutOfMemory`](crate::ErrorKind::OutOfMemory); raise `ulimit -l` or the
    /// working-set size, or lower [`Options::cache_bytes`].
    pub fn lock_cache(mut self, lock: bool) -> Self {
        self.lock_cache = lock;
        self
    }

    /// Places the page cache's memory on NUMA node `node` (the operating system's number, as
    /// in `/sys/devices/system/node/node<n>`), so a process whose threads run on that node's
    /// CPUs reads its hot pages from local memory; `None`, the default, leaves placement to the
    /// system. Opening fails with [`ErrorKind::Io`](crate::ErrorKind::Io), carrying the
    /// system's reason, for a node the machine does not have and where memory cannot be placed
    /// (everywhere but Linux and Android).
    pub fn cache_numa_node(mut self, node: Option<u32>) -> Self {
        self.cache_numa_node = node;
        self
    }

    /// Compresses values of 4 KiB or more that are written from now on, when compressing
    /// saves at least a sixteenth of them, with EggDB's own LZ4-format codec. Off by default.
    /// The first compressed value marks the file, so a release from before compressed values
    /// refuses to open it rather than misread it; reading needs no option.
    pub fn compress_values(mut self, compress: bool) -> Self {
        self.value_compression = if compress {
            eggdb_storage::ValueCompression::Lz4
        } else {
            eggdb_storage::ValueCompression::Off
        };
        self
    }

    /// Chooses how values of 4 KiB or more written from now on are compressed:
    /// [`ValueCompression::Lz4`](crate::ValueCompression::Lz4) is what
    /// [`Options::compress_values`] turns on, and
    /// [`ValueCompression::Zstd`](crate::ValueCompression::Zstd) uses EggDB's own Zstandard codec,
    /// slower to write and meant for cold data. Either marks the file with its own incompatible
    /// feature at the first compressed value; reading needs no option.
    pub fn value_compression(mut self, compression: crate::ValueCompression) -> Self {
        self.value_compression = compression;
        self
    }

    /// Encrypts the database's pages with `key` (D187): a new file is created encrypted, and an
    /// encrypted file opens only with the key it was created with. Every page that holds keys
    /// or values, in place and in the write-ahead log, is encrypted with AES-256 in counter
    /// mode under a key derived from this one. A file created without a key cannot take one
    /// yet. Keep the key safe: without it the data cannot be read.
    pub fn encryption_key(mut self, key: [u8; 32]) -> Self {
        self.encryption_key = Some(EncryptionKey(key));
        self
    }

    /// Keeps a continuous archive of every commit in `directory` (D1019): each commit's
    /// changed keys and their new values are appended and synced before the commit and
    /// confirmed after it, so the archive and a snapshot taken with [`Database::backup_to`]
    /// restore the database to any commit since ([`replay_changes`](crate::replay_changes)).
    /// Commits made while the database was opened without this option show as a gap that no
    /// replay crosses. A read-only open keeps nothing.
    pub fn archive_changes_to(mut self, directory: impl Into<std::path::PathBuf>) -> Self {
        self.change_archive = Some(directory.into());
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
    /// Larger pages allow longer keys (see
    /// [`Database::max_key_bytes`](crate::Database::max_key_bytes)), and keep longer values
    /// in the B+tree's leaves instead of on overflow pages.
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

    /// How long opening the file waits for another program that has it open, as SQLite's
    /// busy timeout does: the open tries again, at growing intervals of up to 50 ms, until
    /// the file is free or `timeout` has passed, and then fails with
    /// [`ErrorKind::Locked`](crate::ErrorKind::Locked). [`Duration::ZERO`] (the default)
    /// fails at once. A program that writes beside an AI agent's `eggdb mcp` calls, which
    /// hold the file for a moment each, sets it so its writes wait for the call instead of
    /// failing.
    pub fn busy_timeout(mut self, timeout: Duration) -> Self {
        self.busy_timeout = timeout;
        self
    }

    /// The clock the database reads time from: for commit stamps and for when keys with a time
    /// to live expire. The system's clock by default; a [`SimClock`](crate::SimClock) makes
    /// time move only when a test moves it, so a test can expire a key without waiting.
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let path = std::env::temp_dir().join(format!("eggdb-doc-clock-{}.db", std::process::id()));
    /// use eggdb::{Database, Options, SimClock};
    /// use std::sync::Arc;
    /// use std::time::Duration;
    ///
    /// let clock = Arc::new(SimClock::new(1_800_000_000_000_000_000));
    /// let db = Database::open_with(&path, Options::new().clock(clock.clone()))?;
    /// db.put_with_expiry("session", "abc", Duration::from_secs(30))?;
    /// clock.advance(Duration::from_secs(29));
    /// assert_eq!(db.get("session")?, Some(b"abc".to_vec()));
    /// clock.advance(Duration::from_secs(1));
    /// assert_eq!(db.get("session")?, None);
    /// # drop(db);
    /// # std::fs::remove_file(&path)?;
    /// # Ok(()) }
    /// ```
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = ClockChoice(Some(clock));
        self
    }

    pub(crate) fn creates_if_missing(&self) -> bool {
        self.create_if_missing && !self.read_only
    }

    pub(crate) fn snapshot_memory(&self) -> usize {
        self.snapshot_memory_bytes.unwrap_or(self.cache_bytes)
    }

    pub(crate) fn new_file_page_size(&self) -> Result<PageSize> {
        let bytes = self.page_size_bytes;
        let log2 = u8::try_from(bytes.trailing_zeros()).ok();
        match log2.filter(|_| bytes.is_power_of_two()) {
            Some(log2) => PageSize::from_log2(log2).map_err(|_| invalid_page_size(bytes)),
            None => Err(invalid_page_size(bytes)),
        }
    }

    pub(crate) fn check_cache_holds(&self, page_size: PageSize) -> Result<()> {
        if self.cache_bytes >= page_size.bytes() {
            return Ok(());
        }
        Err(Error::new(
            ErrorKind::InvalidOption,
            format!(
                "a page cache of {} bytes holds no page of {} bytes; give it at least one page",
                self.cache_bytes,
                page_size.bytes()
            ),
        ))
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
