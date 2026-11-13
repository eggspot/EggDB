use crate::change_archive::{ArchivedEvent, read_change_archive};
use crate::{CommitStamp, Database, Error, ErrorKind, Result};
use std::path::Path;

/// What [`replay_changes`] applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ReplayReport {
    /// The archived commits applied, each as one commit of the database.
    pub commits: u64,
    /// The keys those commits changed.
    pub changes: u64,
    /// The stamp of the last archived commit applied, or the starting stamp when none was.
    pub reached: CommitStamp,
}

fn uncovered(message: String) -> Error {
    Error::new(ErrorKind::NotFound, message)
}

/// Replays the change archive in `directory` (see
/// [`Options::archive_changes_to`](crate::Options::archive_changes_to)) onto `db`, which holds
/// the database as of the commit stamped `after` (a snapshot's [`BackupReport::stamp`](crate::BackupReport::stamp)):
/// every archived commit after `after`, up to and including `up_to` when given, is applied in
/// order, each as one commit. It fails without changing anything when the archive does not
/// reach back to `after`, or when commits in the range were made while the archive was not
/// kept.
pub fn replay_changes(db: &Database, directory: &Path, after: CommitStamp, up_to: Option<CommitStamp>) -> Result<ReplayReport> {
    let events = read_change_archive(directory)?;
    replay_events(db, events, directory, after, up_to)
}

/// Replays archived `events` (from [`read_change_archive`] or
/// [`read_change_file`](crate::read_change_file)) onto `db` as [`replay_changes`] does;
/// `source` names where they came from in messages.
pub fn replay_events(
    db: &Database,
    events: Vec<ArchivedEvent>,
    source: &Path,
    after: CommitStamp,
    up_to: Option<CommitStamp>,
) -> Result<ReplayReport> {
    let directory = source;
    let limit = up_to.map_or(u64::MAX, CommitStamp::bits);
    let covered = events.iter().any(|event| match event {
        ArchivedEvent::Base(stamp) | ArchivedEvent::Gap(stamp) | ArchivedEvent::Commit(stamp, _) => stamp.bits() <= after.bits(),
    });
    if !covered {
        return Err(uncovered(format!(
            "the change archive {} begins after the snapshot's commit; restore a later snapshot",
            directory.display()
        )));
    }
    if let Some(ArchivedEvent::Gap(stamp)) = events
        .iter()
        .find(|event| matches!(event, ArchivedEvent::Gap(stamp) if stamp.bits() > after.bits() && stamp.bits() <= limit))
    {
        return Err(uncovered(format!(
            "commits up to stamp {} were made without the change archive; restore a snapshot taken after it",
            stamp.bits()
        )));
    }
    let mut report = ReplayReport { reached: after, ..ReplayReport::default() };
    for event in events {
        let ArchivedEvent::Commit(stamp, changes) = event else {
            continue;
        };
        if stamp.bits() <= after.bits() {
            continue;
        }
        if stamp.bits() > limit {
            break;
        }
        let mut transaction = db.begin_write()?;
        transaction.note_expiring_keys();
        for (key, value) in &changes {
            match value {
                Some(value) => transaction.put_raw(key, value)?,
                None => {
                    transaction.delete_raw(key)?;
                }
            }
        }
        transaction.commit()?;
        report.commits += 1;
        report.changes += changes.len() as u64;
        report.reached = stamp;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Options;

    #[test]
    fn a_snapshot_and_the_archive_restore_the_database_to_any_commit() {
        let base = std::env::temp_dir().join(format!("eggdb-replay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let archive = base.join("changes");
        let db = Database::open_with(base.join("live.eggdb"), Options::new().archive_changes_to(&archive)).unwrap();
        db.put("a", "1").unwrap();
        let snapshot = db.backup_to(base.join("snapshot.eggdb")).unwrap();
        db.put("b", "2").unwrap();
        let middle = db.begin_read().unwrap().stamp();
        db.put("a", "3").unwrap();
        db.delete("b").unwrap();
        drop(db);
        let restored = Database::open(base.join("snapshot.eggdb")).unwrap();
        let report = replay_changes(&restored, &archive, snapshot.stamp, Some(middle)).unwrap();
        assert_eq!(report.commits, 1);
        assert_eq!(restored.get("a").unwrap(), Some(b"1".to_vec()));
        assert_eq!(restored.get("b").unwrap(), Some(b"2".to_vec()));
        let rest = replay_changes(&restored, &archive, middle, None).unwrap();
        assert_eq!(rest.commits, 2);
        assert_eq!(restored.get("a").unwrap(), Some(b"3".to_vec()));
        assert_eq!(restored.get("b").unwrap(), None);
        drop(restored);
        let _ = std::fs::remove_dir_all(&base);
    }
}
