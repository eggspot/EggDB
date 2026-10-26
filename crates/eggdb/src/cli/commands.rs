use super::Console;
use super::args::{ColorChoice, CommandName, Format, Request};
use super::byte_argument::ByteArgument;
use super::failure::{Exit, Failure};
use super::output::{Output, Style};
use super::render::{self, JsonObject, LineBreaks};
use super::reports::{self, Damage};
use eggdb::{Database, Error, ErrorKind, Options};
use eggdb_storage::{BTree, PageSize, StorageError};
use std::io::Read;
use std::ops::Bound;

const DEFAULT_CACHE_BYTES: usize = 16 << 20;
const CACHE_HEADROOM_BYTES: usize = 1 << 20;
const SCAN_VALUE_SHOWN_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    ReadOnly,
    Write { create: bool },
}

struct Session<'a> {
    request: Request,
    file: String,
    file_display: String,
    format: Format,
    out: Output<'a>,
    messages: Output<'a>,
    stdin: &'a mut dyn Read,
    stdin_is_terminal: bool,
    stdin_taken: bool,
}

pub(crate) fn default_format(command: CommandName, stdout_is_terminal: bool) -> Format {
    match (stdout_is_terminal, command) {
        (true, _) => Format::Text,
        (false, CommandName::Get) => Format::Raw,
        (false, _) => Format::Json,
    }
}

pub(crate) fn uses_color(choice: ColorChoice, terminal_wants_color: bool) -> bool {
    match choice {
        ColorChoice::Auto => terminal_wants_color,
        ColorChoice::Always => true,
        ColorChoice::Never => false,
    }
}

pub(crate) fn execute(request: Request, console: &mut Console<'_>) -> Result<Exit, Failure> {
    let terminal = console.terminal;
    let format = request
        .format
        .unwrap_or_else(|| default_format(request.command, terminal.stdout));
    let stdout_color = format == Format::Text && uses_color(request.color, terminal.stdout_color);
    let stderr_color = uses_color(request.color, terminal.stderr_color);
    let file_display = request.file.display().to_string();
    let mut session = Session {
        file: render::text(file_display.as_bytes()),
        file_display,
        request,
        format,
        out: Output::new(&mut *console.stdout, stdout_color),
        messages: Output::new(&mut *console.stderr, stderr_color),
        stdin: &mut *console.stdin,
        stdin_is_terminal: terminal.stdin,
        stdin_taken: false,
    };
    let result = match session.request.command {
        CommandName::Create => session.create(),
        CommandName::Put => session.put(),
        CommandName::Get => session.get(),
        CommandName::Del => session.del(),
        CommandName::Scan => session.scan(),
        CommandName::Check => session.check(),
        CommandName::Stats => session.stats(),
    };
    let flushed = session.out.finish();
    let _ = session.messages.finish();
    let exit = result?;
    flushed?;
    Ok(exit)
}

impl Session<'_> {
    fn create(&mut self) -> Result<Exit, Failure> {
        if std::fs::symlink_metadata(&self.request.file).is_ok() {
            return Err(Failure::database(
                format!(
                    "{} already exists; eggdb create never replaces a file. Use it as it is, or choose another name.",
                    self.file
                ),
                Exit::DatabaseError,
            ));
        }
        let db = self.open(Access::Write { create: true }, None)?;
        let page_size = db.page_size();
        self.close(db)?;
        match self.format {
            Format::Json => self.out.text(
                &JsonObject::new()
                    .string("file", &self.file_display)
                    .number("page_size", page_size as u64)
                    .line(),
            ),
            _ => self.out.text(&format!(
                "created {} ({} pages)\n",
                self.file,
                render::page_size(page_size)
            )),
        }
        Ok(Exit::Done)
    }

    fn put(&mut self) -> Result<Exit, Failure> {
        let key = self.resolve(self.request.key.clone())?;
        let value = match self.request.value.clone() {
            Some(value) => value,
            None if self.stdin_is_terminal => {
                return Err(Failure::usage(
                    "put needs a value: give it as an argument, as @file, or pipe it to stdin",
                    Some(CommandName::Put),
                ));
            }
            None => ByteArgument::Stdin,
        };
        let value = self.resolve(Some(value))?;
        if self.request.create && std::fs::symlink_metadata(&self.request.file).is_err() {
            self.refuse_what_a_new_file_cannot_store(&key, &value)?;
        }
        let cache = cache_for_value(value.len());
        let db = self.open(
            Access::Write {
                create: self.request.create,
            },
            Some(cache),
        )?;
        db.put(&key, &value).map_err(|error| {
            operation_failure(
                &error,
                &format!("cannot store {} in {}", render::text(&key), self.file),
            )
        })?;
        self.close(db)?;
        if self.format == Format::Json {
            self.out.text(
                &JsonObject::new()
                    .bytes("key", &key)
                    .boolean("stored", true)
                    .line(),
            );
        }
        Ok(Exit::Done)
    }

    fn refuse_what_a_new_file_cannot_store(&self, key: &[u8], value: &[u8]) -> Result<(), Failure> {
        let max_key = BTree::max_key_bytes(PageSize::DEFAULT);
        let refusal = if key.len() > max_key {
            StorageError::KeyTooLarge {
                bytes: key.len(),
                max: max_key,
            }
        } else if value.len() > BTree::MAX_VALUE_BYTES {
            StorageError::ValueTooLarge {
                bytes: value.len(),
                max: BTree::MAX_VALUE_BYTES,
            }
        } else {
            return Ok(());
        };
        Err(Failure::database(
            format!(
                "cannot store {} in {}: {refusal}",
                render::text(key),
                self.file
            ),
            Exit::DatabaseError,
        ))
    }

    fn get(&mut self) -> Result<Exit, Failure> {
        let key = self.resolve(self.request.key.clone())?;
        let db = self.open(Access::ReadOnly, None)?;
        let found = db.get(&key).map_err(|error| {
            operation_failure(
                &error,
                &format!("cannot read {} from {}", render::text(&key), self.file),
            )
        })?;
        drop(db);
        let Some(value) = found else {
            if self.format != Format::Json {
                self.error_message(&format!("key not found: {}", render::text(&key)));
            }
            return Ok(Exit::NotFound);
        };
        match self.format {
            Format::Text => {
                self.out
                    .text(&render::text_with(&value, LineBreaks::Allowed));
                self.out.text("\n");
            }
            Format::Json => self.out.text(
                &JsonObject::new()
                    .bytes("key", &key)
                    .bytes("value", &value)
                    .line(),
            ),
            Format::Hex => {
                self.out.text(&render::hex(&value));
                self.out.text("\n");
            }
            Format::Raw => self.out.write_through(&value)?,
        }
        Ok(Exit::Done)
    }

    fn del(&mut self) -> Result<Exit, Failure> {
        let key = self.resolve(self.request.key.clone())?;
        let db = self.open(Access::Write { create: false }, None)?;
        let deleted = db.delete(&key).map_err(|error| {
            operation_failure(
                &error,
                &format!("cannot delete {} from {}", render::text(&key), self.file),
            )
        })?;
        self.close(db)?;
        if self.format == Format::Json {
            self.out.text(
                &JsonObject::new()
                    .bytes("key", &key)
                    .boolean("deleted", deleted)
                    .line(),
            );
        } else if !deleted {
            self.error_message(&format!(
                "nothing to delete: {} is not in {}",
                render::text(&key),
                self.file
            ));
        }
        Ok(if deleted { Exit::Done } else { Exit::NotFound })
    }

    fn scan(&mut self) -> Result<Exit, Failure> {
        let prefix = self.resolve(self.request.scan.prefix.clone())?;
        let from = self.resolve(self.request.scan.from.clone())?;
        let to = self.resolve(self.request.scan.to.clone())?;
        let has_prefix = self.request.scan.prefix.is_some();
        let has_from = self.request.scan.from.is_some();
        let has_to = self.request.scan.to.is_some();
        let db = self.open(Access::ReadOnly, None)?;
        let scan_failure =
            |error: Error, file: &str| operation_failure(&error, &format!("cannot scan {file}"));
        let transaction = db
            .begin_read()
            .map_err(|error| scan_failure(error, &self.file))?;
        let mut scan = if has_prefix {
            transaction.scan_prefix(&prefix)
        } else {
            let start = if has_from {
                Bound::Included(from)
            } else {
                Bound::Unbounded
            };
            let end = if has_to {
                Bound::Excluded(to)
            } else {
                Bound::Unbounded
            };
            transaction.scan((start, end))
        };
        let limit = self.request.scan.limit.unwrap_or(u64::MAX);
        let mut entries = 0u64;
        while entries < limit {
            if self.request.scan.reverse {
                match scan.next_back() {
                    Some(Ok((key, value))) => self.entry(&key, &value),
                    Some(Err(error)) => return Err(scan_failure(error, &self.file)),
                    None => break,
                }
            } else {
                match scan.next_entry() {
                    Ok(Some((key, value))) => self.entry(key, value),
                    Ok(None) => break,
                    Err(error) => return Err(scan_failure(error, &self.file)),
                }
            }
            entries += 1;
            self.out.line_done()?;
        }
        if self.format == Format::Text {
            self.out.finish()?;
            self.messages.styled(Style::Dim, &render::entries(entries));
            self.messages.text("\n");
        }
        Ok(Exit::Done)
    }

    fn entry(&mut self, key: &[u8], value: &[u8]) {
        let keys_only = self.request.scan.keys_only;
        match self.format {
            Format::Text => {
                self.out.styled(Style::Bold, &render::text(key));
                if !keys_only {
                    self.out.text("\t");
                    match render::text_cut(value, SCAN_VALUE_SHOWN_BYTES) {
                        Some(shown) => {
                            self.out.text(&shown);
                            self.out.styled(
                                Style::Dim,
                                &format!(" … ({} bytes)", render::count(value.len() as u64)),
                            );
                        }
                        None => self.out.text(&render::text(value)),
                    }
                }
                self.out.text("\n");
            }
            Format::Json => {
                let object = JsonObject::new().bytes("key", key);
                let object = if keys_only {
                    object
                } else {
                    object.bytes("value", value)
                };
                self.out.text(&object.line());
            }
            Format::Hex => {
                self.out.text(&render::hex(key));
                if !keys_only {
                    self.out.text("\t");
                    self.out.text(&render::hex(value));
                }
                self.out.text("\n");
            }
            Format::Raw => {
                self.out.bytes(key);
                if !keys_only {
                    self.out.text("\t");
                    self.out.bytes(value);
                }
                self.out.text("\n");
            }
        }
    }

    fn check(&mut self) -> Result<Exit, Failure> {
        let db = match self.open_database(Access::ReadOnly, None) {
            Ok(db) => db,
            Err(error) if error.kind() == ErrorKind::Corrupt => {
                return Ok(self.damaged(None, &error));
            }
            Err(error) => return Err(self.open_failure(&error, Access::ReadOnly)),
        };
        let stats = db
            .stats()
            .map_err(|error| operation_failure(&error, &format!("cannot check {}", self.file)))?;
        let report = match db.check() {
            Ok(report) => report,
            Err(error) if error.kind() == ErrorKind::Corrupt => {
                return Ok(self.damaged(Some(stats.last_commit), &error));
            }
            Err(error) => {
                return Err(operation_failure(
                    &error,
                    &format!("cannot check {}", self.file),
                ));
            }
        };
        match self.format {
            Format::Json => {
                self.out
                    .text(&reports::check_json(&self.file_display, &stats, &report))
            }
            _ => reports::check_text(&mut self.out, &self.file, &stats, &report),
        }
        Ok(Exit::Done)
    }

    fn damaged(&mut self, checked_commit: Option<eggdb::CommitStamp>, error: &Error) -> Exit {
        let problem = damage_problem(error, &self.file_display);
        let damage = Damage {
            file: &self.file,
            file_display: &self.file_display,
            checked_commit,
            problem: &problem,
        };
        match self.format {
            Format::Json => self.out.text(&reports::damage_json(&damage)),
            _ => reports::damage_text(&mut self.out, &damage),
        }
        Exit::Damaged
    }

    fn stats(&mut self) -> Result<Exit, Failure> {
        let db = self.open(Access::ReadOnly, None)?;
        let stats = db.stats().map_err(|error| {
            operation_failure(&error, &format!("cannot read the stats of {}", self.file))
        })?;
        match self.format {
            Format::Json => self
                .out
                .text(&reports::stats_json(&self.file_display, &stats)),
            _ => reports::stats_text(&mut self.out, &self.file, &stats),
        }
        Ok(Exit::Done)
    }

    fn resolve(&mut self, argument: Option<ByteArgument>) -> Result<Vec<u8>, Failure> {
        let Some(argument) = argument else {
            return Ok(Vec::new());
        };
        let command = self.request.command;
        argument
            .resolve(self.stdin, &mut self.stdin_taken)
            .map_err(|failure| match failure {
                Failure::Usage { message, .. } => Failure::usage(message, Some(command)),
                failure => failure,
            })
    }

    fn error_message(&mut self, message: &str) {
        super::error_line(&mut self.messages, message);
    }

    fn open(&self, access: Access, cache_for_command: Option<usize>) -> Result<Database, Failure> {
        self.open_database(access, cache_for_command)
            .map_err(|error| self.open_failure(&error, access))
    }

    fn open_database(
        &self,
        access: Access,
        cache_for_command: Option<usize>,
    ) -> Result<Database, Error> {
        let cache = self
            .request
            .cache_bytes
            .or(cache_for_command)
            .unwrap_or(DEFAULT_CACHE_BYTES);
        let mut options = Options::new().cache_bytes(cache);
        options = match access {
            Access::ReadOnly => options.read_only(true).create_if_missing(false),
            Access::Write { create } => options.create_if_missing(create),
        };
        if let Some(page_size) = self.request.page_size {
            options = options.page_size_bytes(page_size);
        }
        Database::open_with(&self.request.file, options)
    }

    fn close(&self, db: Database) -> Result<(), Failure> {
        db.close()
            .map_err(|error| operation_failure(&error, &format!("cannot close {}", self.file)))
    }

    fn open_failure(&self, error: &Error, access: Access) -> Failure {
        let file = &self.file;
        let creates = matches!(access, Access::Write { create: true });
        match error.kind() {
            ErrorKind::NotFound if !creates => match access {
                Access::ReadOnly => Failure::database(
                    format!(
                        "cannot open {file}: no such file. Check the path, or create it with `eggdb create {file}`"
                    ),
                    Exit::DatabaseError,
                ),
                Access::Write { .. } => Failure::Database {
                    message: format!(
                        "cannot open {file}: no such file. Create it with `eggdb create {file}`, or pass --create to put."
                    ),
                    try_command: Some(format!("eggdb create {file}")),
                    exit: Exit::DatabaseError,
                },
            },
            ErrorKind::Locked => match access {
                Access::ReadOnly => Failure::database(
                    format!(
                        "cannot open {file}: another program has it open for writing. Close it, or wait and run the command again."
                    ),
                    Exit::Busy,
                ),
                Access::Write { .. } => Failure::database(
                    format!(
                        "cannot open {file}: another program has it open. Close it before writing, or read with `eggdb get`, `scan`, `check` or `stats`, which work alongside readers."
                    ),
                    Exit::Busy,
                ),
            },
            ErrorKind::NotADatabase => Failure::database(
                format!(
                    "cannot open {file}: not an EggDB database. If it is a database of another kind, EggDB restores its backups in a later release (docs/roadmap.md)."
                ),
                Exit::DatabaseError,
            ),
            ErrorKind::Incompatible => Failure::database(
                format!(
                    "cannot open {file}: it was written by a newer EggDB ({}). Upgrade eggdb from https://github.com/eggspot/EggDB/releases.",
                    incompatibility(error, &self.file_display)
                ),
                Exit::DatabaseError,
            ),
            kind => Failure::database(
                format!(
                    "cannot open {file}: {}",
                    without_open_prefix(&error.to_string(), &self.file_display)
                ),
                Exit::for_error(kind),
            ),
        }
    }
}

fn cache_for_value(value_bytes: usize) -> usize {
    let needed = value_bytes
        .saturating_add(value_bytes / 64)
        .saturating_add(CACHE_HEADROOM_BYTES);
    needed.max(DEFAULT_CACHE_BYTES)
}

fn operation_failure(error: &Error, context: &str) -> Failure {
    Failure::database(format!("{context}: {error}"), Exit::for_error(error.kind()))
}

fn without_open_prefix(message: &str, file: &str) -> String {
    let prefix = format!("cannot open the database {file}: ");
    message.strip_prefix(&prefix).unwrap_or(message).to_owned()
}

fn incompatibility(error: &Error, file: &str) -> String {
    let message = without_open_prefix(&error.to_string(), file);
    match message.split_once("; ") {
        Some((detail, _)) => detail.to_owned(),
        None => message,
    }
}

fn damage_problem(error: &Error, file: &str) -> String {
    let message = without_open_prefix(&error.to_string(), file);
    const ADVICE: [&str; 3] = [
        "; restore from a backup or run `eggdb check`",
        "; restore from a backup",
        "; run `eggdb check`",
    ];
    ADVICE
        .iter()
        .find_map(|advice| message.strip_suffix(advice))
        .map_or(message.clone(), str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::{cache_for_value, default_format, uses_color};
    use crate::cli::args::{ColorChoice, CommandName, Format};

    #[test]
    fn a_pipe_gets_json_except_get_which_gets_raw_bytes() {
        assert_eq!(default_format(CommandName::Get, true), Format::Text);
        assert_eq!(default_format(CommandName::Scan, true), Format::Text);
        assert_eq!(default_format(CommandName::Get, false), Format::Raw);
        assert_eq!(default_format(CommandName::Scan, false), Format::Json);
        assert_eq!(default_format(CommandName::Check, false), Format::Json);
    }

    #[test]
    fn the_color_option_overrides_the_terminal() {
        assert!(uses_color(ColorChoice::Auto, true));
        assert!(!uses_color(ColorChoice::Auto, false));
        assert!(uses_color(ColorChoice::Always, false));
        assert!(!uses_color(ColorChoice::Never, true));
    }

    #[test]
    fn the_cache_fits_the_value() {
        assert_eq!(cache_for_value(0), 16 << 20);
        assert_eq!(cache_for_value(1 << 30), (1 << 30) + (1 << 24) + (1 << 20));
        assert_eq!(cache_for_value(usize::MAX), usize::MAX);
    }
}
