//! Read and write transactions through the embedded API: commits, rollbacks, snapshots, threads and failures.

mod common;

use common::TempPath;
use eggdb::{CommitStamp, Database, ErrorKind, Options};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

fn key(index: u32) -> [u8; 4] {
    index.to_be_bytes()
}

#[test]
fn a_commit_is_seen_by_later_transactions_and_returns_a_rising_stamp() {
    let file = TempPath::new("commit");
    let db = Database::open(file.path()).unwrap();
    let mut tx = db.begin_write().unwrap();
    tx.put("a", "1").unwrap();
    let first = tx.commit().unwrap();
    assert!(first > CommitStamp::NONE);
    let mut tx = db.begin_write().unwrap();
    tx.put("a", "2").unwrap();
    let second = tx.commit().unwrap();
    assert!(second > first);
    let read = db.begin_read().unwrap();
    assert_eq!(read.stamp(), second);
    assert_eq!(read.get("a").unwrap(), Some(b"2".to_vec()));
}

#[test]
fn get_with_reads_a_value_in_place_in_both_kinds_of_transaction() {
    let file = TempPath::new("get-with");
    let db = Database::open(file.path()).unwrap();
    let read = db.begin_read().unwrap();
    assert_eq!(read.get_with("a", <[u8]>::len).unwrap(), None);
    drop(read);
    db.put("a", "123").unwrap();
    let mut tx = db.begin_write().unwrap();
    tx.put("b", "45").unwrap();
    assert_eq!(tx.get_with("a", <[u8]>::len).unwrap(), Some(3));
    assert_eq!(
        tx.get_with("b", <[u8]>::to_vec).unwrap(),
        Some(b"45".to_vec())
    );
    assert_eq!(tx.get_with("c", <[u8]>::len).unwrap(), None);
    tx.commit().unwrap();
    let read = db.begin_read().unwrap();
    assert_eq!(
        read.get_with("b", <[u8]>::to_vec).unwrap(),
        Some(b"45".to_vec())
    );
}

#[test]
fn put_if_absent_stores_a_new_key_once_and_a_rollback_forgets_it() {
    let file = TempPath::new("put-if-absent");
    let db = Database::open(file.path()).unwrap();
    let mut tx = db.begin_write().unwrap();
    assert!(tx.put_if_absent("a", "1").unwrap());
    assert!(!tx.put_if_absent("a", "2").unwrap());
    tx.commit().unwrap();
    let mut tx = db.begin_write().unwrap();
    assert!(!tx.put_if_absent("a", "3").unwrap());
    assert!(tx.put_if_absent("b", "4").unwrap());
    tx.rollback().unwrap();
    assert_eq!(db.get("a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(db.get("b").unwrap(), None);
    assert_eq!(db.check().unwrap().entries, 1);
}

#[test]
fn a_rollback_or_a_drop_forgets_every_change() {
    let file = TempPath::new("rollback");
    let db = Database::open(file.path()).unwrap();
    db.put("kept", "1").unwrap();

    let mut tx = db.begin_write().unwrap();
    tx.put("kept", "2").unwrap();
    tx.put("new", "x").unwrap();
    assert_eq!(tx.get("new").unwrap(), Some(b"x".to_vec()));
    tx.rollback().unwrap();

    let mut tx = db.begin_write().unwrap();
    assert!(tx.delete("kept").unwrap());
    drop(tx);

    assert_eq!(db.get("kept").unwrap(), Some(b"1".to_vec()));
    assert_eq!(db.get("new").unwrap(), None);
}

#[test]
fn rolling_back_the_first_put_leaves_an_empty_database() {
    let file = TempPath::new("first-put");
    let db = Database::open(file.path()).unwrap();
    let mut tx = db.begin_write().unwrap();
    tx.put("a", "1").unwrap();
    tx.rollback().unwrap();
    assert_eq!(db.get("a").unwrap(), None);
    assert_eq!(db.check().unwrap().tree_pages, 0);
    assert!(!db.delete("a").unwrap());
    assert_eq!(db.begin_read().unwrap().scan(..).count(), 0);
    db.put("b", "2").unwrap();
    drop(db);
    assert_eq!(
        Database::open(file.path()).unwrap().get("b").unwrap(),
        Some(b"2".to_vec())
    );
}

#[test]
fn write_commits_on_ok_and_rolls_back_on_err() {
    let file = TempPath::new("write-closure");
    let db = Database::open(file.path()).unwrap();
    let returned = db
        .write(|tx| {
            tx.put("a", "1")?;
            Ok(42)
        })
        .unwrap();
    assert_eq!(returned, 42);
    let error = db
        .write(|tx| {
            tx.put("a", "2")?;
            tx.put("b", "2")?;
            tx.put(vec![0; 600], "")
        })
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TooLarge);
    assert_eq!(db.get("a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(db.get("b").unwrap(), None);
    assert_eq!(db.read(|tx| tx.get("a")).unwrap(), Some(b"1".to_vec()));
}

#[test]
fn a_read_transaction_keeps_its_snapshot_across_later_commits() {
    let file = TempPath::new("snapshot");
    let db = Database::open(file.path()).unwrap();
    db.write(|tx| {
        for index in 0..2000 {
            tx.put(key(index), "old")?;
        }
        Ok(())
    })
    .unwrap();
    let before = db.begin_read().unwrap();
    db.write(|tx| {
        for index in 0..2000 {
            if index % 2 == 0 {
                tx.delete(key(index))?;
            } else {
                tx.put(key(index), "new")?;
            }
        }
        tx.put(key(5000), "added")?;
        Ok(())
    })
    .unwrap();
    let after = db.begin_read().unwrap();
    assert!(after.stamp() > before.stamp());

    assert_eq!(before.get(key(10)).unwrap(), Some(b"old".to_vec()));
    assert_eq!(before.get(key(5000)).unwrap(), None);
    assert_eq!(before.scan(..).count(), 2000);
    assert!(
        before
            .scan(..)
            .all(|entry| entry.unwrap().1 == b"old".to_vec())
    );
    assert_eq!(after.get(key(10)).unwrap(), None);
    assert_eq!(after.get(key(11)).unwrap(), Some(b"new".to_vec()));
    assert_eq!(after.scan(..).count(), 1001);
}

#[test]
fn a_reader_does_not_see_the_open_write_transaction() {
    let file = TempPath::new("uncommitted");
    let db = Database::open(file.path()).unwrap();
    db.put("a", "1").unwrap();
    let mut tx = db.begin_write().unwrap();
    tx.put("a", "2").unwrap();
    tx.put("b", "2").unwrap();
    let reader = db.begin_read().unwrap();
    assert_eq!(reader.get("a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(db.get("b").unwrap(), None);
    tx.commit().unwrap();
    assert_eq!(reader.get("a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(db.get("b").unwrap(), Some(b"2".to_vec()));
}

#[test]
fn readers_on_other_threads_always_see_one_whole_commit() {
    let file = TempPath::new("threads");
    let db =
        Database::open_with(file.path(), Options::new().snapshot_memory_bytes(64 << 20)).unwrap();
    db.write(|tx| {
        for index in 0..100 {
            tx.put(key(index), 0u32.to_be_bytes())?;
        }
        Ok(())
    })
    .unwrap();
    let db = Arc::new(db);
    let readers: Vec<_> = (0..3)
        .map(|_| {
            let db = Arc::clone(&db);
            thread::spawn(move || {
                for _ in 0..200 {
                    let tx = db.begin_read().unwrap();
                    let values: Vec<Vec<u8>> = tx.scan(..).map(|entry| entry.unwrap().1).collect();
                    assert_eq!(values.len(), 100);
                    assert!(values.windows(2).all(|pair| pair[0] == pair[1]));
                }
            })
        })
        .collect();
    for round in 1..=50u32 {
        db.write(|tx| {
            for index in 0..100 {
                tx.put(key(index), round.to_be_bytes())?;
            }
            Ok(())
        })
        .unwrap();
    }
    for reader in readers {
        reader.join().unwrap();
    }
}

#[test]
fn a_second_writer_waits_for_the_first_to_end() {
    let file = TempPath::new("writer-waits");
    let db = Database::open(file.path()).unwrap();
    let started = Barrier::new(2);
    thread::scope(|scope| {
        scope.spawn(|| {
            let mut tx = db.begin_write().unwrap();
            tx.put("first", "1").unwrap();
            started.wait();
            thread::sleep(Duration::from_millis(100));
            tx.commit().unwrap();
        });
        started.wait();
        let began = Instant::now();
        let mut tx = db.begin_write().unwrap();
        assert!(began.elapsed() >= Duration::from_millis(50));
        assert_eq!(tx.get("first").unwrap(), Some(b"1".to_vec()));
        tx.put("second", "2").unwrap();
        tx.commit().unwrap();
    });
    assert_eq!(db.check().unwrap().entries, 2);
}

#[test]
fn a_writer_that_waits_too_long_fails_with_writer_active() {
    let file = TempPath::new("writer-timeout");
    let options = Options::new().write_timeout(Duration::from_millis(50));
    let db = Database::open_with(file.path(), options).unwrap();
    let holding = Barrier::new(2);
    let done = Barrier::new(2);
    thread::scope(|scope| {
        scope.spawn(|| {
            let tx = db.begin_write().unwrap();
            holding.wait();
            done.wait();
            drop(tx);
        });
        holding.wait();
        let error = db.try_begin_write().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::WriterActive);
        let began = Instant::now();
        let error = db.begin_write().unwrap_err();
        assert!(began.elapsed() >= Duration::from_millis(50));
        assert_eq!(error.kind(), ErrorKind::WriterActive);
        assert!(error.to_string().contains("write_timeout"), "{error}");
        assert_eq!(
            db.put("k", "v").unwrap_err().kind(),
            ErrorKind::WriterActive
        );
        done.wait();
    });
    db.put("k", "v").unwrap();
}

#[test]
fn a_thread_that_holds_the_writer_is_refused_a_second_one_at_once() {
    let file = TempPath::new("same-thread");
    let db = Database::open_with(
        file.path(),
        Options::new().write_timeout(Duration::from_secs(60)),
    )
    .unwrap();
    let tx = db.begin_write().unwrap();
    let began = Instant::now();
    let error = db.put("k", "v").unwrap_err();
    assert!(began.elapsed() < Duration::from_secs(10));
    assert_eq!(error.kind(), ErrorKind::WriterActive);
    assert!(error.to_string().contains("this thread"), "{error}");
    let error = db.checkpoint().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::WriterActive);
    drop(tx);
    db.put("k", "v").unwrap();
    db.checkpoint().unwrap();
}

#[test]
fn a_transaction_larger_than_the_cache_fails_and_is_rolled_back() {
    let file = TempPath::new("cache-full");
    let db = Database::open_with(file.path(), Options::new().cache_bytes(16 * 4096)).unwrap();
    db.put("before", "1").unwrap();
    let mut tx = db.begin_write().unwrap();
    let error = (0..10_000u32)
        .map(|index| tx.put(key(index), [0; 200]))
        .find_map(Result::err)
        .unwrap();
    assert_eq!(error.kind(), ErrorKind::CacheFull);
    assert!(error.to_string().contains("larger cache"), "{error}");
    let error = tx.put("after", "1").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TransactionFailed);
    assert_eq!(
        tx.get("before").unwrap_err().kind(),
        ErrorKind::TransactionFailed
    );
    assert_eq!(
        tx.commit().unwrap_err().kind(),
        ErrorKind::TransactionFailed
    );

    db.put("next", "1").unwrap();
    assert_eq!(db.check().unwrap().entries, 2);
    assert_eq!(db.get(key(0)).unwrap(), None);
}

#[test]
fn a_snapshot_that_outgrows_its_memory_expires() {
    let file = TempPath::new("snapshot-too-old");
    let options = Options::new().snapshot_memory_bytes(4 * 4096);
    let db = Database::open_with(file.path(), options).unwrap();
    db.write(|tx| {
        for index in 0..3000 {
            tx.put(key(index), [1; 100])?;
        }
        Ok(())
    })
    .unwrap();
    let old = db.begin_read().unwrap();
    db.write(|tx| {
        for index in 0..3000 {
            tx.put(key(index), [2; 100])?;
        }
        Ok(())
    })
    .unwrap();
    let error = old.get(key(1)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::SnapshotTooOld);
    let mut scan = old.scan(..);
    assert_eq!(
        scan.next().unwrap().unwrap_err().kind(),
        ErrorKind::SnapshotTooOld
    );
    assert!(scan.next().is_none());
    assert_eq!(db.get(key(1)).unwrap(), Some(vec![2; 100]));
}

#[test]
fn reads_of_an_empty_database_find_nothing() {
    let file = TempPath::new("empty");
    let db = Database::open(file.path()).unwrap();
    let tx = db.begin_read().unwrap();
    assert_eq!(tx.stamp(), CommitStamp::NONE);
    assert_eq!(tx.get("a").unwrap(), None);
    assert_eq!(tx.scan(..).count(), 0);
    assert_eq!(tx.scan_prefix("").rev().count(), 0);
    drop(tx);
    let mut tx = db.begin_write().unwrap();
    assert!(!tx.delete("a").unwrap());
    assert_eq!(tx.scan(..).next_back().map(|entry| entry.is_ok()), None);
    tx.commit().unwrap();
}

#[test]
fn a_refused_first_put_leaves_a_database_with_no_tree() {
    let file = TempPath::new("refused-first-put");
    let db = Database::open(file.path()).unwrap();
    let mut tx = db.begin_write().unwrap();
    assert_eq!(
        tx.put(vec![0; 513], "").unwrap_err().kind(),
        ErrorKind::TooLarge
    );
    assert_eq!(
        tx.put("k", vec![0; 17 << 20]).unwrap_err().kind(),
        ErrorKind::TooLarge
    );
    tx.commit().unwrap();
    let report = db.check().unwrap();
    assert_eq!((report.tree_height, report.tree_pages), (0, 0));
    db.put("k", "v").unwrap();
    assert_eq!(db.check().unwrap().tree_height, 1);
}

#[test]
fn key_value_writes_and_sql_share_one_commit_or_one_rollback() {
    use eggdb::sql::Dialect;
    let file = common::TempPath::new("kv-and-sql");
    let db = eggdb::Database::open(file.path()).unwrap();
    db.sql_store()
        .session(Dialect::PostgreSql)
        .execute(
            "CREATE TABLE orders (id integer PRIMARY KEY, total integer)",
            &mut |_| {},
        )
        .unwrap();
    let mut tx = db.begin_write().unwrap();
    tx.put("order:1:status", "paid").unwrap();
    tx.sql(Dialect::PostgreSql, "INSERT INTO orders VALUES (1, 50)")
        .unwrap();
    let error = tx
        .sql(Dialect::PostgreSql, "INSERT INTO orders VALUES (2, 10), (1, 99)")
        .unwrap_err();
    assert_eq!(error.state().code(), "23505");
    let counted = tx
        .sql(Dialect::PostgreSql, "SELECT count(*) FROM orders")
        .unwrap();
    assert_eq!(counted[0].rows[0][0], eggdb::sql::Value::BigInt(1));
    tx.commit().unwrap();
    assert_eq!(db.get("order:1:status").unwrap(), Some(b"paid".to_vec()));
    let mut tx = db.begin_write().unwrap();
    tx.put("order:2:status", "paid").unwrap();
    tx.sql(Dialect::PostgreSql, "INSERT INTO orders VALUES (2, 10)")
        .unwrap();
    tx.rollback().unwrap();
    assert_eq!(db.get("order:2:status").unwrap(), None);
    let mut rows = Vec::new();
    db.sql_store()
        .session(Dialect::PostgreSql)
        .execute("SELECT id FROM orders ORDER BY id", &mut |result| {
            rows.extend(result.rows)
        })
        .unwrap();
    assert_eq!(rows, vec![vec![eggdb::sql::Value::Integer(1)]]);
}
