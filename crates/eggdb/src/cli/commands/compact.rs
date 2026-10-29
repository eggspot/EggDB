use super::backup::engine_failure;
use super::{Access, Session};
use crate::cli::args::Format;
use crate::cli::failure::{Exit, Failure};
use crate::cli::reports::{self, Copied};
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Instant;

const COMPACTION_STAGING_SUFFIX: &str = ".compacting";

impl Session<'_> {
    pub(super) fn compact(&mut self) -> Result<Exit, Failure> {
        if self.shared.is_some() {
            return Err(shell_holds_the_file(&self.file));
        }
        let file = &self.file;
        if std::fs::symlink_metadata(self.staging_path()).is_ok() {
            return Err(Failure::database(
                format!(
                    "cannot compact {file}: {file}{COMPACTION_STAGING_SUFFIX} exists, left by a compact that did not finish or one still running. Remove it and compact again."
                ),
                Exit::DatabaseError,
            ));
        }
        let started = Instant::now();
        let db = self
            .open_database(Access::ReadOnly, None)
            .map_err(|error| self.open_failure(&error, Access::ReadOnly))?;
        let page_size = db.page_size();
        let report = db.compact().map_err(|error| engine_failure(&error))?;
        let elapsed = started.elapsed();
        match self.format {
            Format::Json => {
                let compacted = Copied {
                    file: &self.file_display,
                    copy: &self.file_display,
                    page_size,
                    elapsed,
                };
                self.out.text(&reports::compact_json(&compacted, &report));
            }
            _ => {
                let compacted = Copied {
                    file: &self.file,
                    copy: &self.file,
                    page_size,
                    elapsed,
                };
                reports::compact_text(&mut self.out, &compacted, &report);
            }
        }
        Ok(Exit::Done)
    }

    fn staging_path(&self) -> PathBuf {
        let mut name = OsString::from(self.request.file.as_os_str());
        name.push(COMPACTION_STAGING_SUFFIX);
        PathBuf::from(name)
    }
}

pub(super) fn shell_holds_the_file(file: &str) -> Failure {
    Failure::Database {
        message: "compact needs the file to itself, and this shell holds it. Quit the shell, then run it from the command line.".to_owned(),
        try_command: Some(format!("eggdb compact {file}")),
        exit: Exit::Usage,
        code: "database_busy",
    }
}
