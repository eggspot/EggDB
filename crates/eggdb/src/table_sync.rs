//! SQL tables synced with a server from a device, offline-first (D1107): the tables enabled with
//! `eggdb_sync.enable` log their changes while the device is offline, and [`sync_tables`] stamps
//! them into the device's sync replica, exchanges changes with the server over HTTP as the
//! signed-in user, and writes the rows other devices changed back into the tables.

use crate::{Database, Error, ErrorKind, Result};
use eggdb_sync::{Cursors, Replica};
use std::time::Duration;

const CURSORS: &str = "eggdb.sync_cursors";
const DEVICE_SPACE: u32 = 0;
const BATCH: usize = 500;
const TIMEOUT: Duration = Duration::from_secs(30);

/// What one [`sync_tables`] call moved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TableSyncReport {
    /// Local row changes handed to the replica.
    pub sent: usize,
    /// Rows other devices changed, written into the tables.
    pub received: usize,
}

fn sync_failed(error: impl std::fmt::Display) -> Error {
    Error::new(ErrorKind::Io, format!("sync: {error}"))
}

fn sql_failed(error: eggdb_sql::SqlError) -> Error {
    Error::new(ErrorKind::Other, format!("sync: {}", error.message()))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

fn load_cursors(db: &Database, url: &str) -> Result<Cursors> {
    db.create_collection(CURSORS)?;
    let stored = db.read(|tx| tx.collection(CURSORS)?.get(url))?;
    Ok(match stored.as_deref() {
        Some(bytes) if bytes.len() == 16 => Cursors {
            sent: u64::from_be_bytes(bytes[..8].try_into().unwrap_or_default()),
            received: u64::from_be_bytes(bytes[8..].try_into().unwrap_or_default()),
        },
        _ => Cursors::default(),
    })
}

fn store_cursors(db: &Database, url: &str, cursors: Cursors) -> Result<()> {
    let mut bytes = cursors.sent.to_be_bytes().to_vec();
    bytes.extend_from_slice(&cursors.received.to_be_bytes());
    db.write(|tx| tx.collection(CURSORS)?.put(url, bytes))
}

/// Syncs `db`'s synced tables with the server at `url` as the user `token` signs in, this
/// device being replica `device_id` (unique among the user's devices): sends every change
/// logged since the last call, takes in the server's, and repeats until both sides are caught
/// up. Cursors are kept in the file per server, so each call sends and receives only what is
/// new; a failed call leaves the rest for the next one.
pub fn sync_tables(db: &Database, device_id: u64, url: &str, token: &str) -> Result<TableSyncReport> {
    let keyspace = crate::keyspace::DatabaseKeyspace { db, prefix: crate::keyspace::SYNC_COLLECTIONS };
    let replica = Replica::new(&keyspace, DEVICE_SPACE, device_id);
    let store = db.sql_store();
    let mut report = TableSyncReport::default();
    loop {
        let sent = eggdb_sql::sync_tables::drain_outbox(&store, BATCH as u32, &mut |local| {
            for (key, value) in local {
                replica.write(key, value.as_deref(), now_ms()).map_err(|error| error.to_string())?;
            }
            Ok(())
        })
        .map_err(sql_failed)?;
        if sent == 0 {
            break;
        }
        report.sent += sent;
    }
    let mut cursors = load_cursors(db, url)?;
    loop {
        let before = cursors;
        let changed = eggdb_sync::http::sync_over_http_changed(&replica, &mut cursors, url, token, BATCH, TIMEOUT)
            .map_err(sync_failed)?;
        let rows: Vec<eggdb_sql::sync_tables::OutboxChange> =
            changed.into_iter().map(|change| (change.key, change.value)).collect();
        report.received += eggdb_sql::sync_tables::write_rows(&store, &rows).map_err(sql_failed)?;
        store_cursors(db, url, cursors)?;
        if cursors == before {
            break;
        }
    }
    Ok(report)
}
