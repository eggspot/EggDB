use eggdb::ErrorKind;
use eggdb::sql::SqlError;
use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_int};
use std::panic::{self, AssertUnwindSafe};

/// The call succeeded.
pub const EGGDB_OK: c_int = 0;
/// The database file does not exist, and the options did not allow creating it.
pub const EGGDB_ERR_NOT_FOUND: c_int = 1;
/// The operating system refused access to the file or its directory.
pub const EGGDB_ERR_PERMISSION_DENIED: c_int = 2;
/// The file is open elsewhere, in this process or another.
pub const EGGDB_ERR_LOCKED: c_int = 3;
/// The database was opened read-only and the call would change it.
pub const EGGDB_ERR_READ_ONLY: c_int = 4;
/// The file is not an EggDB database.
pub const EGGDB_ERR_NOT_A_DATABASE: c_int = 5;
/// The file was written by a newer EggDB, with a format this release cannot read or change.
pub const EGGDB_ERR_INCOMPATIBLE: c_int = 6;
/// The file is damaged.
pub const EGGDB_ERR_CORRUPT: c_int = 7;
/// The file system failed to read, write or sync.
pub const EGGDB_ERR_IO: c_int = 8;
/// Memory could not be had.
pub const EGGDB_ERR_OUT_OF_MEMORY: c_int = 9;
/// A key or a value is longer than EggDB stores; nothing changed and the transaction stays open.
pub const EGGDB_ERR_TOO_LARGE: c_int = 10;
/// A write transaction changed more pages than the page cache holds and was rolled back.
pub const EGGDB_ERR_CACHE_FULL: c_int = 11;
/// Another write transaction is open, or this thread began the open one.
pub const EGGDB_ERR_WRITER_ACTIVE: c_int = 12;
/// A read transaction's snapshot expired; begin a new one.
pub const EGGDB_ERR_SNAPSHOT_TOO_OLD: c_int = 13;
/// The write transaction failed earlier and was rolled back then.
pub const EGGDB_ERR_TRANSACTION_FAILED: c_int = 14;
/// An earlier write or sync failed; close the database and open it again.
pub const EGGDB_ERR_NEEDS_REOPEN: c_int = 15;
/// The database can take no more commits.
pub const EGGDB_ERR_DATABASE_FULL: c_int = 16;
/// An open option is outside what EggDB accepts.
pub const EGGDB_ERR_INVALID_OPTION: c_int = 17;
/// There is no collection of that name.
pub const EGGDB_ERR_COLLECTION_NOT_FOUND: c_int = 18;
/// The name can't name a collection.
pub const EGGDB_ERR_INVALID_COLLECTION_NAME: c_int = 19;
/// The key starts with the bytes EggDB keeps for collections.
pub const EGGDB_ERR_RESERVED_KEY: c_int = 20;
/// A file is already where a backup or a compaction would put one.
pub const EGGDB_ERR_ALREADY_EXISTS: c_int = 21;
/// The value is not a counter, or the step would take it out of range.
pub const EGGDB_ERR_NOT_AN_INTEGER: c_int = 22;
/// Any other failure of the database.
pub const EGGDB_ERR_OTHER: c_int = 23;
/// An argument is wrong: a NULL that must not be, text that is not UTF-8, an unknown flag or
/// dialect.
pub const EGGDB_ERR_INVALID_ARGUMENT: c_int = 100;
/// The database handle still has transactions or SQL sessions; end them first.
pub const EGGDB_ERR_IN_USE: c_int = 101;
/// The call changes data and the transaction is a read transaction.
pub const EGGDB_ERR_READ_TRANSACTION: c_int = 102;
/// A SQL statement failed; `eggdb_last_sqlstate` gives its SQLSTATE.
pub const EGGDB_ERR_SQL: c_int = 103;
/// EggDB hit a bug of its own; the call did nothing it reports.
pub const EGGDB_ERR_INTERNAL: c_int = 104;

const STATUS_NAMES: [(c_int, &CStr); 29] = [
    (EGGDB_OK, c"EGGDB_OK"),
    (EGGDB_ERR_NOT_FOUND, c"EGGDB_ERR_NOT_FOUND"),
    (EGGDB_ERR_PERMISSION_DENIED, c"EGGDB_ERR_PERMISSION_DENIED"),
    (EGGDB_ERR_LOCKED, c"EGGDB_ERR_LOCKED"),
    (EGGDB_ERR_READ_ONLY, c"EGGDB_ERR_READ_ONLY"),
    (EGGDB_ERR_NOT_A_DATABASE, c"EGGDB_ERR_NOT_A_DATABASE"),
    (EGGDB_ERR_INCOMPATIBLE, c"EGGDB_ERR_INCOMPATIBLE"),
    (EGGDB_ERR_CORRUPT, c"EGGDB_ERR_CORRUPT"),
    (EGGDB_ERR_IO, c"EGGDB_ERR_IO"),
    (EGGDB_ERR_OUT_OF_MEMORY, c"EGGDB_ERR_OUT_OF_MEMORY"),
    (EGGDB_ERR_TOO_LARGE, c"EGGDB_ERR_TOO_LARGE"),
    (EGGDB_ERR_CACHE_FULL, c"EGGDB_ERR_CACHE_FULL"),
    (EGGDB_ERR_WRITER_ACTIVE, c"EGGDB_ERR_WRITER_ACTIVE"),
    (EGGDB_ERR_SNAPSHOT_TOO_OLD, c"EGGDB_ERR_SNAPSHOT_TOO_OLD"),
    (EGGDB_ERR_TRANSACTION_FAILED, c"EGGDB_ERR_TRANSACTION_FAILED"),
    (EGGDB_ERR_NEEDS_REOPEN, c"EGGDB_ERR_NEEDS_REOPEN"),
    (EGGDB_ERR_DATABASE_FULL, c"EGGDB_ERR_DATABASE_FULL"),
    (EGGDB_ERR_INVALID_OPTION, c"EGGDB_ERR_INVALID_OPTION"),
    (EGGDB_ERR_COLLECTION_NOT_FOUND, c"EGGDB_ERR_COLLECTION_NOT_FOUND"),
    (
        EGGDB_ERR_INVALID_COLLECTION_NAME,
        c"EGGDB_ERR_INVALID_COLLECTION_NAME",
    ),
    (EGGDB_ERR_RESERVED_KEY, c"EGGDB_ERR_RESERVED_KEY"),
    (EGGDB_ERR_ALREADY_EXISTS, c"EGGDB_ERR_ALREADY_EXISTS"),
    (EGGDB_ERR_NOT_AN_INTEGER, c"EGGDB_ERR_NOT_AN_INTEGER"),
    (EGGDB_ERR_OTHER, c"EGGDB_ERR_OTHER"),
    (EGGDB_ERR_INVALID_ARGUMENT, c"EGGDB_ERR_INVALID_ARGUMENT"),
    (EGGDB_ERR_IN_USE, c"EGGDB_ERR_IN_USE"),
    (EGGDB_ERR_READ_TRANSACTION, c"EGGDB_ERR_READ_TRANSACTION"),
    (EGGDB_ERR_SQL, c"EGGDB_ERR_SQL"),
    (EGGDB_ERR_INTERNAL, c"EGGDB_ERR_INTERNAL"),
];

const INTERNAL_MESSAGE: &str = "EggDB hit an internal error (a bug, please report it at https://github.com/eggspot/EggDB/issues) and stopped this call; handles passed to it are still valid, and a transaction it was changing should be rolled back";

#[derive(Debug)]
pub(crate) struct Failure {
    status: c_int,
    message: String,
    sqlstate: Option<String>,
}

impl Failure {
    pub(crate) fn new(status: c_int, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            sqlstate: None,
        }
    }

    pub(crate) fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(EGGDB_ERR_INVALID_ARGUMENT, message)
    }

    pub(crate) fn remote_sql(sqlstate: &str, message: &str) -> Self {
        Self {
            status: EGGDB_ERR_SQL,
            message: message.to_owned(),
            sqlstate: Some(sqlstate.to_owned()),
        }
    }

    pub(crate) fn sql(error: &SqlError) -> Self {
        Self {
            status: EGGDB_ERR_SQL,
            message: error.to_string(),
            sqlstate: Some(error.state().code().to_owned()),
        }
    }
}

impl From<eggdb::Error> for Failure {
    fn from(error: eggdb::Error) -> Self {
        Self::new(status_of(error.kind()), error.to_string())
    }
}

fn status_of(kind: ErrorKind) -> c_int {
    match kind {
        ErrorKind::NotFound => EGGDB_ERR_NOT_FOUND,
        ErrorKind::PermissionDenied => EGGDB_ERR_PERMISSION_DENIED,
        ErrorKind::Locked => EGGDB_ERR_LOCKED,
        ErrorKind::ReadOnly => EGGDB_ERR_READ_ONLY,
        ErrorKind::NotADatabase => EGGDB_ERR_NOT_A_DATABASE,
        ErrorKind::Incompatible => EGGDB_ERR_INCOMPATIBLE,
        ErrorKind::Corrupt => EGGDB_ERR_CORRUPT,
        ErrorKind::Io => EGGDB_ERR_IO,
        ErrorKind::OutOfMemory => EGGDB_ERR_OUT_OF_MEMORY,
        ErrorKind::TooLarge => EGGDB_ERR_TOO_LARGE,
        ErrorKind::CacheFull => EGGDB_ERR_CACHE_FULL,
        ErrorKind::WriterActive => EGGDB_ERR_WRITER_ACTIVE,
        ErrorKind::SnapshotTooOld => EGGDB_ERR_SNAPSHOT_TOO_OLD,
        ErrorKind::TransactionFailed => EGGDB_ERR_TRANSACTION_FAILED,
        ErrorKind::NeedsReopen => EGGDB_ERR_NEEDS_REOPEN,
        ErrorKind::DatabaseFull => EGGDB_ERR_DATABASE_FULL,
        ErrorKind::InvalidOption => EGGDB_ERR_INVALID_OPTION,
        ErrorKind::CollectionNotFound => EGGDB_ERR_COLLECTION_NOT_FOUND,
        ErrorKind::InvalidCollectionName => EGGDB_ERR_INVALID_COLLECTION_NAME,
        ErrorKind::ReservedKey => EGGDB_ERR_RESERVED_KEY,
        ErrorKind::AlreadyExists => EGGDB_ERR_ALREADY_EXISTS,
        ErrorKind::NotAnInteger => EGGDB_ERR_NOT_AN_INTEGER,
        _ => EGGDB_ERR_OTHER,
    }
}

struct LastFailure {
    message: CString,
    sqlstate: CString,
}

thread_local! {
    static LAST_FAILURE: RefCell<LastFailure> = RefCell::new(LastFailure {
        message: CString::default(),
        sqlstate: CString::default(),
    });
}

fn c_text(text: &str) -> CString {
    CString::new(text.replace('\0', "\u{fffd}")).unwrap_or_default()
}

fn record(failure: Failure) -> c_int {
    let replacement = LastFailure {
        message: c_text(&failure.message),
        sqlstate: c_text(failure.sqlstate.as_deref().unwrap_or("")),
    };
    let _ = LAST_FAILURE.try_with(|last| {
        if let Ok(mut last) = last.try_borrow_mut() {
            *last = replacement;
        }
    });
    failure.status
}

pub(crate) fn guarded(body: impl FnOnce() -> Result<(), Failure>) -> c_int {
    match panic::catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(())) => EGGDB_OK,
        Ok(Err(failure)) => record(failure),
        Err(_) => record(Failure::new(EGGDB_ERR_INTERNAL, INTERNAL_MESSAGE)),
    }
}

fn last_text(read: fn(&LastFailure) -> *const c_char) -> *const c_char {
    LAST_FAILURE
        .try_with(|last| last.try_borrow().map(|last| read(&last)).ok())
        .ok()
        .flatten()
        .unwrap_or(c"".as_ptr())
}

/// The message of the last call on this thread that failed, or `""` when none has: what
/// happened and what to do about it. It stays valid until the next call on this thread fails.
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_last_error() -> *const c_char {
    last_text(|last| last.message.as_ptr())
}

/// The SQLSTATE of the last call on this thread that failed with `EGGDB_ERR_SQL`, such as
/// `"42P01"`, or `""` when the last failure was not a SQL statement's. It stays valid until the
/// next call on this thread fails.
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_last_sqlstate() -> *const c_char {
    last_text(|last| last.sqlstate.as_ptr())
}

/// The name of a status code, such as `"EGGDB_ERR_LOCKED"`, or `"EGGDB_UNKNOWN_STATUS"` for a
/// code this release does not return. The text is static.
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_status_name(status: c_int) -> *const c_char {
    STATUS_NAMES
        .iter()
        .find(|(code, _)| *code == status)
        .map_or(c"EGGDB_UNKNOWN_STATUS", |&(_, name)| name)
        .as_ptr()
}

#[cfg(test)]
mod tests {
    use super::{
        EGGDB_ERR_INTERNAL, EGGDB_ERR_LOCKED, EGGDB_OK, Failure, STATUS_NAMES, eggdb_last_error,
        eggdb_status_name, guarded,
    };
    use std::ffi::CStr;

    fn last_error() -> String {
        // SAFETY: eggdb_last_error returns a NUL-terminated string that stays valid until the
        // next failing call on this thread, and this thread makes none before the copy.
        unsafe { CStr::from_ptr(eggdb_last_error()) }
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn a_failure_becomes_the_status_and_the_last_error_of_its_thread() {
        let status = guarded(|| Err(Failure::new(EGGDB_ERR_LOCKED, "locked by another program")));
        assert_eq!(status, EGGDB_ERR_LOCKED);
        assert_eq!(last_error(), "locked by another program");
        assert_eq!(guarded(|| Ok(())), EGGDB_OK);
        assert_eq!(last_error(), "locked by another program");
    }

    #[test]
    fn a_panic_stays_inside_the_library_as_an_internal_error() {
        let status = guarded(|| panic!("a bug"));
        assert_eq!(status, EGGDB_ERR_INTERNAL);
        assert!(last_error().starts_with("EggDB hit an internal error"));
    }

    #[test]
    fn a_message_with_a_nul_byte_is_kept_readable() {
        guarded(|| Err(Failure::invalid_argument("before\0after")));
        assert_eq!(last_error(), "before\u{fffd}after");
    }

    #[test]
    fn every_status_has_its_own_name() {
        for (code, name) in STATUS_NAMES {
            // SAFETY: eggdb_status_name returns a static NUL-terminated string.
            let named = unsafe { CStr::from_ptr(eggdb_status_name(code)) };
            assert_eq!(named, name);
        }
        // SAFETY: as above.
        let unknown = unsafe { CStr::from_ptr(eggdb_status_name(9999)) };
        assert_eq!(unknown, c"EGGDB_UNKNOWN_STATUS");
    }
}
