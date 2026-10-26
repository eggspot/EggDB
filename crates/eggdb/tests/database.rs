//! Opening, creating, locking, closing, checking and measuring a database through the embedded API.

mod common;

use common::TempPath;
use eggdb::{CommitStamp, Database, Durability, ErrorKind, Options};
use std::error::Error as _;
use std::time::Duration;

#[test]
fn open_creates_a_missing_file_and_a_reopen_keeps_every_commit() {
    let file = TempPath::new("reopen");
    assert!(!file.path().exists());
    let db = Database::open(file.path()).unwrap();
    assert!(file.path().exists());
    assert_eq!(db.path(), file.path());
    for index in 0..300u32 {
        db.put(index.to_be_bytes(), format!("value {index}"))
            .unwrap();
    }
    assert!(db.delete(7u32.to_be_bytes()).unwrap());
    drop(db);

    let db = Database::open(file.path()).unwrap();
    assert_eq!(db.get(7u32.to_be_bytes()).unwrap(), None);
    assert_eq!(
        db.get(299u32.to_be_bytes()).unwrap(),
        Some(b"value 299".to_vec())
    );
    assert_eq!(db.check().unwrap().entries, 299);
}

#[test]
fn close_writes_a_checkpoint_so_the_next_open_replays_nothing() {
    let file = TempPath::new("close");
    let db = Database::open(file.path()).unwrap();
    db.put("k", "v").unwrap();
    assert!(db.stats().unwrap().log_bytes_since_checkpoint > 0);
    db.close().unwrap();

    let db = Database::open(file.path()).unwrap();
    assert_eq!(db.stats().unwrap().log_bytes_since_checkpoint, 0);
    assert_eq!(db.get("k").unwrap(), Some(b"v".to_vec()));
    db.close().unwrap();
}

#[test]
fn closing_a_database_that_was_never_changed_works() {
    let file = TempPath::new("close-empty");
    Database::open(file.path()).unwrap().close().unwrap();
    Database::open(file.path()).unwrap().close().unwrap();
}

#[test]
fn a_missing_file_is_not_created_when_the_options_say_so() {
    let file = TempPath::new("missing");
    let error =
        Database::open_with(file.path(), Options::new().create_if_missing(false)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound);
    let message = error.to_string();
    assert!(
        message.contains(&file.path().display().to_string()),
        "{message}"
    );
    assert!(message.contains("create_if_missing"), "{message}");
    assert!(!file.path().exists());

    let error = Database::open_with(file.path(), Options::new().read_only(true)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert!(error.to_string().contains("read-only"), "{error}");
    assert!(!file.path().exists());

    let in_missing_directory = file.path().with_extension("missing").join("db");
    let error = Database::open(&in_missing_directory).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert!(error.to_string().contains("directory"), "{error}");
    assert!(error.source().is_some());
}

#[test]
fn a_file_that_is_not_a_database_is_refused_and_left_alone() {
    let file = TempPath::new("not-a-database");
    let text = "just some text, long enough to be mistaken for nothing\n".repeat(200);
    std::fs::write(file.path(), &text).unwrap();
    let error = Database::open(file.path()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotADatabase, "{error}");
    assert!(error.source().is_some());
    assert_eq!(std::fs::read_to_string(file.path()).unwrap(), text);

    let empty = TempPath::new("empty-file");
    std::fs::write(empty.path(), b"").unwrap();
    assert_eq!(
        Database::open(empty.path()).unwrap_err().kind(),
        ErrorKind::NotADatabase
    );
}

#[test]
fn a_second_open_of_the_same_file_is_refused_while_the_first_is_open() {
    let file = TempPath::new("locked");
    let db = Database::open(file.path()).unwrap();
    let error = Database::open(file.path()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Locked, "{error}");
    let error = Database::open_with(file.path(), Options::new().read_only(true)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Locked, "{error}");
    drop(db);
    Database::open(file.path()).unwrap();
}

#[test]
fn read_only_opens_share_the_file_and_refuse_every_change() {
    let file = TempPath::new("read-only");
    Database::open(file.path()).unwrap().put("k", "v").unwrap();
    let options = Options::new().read_only(true);
    let first = Database::open_with(file.path(), options.clone()).unwrap();
    let second = Database::open_with(file.path(), options).unwrap();
    assert!(first.is_read_only());
    assert_eq!(first.get("k").unwrap(), Some(b"v".to_vec()));
    assert_eq!(second.get("k").unwrap(), Some(b"v".to_vec()));
    for error in [
        first.put("k", "w").unwrap_err(),
        first.begin_write().unwrap_err(),
        first.try_begin_write().unwrap_err(),
        first.checkpoint().unwrap_err(),
        first.sync().unwrap_err(),
    ] {
        assert_eq!(error.kind(), ErrorKind::ReadOnly, "{error}");
    }
    assert_eq!(
        Database::open(file.path()).unwrap_err().kind(),
        ErrorKind::Locked
    );
    first.close().unwrap();
    drop(second);
    assert_eq!(
        Database::open(file.path()).unwrap().get("k").unwrap(),
        Some(b"v".to_vec())
    );
}

#[test]
fn options_outside_what_eggdb_accepts_fail_the_open_with_a_readable_message() {
    let file = TempPath::new("invalid-options");
    for page_size in [0, 1000, 2048, 4097, 1 << 17] {
        let error = Database::open_with(file.path(), Options::new().page_size_bytes(page_size))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidOption, "{error}");
        assert!(error.to_string().contains("page size"), "{error}");
    }
    let error = Database::open_with(file.path(), Options::new().cache_bytes(100)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidOption, "{error}");
    assert!(!file.path().exists());
}

#[test]
fn the_page_size_is_chosen_at_creation_and_sets_the_size_limits() {
    let file = TempPath::new("page-size");
    let db = Database::open_with(file.path(), Options::new().page_size_bytes(1 << 16)).unwrap();
    assert_eq!(db.page_size(), 65_536);
    assert_eq!(db.max_key_bytes(), 8192);
    assert_eq!(db.max_value_bytes(), 1 << 30);
    db.put(vec![1; 8192], vec![2; 12_270 - 8192]).unwrap();
    db.put(vec![3; 8192], vec![4; 12_270 - 8192 + 1]).unwrap();
    let report = db.check().unwrap();
    assert_eq!((report.overflow_values, report.overflow_pages), (1, 1));
    drop(db);

    let db = Database::open_with(file.path(), Options::new().page_size_bytes(4096)).unwrap();
    assert_eq!(db.page_size(), 65_536);
    assert_eq!(
        db.get(vec![1; 8192]).unwrap().map(|value| value.len()),
        Some(12_270 - 8192)
    );
    assert_eq!(
        db.get(vec![3; 8192]).unwrap(),
        Some(vec![4; 12_270 - 8192 + 1])
    );
}

#[test]
fn keys_and_values_over_the_limits_are_refused_and_change_nothing() {
    let file = TempPath::new("limits");
    let db = Database::open_with(file.path(), Options::new().cache_bytes(64 * 4096)).unwrap();
    assert_eq!((db.max_key_bytes(), db.max_value_bytes()), (512, 1 << 30));
    let over_the_limit = vec![0; db.max_value_bytes() + 1];
    let error = db.put("first", &over_the_limit).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TooLarge);
    assert!(error.to_string().contains("1073741825"), "{error}");
    let mut tx = db.begin_write().unwrap();
    tx.put("kept", "1").unwrap();
    let error = tx.put(vec![0; 513], "").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TooLarge);
    assert!(error.to_string().contains("513"), "{error}");
    let error = tx.put("key", &over_the_limit).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TooLarge);
    drop(over_the_limit);
    let error = tx.put("key", vec![0; 64 * 4056]).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TooLarge);
    assert!(error.to_string().contains("larger cache"), "{error}");
    tx.put("key", vec![0; 748]).unwrap();
    tx.put(vec![9; 512], vec![0; 238]).unwrap();
    tx.commit().unwrap();
    assert_eq!(db.get("kept").unwrap(), Some(b"1".to_vec()));
    let report = db.check().unwrap();
    assert_eq!((report.entries, report.overflow_values), (3, 1));
}

#[test]
fn the_empty_key_and_the_empty_value_are_stored_like_any_other() {
    let file = TempPath::new("empty-key");
    let db = Database::open(file.path()).unwrap();
    db.put("", "empty key").unwrap();
    db.put("empty value", "").unwrap();
    assert_eq!(db.get("").unwrap(), Some(b"empty key".to_vec()));
    assert_eq!(db.get("empty value").unwrap(), Some(Vec::new()));
    assert_eq!(db.get("absent").unwrap(), None);
}

#[test]
fn check_counts_what_the_database_holds() {
    let file = TempPath::new("check");
    let db = Database::open(file.path()).unwrap();
    let empty = db.check().unwrap();
    assert_eq!(
        (
            empty.stamp,
            empty.entries,
            empty.tree_height,
            empty.tree_pages
        ),
        (CommitStamp::NONE, 0, 0, 0)
    );
    db.write(|tx| {
        for index in 0..5000u32 {
            tx.put(index.to_be_bytes(), [7; 20])?;
        }
        Ok(())
    })
    .unwrap();
    let report = db.check().unwrap();
    assert_eq!(report.entries, 5000);
    assert_eq!(report.key_bytes, 5000 * 4);
    assert_eq!(report.value_bytes, 5000 * 20);
    assert!(report.tree_height >= 2, "{report:?}");
    assert!(report.tree_pages > 1, "{report:?}");
    assert_eq!(report.stamp, db.stats().unwrap().last_commit);
    assert_eq!((report.overflow_values, report.leaked_pages), (0, 0));
    db.put("large", vec![5; 10_000]).unwrap();
    db.delete(0u32.to_be_bytes()).unwrap();
    let report = db.check().unwrap();
    assert_eq!((report.overflow_values, report.overflow_pages), (1, 3));
    assert_eq!(report.value_bytes, 4999 * 20 + 10_000);
    assert_eq!(report.leaked_pages, 0);
    let stats = db.stats().unwrap();
    assert_eq!(report.free_pages, stats.free_pages);
    assert_eq!(
        2 + report.tree_pages + report.overflow_pages + report.free_pages + report.log_pages,
        stats.page_count
    );
}

#[test]
fn stats_report_the_file_the_cache_and_the_last_commit() {
    let file = TempPath::new("stats");
    let db = Database::open_with(file.path(), Options::new().cache_bytes(1 << 20)).unwrap();
    let before = db.stats().unwrap();
    assert_eq!(before.page_size, 4096);
    assert_eq!(before.cache_pages, 256);
    assert_eq!(before.last_commit, CommitStamp::NONE);
    assert_eq!(before.file_bytes, before.page_count * 4096);
    assert_eq!(
        before.file_bytes,
        std::fs::metadata(file.path()).unwrap().len()
    );

    let stamp = db
        .write(|tx| {
            tx.put("k", "v")?;
            Ok(())
        })
        .and_then(|()| db.stats())
        .unwrap()
        .last_commit;
    assert!(stamp > CommitStamp::NONE);
    let after = db.stats().unwrap();
    assert!(after.page_count > before.page_count);
    assert!(after.cached_pages > 0);
    assert!(after.cache_hits + after.cache_misses > 0);
    assert!(after.log_bytes_since_checkpoint > 0);
    db.checkpoint().unwrap();
    assert_eq!(db.stats().unwrap().log_bytes_since_checkpoint, 0);
}

#[test]
fn every_durability_mode_keeps_commits_across_a_close() {
    for durability in [Durability::Full, Durability::Normal, Durability::Off] {
        let file = TempPath::new("durability");
        let options = Options::new()
            .durability(durability)
            .auto_checkpoint_log_bytes(None);
        let db = Database::open_with(file.path(), options).unwrap();
        for index in 0..50u8 {
            db.put([index], [index; 100]).unwrap();
        }
        db.sync().unwrap();
        db.close().unwrap();
        let db = Database::open(file.path()).unwrap();
        assert_eq!(db.check().unwrap().entries, 50, "{durability:?}");
        assert_eq!(db.get([49]).unwrap(), Some(vec![49; 100]));
    }
}

#[test]
fn errors_convert_to_io_errors_of_the_matching_kind() {
    let file = TempPath::new("io-error");
    let error =
        Database::open_with(file.path(), Options::new().create_if_missing(false)).unwrap_err();
    let io: std::io::Error = error.into();
    assert_eq!(io.kind(), std::io::ErrorKind::NotFound);
    assert!(io.to_string().contains("does not exist"), "{io}");
}

#[test]
fn a_database_and_its_errors_can_cross_threads() {
    fn shared<T: Send + Sync>() {}
    shared::<Database>();
    shared::<eggdb::Error>();
    shared::<eggdb::ReadTransaction<'static>>();
    fn sent<T: Send>() {}
    sent::<eggdb::WriteTransaction<'static>>();
    let _ = Duration::ZERO;
}

#[test]
fn a_damaged_page_is_reported_as_corrupt_never_as_wrong_data() {
    let file = TempPath::new("corrupt");
    let db = Database::open(file.path()).unwrap();
    db.write(|tx| {
        for index in 0..2000u32 {
            tx.put(index.to_be_bytes(), [5; 50])?;
        }
        Ok(())
    })
    .unwrap();
    db.close().unwrap();
    let mut bytes = std::fs::read(file.path()).unwrap();
    for page in bytes.chunks_mut(4096).skip(2) {
        page[2048] ^= 0x5A;
    }
    std::fs::write(file.path(), &bytes).unwrap();

    let error = Database::open(file.path())
        .and_then(|db| db.check().map(|_| ()))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Corrupt, "{error}");
}
