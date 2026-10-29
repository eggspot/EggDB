//! EggDB's C API: the embedded database for any language that can call C, declared in
//! `include/eggdb.h` and built as a shared library (`libeggdb_capi.so`, `libeggdb_capi.dylib`,
//! `eggdb_capi.dll`) and a static one (`libeggdb_capi.a`, `eggdb_capi.lib`).
//!
//! The guide is `docs/c-api.md` in the repository, and `examples/python/eggdb_ctypes.py` uses
//! it from Python through `ctypes`. In short:
//! - every call returns a status, `EGGDB_OK` (0) or an `EGGDB_ERR_*` code, and on failure
//!   [`eggdb_last_error`] gives the message for the calling thread;
//! - handles are opaque: [`EggdbDb`] from [`eggdb_open`], [`EggdbTxn`] from
//!   [`eggdb_begin_read`] and [`eggdb_begin_write`], [`EggdbScan`] from [`eggdb_scan_open`] and
//!   [`EggdbSql`] from [`eggdb_sql_open`];
//! - a value [`eggdb_get`] returns and the names [`eggdb_collection_names`] returns are the
//!   caller's, freed with [`eggdb_free`]; a scan's entries and a SQL statement's results are
//!   batches the handle owns, read in place until the next call on that handle;
//! - a panic never crosses into the caller: it becomes `EGGDB_ERR_INTERNAL`.

mod args;
mod database;
mod scan;
mod sql;
mod status;
mod transaction;

pub use database::{
    EGGDB_DURABILITY_FULL, EGGDB_DURABILITY_NORMAL, EGGDB_DURABILITY_OFF, EGGDB_OPEN_NO_CREATE,
    EGGDB_OPEN_READ_ONLY, EggdbBackupReport, EggdbCompactReport, EggdbDb, EggdbOptions,
    eggdb_backup, eggdb_close, eggdb_compact, eggdb_compact_file, eggdb_open,
    eggdb_remove_expired,
};
pub use scan::{
    EGGDB_SCAN_END_INCLUSIVE, EGGDB_SCAN_PREFIX, EGGDB_SCAN_REVERSE, EggdbEntry, EggdbScan,
    eggdb_scan_free, eggdb_scan_next, eggdb_scan_open,
};
pub use sql::{
    EGGDB_BLOB, EGGDB_INTEGER, EGGDB_NULL, EGGDB_REAL, EGGDB_TEXT, EggdbResult, EggdbSql,
    EggdbValue, eggdb_sql_close, eggdb_sql_execute, eggdb_sql_open,
};
pub use status::{
    EGGDB_ERR_ALREADY_EXISTS, EGGDB_ERR_CACHE_FULL, EGGDB_ERR_COLLECTION_NOT_FOUND,
    EGGDB_ERR_CORRUPT, EGGDB_ERR_DATABASE_FULL, EGGDB_ERR_IN_USE, EGGDB_ERR_INCOMPATIBLE,
    EGGDB_ERR_INTERNAL, EGGDB_ERR_INVALID_ARGUMENT, EGGDB_ERR_INVALID_COLLECTION_NAME,
    EGGDB_ERR_INVALID_OPTION, EGGDB_ERR_IO, EGGDB_ERR_LOCKED, EGGDB_ERR_NEEDS_REOPEN,
    EGGDB_ERR_NOT_A_DATABASE, EGGDB_ERR_NOT_AN_INTEGER, EGGDB_ERR_NOT_FOUND, EGGDB_ERR_OTHER,
    EGGDB_ERR_OUT_OF_MEMORY, EGGDB_ERR_PERMISSION_DENIED, EGGDB_ERR_READ_ONLY,
    EGGDB_ERR_READ_TRANSACTION, EGGDB_ERR_RESERVED_KEY, EGGDB_ERR_SNAPSHOT_TOO_OLD,
    EGGDB_ERR_SQL, EGGDB_ERR_TOO_LARGE, EGGDB_ERR_TRANSACTION_FAILED, EGGDB_ERR_WRITER_ACTIVE,
    EGGDB_OK, eggdb_last_error, eggdb_last_sqlstate, eggdb_status_name,
};
pub use transaction::{
    EggdbTxn, eggdb_begin_read, eggdb_begin_write, eggdb_clear_expiry, eggdb_collection_names,
    eggdb_commit, eggdb_compare_and_set, eggdb_create_collection, eggdb_delete,
    eggdb_drop_collection, eggdb_get, eggdb_increment, eggdb_put, eggdb_put_with_expiry,
    eggdb_rollback, eggdb_set_expiry, eggdb_time_to_live,
};

use std::ffi::{CStr, c_char};

const VERSION: &CStr = match CStr::from_bytes_with_nul(
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes(),
) {
    Ok(version) => version,
    Err(_) => c"unknown",
};

/// The version of this EggDB library, such as `"0.1.0"`. The text is static.
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_version() -> *const c_char {
    VERSION.as_ptr()
}

/// Frees a buffer this library gave the caller ([`eggdb_get`]'s value,
/// [`eggdb_collection_names`]' names), with the length it gave. NULL does nothing.
///
/// # Safety
/// `data` is NULL or a buffer this library returned with exactly `len` bytes, not freed yet;
/// it must not be used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_free(data: *mut u8, len: usize) {
    if data.is_null() {
        return;
    }
    let buffer = std::ptr::slice_from_raw_parts_mut(data, len);
    // SAFETY: the caller passes a buffer this library made from a `Box<[u8]>` of `len` bytes
    // with `Box::into_raw` (`args::library_bytes`) and gives it up with this call.
    drop(unsafe { Box::from_raw(buffer) });
}

#[cfg(test)]
mod tests {
    use super::{eggdb_free, eggdb_version};
    use std::ffi::CStr;

    #[test]
    fn the_version_is_the_package_version() {
        // SAFETY: eggdb_version returns a static NUL-terminated string.
        let version = unsafe { CStr::from_ptr(eggdb_version()) };
        assert_eq!(version.to_str(), Ok(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn freeing_null_does_nothing() {
        // SAFETY: NULL is always accepted.
        unsafe { eggdb_free(std::ptr::null_mut(), 0) };
    }
}
