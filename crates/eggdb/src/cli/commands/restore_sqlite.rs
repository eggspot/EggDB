use super::{Access, Session};
use crate::cli::args::Format;
use crate::cli::failure::{Exit, Failure};
use crate::cli::render::{self, JsonObject};
use std::path::Path;

impl Session<'_> {
    /// `eggdb restore <file> app.sqlite`: reads a file in SQLite's format (its tables, rows,
    /// indexes, views and triggers, and committed pages still in its `-wal`) into the EggDB
    /// file, as `eggdb sql` reads one into memory.
    pub(super) fn restore_sqlite(&mut self, dump: &Path) -> Result<Exit, Failure> {
        let shown = render::text(dump.display().to_string().as_bytes());
        let db = self.open(Access::Write { create: true }, None)?;
        let warnings = crate::cli::sqlite_import::import(&db, dump).map_err(|error| {
            Failure::database(
                format!("cannot restore {shown}: {error}"),
                Exit::for_error(error.kind()),
            )
        })?;
        match self.format {
            Format::Json => self.out.text(
                &JsonObject::new()
                    .string("format", "sqlite")
                    .number("skipped", warnings.len() as u64)
                    .line(),
            ),
            _ => {
                self.out.text(&format!(
                    "restored {shown} (SQLite file): {} statements skipped\n",
                    warnings.len()
                ));
                for warning in &warnings {
                    self.out
                        .text(&format!("skipped: {}\n", render::text(warning.as_bytes())));
                }
            }
        }
        self.close(db)?;
        Ok(Exit::Done)
    }
}
