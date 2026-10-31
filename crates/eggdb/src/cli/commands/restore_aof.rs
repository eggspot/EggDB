use super::{Access, Session};
use crate::cli::args::{CommandName, Format};
use crate::cli::failure::{Exit, Failure};
use crate::cli::render::{self, JsonObject};
use std::path::Path;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

impl Session<'_> {
    pub(super) fn restore_aof(&mut self, dump: &Path) -> Result<Exit, Failure> {
        let shown = render::text(dump.display().to_string().as_bytes());
        let input = std::fs::File::open(dump).map_err(|error| {
            Failure::usage(format!("cannot read {shown}: {error}"), Some(CommandName::Restore))
        })?;
        let db = self.open(Access::Write { create: true }, None)?;
        let keyspace = super::serve_resp::DatabaseKeyspace {
            db: &db,
            prefix: super::serve_resp::RESP_COLLECTIONS,
        };
        let report = eggdb_resp::replay_aof(&keyspace, std::io::BufReader::new(input), now_ms())
            .map_err(|message| {
                Failure::database(format!("cannot restore {shown}: {message}"), Exit::DatabaseError)
            })?;
        match self.format {
            Format::Json => self.out.text(
                &JsonObject::new()
                    .string("format", "redis-aof")
                    .number("commands", report.commands)
                    .number("failed", report.failed)
                    .line(),
            ),
            _ => {
                self.out.text(&format!(
                    "restored {shown} (Redis append-only file): {} commands, {} failed\n",
                    report.commands, report.failed
                ));
                if let Some(error) = &report.first_error {
                    self.out.text(&format!("first failure: {}\n", render::text(error.as_bytes())));
                }
            }
        }
        self.close(db)?;
        Ok(if report.failed == 0 { Exit::Done } else { Exit::DatabaseError })
    }
}
