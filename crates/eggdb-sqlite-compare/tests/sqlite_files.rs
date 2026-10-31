//! EggDB's reader of SQLite's file format against files SQLite itself writes.

use eggdb_sqlite_format::{SqliteFile, SqliteValue};
use rusqlite::Connection;

fn scratch(name: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!("eggdb-sqlite-files-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    directory.join(name)
}

#[test]
fn files_sqlite_writes_read_back_row_for_row() {
    for wal in [false, true] {
        let path = scratch(if wal { "wal.sqlite" } else { "rollback.sqlite" });
        let _ = std::fs::remove_file(&path);
        let connection = Connection::open(&path).unwrap();
        if wal {
            connection.pragma_update(None, "journal_mode", "WAL").unwrap();
            connection.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        }
        connection
            .execute_batch(
                "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT, score REAL, data BLOB);
                 CREATE INDEX notes_score ON notes(score);
                 CREATE VIEW best AS SELECT id FROM notes WHERE score > 1;",
            )
            .unwrap();
        let long = "x".repeat(20_000);
        for id in 1..=300i64 {
            let body = if id == 7 { long.clone() } else { format!("note {id}") };
            connection
                .execute(
                    "INSERT INTO notes VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![id * 3, body, id as f64 / 2.0, vec![id as u8; 3]],
                )
                .unwrap();
        }
        let file = SqliteFile::open(&path).unwrap();
        let schema = file.schema().unwrap();
        let table = schema.iter().find(|entry| entry.name == "notes").unwrap();
        assert!(schema.iter().any(|entry| entry.kind == "index" && entry.name == "notes_score"));
        assert!(schema.iter().any(|entry| entry.kind == "view" && entry.name == "best"));
        let rows = file.rows(table.root_page).unwrap();
        assert_eq!(rows.len(), 300, "wal: {wal}");
        assert_eq!(rows[0].rowid, 3);
        assert_eq!(rows[0].values[0], SqliteValue::Null);
        assert_eq!(rows[6].values[1], SqliteValue::Text(long.clone()));
        assert_eq!(rows[1].values[2], SqliteValue::Real(1.0));
        assert_eq!(rows[299].values[3], SqliteValue::Blob(vec![44; 3]));
        drop(connection);
    }
}
