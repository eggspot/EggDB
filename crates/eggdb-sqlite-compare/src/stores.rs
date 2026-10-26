use crate::bench_file::BenchFile;
use crate::entries::{key, value};
use eggdb::{Database, Durability, Options};
use rusqlite::Connection;

pub(crate) const CACHE_BYTES: usize = 64 << 20;
pub(crate) const PAGE_BYTES: usize = 4096;
const AUTO_CHECKPOINT_LOG_BYTES: usize = 1 << 20;
const SQLITE_SIDE_FILES: [&str; 3] = ["-wal", "-shm", "-journal"];

pub(crate) fn failed<E: std::fmt::Display>(what: &str) -> impl Fn(E) -> String + '_ {
    move |error| format!("{what}: {error}")
}

pub(crate) struct Stores {
    pub(crate) eggdb: Database,
    pub(crate) sqlite: Connection,
    _eggdb_file: BenchFile,
    _sqlite_file: SqliteFile,
}

struct SqliteFile {
    main: BenchFile,
}

impl Drop for SqliteFile {
    fn drop(&mut self) {
        for suffix in SQLITE_SIDE_FILES {
            let mut side_file = self.main.path.clone().into_os_string();
            side_file.push(suffix);
            let _ = std::fs::remove_file(side_file);
        }
    }
}

impl Stores {
    pub(crate) fn open(name: &str, durability: Durability, entries: u64) -> Result<Self, String> {
        let eggdb_file = BenchFile::new(&format!("compare-{name}-eggdb"))?;
        let sqlite_file = SqliteFile {
            main: BenchFile::new(&format!("compare-{name}-sqlite"))?,
        };
        let eggdb = open_eggdb(&eggdb_file, durability)?;
        let sqlite = open_sqlite(&sqlite_file.main, durability)?;
        let stores = Self {
            eggdb,
            sqlite,
            _eggdb_file: eggdb_file,
            _sqlite_file: sqlite_file,
        };
        stores.fill(entries)?;
        Ok(stores)
    }

    fn fill(&self, entries: u64) -> Result<(), String> {
        self.eggdb
            .write(|tx| {
                for index in 0..entries {
                    tx.put(key(index), value(index))?;
                }
                Ok(())
            })
            .map_err(failed("fill EggDB"))?;
        self.sqlite
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(failed("begin the SQLite fill"))?;
        let mut insert = self
            .sqlite
            .prepare("INSERT INTO kv(k, v) VALUES (?1, ?2)")
            .map_err(failed("prepare the SQLite fill"))?;
        for index in 0..entries {
            insert
                .execute((&key(index)[..], &value(index)[..]))
                .map_err(failed("fill SQLite"))?;
        }
        self.sqlite
            .execute_batch("COMMIT")
            .map_err(failed("commit the SQLite fill"))
    }
}

fn open_eggdb(file: &BenchFile, durability: Durability) -> Result<Database, String> {
    let options = Options::new()
        .cache_bytes(CACHE_BYTES)
        .page_size_bytes(PAGE_BYTES)
        .durability(durability)
        .auto_checkpoint_log_bytes(Some(AUTO_CHECKPOINT_LOG_BYTES as u64));
    Database::open_with(&file.path, options).map_err(failed("open EggDB"))
}

pub(crate) fn synchronous(durability: Durability) -> Result<&'static str, String> {
    match durability {
        Durability::Full => Ok("FULL"),
        Durability::Normal => Ok("NORMAL"),
        Durability::Off => Ok("OFF"),
        other => Err(format!("no SQLite synchronous setting matches {other:?}")),
    }
}

fn open_sqlite(file: &BenchFile, durability: Durability) -> Result<Connection, String> {
    let sqlite = Connection::open(&file.path).map_err(failed("open SQLite"))?;
    let full_fsync = if cfg!(target_vendor = "apple") {
        "ON"
    } else {
        "OFF"
    };
    sqlite
        .execute_batch(&format!(
            "PRAGMA page_size = {PAGE_BYTES};
             PRAGMA locking_mode = EXCLUSIVE;
             PRAGMA synchronous = {};
             PRAGMA cache_size = -{};
             PRAGMA mmap_size = 0;
             PRAGMA wal_autocheckpoint = {};
             PRAGMA fullfsync = {full_fsync};
             PRAGMA checkpoint_fullfsync = {full_fsync};",
            synchronous(durability)?,
            CACHE_BYTES / 1024,
            AUTO_CHECKPOINT_LOG_BYTES / PAGE_BYTES,
        ))
        .map_err(failed("configure SQLite"))?;
    let journal_mode: String = sqlite
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(failed("switch SQLite to WAL"))?;
    if journal_mode != "wal" {
        return Err(format!("SQLite kept the journal mode {journal_mode}"));
    }
    sqlite
        .execute_batch("CREATE TABLE kv(k BLOB PRIMARY KEY, v BLOB) WITHOUT ROWID, STRICT")
        .map_err(failed("create the SQLite table"))?;
    Ok(sqlite)
}

#[cfg(test)]
mod tests {
    use super::{PAGE_BYTES, Stores, synchronous};
    use eggdb::Durability;

    fn pragma(stores: &Stores, name: &str) -> String {
        stores
            .sqlite
            .query_row(&format!("PRAGMA {name}"), [], |row| {
                row.get::<_, rusqlite::types::Value>(0)
            })
            .map(|value| match value {
                rusqlite::types::Value::Integer(number) => number.to_string(),
                rusqlite::types::Value::Text(text) => text,
                other => format!("{other:?}"),
            })
            .unwrap()
    }

    #[test]
    fn sqlite_runs_with_the_settings_that_match_eggdb() {
        for durability in [Durability::Full, Durability::Normal, Durability::Off] {
            let stores = Stores::open("settings", durability, 10).unwrap();
            assert_eq!(pragma(&stores, "journal_mode"), "wal");
            assert_eq!(pragma(&stores, "locking_mode"), "exclusive");
            assert_eq!(pragma(&stores, "page_size"), PAGE_BYTES.to_string());
            assert_eq!(pragma(&stores, "mmap_size"), "0");
            assert_eq!(pragma(&stores, "cache_size"), "-65536");
            assert_eq!(pragma(&stores, "wal_autocheckpoint"), "256");
            let expected = match synchronous(durability).unwrap() {
                "FULL" => "2",
                "NORMAL" => "1",
                _ => "0",
            };
            assert_eq!(pragma(&stores, "synchronous"), expected);
            assert_eq!(stores.eggdb.page_size(), PAGE_BYTES);
        }
    }
}
