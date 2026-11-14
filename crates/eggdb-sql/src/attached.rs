use std::cell::RefCell;
use std::sync::OnceLock;

/// Reads a database file for SQLite's `ATTACH` (D1131): given the file's path and the alias it
/// is attached as, the SQL that recreates its tables and rows under `"alias"."table"`, in
/// SQLite's dialect, or why the file cannot be read.
pub type AttachReader = Box<dyn Fn(&str, &str) -> Result<Vec<String>, String> + Send + Sync>;

static READER: OnceLock<AttachReader> = OnceLock::new();

/// Sets how `ATTACH 'file' AS alias` reads a file, once per process; later calls keep the
/// first reader. Without one, only `ATTACH ':memory:'` works.
pub fn set_attach_reader(reader: AttachReader) {
    let _ = READER.set(reader);
}

pub(crate) fn read(path: &str, alias: &str) -> Option<Result<Vec<String>, String>> {
    READER.get().map(|reader| reader(path, alias))
}

pub(crate) const MARKER: &str = "eggdb: attached by ATTACH";

thread_local! {
    static ACTIVE: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

pub(crate) struct EnteredAliases {
    previous: Vec<String>,
}

impl Drop for EnteredAliases {
    fn drop(&mut self) {
        let previous = std::mem::take(&mut self.previous);
        ACTIVE.with(|active| *active.borrow_mut() = previous);
    }
}

/// Makes `aliases` the attached databases the statements parsed until the guard drops can
/// name, as `alias.table`.
pub(crate) fn enter(aliases: &[String]) -> EnteredAliases {
    let previous = ACTIVE.with(|active| std::mem::replace(&mut *active.borrow_mut(), aliases.to_vec()));
    EnteredAliases { previous }
}

pub(crate) fn is_attached(alias: &str) -> bool {
    ACTIVE.with(|active| active.borrow().iter().any(|attached| attached.eq_ignore_ascii_case(alias)))
}

pub(crate) fn is_memory(path: &str) -> bool {
    path.is_empty() || path == ":memory:" || path.starts_with("file::memory:")
}
