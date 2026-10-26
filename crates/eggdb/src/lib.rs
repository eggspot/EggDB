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
//! - [`Error`], with an [`ErrorKind`] to match on.

mod database;
mod error;
mod options;
mod scan;
mod transaction;
mod writer_slot;

pub use database::{CheckReport, Database, Stats};
pub use eggdb_storage::CommitStamp;
pub use error::{Error, ErrorKind, Result};
pub use options::{Durability, Options};
pub use scan::{KeyRange, Scan};
pub use transaction::{ReadTransaction, WriteTransaction};

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
