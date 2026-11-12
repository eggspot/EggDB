/// Creates the files that statements such as SQLite's `VACUUM INTO` write. An embedded database
/// gives its sessions one with [`crate::Session::set_new_file_writer`]; a server gives none, so
/// its clients cannot write files on the server's machine.
pub trait NewFileWriter: Sync {
    /// Creates the file at `path` holding `bytes`, and fails when a file is already there.
    fn write_new_file(&self, path: &str, bytes: &[u8]) -> Result<(), String>;
}

/// Reads files that SQL functions such as `read_parquet` open. An embedded database gives its
/// sessions one with [`crate::Session::set_file_reader`]; a server gives none, so its clients
/// cannot read files on the server's machine.
pub trait FileReader: Sync {
    /// The bytes of the file at `path`, or an error when it is missing, unreadable or larger
    /// than `most_bytes`.
    fn read_file(&self, path: &str, most_bytes: usize) -> Result<Vec<u8>, String>;
}

thread_local! {
    static CURRENT_READER: std::cell::Cell<Option<&'static dyn FileReader>> = const { std::cell::Cell::new(None) };
}

/// Runs `body` with `reader` as the file reader of the statements it runs on this thread.
pub(crate) fn with_reader<T>(reader: Option<&'static dyn FileReader>, body: impl FnOnce() -> T) -> T {
    let earlier = CURRENT_READER.with(|current| current.replace(reader));
    let outcome = body();
    CURRENT_READER.with(|current| current.set(earlier));
    outcome
}

/// The bytes of the file at `path`, through the running session's reader; `0A000` when the
/// session has none, as a server's sessions do.
pub(crate) fn read_file(path: &str, most_bytes: usize) -> crate::error::SqlResult<Vec<u8>> {
    let Some(reader) = CURRENT_READER.with(std::cell::Cell::get) else {
        return Err(crate::error::SqlError::new(
            crate::error::SqlState::FEATURE_NOT_SUPPORTED,
            "this session cannot read files on the server's machine",
        ));
    };
    reader
        .read_file(path, most_bytes)
        .map_err(|message| crate::error::SqlError::new(crate::error::SqlState::UNDEFINED_FILE, message))
}
