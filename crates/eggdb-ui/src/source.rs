use std::time::Duration;

/// The database the UI shows. The `eggdb` command implements it on an open database file;
/// tests implement it on a map. Every method is one short transaction: the UI holds nothing
/// open between two key presses, and a write transaction lives only inside [`Source::write`].
pub trait Source {
    /// The header's facts about the database now.
    fn summary(&self) -> Result<Summary, SourceError>;

    /// Up to `limit` keys that start with `prefix`, from `start`: ascending for
    /// [`Start::First`], [`Start::After`] and [`Start::AtOrAfter`], descending for
    /// [`Start::Last`] and [`Start::Before`].
    fn keys(
        &self,
        prefix: &[u8],
        start: Start<'_>,
        limit: usize,
    ) -> Result<Vec<KeyRow>, SourceError>;

    /// The value stored under `key`, whole.
    fn value(&self, key: &[u8]) -> Result<Option<Vec<u8>>, SourceError>;

    /// Counts up to `limit` keys that start with `prefix`, after `after` when it is given.
    fn count(
        &self,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u64,
    ) -> Result<CountStep, SourceError>;

    /// Makes `change` in one write transaction and commits it, durable before this returns.
    /// When another write transaction is open, `busy` says whether to fail at once with
    /// [`ErrorKind::WriterActive`] or to wait for it up to [`Summary::write_timeout`]. Returns
    /// the commit's time, UTC, as `HH:MM:SSZ`.
    fn write(&mut self, change: &Change, busy: WriterBusy) -> Result<String, SourceError>;

    /// Starts checking every page of the database in the background. Until
    /// [`Source::check_outcome`] gives the outcome, the UI calls no other method.
    fn start_check(&mut self) -> Result<(), SourceError>;

    /// The outcome of the check [`Source::start_check`] started, once it has ended.
    fn check_outcome(&mut self) -> Option<Result<CheckSummary, SourceError>>;

    /// Closes the database and opens it again, which recovers its last durable state after
    /// [`ErrorKind::NeedsReopen`].
    fn reopen(&mut self) -> Result<(), SourceError>;

    /// Runs `text` as a SQL script, statement by statement, as `eggdb sql` does, and rolls
    /// back a transaction block the script leaves open. A source without SQL answers with an
    /// error.
    fn run_sql(&mut self, text: &str) -> SqlRun {
        let _ = text;
        SqlRun {
            failure: Some(SqlFailure {
                lines: vec!["ERROR:  this database has no SQL".to_owned()],
                position: None,
            }),
            ..SqlRun::default()
        }
    }
}

/// What a [`Source::run_sql`] run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SqlRun {
    /// Every statement's command tag that ran, in order (`CREATE TABLE`, `INSERT 0 3`).
    pub tags: Vec<String>,
    /// Notices and warnings the session gave, one per entry.
    pub notices: Vec<String>,
    /// The last result that had columns.
    pub table: Option<SqlTable>,
    /// The error that stopped the run.
    pub failure: Option<SqlFailure>,
    /// How long the run took.
    pub elapsed: Duration,
}

/// A result with columns, as text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SqlTable {
    /// The column names.
    pub columns: Vec<String>,
    /// Whether each column holds numbers, which are aligned right.
    pub numeric: Vec<bool>,
    /// The rows, each cell as text; NULL is `None`.
    pub rows: Vec<Vec<Option<String>>>,
}

/// A SQL error, printed as `eggdb sql` prints it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SqlFailure {
    /// The lines: `ERROR:  …`, `LINE n: …` and its caret, `DETAIL:`, `HINT:`, `SQLSTATE:`.
    pub lines: Vec<String>,
    /// The byte offset in the script the error points at.
    pub position: Option<usize>,
}

/// Where a [`Source::keys`] scan starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start<'a> {
    /// The first key of the prefix, ascending.
    First,
    /// The last key of the prefix, descending.
    Last,
    /// The first key after this one, ascending.
    After(&'a [u8]),
    /// This key or the first after it, ascending.
    AtOrAfter(&'a [u8]),
    /// The last key before this one, descending.
    Before(&'a [u8]),
}

/// One key of the list: the key and its value's size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRow {
    /// The key.
    pub key: Vec<u8>,
    /// The value's size in bytes.
    pub value_bytes: u64,
}

/// One step of a count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CountStep {
    /// The keys counted in this step.
    pub counted: u64,
    /// The last key counted when the step stopped at its limit; `None` when the count is done.
    pub resume_after: Option<Vec<u8>>,
}

/// One change the user confirmed: [`Source::write`] makes it in one commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Stores `value` under `key`, adding the key or replacing its value.
    Put {
        /// The key.
        key: Vec<u8>,
        /// The new value.
        value: Vec<u8>,
    },
    /// Removes `key`.
    Delete {
        /// The key.
        key: Vec<u8>,
    },
}

impl Change {
    /// The key the change is to.
    pub fn key(&self) -> &[u8] {
        match self {
            Self::Put { key, .. } | Self::Delete { key } => key,
        }
    }
}

/// What [`Source::write`] does when another write transaction is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterBusy {
    /// Fail at once with [`ErrorKind::WriterActive`].
    Fail,
    /// Wait for it to end, up to [`Summary::write_timeout`].
    Wait,
}

/// What [`Source::check_outcome`] reports about a database whose every page passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CheckSummary {
    /// How many keys the database holds.
    pub entries: u64,
    /// The bytes of every key together.
    pub key_bytes: u64,
    /// The bytes of every value together.
    pub value_bytes: u64,
    /// The B+tree's levels.
    pub tree_height: u32,
    /// The B+tree's pages.
    pub tree_pages: u64,
    /// Values stored on overflow pages.
    pub overflow_values: u64,
    /// The overflow pages holding them.
    pub overflow_pages: u64,
    /// Pages free for later commits.
    pub free_pages: u64,
    /// Pages of the write-ahead log.
    pub log_pages: u64,
    /// Pages in use by nothing at all.
    pub leaked_pages: u64,
}

/// Why a [`Source`] call failed: the engine's error kind and its message, which is written
/// for people and models alike.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceError {
    /// What kind of failure it was.
    pub kind: ErrorKind,
    /// What happened and what to do about it.
    pub message: String,
}

impl SourceError {
    /// An error of `kind` saying `message`.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// The kind of a failure, one for each kind `eggdb::ErrorKind` has, named as it names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// The database file does not exist.
    NotFound,
    /// The operating system refused access to the file.
    PermissionDenied,
    /// The file is open elsewhere.
    Locked,
    /// The database is open read-only.
    ReadOnly,
    /// The file is not an EggDB database.
    NotADatabase,
    /// The file was written by a newer EggDB.
    Incompatible,
    /// A page or structure of the file is damaged.
    Corrupt,
    /// The file system failed.
    Io,
    /// Memory could not be had.
    OutOfMemory,
    /// A key or value is longer than the database stores, or than one commit's pages fit in
    /// the page cache; nothing changed.
    TooLarge,
    /// The commit changed more pages than the page cache holds; it was rolled back.
    CacheFull,
    /// Another write transaction is open.
    WriterActive,
    /// A read transaction's snapshot expired.
    SnapshotTooOld,
    /// The write transaction failed earlier and was rolled back.
    TransactionFailed,
    /// An earlier write failed; the database must be opened again.
    NeedsReopen,
    /// The file can take no more commits.
    DatabaseFull,
    /// An option is outside what EggDB accepts.
    InvalidOption,
    /// There is no collection of that name.
    CollectionNotFound,
    /// The name can't name a collection.
    InvalidCollectionName,
    /// The key starts with the bytes EggDB keeps for collections.
    ReservedKey,
    /// Anything else.
    Other,
}

impl ErrorKind {
    /// The kind's name, as `eggdb::ErrorKind` spells it.
    pub fn name(self) -> &'static str {
        match self {
            Self::NotFound => "NotFound",
            Self::PermissionDenied => "PermissionDenied",
            Self::Locked => "Locked",
            Self::ReadOnly => "ReadOnly",
            Self::NotADatabase => "NotADatabase",
            Self::Incompatible => "Incompatible",
            Self::Corrupt => "Corrupt",
            Self::Io => "Io",
            Self::OutOfMemory => "OutOfMemory",
            Self::TooLarge => "TooLarge",
            Self::CacheFull => "CacheFull",
            Self::WriterActive => "WriterActive",
            Self::SnapshotTooOld => "SnapshotTooOld",
            Self::TransactionFailed => "TransactionFailed",
            Self::NeedsReopen => "NeedsReopen",
            Self::DatabaseFull => "DatabaseFull",
            Self::InvalidOption => "InvalidOption",
            Self::CollectionNotFound => "CollectionNotFound",
            Self::InvalidCollectionName => "InvalidCollectionName",
            Self::ReservedKey => "ReservedKey",
            Self::Other => "Other",
        }
    }
}

/// What the header and the check screen show about the database, read from the file's header
/// without touching a page.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Summary {
    /// The file's name as the user gave it.
    pub name: String,
    /// The file's size in bytes.
    pub file_bytes: u64,
    /// The pages in the file.
    pub page_count: u64,
    /// The pages free for later commits.
    pub free_pages: u64,
    /// The page size in bytes.
    pub page_size: usize,
    /// The time of the last commit, UTC, as `HH:MM:SSZ`; `None` before the first commit.
    pub last_commit: Option<String>,
    /// The last commit's date, time and stamp in full, for the check screen.
    pub last_commit_detail: Option<String>,
    /// Whether the database is open read-only.
    pub read_only: bool,
    /// The longest key, in bytes.
    pub max_key_bytes: usize,
    /// The largest value, in bytes.
    pub max_value_bytes: u64,
    /// The page cache's size in bytes.
    pub cache_bytes: u64,
    /// The pages the cache holds now.
    pub cached_pages: u64,
    /// Page lookups the cache answered since the database was opened.
    pub cache_hits: u64,
    /// Page lookups that read the file.
    pub cache_misses: u64,
    /// Bytes of write-ahead log since the last checkpoint.
    pub log_bytes: u64,
    /// How long a write waits for another write transaction to end.
    pub write_timeout: Duration,
}
