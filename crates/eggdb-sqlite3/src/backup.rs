use crate::{SQLITE_BUSY, SQLITE_DONE, SQLITE_ERROR, SQLITE_MISUSE, SQLITE_OK, Sqlite3};
use eggdb::{Database, SQL_COLLECTION};
use std::ffi::{CStr, c_char, c_int};
use std::sync::Arc;

/// An online backup in progress (`sqlite3_backup`), from [`sqlite3_backup_init`].
#[derive(Debug)]
pub struct Sqlite3Backup {
    destination: *mut Sqlite3,
    source: Arc<Database>,
    target: Arc<Database>,
    page_count: c_int,
    done: bool,
    code: c_int,
}

fn names_main(name: *const c_char) -> bool {
    if name.is_null() {
        return true;
    }
    // SAFETY: the caller passes a NUL-terminated string, as sqlite3_backup_init requires.
    let name = unsafe { CStr::from_ptr(name) };
    name.to_bytes().eq_ignore_ascii_case(b"main")
}

fn copy_everything(source: &Database, target: &Database) -> eggdb::Result<()> {
    let read = source.begin_read()?;
    let mut write = target.begin_write()?;
    let old: Vec<Vec<u8>> = write
        .scan(..)
        .map(|entry| entry.map(|(key, _)| key))
        .collect::<eggdb::Result<_>>()?;
    for key in old {
        write.delete(&key)?;
    }
    for name in write.collection_names()? {
        if name != SQL_COLLECTION {
            write.drop_collection(&name)?;
        }
    }
    for entry in read.scan(..) {
        let (key, value) = entry?;
        write.put(&key, &value)?;
    }
    for name in read.collection_names()? {
        if name == SQL_COLLECTION {
            continue;
        }
        write.create_collection(&name)?;
        let entries: Vec<(Vec<u8>, Vec<u8>)> = read.collection(&name)?.scan(..).collect::<eggdb::Result<_>>()?;
        let mut collection = write.collection(&name)?;
        for (key, value) in entries {
            collection.put(&key, &value)?;
        }
    }
    write.commit()?;
    Ok(())
}

/// Starts copying the database `source` has open into the one `destination` has open, each
/// named `main` (`temp` and attached databases are refused), as SQLite's online backup does
/// (D1145). Returns NULL, with the error recorded on `destination`, when they are the same
/// connection or a name is not `main`.
///
/// # Safety
/// `destination` and `source` are live connections from `sqlite3_open_v2`, and the names NULL
/// or NUL-terminated strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_backup_init(
    destination: *mut Sqlite3,
    destination_name: *const c_char,
    source: *mut Sqlite3,
    source_name: *const c_char,
) -> *mut Sqlite3Backup {
    if destination.is_null() || source.is_null() {
        return std::ptr::null_mut();
    }
    if std::ptr::eq(destination, source) {
        // SAFETY: the caller passes a live connection.
        unsafe { &mut *destination }.record(SQLITE_ERROR, "source and destination must be distinct");
        return std::ptr::null_mut();
    }
    if !names_main(destination_name) || !names_main(source_name) {
        // SAFETY: the caller passes a live connection.
        unsafe { &mut *destination }.record(SQLITE_ERROR, "unknown database; only main can be backed up");
        return std::ptr::null_mut();
    }
    // SAFETY: the caller passes live connections, distinct as checked above.
    let source_database = unsafe { &*source }.database();
    // SAFETY: as above.
    let target_database = unsafe { &*destination }.database();
    let page_count = source_database
        .stats()
        .map(|stats| c_int::try_from(stats.page_count).unwrap_or(c_int::MAX))
        .unwrap_or(0);
    Box::into_raw(Box::new(Sqlite3Backup {
        destination,
        source: source_database,
        target: target_database,
        page_count,
        done: false,
        code: SQLITE_OK,
    }))
}

/// Copies the source into the destination. EggDB copies every entry in one step, whatever
/// `pages` asks for, so the first step returns `SQLITE_DONE`; a destination with a write in
/// progress returns `SQLITE_BUSY` and may be stepped again.
///
/// # Safety
/// `backup` is a live backup from [`sqlite3_backup_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_backup_step(backup: *mut Sqlite3Backup, pages: c_int) -> c_int {
    let _ = pages;
    if backup.is_null() {
        return SQLITE_MISUSE;
    }
    // SAFETY: the caller passes a live backup.
    let backup = unsafe { &mut *backup };
    if backup.done {
        return SQLITE_DONE;
    }
    match copy_everything(&backup.source, &backup.target) {
        Ok(()) => {
            backup.done = true;
            backup.code = SQLITE_OK;
            SQLITE_DONE
        }
        Err(error) if error.kind() == eggdb::ErrorKind::WriterActive || error.kind() == eggdb::ErrorKind::Locked => SQLITE_BUSY,
        Err(error) => {
            backup.code = SQLITE_ERROR;
            // SAFETY: the destination connection outlives its backups, as SQLite requires.
            unsafe { &mut *backup.destination }.record(SQLITE_ERROR, &error.to_string());
            SQLITE_ERROR
        }
    }
}

/// Ends the backup and frees it; returns the error of a step that failed, else `SQLITE_OK`.
///
/// # Safety
/// `backup` is NULL or a live backup from [`sqlite3_backup_init`], used no more.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_backup_finish(backup: *mut Sqlite3Backup) -> c_int {
    if backup.is_null() {
        return SQLITE_OK;
    }
    // SAFETY: the caller hands back a backup from sqlite3_backup_init, used no more.
    let backup = unsafe { Box::from_raw(backup) };
    backup.code
}

/// The pages left to copy: 0 once the backup is done, else the source's page count.
///
/// # Safety
/// `backup` is a live backup from [`sqlite3_backup_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_backup_remaining(backup: *mut Sqlite3Backup) -> c_int {
    if backup.is_null() {
        return 0;
    }
    // SAFETY: the caller passes a live backup.
    let backup = unsafe { &*backup };
    if backup.done { 0 } else { backup.page_count }
}

/// The source's page count when the backup began.
///
/// # Safety
/// `backup` is a live backup from [`sqlite3_backup_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_backup_pagecount(backup: *mut Sqlite3Backup) -> c_int {
    if backup.is_null() {
        return 0;
    }
    // SAFETY: the caller passes a live backup.
    unsafe { &*backup }.page_count
}
