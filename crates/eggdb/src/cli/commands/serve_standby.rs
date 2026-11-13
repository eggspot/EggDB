use eggdb::{ArchivedEvent, Database};
use std::path::Path;
use eggdb_platform::time::{SystemTime, UNIX_EPOCH};
use std::time::Duration;

const POLL: Duration = Duration::from_secs(1);

fn now_micros() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| u64::try_from(since.as_micros()).unwrap_or(u64::MAX))
}

/// Keeps `db` a standby of the primary whose change archive (D1019) arrives in `directory`
/// (D1027): every archived commit after the standby's position is applied in order, each once
/// it is `delay` old, and the archive is read again every second for new commits. Returns why
/// it stopped when the archive cannot continue the standby's history.
pub(super) fn follow_archive(db: &Database, directory: &Path, delay: Duration, stop: &(dyn Fn() -> bool + Sync)) -> Result<(), String> {
    let mut position = db
        .standby_position()
        .map_err(|error| error.to_string())?
        .ok_or("restore this standby from an archive of the primary first (eggdb restore <file> <archive>), so it knows where it starts")?;
    let delay_micros = u64::try_from(delay.as_micros()).unwrap_or(u64::MAX);
    let mut first_segment = 0u64;
    while !stop() {
        let (events, numbers) = eggdb::read_change_archive_from(directory, first_segment).map_err(|error| error.to_string())?;
        for event in events {
            match event {
                ArchivedEvent::Commit(stamp, changes) if stamp.bits() > position.bits() => {
                    if stamp.unix_micros().saturating_add(delay_micros) > now_micros() {
                        break;
                    }
                    db.apply_archived(stamp, &changes).map_err(|error| format!("applying the archived commit {} failed: {error}", stamp.bits()))?;
                    position = stamp;
                }
                ArchivedEvent::Gap(stamp) if stamp.bits() > position.bits() => {
                    return Err(format!(
                        "the primary made commits up to stamp {} without archiving them; restore this standby again from a later copy",
                        stamp.bits()
                    ));
                }
                _ => {}
            }
        }
        if let Some(settled) = numbers.len().checked_sub(2).map(|index| numbers[index]) {
            first_segment = first_segment.max(settled);
        }
        std::thread::sleep(POLL);
    }
    Ok(())
}
