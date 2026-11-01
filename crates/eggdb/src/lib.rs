//! EggDB: one database for every kind of data, written from scratch in Rust.
//!
//! This release is an embedded, crash-safe, transactional key-value store in one file: an
//! ordered map from byte-string keys to byte-string values, opened in your process like
//! SQLite. The guide is `docs/embedded.md` in the repository.
//!
//! # Quickstart
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let path = std::env::temp_dir().join(format!("eggdb-doc-quickstart-{}.db", std::process::id()));
//! use eggdb::Database;
//!
//! let db = Database::open(&path)?;
//!
//! db.write(|tx| {
//!     tx.put("fruit:apple", "red")?;
//!     tx.put("fruit:banana", "yellow")?;
//!     tx.put("veg:carrot", "orange")?;
//!     Ok(())
//! })?;
//!
//! assert_eq!(db.get("fruit:apple")?, Some(b"red".to_vec()));
//!
//! let tx = db.begin_read()?;
//! for entry in tx.scan_prefix("fruit:") {
//!     let (key, value) = entry?;
//!     println!("{} = {}", String::from_utf8_lossy(&key), String::from_utf8_lossy(&value));
//! }
//! drop(tx);
//!
//! db.close()?;
//! # std::fs::remove_file(&path)?;
//! # Ok(()) }
//! ```
//!
//! # What there is
//! - [`Database`]: [`open`](Database::open) a file (created if missing) or
//!   [`open_with`](Database::open_with) [`Options`] such as [`Durability`], the cache size or
//!   read-only access; one-step [`get`](Database::get), [`put`](Database::put) and
//!   [`delete`](Database::delete); [`check`](Database::check), [`stats`](Database::stats),
//!   [`sync`](Database::sync), [`checkpoint`](Database::checkpoint) and
//!   [`close`](Database::close).
//! - [`ReadTransaction`]: a snapshot of the last commit, from
//!   [`begin_read`](Database::begin_read) or [`read`](Database::read).
//! - [`WriteTransaction`]: the one writer, from [`begin_write`](Database::begin_write) or
//!   [`write`](Database::write), committed atomically or rolled back.
//! - [`Scan`]: the entries of a [`KeyRange`] or a prefix, in key order or reversed.
//! - Collections: named key spaces in the same file beside the default one, each its own
//!   ordered map. [`Database::create_collection`], [`Database::drop_collection`] and
//!   [`Database::collection_names`] manage them; [`Database::collection`] gives a
//!   [`Collection`] for one-step reads and writes, and [`ReadTransaction::collection`] and
//!   [`WriteTransaction::collection`] a [`ReadCollection`] or [`WriteCollection`] inside a
//!   transaction. One transaction can change several collections and the default key space
//!   together.
//! - Expiring keys and counters: [`WriteTransaction::put_with_expiry`],
//!   [`set_expiry`](WriteTransaction::set_expiry), [`clear_expiry`](WriteTransaction::clear_expiry)
//!   and [`time_to_live`](ReadTransaction::time_to_live) give keys a time to live, after which
//!   no read sees them and commits remove them; [`WriteTransaction::increment`] adds to a
//!   counter and [`WriteTransaction::compare_and_set`] changes a value only when it is the one
//!   expected, each atomic. [`Database`] and [`Collection`] have one-step versions of each, and
//!   [`Options::clock`] sets the clock time is read from, a [`SimClock`] in tests.
//! - [`Error`], with an [`ErrorKind`] to match on.
//! - [`sql`]: SQL on the same file, in PostgreSQL's, MySQL's, SQLite's or Oracle's dialect, from
//!   [`Database::sql_store`].

mod collection;
mod concurrent;
mod database;
mod error;
mod expiry;
mod options;
mod scan;
pub mod sql;
mod transaction;
mod writer_slot;

pub use collection::{Collection, ReadCollection, WriteCollection};
pub use concurrent::{ConcurrentTransaction, Isolation};
pub use database::{BackupReport, CheckReport, CompactReport, Database, Stats};
pub use eggdb_platform::{Clock, SimClock};
pub use eggdb_storage::CommitStamp;
pub use error::{Error, ErrorKind, Result};
pub use options::{Durability, Options};
pub use scan::{KeyRange, Scan};
pub use transaction::{ReadTransaction, ValueReader, WriteTransaction};

#[cfg(doctest)]
#[doc = include_str!("../../../docs/embedded.md")]
pub struct EmbeddedGuide;

#[cfg(doctest)]
#[doc = include_str!("../../../README.md")]
pub struct Readme;

/// The version of this EggDB build.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::VERSION;

    #[test]
    fn version_matches_the_package() {
        assert_eq!(VERSION, "0.1.0");
    }
}
