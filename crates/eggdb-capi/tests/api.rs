//! The C API called as a C program calls it: open and close, transactions, keys and values,
//! scans in batches, collections, times to live and counters, SQL with typed results, backup and
//! compaction, and every misuse refused with a status instead of a crash.

use eggdb_capi::{
    EGGDB_BLOB, EGGDB_ERR_ALREADY_EXISTS, EGGDB_ERR_COLLECTION_NOT_FOUND, EGGDB_ERR_IN_USE,
    EGGDB_ERR_INVALID_ARGUMENT, EGGDB_ERR_NOT_AN_INTEGER, EGGDB_ERR_NOT_FOUND,
    EGGDB_ERR_READ_TRANSACTION, EGGDB_ERR_SQL, EGGDB_INTEGER, EGGDB_NULL, EGGDB_OK,
    EGGDB_OPEN_NO_CREATE, EGGDB_REAL, EGGDB_SCAN_END_INCLUSIVE, EGGDB_SCAN_PREFIX,
    EGGDB_SCAN_REVERSE, EGGDB_TEXT, EggdbBackupReport, EggdbCompactReport, EggdbDb, EggdbEntry,
    EggdbOptions, EggdbParameter, EggdbResult, EggdbScan, EggdbSql, EggdbTxn, EggdbValue, eggdb_backup,
    eggdb_begin_read, eggdb_begin_write, eggdb_clear_expiry, eggdb_close,
    eggdb_collection_names, eggdb_commit, eggdb_compact, eggdb_compact_file,
    eggdb_compare_and_set, eggdb_create_collection, eggdb_delete, eggdb_drop_collection,
    eggdb_free, eggdb_get, eggdb_increment, eggdb_last_error, eggdb_last_sqlstate, eggdb_open,
    eggdb_put, eggdb_put_with_expiry, eggdb_rollback, eggdb_scan_free, eggdb_scan_next,
    eggdb_scan_open, eggdb_set_expiry, eggdb_sql_close, eggdb_sql_execute,
    eggdb_sql_execute_with_parameters, eggdb_sql_open,
    eggdb_time_to_live,
};
use std::ffi::{CStr, CString, c_char, c_int};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT: AtomicU32 = AtomicU32::new(0);

struct TempPath {
    path: PathBuf,
}

impl TempPath {
    fn new(name: &str) -> Self {
        let number = NEXT.fetch_add(1, Ordering::Relaxed);
        let file = format!("eggdb-capi-{name}-{}-{number}.db", std::process::id());
        Self {
            path: std::env::temp_dir().join(file),
        }
    }

    fn c_path(&self) -> CString {
        c_text(&self.path.display().to_string())
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

type Entries = Vec<(Vec<u8>, Vec<u8>)>;
type Cell = (i32, i64, f64, Option<Vec<u8>>);

fn c_text(text: &str) -> CString {
    CString::new(text).unwrap_or_else(|_| panic!("{text:?} holds a NUL byte"))
}

fn last_error() -> String {
    // SAFETY: eggdb_last_error returns a NUL-terminated string that stays valid until the next
    // failing call on this thread, and none happens before it is copied.
    unsafe { CStr::from_ptr(eggdb_last_error()) }
        .to_string_lossy()
        .into_owned()
}

fn last_sqlstate() -> String {
    // SAFETY: as for eggdb_last_error.
    unsafe { CStr::from_ptr(eggdb_last_sqlstate()) }
        .to_string_lossy()
        .into_owned()
}

fn ok(status: c_int) {
    assert_eq!(status, EGGDB_OK, "{}", last_error());
}

fn collection_ptr(collection: Option<&CStr>) -> *const c_char {
    collection.map_or(ptr::null(), CStr::as_ptr)
}

fn open_at(path: &Path) -> *mut EggdbDb {
    let path = c_text(&path.display().to_string());
    let mut db = ptr::null_mut();
    // SAFETY: a NUL-terminated path, no options, and a place for the handle.
    ok(unsafe { eggdb_open(path.as_ptr(), ptr::null(), &mut db) });
    assert!(!db.is_null());
    db
}

fn close(db: *mut EggdbDb) {
    // SAFETY: a live handle from eggdb_open, given up here.
    ok(unsafe { eggdb_close(db) });
}

fn begin_read(db: *mut EggdbDb) -> *mut EggdbTxn {
    let mut txn = ptr::null_mut();
    // SAFETY: a live handle from eggdb_open and a place for the transaction.
    ok(unsafe { eggdb_begin_read(db, &mut txn) });
    txn
}

fn begin_write(db: *mut EggdbDb) -> *mut EggdbTxn {
    let mut txn = ptr::null_mut();
    // SAFETY: a live handle from eggdb_open and a place for the transaction.
    ok(unsafe { eggdb_begin_write(db, &mut txn) });
    txn
}

fn commit(txn: *mut EggdbTxn) -> u64 {
    let mut stamp = 0;
    // SAFETY: a live transaction, given up here, and a place for the stamp.
    ok(unsafe { eggdb_commit(txn, &mut stamp) });
    stamp
}

fn rollback(txn: *mut EggdbTxn) {
    // SAFETY: a live transaction, given up here.
    ok(unsafe { eggdb_rollback(txn) });
}

fn put_in(txn: *mut EggdbTxn, collection: Option<&CStr>, key: &[u8], value: &[u8]) -> c_int {
    // SAFETY: a live transaction, NULL or a collection name, and the key's and value's bytes.
    unsafe {
        eggdb_put(
            txn,
            collection_ptr(collection),
            key.as_ptr(),
            key.len(),
            value.as_ptr(),
            value.len(),
        )
    }
}

fn put(txn: *mut EggdbTxn, key: &[u8], value: &[u8]) {
    ok(put_in(txn, None, key, value));
}

fn get_in(txn: *mut EggdbTxn, collection: Option<&CStr>, key: &[u8]) -> (c_int, Option<Vec<u8>>) {
    let mut value = ptr::null_mut();
    let mut value_len = 0;
    // SAFETY: a live transaction, NULL or a collection name, the key's bytes and places for the
    // value.
    let status = unsafe {
        eggdb_get(
            txn,
            collection_ptr(collection),
            key.as_ptr(),
            key.len(),
            &mut value,
            &mut value_len,
        )
    };
    if value.is_null() {
        return (status, None);
    }
    // SAFETY: eggdb_get gave a buffer of `value_len` bytes, valid until freed.
    let copied = unsafe { std::slice::from_raw_parts(value, value_len) }.to_vec();
    // SAFETY: the buffer eggdb_get gave, with its length, freed once.
    unsafe { eggdb_free(value, value_len) };
    (status, Some(copied))
}

fn get(txn: *mut EggdbTxn, key: &[u8]) -> Option<Vec<u8>> {
    let (status, value) = get_in(txn, None, key);
    ok(status);
    value
}

fn lent(data: *const u8, len: usize) -> Vec<u8> {
    // SAFETY: the library lends `len` bytes at `data` until the handle's next call, which comes
    // after this copy.
    unsafe { std::slice::from_raw_parts(data, len) }.to_vec()
}

fn open_scan(
    txn: *mut EggdbTxn,
    collection: Option<&CStr>,
    start: Option<&[u8]>,
    end: Option<&[u8]>,
    flags: u32,
) -> (c_int, *mut EggdbScan) {
    let mut scan = ptr::null_mut();
    // SAFETY: a live transaction, NULL or a collection name, NULL or the bounds' bytes, and a
    // place for the scan.
    let status = unsafe {
        eggdb_scan_open(
            txn,
            collection_ptr(collection),
            start.map_or(ptr::null(), <[u8]>::as_ptr),
            start.map_or(0, <[u8]>::len),
            end.map_or(ptr::null(), <[u8]>::as_ptr),
            end.map_or(0, <[u8]>::len),
            flags,
            &mut scan,
        )
    };
    (status, scan)
}

fn next_batch(txn: *mut EggdbTxn, scan: *mut EggdbScan, max_entries: usize) -> Entries {
    let mut entries: *const EggdbEntry = ptr::null();
    let mut count = 0;
    // SAFETY: a live transaction and scan, and places for the batch.
    ok(unsafe { eggdb_scan_next(txn, scan, max_entries, &mut entries, &mut count) });
    if count == 0 {
        return Vec::new();
    }
    // SAFETY: the scan lends `count` entries at `entries` until its next call.
    let batch = unsafe { std::slice::from_raw_parts(entries, count) };
    batch
        .iter()
        .map(|entry| (lent(entry.key, entry.key_len), lent(entry.value, entry.value_len)))
        .collect()
}

fn scan_all(
    txn: *mut EggdbTxn,
    collection: Option<&CStr>,
    start: Option<&[u8]>,
    end: Option<&[u8]>,
    flags: u32,
    max_entries: usize,
) -> Entries {
    let (status, scan) = open_scan(txn, collection, start, end, flags);
    ok(status);
    let mut all = Vec::new();
    loop {
        let batch = next_batch(txn, scan, max_entries);
        if batch.is_empty() {
            break;
        }
        assert!(max_entries == 0 || batch.len() <= max_entries);
        all.extend(batch);
    }
    // SAFETY: the scan from eggdb_scan_open, freed once.
    unsafe { eggdb_scan_free(scan) };
    all
}

fn keys(entries: &Entries) -> Vec<String> {
    entries
        .iter()
        .map(|(key, _)| String::from_utf8_lossy(key).into_owned())
        .collect()
}

fn numbered(index: usize) -> Vec<u8> {
    format!("k{index:04}").into_bytes()
}

#[test]
fn a_write_transaction_commits_its_puts_and_deletes() {
    let file = TempPath::new("commit");
    let db = open_at(&file.path);
    let txn = begin_write(db);
    put(txn, b"a", b"1");
    put(txn, b"b", b"2");
    put(txn, b"empty", b"");
    assert_eq!(get(txn, b"a"), Some(b"1".to_vec()));
    assert!(commit(txn) > 0);

    let txn = begin_read(db);
    assert_eq!(get(txn, b"a"), Some(b"1".to_vec()));
    assert_eq!(get(txn, b"empty"), Some(Vec::new()));
    assert_eq!(get(txn, b"missing"), None);
    rollback(txn);

    let txn = begin_write(db);
    let mut deleted = -1;
    // SAFETY: a live write transaction, the key's bytes and a place for the outcome.
    ok(unsafe { eggdb_delete(txn, ptr::null(), b"b".as_ptr(), 1, &mut deleted) });
    assert_eq!(deleted, 1);
    // SAFETY: as above.
    ok(unsafe { eggdb_delete(txn, ptr::null(), b"b".as_ptr(), 1, &mut deleted) });
    assert_eq!(deleted, 0);
    rollback(txn);

    let txn = begin_read(db);
    assert_eq!(get(txn, b"b"), Some(b"2".to_vec()));
    rollback(txn);
    close(db);

    let reopened = open_at(&file.path);
    let txn = begin_read(reopened);
    assert_eq!(get(txn, b"b"), Some(b"2".to_vec()));
    commit(txn);
    close(reopened);
}

#[test]
fn a_read_transaction_refuses_changes() {
    let file = TempPath::new("read-only-txn");
    let db = open_at(&file.path);
    let txn = begin_read(db);
    assert_eq!(put_in(txn, None, b"k", b"v"), EGGDB_ERR_READ_TRANSACTION);
    assert!(last_error().contains("eggdb_begin_write"));
    rollback(txn);
    close(db);
}

#[test]
fn a_scan_reads_in_batches_forward_backward_by_range_and_by_prefix() {
    let file = TempPath::new("scan");
    let db = open_at(&file.path);
    let txn = begin_write(db);
    for index in 0..1000 {
        put(txn, &numbered(index), format!("v{index}").as_bytes());
    }
    commit(txn);

    let txn = begin_read(db);
    let forward = scan_all(txn, None, None, None, 0, 7);
    assert_eq!(forward.len(), 1000);
    assert_eq!(forward[0], (numbered(0), b"v0".to_vec()));
    assert_eq!(forward[999], (numbered(999), b"v999".to_vec()));
    assert!(forward.windows(2).all(|pair| pair[0].0 < pair[1].0));

    let backward = scan_all(txn, None, None, None, EGGDB_SCAN_REVERSE, 0);
    assert_eq!(backward.len(), 1000);
    assert_eq!(backward[0].0, numbered(999));
    assert_eq!(backward[999].0, numbered(0));

    let prefixed = scan_all(txn, None, Some(b"k01"), None, EGGDB_SCAN_PREFIX, 9);
    assert_eq!(prefixed.len(), 100);
    assert_eq!(prefixed[0].0, numbered(100));
    assert_eq!(prefixed[99].0, numbered(199));

    let start = numbered(100);
    let end = numbered(200);
    let half_open = scan_all(txn, None, Some(&start), Some(&end), 0, 33);
    assert_eq!(half_open.len(), 100);
    let closed = scan_all(txn, None, Some(&start), Some(&end), EGGDB_SCAN_END_INCLUSIVE, 33);
    assert_eq!(closed.len(), 101);
    assert_eq!(closed[100].0, end);
    let reversed = scan_all(
        txn,
        None,
        Some(&start),
        Some(&end),
        EGGDB_SCAN_REVERSE | EGGDB_SCAN_END_INCLUSIVE,
        10,
    );
    assert_eq!(reversed.first().map(|entry| entry.0.clone()), Some(end.clone()));
    assert_eq!(reversed.last().map(|entry| entry.0.clone()), Some(start.clone()));
    commit(txn);
    close(db);
}

#[test]
fn a_scan_continues_in_a_later_transaction_and_sees_its_writes() {
    let file = TempPath::new("scan-resume");
    let db = open_at(&file.path);
    let txn = begin_write(db);
    for index in 0..10 {
        put(txn, &numbered(index), b"v");
    }
    let (status, scan) = open_scan(txn, None, None, None, 0);
    ok(status);
    assert_eq!(keys(&next_batch(txn, scan, 4)), ["k0000", "k0001", "k0002", "k0003"]);
    put(txn, &numbered(2), b"behind");
    put(txn, b"k0005a", b"ahead");
    commit(txn);

    let txn = begin_read(db);
    let rest = next_batch(txn, scan, 0);
    assert_eq!(rest.len(), 7);
    assert_eq!(rest[0].0, numbered(4));
    assert_eq!(rest[2].0, b"k0005a".to_vec());
    assert!(next_batch(txn, scan, 0).is_empty());
    assert!(next_batch(txn, scan, 0).is_empty());
    // SAFETY: the scan from eggdb_scan_open, freed once.
    unsafe { eggdb_scan_free(scan) };
    rollback(txn);
    close(db);
}

#[test]
fn collections_keep_their_own_keys() {
    let file = TempPath::new("collections");
    let db = open_at(&file.path);
    let users = c_text("users");
    let txn = begin_write(db);
    let mut created = -1;
    // SAFETY: a live write transaction, a collection name and a place for the outcome.
    ok(unsafe { eggdb_create_collection(txn, users.as_ptr(), &mut created) });
    assert_eq!(created, 1);
    // SAFETY: as above.
    ok(unsafe { eggdb_create_collection(txn, users.as_ptr(), &mut created) });
    assert_eq!(created, 0);
    ok(put_in(txn, Some(&users), b"ada", b"Ada Lovelace"));
    put(txn, b"ada", b"default");
    commit(txn);

    let txn = begin_read(db);
    assert_eq!(get_in(txn, Some(&users), b"ada").1, Some(b"Ada Lovelace".to_vec()));
    assert_eq!(get(txn, b"ada"), Some(b"default".to_vec()));
    assert_eq!(keys(&scan_all(txn, Some(&users), None, None, 0, 0)), ["ada"]);
    assert_eq!(keys(&scan_all(txn, None, None, None, 0, 0)), ["ada"]);

    let mut names = ptr::null_mut();
    let mut names_len = 0;
    let mut count = 0;
    // SAFETY: a live transaction and places for the names.
    ok(unsafe { eggdb_collection_names(txn, &mut names, &mut names_len, &mut count) });
    assert_eq!(count, 1);
    assert_eq!(lent(names, names_len), b"users\0".to_vec());
    // SAFETY: the buffer eggdb_collection_names gave, with its length, freed once.
    unsafe { eggdb_free(names, names_len) };

    let missing = c_text("missing");
    assert_eq!(get_in(txn, Some(&missing), b"ada").0, EGGDB_ERR_COLLECTION_NOT_FOUND);
    let (status, scan) = open_scan(txn, Some(&missing), None, None, 0);
    assert_eq!(status, EGGDB_ERR_COLLECTION_NOT_FOUND);
    assert!(scan.is_null());
    rollback(txn);

    let txn = begin_write(db);
    let mut dropped = -1;
    // SAFETY: a live write transaction, a collection name and a place for the outcome.
    ok(unsafe { eggdb_drop_collection(txn, users.as_ptr(), &mut dropped) });
    assert_eq!(dropped, 1);
    commit(txn);
    let txn = begin_read(db);
    assert_eq!(get_in(txn, Some(&users), b"ada").0, EGGDB_ERR_COLLECTION_NOT_FOUND);
    assert_eq!(get(txn, b"ada"), Some(b"default".to_vec()));
    rollback(txn);
    close(db);
}

fn time_to_live(txn: *mut EggdbTxn, key: &[u8]) -> i64 {
    let mut ttl_ms = 0;
    // SAFETY: a live transaction, the key's bytes and a place for the time left.
    ok(unsafe { eggdb_time_to_live(txn, ptr::null(), key.as_ptr(), key.len(), &mut ttl_ms) });
    ttl_ms
}

fn increment(txn: *mut EggdbTxn, key: &[u8], by: i64) -> (c_int, i64) {
    let mut count = 0;
    // SAFETY: a live write transaction, the key's bytes and a place for the count.
    let status =
        unsafe { eggdb_increment(txn, ptr::null(), key.as_ptr(), key.len(), by, &mut count) };
    (status, count)
}

fn compare_and_set(
    txn: *mut EggdbTxn,
    key: &[u8],
    expected: Option<&[u8]>,
    new_value: Option<&[u8]>,
) -> c_int {
    let mut swapped = -1;
    // SAFETY: a live write transaction, the key's bytes, NULL or the values' bytes, and a place
    // for the outcome.
    ok(unsafe {
        eggdb_compare_and_set(
            txn,
            ptr::null(),
            key.as_ptr(),
            key.len(),
            expected.map_or(ptr::null(), <[u8]>::as_ptr),
            expected.map_or(0, <[u8]>::len),
            new_value.map_or(ptr::null(), <[u8]>::as_ptr),
            new_value.map_or(0, <[u8]>::len),
            &mut swapped,
        )
    });
    swapped
}

#[test]
fn keys_expire_and_counters_count() {
    let file = TempPath::new("expiry");
    let db = open_at(&file.path);
    let txn = begin_write(db);
    // SAFETY: a live write transaction and the key's and value's bytes.
    ok(unsafe { eggdb_put_with_expiry(txn, ptr::null(), b"session".as_ptr(), 7, b"alice".as_ptr(), 5, 60_000) });
    put(txn, b"plain", b"v");
    commit(txn);

    let txn = begin_write(db);
    let left = time_to_live(txn, b"session");
    assert!((1..=60_000).contains(&left), "{left}");
    assert_eq!(time_to_live(txn, b"plain"), -1);
    assert_eq!(time_to_live(txn, b"missing"), -1);
    let mut cleared = -1;
    // SAFETY: a live write transaction, the key's bytes and a place for the outcome.
    ok(unsafe { eggdb_clear_expiry(txn, ptr::null(), b"session".as_ptr(), 7, &mut cleared) });
    assert_eq!(cleared, 1);
    assert_eq!(time_to_live(txn, b"session"), -1);
    let mut found = -1;
    // SAFETY: as above.
    ok(unsafe { eggdb_set_expiry(txn, ptr::null(), b"plain".as_ptr(), 5, 0, &mut found) });
    assert_eq!(found, 1);
    assert_eq!(get(txn, b"plain"), None);

    assert_eq!(increment(txn, b"visits", 5), (EGGDB_OK, 5));
    assert_eq!(increment(txn, b"visits", -2), (EGGDB_OK, 3));
    assert_eq!(get(txn, b"visits"), Some(b"3".to_vec()));
    assert_eq!(increment(txn, b"session", 1).0, EGGDB_ERR_NOT_AN_INTEGER);

    assert_eq!(compare_and_set(txn, b"lock", None, Some(b"me")), 1);
    assert_eq!(compare_and_set(txn, b"lock", None, Some(b"you")), 0);
    assert_eq!(compare_and_set(txn, b"lock", Some(b"me"), None), 1);
    assert_eq!(get(txn, b"lock"), None);
    assert_eq!(compare_and_set(txn, b"blank", None, Some(b"")), 1);
    assert_eq!(get(txn, b"blank"), Some(Vec::new()));
    commit(txn);

    let mut removed = u64::MAX;
    // SAFETY: a live database handle and a place for the count.
    ok(unsafe { eggdb_capi::eggdb_remove_expired(db, &mut removed) });
    assert_eq!(removed, 0);
    close(db);
}

fn open_sql(db: *mut EggdbDb, dialect: &str) -> *mut EggdbSql {
    let dialect = c_text(dialect);
    let mut sql = ptr::null_mut();
    // SAFETY: a live database handle, a dialect name and a place for the session.
    ok(unsafe { eggdb_sql_open(db, dialect.as_ptr(), &mut sql) });
    sql
}

fn result_cells(result: &EggdbResult) -> Vec<Vec<Cell>> {
    if result.row_count == 0 {
        return Vec::new();
    }
    // SAFETY: the session lends `row_count × column_count` values until its next call.
    let values: &[EggdbValue] = unsafe {
        std::slice::from_raw_parts(result.values, result.row_count * result.column_count)
    };
    values
        .chunks(result.column_count)
        .map(|row| {
            row.iter()
                .map(|value| {
                    let data = (!value.data.is_null()).then(|| lent(value.data, value.len));
                    (value.kind, value.integer, value.real, data)
                })
                .collect()
        })
        .collect()
}

fn column_names(result: &EggdbResult) -> Vec<String> {
    if result.column_count == 0 {
        return Vec::new();
    }
    // SAFETY: the session lends `column_count` names until its next call.
    let names = unsafe { std::slice::from_raw_parts(result.column_names, result.column_count) };
    names
        .iter()
        .map(|&name| {
            // SAFETY: each name is a NUL-terminated string the session lends.
            unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned()
        })
        .collect()
}

fn tag(result: &EggdbResult) -> String {
    // SAFETY: the tag is a NUL-terminated string the session lends.
    unsafe { CStr::from_ptr(result.tag) }
        .to_string_lossy()
        .into_owned()
}

fn execute(sql: *mut EggdbSql, text: &str) -> (c_int, Vec<EggdbResult>) {
    let text = c_text(text);
    let mut results: *const EggdbResult = ptr::null();
    let mut count = 0;
    // SAFETY: a live session, the statement text and places for the results.
    let status = unsafe { eggdb_sql_execute(sql, text.as_ptr(), &mut results, &mut count) };
    if count == 0 {
        return (status, Vec::new());
    }
    // SAFETY: the session lends `count` results until its next call.
    let lent_results = unsafe { std::slice::from_raw_parts(results, count) };
    (status, lent_results.to_vec())
}

fn execute_with_parameters(
    sql: *mut EggdbSql,
    text: &str,
    values: &[Option<&str>],
) -> (c_int, Vec<EggdbResult>) {
    let text = c_text(text);
    let parameters: Vec<EggdbParameter> = values
        .iter()
        .map(|value| EggdbParameter {
            text: value.map_or(ptr::null(), str::as_ptr),
            len: value.map_or(0, str::len),
        })
        .collect();
    let mut results: *const EggdbResult = ptr::null();
    let mut count = 0;
    // SAFETY: a live session, the statement text, the parameters with their bytes and places
    // for the results.
    let status = unsafe {
        eggdb_sql_execute_with_parameters(
            sql,
            text.as_ptr(),
            parameters.as_ptr(),
            parameters.len(),
            &mut results,
            &mut count,
        )
    };
    if count == 0 {
        return (status, Vec::new());
    }
    // SAFETY: the session lends `count` results until its next call.
    let lent_results = unsafe { std::slice::from_raw_parts(results, count) };
    (status, lent_results.to_vec())
}

#[test]
fn sql_runs_with_typed_values_in_every_dialect() {
    let file = TempPath::new("sql");
    let db = open_at(&file.path);
    let postgres = open_sql(db, "postgresql");
    let (status, results) = execute(
        postgres,
        "CREATE TABLE items (id integer PRIMARY KEY, name text, price double precision, data bytea); \
         INSERT INTO items VALUES (1, 'tea', 2.5, '\\x0001'::bytea), (2, 'cake', NULL, NULL)",
    );
    ok(status);
    assert_eq!(results.len(), 2);
    assert_eq!(tag(&results[1]), "INSERT 0 2");

    let sqlite = open_sql(db, "sqlite");
    let (status, results) = execute(sqlite, "SELECT id, name, price, data FROM items ORDER BY id");
    ok(status);
    assert_eq!(results.len(), 1);
    let result = &results[0];
    assert_eq!(column_names(result), ["id", "name", "price", "data"]);
    assert_eq!(result.row_count, 2);
    let rows = result_cells(result);
    assert_eq!(rows[0][0].0, EGGDB_INTEGER);
    assert_eq!(rows[0][0].1, 1);
    assert_eq!(rows[0][0].3, Some(b"1".to_vec()));
    assert_eq!(rows[0][1].0, EGGDB_TEXT);
    assert_eq!(rows[0][1].3, Some(b"tea".to_vec()));
    assert_eq!(rows[0][2].0, EGGDB_REAL);
    assert_eq!(rows[0][2].2, 2.5);
    assert_eq!(rows[0][3].0, EGGDB_BLOB);
    assert_eq!(rows[0][3].3, Some(vec![0, 1]));
    assert_eq!(rows[1][2].0, EGGDB_NULL);
    assert_eq!(rows[1][2].3, None);

    let (status, results) = execute(sqlite, "SELECT * FROM no_such_table");
    assert_eq!(status, EGGDB_ERR_SQL);
    assert!(results.is_empty());
    assert!(last_error().contains("no_such_table"), "{}", last_error());
    assert_eq!(last_sqlstate().len(), 5);

    let (status, results) = execute(postgres, "SELECT count(*) AS n FROM items");
    ok(status);
    assert_eq!(tag(&results[0]), "SELECT 1");
    assert_eq!(column_names(&results[0]), ["n"]);
    assert_eq!(result_cells(&results[0])[0][0].1, 2);
    // SAFETY: live sessions from eggdb_sql_open, each given up once.
    ok(unsafe { eggdb_sql_close(postgres) });
    // SAFETY: as above.
    ok(unsafe { eggdb_sql_close(sqlite) });
    close(db);
}

#[test]
fn parameters_carry_values_without_quoting() {
    let file = TempPath::new("sql-parameters");
    let db = open_at(&file.path);
    let postgres = open_sql(db, "postgresql");
    ok(execute(postgres, "CREATE TABLE people (id integer PRIMARY KEY, name text)").0);
    let (status, results) = execute_with_parameters(
        postgres,
        "INSERT INTO people VALUES ($1, $2)",
        &[Some("1"), Some("O'Brien; DROP TABLE people")],
    );
    ok(status);
    assert_eq!(tag(&results[0]), "INSERT 0 1");
    ok(execute_with_parameters(postgres, "INSERT INTO people VALUES ($1, $2)", &[Some("2"), None]).0);

    let sqlite = open_sql(db, "sqlite");
    let (status, results) = execute_with_parameters(
        sqlite,
        "SELECT id FROM people WHERE name = ?",
        &[Some("O'Brien; DROP TABLE people")],
    );
    ok(status);
    assert_eq!(result_cells(&results[0])[0][0].1, 1);
    let (status, results) =
        execute_with_parameters(postgres, "SELECT name FROM people WHERE id = $1", &[Some("2")]);
    ok(status);
    assert_eq!(results[0].row_count, 1);
    assert_eq!(result_cells(&results[0])[0][0].0, EGGDB_NULL);
    // SAFETY: live sessions from eggdb_sql_open, each given up once.
    ok(unsafe { eggdb_sql_close(postgres) });
    // SAFETY: as above.
    ok(unsafe { eggdb_sql_close(sqlite) });
    close(db);
}

#[test]
fn a_transaction_block_holds_until_commit_or_rollback() {
    let file = TempPath::new("sql-block");
    let db = open_at(&file.path);
    let sql = open_sql(db, "postgresql");
    ok(execute(sql, "CREATE TABLE t (n integer)").0);
    ok(execute(sql, "BEGIN; INSERT INTO t VALUES (1)").0);
    let (status, results) = execute(sql, "SELECT count(*) FROM t");
    ok(status);
    assert_eq!(result_cells(&results[0])[0][0].1, 1);
    ok(execute(sql, "ROLLBACK").0);
    let (status, results) = execute(sql, "SELECT count(*) FROM t");
    ok(status);
    assert_eq!(result_cells(&results[0])[0][0].1, 0);
    ok(execute(sql, "BEGIN; INSERT INTO t VALUES (2)").0);
    // SAFETY: a live session, given up here; its open block is rolled back.
    ok(unsafe { eggdb_sql_close(sql) });
    let sql = open_sql(db, "postgresql");
    let (status, results) = execute(sql, "SELECT count(*) FROM t");
    ok(status);
    assert_eq!(result_cells(&results[0])[0][0].1, 0);
    // SAFETY: a live session, given up here.
    ok(unsafe { eggdb_sql_close(sql) });
    close(db);
}

#[test]
fn closing_waits_until_transactions_and_sessions_end() {
    let file = TempPath::new("in-use");
    let db = open_at(&file.path);
    let txn = begin_read(db);
    let sql = open_sql(db, "sqlite");
    // SAFETY: a live handle; the refusal leaves it open.
    assert_eq!(unsafe { eggdb_close(db) }, EGGDB_ERR_IN_USE);
    assert!(last_error().contains("still use it"));
    rollback(txn);
    // SAFETY: as above.
    assert_eq!(unsafe { eggdb_close(db) }, EGGDB_ERR_IN_USE);
    // SAFETY: a live session, given up here.
    ok(unsafe { eggdb_sql_close(sql) });
    close(db);
}

#[test]
fn a_backup_copies_one_commit_and_a_compaction_rewrites_the_file() {
    let file = TempPath::new("backup");
    let copy = TempPath::new("backup-copy");
    let db = open_at(&file.path);
    let txn = begin_write(db);
    for index in 0..2000 {
        put(txn, &numbered(index), &[7u8; 100]);
    }
    commit(txn);
    let txn = begin_write(db);
    for index in 10..2000 {
        // SAFETY: a live write transaction and the key's bytes; the outcome is not wanted.
        ok(unsafe { eggdb_delete(txn, ptr::null(), numbered(index).as_ptr(), 5, ptr::null_mut()) });
    }
    commit(txn);

    let copy_path = copy.c_path();
    let mut backup = EggdbBackupReport::default();
    // SAFETY: a live database handle, a path and a place for the report.
    ok(unsafe { eggdb_backup(db, copy_path.as_ptr(), &mut backup) });
    assert_eq!(backup.entries, 10);
    // SAFETY: as above.
    let again = unsafe { eggdb_backup(db, copy_path.as_ptr(), ptr::null_mut()) };
    assert_eq!(again, EGGDB_ERR_ALREADY_EXISTS);

    let mut compacted = EggdbCompactReport::default();
    // SAFETY: a live database handle used by nothing else, given up here.
    ok(unsafe { eggdb_compact(db, &mut compacted) });
    assert!(compacted.pages_after < compacted.pages_before);
    assert_eq!(compacted.entries, 10);

    let reopened = open_at(&file.path);
    let txn = begin_read(reopened);
    assert_eq!(get(txn, &numbered(3)), Some(vec![7u8; 100]));
    rollback(txn);
    close(reopened);

    let mut copy_report = EggdbCompactReport::default();
    // SAFETY: a path and a place for the report.
    ok(unsafe { eggdb_compact_file(copy_path.as_ptr(), &mut copy_report) });
    assert_eq!(copy_report.entries, 10);
}

#[test]
fn a_failed_open_leaves_no_handle_and_says_why() {
    let file = TempPath::new("missing");
    let path = file.c_path();
    let options = EggdbOptions {
        flags: EGGDB_OPEN_NO_CREATE,
        ..EggdbOptions::default()
    };
    let mut db = ptr::dangling_mut();
    // SAFETY: a path, options and a place for the handle.
    let status = unsafe { eggdb_open(path.as_ptr(), &options, &mut db) };
    assert_eq!(status, EGGDB_ERR_NOT_FOUND);
    assert!(db.is_null());
    assert!(last_error().contains(&file.path.display().to_string()));
}

#[test]
fn misuse_is_refused_with_a_status_not_a_crash() {
    let file = TempPath::new("misuse");
    let db = open_at(&file.path);
    let txn = begin_write(db);
    let mut db_out = ptr::null_mut();
    // SAFETY: NULL is refused before anything is read.
    assert_eq!(unsafe { eggdb_open(ptr::null(), ptr::null(), &mut db_out) }, EGGDB_ERR_INVALID_ARGUMENT);
    let path = file.c_path();
    // SAFETY: a NULL place for the handle is refused before the file is opened.
    assert_eq!(unsafe { eggdb_open(path.as_ptr(), ptr::null(), ptr::null_mut()) }, EGGDB_ERR_INVALID_ARGUMENT);
    // SAFETY: a NULL key with a length is refused before it is read.
    let null_key = unsafe { eggdb_put(txn, ptr::null(), ptr::null(), 3, b"v".as_ptr(), 1) };
    assert_eq!(null_key, EGGDB_ERR_INVALID_ARGUMENT);
    let not_utf8: &[u8] = b"\xff\xfe\0";
    // SAFETY: a NUL-terminated collection name that is not UTF-8.
    let bad_name = unsafe { eggdb_put(txn, not_utf8.as_ptr().cast(), b"k".as_ptr(), 1, b"v".as_ptr(), 1) };
    assert_eq!(bad_name, EGGDB_ERR_INVALID_ARGUMENT);
    assert!(last_error().contains("UTF-8"));
    let (status, scan) = open_scan(txn, None, None, None, 0x100);
    assert_eq!(status, EGGDB_ERR_INVALID_ARGUMENT);
    assert!(scan.is_null());
    let unknown = c_text("cobol");
    let mut sql = ptr::null_mut();
    // SAFETY: a live database handle, a dialect name and a place for the session.
    assert_eq!(unsafe { eggdb_sql_open(db, unknown.as_ptr(), &mut sql) }, EGGDB_ERR_INVALID_ARGUMENT);
    assert!(sql.is_null());
    // SAFETY: NULL is refused.
    assert_eq!(unsafe { eggdb_commit(ptr::null_mut(), ptr::null_mut()) }, EGGDB_ERR_INVALID_ARGUMENT);
    // SAFETY: NULL does nothing.
    assert_eq!(unsafe { eggdb_rollback(ptr::null_mut()) }, EGGDB_OK);
    // SAFETY: NULL does nothing.
    unsafe { eggdb_scan_free(ptr::null_mut()) };
    // SAFETY: NULL does nothing.
    assert_eq!(unsafe { eggdb_sql_close(ptr::null_mut()) }, EGGDB_OK);
    // SAFETY: NULL does nothing.
    assert_eq!(unsafe { eggdb_close(ptr::null_mut()) }, EGGDB_OK);
    rollback(txn);
    close(db);
}

#[test]
fn threads_share_one_database_handle() {
    let file = TempPath::new("threads");
    let db = open_at(&file.path);
    let txn = begin_write(db);
    put(txn, b"shared", b"value");
    commit(txn);
    let address = db as usize;
    let readers: Vec<_> = (0..4)
        .map(|_| {
            std::thread::spawn(move || {
                let db = address as *mut EggdbDb;
                for _ in 0..100 {
                    let txn = begin_read(db);
                    assert_eq!(get(txn, b"shared"), Some(b"value".to_vec()));
                    rollback(txn);
                }
            })
        })
        .collect();
    for reader in readers {
        assert!(reader.join().is_ok());
    }
    close(db);
}
