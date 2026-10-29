use crate::args;
use crate::status::{EGGDB_ERR_IN_USE, Failure, guarded};
use eggdb::{BackupReport, CompactReport, Database, Durability, Options};
use std::ffi::{c_char, c_int};
use std::sync::Arc;

/// `eggdb_options.flags`: open the database read-only; several read-only opens can share the
/// file.
pub const EGGDB_OPEN_READ_ONLY: u32 = 1;
/// `eggdb_options.flags`: fail with `EGGDB_ERR_NOT_FOUND` instead of creating a missing file.
pub const EGGDB_OPEN_NO_CREATE: u32 = 2;
/// `eggdb_options.durability`: a commit returns once it is on the disk (the default).
pub const EGGDB_DURABILITY_FULL: u32 = 0;
/// `eggdb_options.durability`: a commit returns once it is written; a power loss can lose the
/// latest commits but never corrupts the file.
pub const EGGDB_DURABILITY_NORMAL: u32 = 1;
/// `eggdb_options.durability`: nothing waits for the disk; a power loss can corrupt the file.
pub const EGGDB_DURABILITY_OFF: u32 = 2;

const KNOWN_OPEN_FLAGS: u32 = EGGDB_OPEN_READ_ONLY | EGGDB_OPEN_NO_CREATE;

/// How [`eggdb_open`] opens a database (`eggdb_options` in `eggdb.h`); NULL takes the defaults.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct EggdbOptions {
    /// `EGGDB_OPEN_READ_ONLY` and `EGGDB_OPEN_NO_CREATE`, or 0.
    pub flags: u32,
    /// `EGGDB_DURABILITY_FULL`, `EGGDB_DURABILITY_NORMAL` or `EGGDB_DURABILITY_OFF`.
    pub durability: u32,
    /// The page cache in bytes, which is also the most one write transaction can change; 0 takes
    /// the default, 16 MiB.
    pub cache_bytes: u64,
}

/// What [`eggdb_backup`] wrote (`eggdb_backup_report` in `eggdb.h`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct EggdbBackupReport {
    /// The stamp of the commit the copy holds, as `eggdb_commit` reports stamps.
    pub stamp: u64,
    /// The entries copied, with the records EggDB keeps beside keys.
    pub entries: u64,
    /// The bytes of every key copied.
    pub key_bytes: u64,
    /// The bytes of every value copied.
    pub value_bytes: u64,
    /// The copy's pages.
    pub page_count: u64,
    /// The copy's size in bytes.
    pub file_bytes: u64,
}

/// What [`eggdb_compact`] and [`eggdb_compact_file`] did (`eggdb_compact_report` in
/// `eggdb.h`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct EggdbCompactReport {
    /// The stamp of the commit the compacted file holds.
    pub stamp: u64,
    /// The entries the compacted file holds.
    pub entries: u64,
    /// The file's pages before.
    pub pages_before: u64,
    /// The free pages it had.
    pub free_pages_before: u64,
    /// The pages it had lost track of.
    pub leaked_pages_before: u64,
    /// The compacted file's pages.
    pub pages_after: u64,
    /// The file's size in bytes before.
    pub bytes_before: u64,
    /// The compacted file's size in bytes.
    pub bytes_after: u64,
}

impl From<BackupReport> for EggdbBackupReport {
    fn from(report: BackupReport) -> Self {
        Self {
            stamp: report.stamp.bits(),
            entries: report.entries,
            key_bytes: report.key_bytes,
            value_bytes: report.value_bytes,
            page_count: report.page_count,
            file_bytes: report.file_bytes,
        }
    }
}

impl From<CompactReport> for EggdbCompactReport {
    fn from(report: CompactReport) -> Self {
        Self {
            stamp: report.stamp.bits(),
            entries: report.entries,
            pages_before: report.pages_before,
            free_pages_before: report.free_pages_before,
            leaked_pages_before: report.leaked_pages_before,
            pages_after: report.pages_after,
            bytes_before: report.bytes_before,
            bytes_after: report.bytes_after,
        }
    }
}

/// An open database (`eggdb_db` in `eggdb.h`), from [`eggdb_open`]. Any number of threads may
/// use one at once. Transactions and SQL sessions keep it alive until they end.
#[derive(Debug)]
pub struct EggdbDb {
    pub(crate) database: Arc<Database>,
}

impl EggdbDb {
    fn refuse_while_in_use(&self, doing: &str) -> Result<(), Failure> {
        let others = Arc::strong_count(&self.database) - 1;
        if others == 0 {
            return Ok(());
        }
        Err(Failure::new(
            EGGDB_ERR_IN_USE,
            format!(
                "cannot {doing} {}: {others} transaction(s), scan(s) or SQL session(s) still use it; end them first (eggdb_commit, eggdb_rollback, eggdb_sql_close). The database is still open.",
                self.database.path().display()
            ),
        ))
    }
}

fn open_options(options: Option<&EggdbOptions>) -> Result<Options, Failure> {
    let options = options.copied().unwrap_or_default();
    let unknown = options.flags & !KNOWN_OPEN_FLAGS;
    if unknown != 0 {
        return Err(Failure::invalid_argument(format!(
            "eggdb_options.flags has unknown bits {unknown:#x}; use EGGDB_OPEN_READ_ONLY and EGGDB_OPEN_NO_CREATE"
        )));
    }
    let durability = match options.durability {
        EGGDB_DURABILITY_FULL => Durability::Full,
        EGGDB_DURABILITY_NORMAL => Durability::Normal,
        EGGDB_DURABILITY_OFF => Durability::Off,
        other => {
            return Err(Failure::invalid_argument(format!(
                "eggdb_options.durability is {other}; use EGGDB_DURABILITY_FULL, EGGDB_DURABILITY_NORMAL or EGGDB_DURABILITY_OFF"
            )));
        }
    };
    let mut opened = Options::new()
        .read_only(options.flags & EGGDB_OPEN_READ_ONLY != 0)
        .create_if_missing(options.flags & EGGDB_OPEN_NO_CREATE == 0)
        .durability(durability);
    if options.cache_bytes != 0 {
        let bytes = usize::try_from(options.cache_bytes).map_err(|_| {
            Failure::invalid_argument(format!(
                "eggdb_options.cache_bytes is {}, more than this computer can address",
                options.cache_bytes
            ))
        })?;
        opened = opened.cache_bytes(bytes);
    }
    Ok(opened)
}

/// Opens the database file at `path` (UTF-8), creating an empty one if no file is there, and
/// puts its handle in `*db`; `options` may be NULL for the defaults. On failure `*db` is NULL.
///
/// # Safety
/// `path` is a NUL-terminated string, `options` is NULL or points to an `eggdb_options`, and
/// `db` points to writable memory for one handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_open(
    path: *const c_char,
    options: *const EggdbOptions,
    db: *mut *mut EggdbDb,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one handle at `db`.
        unsafe { args::write(db, std::ptr::null_mut(), "db") }?;
        // SAFETY: eggdb.h asks for a NUL-terminated path.
        let path = unsafe { args::text(path, "path") }?;
        // SAFETY: eggdb.h asks for NULL or an `eggdb_options`.
        let options = open_options(unsafe { args::optional_struct(options) })?;
        let database = Database::open_with(path, options)?;
        let handle = args::give(EggdbDb {
            database: Arc::new(database),
        });
        // SAFETY: as for the first write to `db`.
        unsafe { args::write(db, handle, "db") }
    })
}

/// Closes the database: writes a checkpoint, so the next open replays nothing, and frees the
/// handle. It fails with `EGGDB_ERR_IN_USE`, and leaves the database open, while transactions
/// or SQL sessions still use it. The handle is freed even when the checkpoint fails; the next
/// open then recovers the file as after a crash. NULL does nothing.
///
/// # Safety
/// `db` is NULL or a handle from [`eggdb_open`] that is not closed, used by no other call at
/// the same time; after `EGGDB_OK` or any status but `EGGDB_ERR_IN_USE` it must not be used.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_close(db: *mut EggdbDb) -> c_int {
    guarded(|| {
        if db.is_null() {
            return Ok(());
        }
        // SAFETY: eggdb.h asks for a live handle from eggdb_open.
        unsafe { args::handle(db, "db") }?.refuse_while_in_use("close")?;
        // SAFETY: the caller gives the handle up with this call (eggdb.h).
        let Some(handle) = (unsafe { args::take(db) }) else {
            return Ok(());
        };
        match Arc::try_unwrap(handle.database) {
            Ok(database) => database.close().map_err(Failure::from),
            Err(_) => Ok(()),
        }
    })
}

/// Writes a copy of the database's last commit to a new file at `path` (UTF-8) while readers
/// and the writer keep working, and fills `*report` when it is not NULL. A file at `path`
/// fails with `EGGDB_ERR_ALREADY_EXISTS`: a backup never replaces a file.
///
/// # Safety
/// `db` is a handle from [`eggdb_open`], `path` a NUL-terminated string, and `report` NULL or
/// writable memory for an `eggdb_backup_report`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_backup(
    db: *mut EggdbDb,
    path: *const c_char,
    report: *mut EggdbBackupReport,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for a live handle from eggdb_open.
        let db = unsafe { args::handle(db, "db") }?;
        // SAFETY: eggdb.h asks for a NUL-terminated path.
        let path = unsafe { args::text(path, "path") }?;
        let written = db.database.backup_to(path)?;
        // SAFETY: eggdb.h asks for NULL or writable memory for one report.
        unsafe { args::write_if_wanted(report, EggdbBackupReport::from(written)) };
        Ok(())
    })
}

/// Compacts the database's file: checks it, rewrites it without free or leaked pages and puts
/// the rewritten file in its place atomically, then closes the database and frees the handle,
/// as `Database::compact` does; `*report` is filled when it is not NULL. It fails with
/// `EGGDB_ERR_IN_USE`, and leaves the database open, while transactions or SQL sessions still
/// use it; after any other status the handle is freed.
///
/// # Safety
/// `db` is a handle from [`eggdb_open`] used by no other call at the same time, and `report`
/// NULL or writable memory for an `eggdb_compact_report`; after any status but
/// `EGGDB_ERR_IN_USE` or `EGGDB_ERR_INVALID_ARGUMENT`, `db` must not be used.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_compact(db: *mut EggdbDb, report: *mut EggdbCompactReport) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for a live handle from eggdb_open.
        unsafe { args::handle(db, "db") }?.refuse_while_in_use("compact")?;
        // SAFETY: the caller gives the handle up with this call (eggdb.h).
        let Some(handle) = (unsafe { args::take(db) }) else {
            return Err(Failure::invalid_argument("db is NULL"));
        };
        let database = Arc::try_unwrap(handle.database).map_err(|_| {
            Failure::new(
                EGGDB_ERR_IN_USE,
                "cannot compact the database: another thread began a transaction or SQL session on it during this call; the database closes when that ends",
            )
        })?;
        let compacted = database.compact()?;
        // SAFETY: eggdb.h asks for NULL or writable memory for one report.
        unsafe { args::write_if_wanted(report, EggdbCompactReport::from(compacted)) };
        Ok(())
    })
}

/// Opens the database file at `path` (UTF-8) read-only and compacts it, so programs reading
/// the file keep working throughout, as `Database::compact_file` does; `*report` is filled
/// when it is not NULL. A program that has the file open for writing makes it fail with
/// `EGGDB_ERR_LOCKED`.
///
/// # Safety
/// `path` is a NUL-terminated string and `report` NULL or writable memory for an
/// `eggdb_compact_report`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_compact_file(
    path: *const c_char,
    report: *mut EggdbCompactReport,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for a NUL-terminated path.
        let path = unsafe { args::text(path, "path") }?;
        let compacted = Database::compact_file(path)?;
        // SAFETY: eggdb.h asks for NULL or writable memory for one report.
        unsafe { args::write_if_wanted(report, EggdbCompactReport::from(compacted)) };
        Ok(())
    })
}

/// Removes every key whose time to live has run out, in the default key space and in every
/// collection, and puts how many it removed in `*removed` when it is not NULL.
///
/// # Safety
/// `db` is a handle from [`eggdb_open`] and `removed` NULL or writable memory for a `uint64_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_remove_expired(db: *mut EggdbDb, removed: *mut u64) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for a live handle from eggdb_open.
        let db = unsafe { args::handle(db, "db") }?;
        let count = db.database.remove_expired()?;
        // SAFETY: eggdb.h asks for NULL or writable memory for one uint64_t.
        unsafe { args::write_if_wanted(removed, count) };
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::{EGGDB_DURABILITY_NORMAL, EGGDB_OPEN_READ_ONLY, EggdbOptions, open_options};
    use crate::status::EGGDB_ERR_INVALID_ARGUMENT;
    use eggdb::{Durability, Options};

    fn status_of(options: EggdbOptions) -> Option<std::ffi::c_int> {
        open_options(Some(&options))
            .err()
            .map(|failure| crate::status::guarded(|| Err(failure)))
    }

    #[test]
    fn no_options_are_the_defaults() {
        assert_eq!(open_options(None).ok(), Some(Options::new()));
    }

    #[test]
    fn flags_durability_and_cache_become_options() {
        let options = EggdbOptions {
            flags: EGGDB_OPEN_READ_ONLY,
            durability: EGGDB_DURABILITY_NORMAL,
            cache_bytes: 1 << 20,
        };
        let expected = Options::new()
            .read_only(true)
            .durability(Durability::Normal)
            .cache_bytes(1 << 20);
        assert_eq!(open_options(Some(&options)).ok(), Some(expected));
    }

    #[test]
    fn unknown_flags_and_durabilities_are_refused() {
        let unknown_flag = EggdbOptions {
            flags: 0x80,
            ..EggdbOptions::default()
        };
        assert_eq!(status_of(unknown_flag), Some(EGGDB_ERR_INVALID_ARGUMENT));
        let unknown_durability = EggdbOptions {
            durability: 7,
            ..EggdbOptions::default()
        };
        assert_eq!(status_of(unknown_durability), Some(EGGDB_ERR_INVALID_ARGUMENT));
    }
}
