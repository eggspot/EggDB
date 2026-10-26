/// The database the UI shows. The `eggdb` command implements it on an open database file;
/// tests implement it on a map. Every method is one short read: the UI holds nothing open
/// between two key presses.
pub trait Source {
    /// The header's facts about the database now.
    fn summary(&self) -> Result<Summary, String>;

    /// Up to `limit` keys that start with `prefix`, from `start`: ascending for
    /// [`Start::First`], [`Start::After`] and [`Start::AtOrAfter`], descending for
    /// [`Start::Last`] and [`Start::Before`].
    fn keys(&self, prefix: &[u8], start: Start<'_>, limit: usize) -> Result<Vec<KeyRow>, String>;

    /// The value stored under `key`, whole.
    fn value(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String>;

    /// Counts up to `limit` keys that start with `prefix`, after `after` when it is given.
    fn count(&self, prefix: &[u8], after: Option<&[u8]>, limit: u64) -> Result<CountStep, String>;
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

/// What the header shows about the database.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Summary {
    /// The file's name as the user gave it.
    pub name: String,
    /// The file's size in bytes.
    pub file_bytes: u64,
    /// The pages in the file.
    pub page_count: u64,
    /// The page size in bytes.
    pub page_size: usize,
    /// The time of the last commit, UTC, as `HH:MM:SSZ`; `None` before the first commit.
    pub last_commit: Option<String>,
    /// Whether the database is open read-only.
    pub read_only: bool,
    /// The longest key, in bytes.
    pub max_key_bytes: usize,
    /// The largest value, in bytes.
    pub max_value_bytes: u64,
}
