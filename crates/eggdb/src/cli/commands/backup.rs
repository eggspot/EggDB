use super::{Access, Session};
use crate::cli::args::Format;
use crate::cli::failure::{Exit, Failure};
use crate::cli::render;
use crate::cli::reports::{self, Copied};
use eggdb::{Error, ErrorKind};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Instant;

const BACKUP_STAGING_SUFFIX: &str = ".backing-up";
const WRITER_HOLDS_THE_FILE: &str = "  to copy a file while a program writes to it, use backup <copy> inside eggdb shell, or Database::backup_to in the program.";

impl Session<'_> {
    pub(super) fn backup(&mut self) -> Result<Exit, Failure> {
        let copy = self.request.copy.clone().unwrap_or_default();
        let copy_display = copy.display().to_string();
        let copy_shown = render::text(copy_display.as_bytes());
        self.refuse_backup_destination(&copy, &copy_shown)?;
        let started = Instant::now();
        let db = match self.open_shared_or(Access::ReadOnly) {
            Ok(db) => db,
            Err(error) if error.kind() == ErrorKind::Locked => {
                let failure = self.open_failure(&error, Access::ReadOnly);
                return Err(with_writer_hint(failure));
            }
            Err(error) => return Err(self.open_failure(&error, Access::ReadOnly)),
        };
        let report = db
            .backup_to(&copy)
            .map_err(|error| engine_failure(&error))?;
        let page_size = db.page_size();
        drop(db);
        let elapsed = started.elapsed();
        match self.format {
            Format::Json => {
                let copied = Copied {
                    file: &self.file_display,
                    copy: &copy_display,
                    page_size,
                    elapsed,
                };
                self.out.text(&reports::backup_json(&copied, &report));
            }
            _ => {
                let copied = Copied {
                    file: &self.file,
                    copy: &copy_shown,
                    page_size,
                    elapsed,
                };
                reports::backup_text(&mut self.out, &copied, &report);
            }
        }
        Ok(Exit::Done)
    }

    fn refuse_backup_destination(&self, copy: &Path, copy_shown: &str) -> Result<(), Failure> {
        let file = &self.file;
        if copy == self.request.file.as_path() || is_same_file(&self.request.file, copy) {
            return Err(Failure::database(
                format!("cannot back up {file} to itself. Give the copy another name."),
                Exit::DatabaseError,
            ));
        }
        if std::fs::symlink_metadata(copy).is_ok() {
            return Err(Failure::database(
                format!(
                    "cannot back up {file} to {copy_shown}: {copy_shown} already exists; eggdb backup never replaces a file. Choose another name, or remove it first."
                ),
                Exit::DatabaseError,
            ));
        }
        if std::fs::symlink_metadata(staging_path(copy)).is_ok() {
            return Err(Failure::database(
                format!(
                    "cannot back up {file} to {copy_shown}: {copy_shown}{BACKUP_STAGING_SUFFIX} exists, left by a backup that did not finish or one still running. Remove it and back up again."
                ),
                Exit::DatabaseError,
            ));
        }
        if let Some(directory) = copy
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            && !directory.is_dir()
        {
            let directory = render::text(directory.display().to_string().as_bytes());
            return Err(Failure::Database {
                message: format!(
                    "cannot back up {file} to {copy_shown}: the directory {directory} does not exist. Create it first."
                ),
                try_command: Some(format!("mkdir {directory}")),
                exit: Exit::DatabaseError,
            });
        }
        Ok(())
    }
}

pub(super) fn engine_failure(error: &Error) -> Failure {
    Failure::database(error.to_string(), Exit::for_error(error.kind()))
}

fn with_writer_hint(failure: Failure) -> Failure {
    match failure {
        Failure::Database {
            message,
            try_command,
            exit,
        } => Failure::Database {
            message: format!("{message}\n{WRITER_HOLDS_THE_FILE}"),
            try_command,
            exit,
        },
        failure => failure,
    }
}

fn staging_path(copy: &Path) -> PathBuf {
    let mut name = OsString::from(copy.as_os_str());
    name.push(BACKUP_STAGING_SUFFIX);
    PathBuf::from(name)
}

fn is_same_file(file: &Path, copy: &Path) -> bool {
    match (std::fs::canonicalize(file), std::fs::canonicalize(copy)) {
        (Ok(file), Ok(copy)) => file == copy,
        _ => false,
    }
}
