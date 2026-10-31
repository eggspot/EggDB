use super::Console;
use super::args::{ColorChoice, CommandName, Format, Request};
use super::byte_argument::ByteArgument;
use super::failure::{Exit, Failure, code_for_error};
use super::output::{Output, Style};
use super::render::{self, JsonObject, LineBreaks};
use super::reports::{self, Damage};
use super::ui;
use eggdb::{Database, Error, ErrorKind, Options};
use eggdb_storage::{BTree, PageSize, StorageError};
use std::io::{Read, Write};
use std::ops::Bound;

mod audit;
mod backup;
mod collection;
mod compact;
mod copy_io;
mod csv_import;
mod expiry;
mod explain;
mod mcp;
mod parquet_io;
mod restore;
mod restore_aof;
mod restore_mongodump;
mod row;
mod follow;
mod follow_sql_server;
mod serve;
mod serve_mcp;
mod serve_jobs;
mod serve_mongo;
mod serve_resp;
mod serve_metrics;
mod serve_tls;
mod shell;
mod shell_copy;
pub(super) mod sql;
mod table;
mod transfer;

const DEFAULT_CACHE_BYTES: usize = 16 << 20;
const CACHE_HEADROOM_BYTES: usize = 1 << 20;
const SCAN_VALUE_SHOWN_BYTES: usize = 128;
const EXPIRY_KEY_OVERHEAD_BYTES: usize = 14;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    ReadOnly,
    Write { create: bool },
}

enum Opened<'a> {
    Owned(Box<Database>),
    Shared(&'a Database),
}

impl std::ops::Deref for Opened<'_> {
    type Target = Database;

    fn deref(&self) -> &Database {
        match self {
            Self::Owned(db) => db,
            Self::Shared(db) => db,
        }
    }
}

struct Session<'a> {
    shared: Option<&'a Database>,
    request: Request,
    file: String,
    file_display: String,
    format: Format,
    out: Output<'a>,
    messages: Output<'a>,
    stdin: &'a mut dyn Read,
    stdin_is_terminal: bool,
    stdin_taken: bool,
    stderr_is_terminal: bool,
}

pub(crate) fn default_format(command: CommandName, stdout_is_terminal: bool) -> Format {
    match (stdout_is_terminal, command) {
        (true, _) | (false, CommandName::Sql) => Format::Text,
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
    if request.command == CommandName::Shell {
        return shell::run(request, console);
    }
    if request.command == CommandName::Mcp {
        return mcp::run(request, console);
    }
    execute_on(request, console, None)
}

fn session_for<'a>(
    request: Request,
    console: &'a mut Console<'_>,
    shared: Option<&'a Database>,
) -> Session<'a> {
    let terminal = console.terminal;
    let format = request
        .format
        .unwrap_or_else(|| default_format(request.command, terminal.stdout));
    let stdout_color = format == Format::Text && uses_color(request.color, terminal.stdout_color);
    let stderr_color = uses_color(request.color, terminal.stderr_color);
    let file_display = request.file.display().to_string();
    Session {
        shared,
        file: render::text(file_display.as_bytes()),
        file_display,
        request,
        format,
        out: Output::new(&mut *console.stdout, stdout_color),
        messages: Output::new(&mut *console.stderr, stderr_color),
        stdin: &mut *console.stdin,
        stdin_is_terminal: terminal.stdin,
        stdin_taken: false,
        stderr_is_terminal: terminal.stderr,
    }
}

fn execute_on(
    request: Request,
    console: &mut Console<'_>,
    shared: Option<&Database>,
) -> Result<Exit, Failure> {
    let terminal = console.terminal;
    if request.command == CommandName::Ui && !(terminal.stdin && terminal.stdout) {
        return Err(Failure::usage(
            "eggdb ui needs a terminal; use eggdb get/scan instead",
            Some(CommandName::Ui),
        ));
    }
    let mut session = session_for(request, console, shared);
    let result = match session.request.command {
        CommandName::Create => session.create(),
        CommandName::Put => session.put(),
        CommandName::Get => session.get(),
        CommandName::Del => session.del(),
        CommandName::Scan => session.scan(),
        CommandName::Check => session.check(),
        CommandName::Stats => session.stats(),
        CommandName::Backup => session.backup(),
        CommandName::Compact => session.compact(),
        CommandName::Ui => session.ui(),
        CommandName::Export => session.export(),
        CommandName::Import => session.import(),
        CommandName::Sql => session.sql(),
        CommandName::Serve => session.serve(),
        CommandName::Restore => session.restore(),
        CommandName::Follow => session.follow(),
        CommandName::Shell => Err(Failure::usage(
            "eggdb shell does not run inside itself",
            Some(CommandName::Shell),
        )),
        CommandName::Mcp => Err(Failure::usage(
            "eggdb mcp runs on its own, with the agent's client on stdin and stdout",
            Some(CommandName::Mcp),
        )),
        CommandName::Audit => session.audit(),
        CommandName::Collection => session.collection_command(),
        CommandName::Ttl => session.ttl(),
        CommandName::Expire => session.expire(),
        CommandName::Persist => session.persist(),
        CommandName::Incr | CommandName::Decr => session.counter(),
        CommandName::Cas => session.compare_and_set(),
        CommandName::Explain => session.explain(),
        CommandName::Table => session.table_command(),
        CommandName::Row => session.row_list(),
    };
    let flushed = session.out.finish();
    let _ = session.messages.finish();
    let exit = result?;
    flushed?;
    Ok(exit)
}

impl<'a> Session<'a> {
    fn create(&mut self) -> Result<Exit, Failure> {
        if std::fs::symlink_metadata(&self.request.file).is_ok() {
            return Err(Failure::coded(
                "already_exists",
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
        if let ByteArgument::File(path) = &value
            && self.request.collection.is_none()
            && self.request.ttl.is_none()
        {
            let path = path.clone();
            return self.put_streamed(&key, &path);
        }
        let value = self.resolve(Some(value))?;
        if self.request.create
            && self.request.collection.is_none()
            && std::fs::symlink_metadata(&self.request.file).is_err()
        {
            self.refuse_what_a_new_file_cannot_store(&key, value.len(), self.request.ttl.is_some())?;
        }
        let cache = cache_for_value(value.len());
        let db = self.open(
            Access::Write {
                create: self.request.create,
            },
            Some(cache),
        )?;
        let ttl = self.request.ttl;
        let stored = match (&self.request.collection, ttl) {
            (Some(name), _) => db.write(|transaction| {
                if self.request.create {
                    transaction.create_collection(name)?;
                }
                let mut collection = transaction.collection(name)?;
                match ttl {
                    Some(ttl) => collection.put_with_expiry(&key, &value, ttl),
                    None => collection.put(&key, &value),
                }
            }),
            (None, Some(ttl)) => db.put_with_expiry(&key, &value, ttl),
            (None, None) => db.put(&key, &value),
        };
        stored.map_err(|error| {
            self.key_space_failure(
                &error,
                &format!("cannot store {} in {}", render::text(&key), self.file),
            )
        })?;
        self.close(db)?;
        if self.format == Format::Json {
            let object = JsonObject::new()
                .bytes("key", &key)
                .boolean("stored", true);
            let object = match ttl {
                Some(ttl) => expiry::with_time_to_live(object, Some(ttl)),
                None => object,
            };
            self.out.text(&object.line());
        }
        Ok(Exit::Done)
    }

    fn put_streamed(&mut self, key: &[u8], path: &std::path::Path) -> Result<Exit, Failure> {
        let unreadable =
            |error: std::io::Error| Failure::usage(format!("cannot read @{}: {error}", path.display()), Some(CommandName::Put));
        let mut source = std::fs::File::open(path).map_err(unreadable)?;
        let len = source.metadata().map_err(unreadable)?.len();
        let value_bytes = usize::try_from(len).unwrap_or(usize::MAX);
        if self.request.create && std::fs::symlink_metadata(&self.request.file).is_err() {
            self.refuse_what_a_new_file_cannot_store(key, value_bytes, false)?;
        }
        let db = self.open(
            Access::Write {
                create: self.request.create,
            },
            Some(cache_for_value(value_bytes)),
        )?;
        db.write(|transaction| transaction.put_reader(key, len, &mut source))
            .map_err(|error| {
                self.key_space_failure(
                    &error,
                    &format!("cannot store {} in {}", render::text(key), self.file),
                )
            })?;
        self.close(db)?;
        if self.format == Format::Json {
            self.out.text(
                &JsonObject::new()
                    .bytes("key", key)
                    .boolean("stored", true)
                    .line(),
            );
        }
        Ok(Exit::Done)
    }

    fn refuse_what_a_new_file_cannot_store(
        &self,
        key: &[u8],
        value_bytes: usize,
        expires: bool,
    ) -> Result<(), Failure> {
        let deadline_bytes = if expires { EXPIRY_KEY_OVERHEAD_BYTES } else { 0 };
        let max_key = BTree::max_key_bytes(PageSize::DEFAULT).saturating_sub(deadline_bytes);
        let refusal = if key.len() > max_key {
            StorageError::KeyTooLarge {
                bytes: key.len(),
                max: max_key,
            }
        } else if value_bytes > BTree::MAX_VALUE_BYTES {
            StorageError::ValueTooLarge {
                bytes: value_bytes,
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
        if self.format == Format::Raw && self.request.collection.is_none() {
            return self.get_streamed(&db, &key);
        }
        let found = match &self.request.collection {
            Some(name) => db.collection(name).get(&key),
            None => db.get(&key),
        };
        let found = found.map_err(|error| {
            self.key_space_failure(
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

    fn get_streamed(&mut self, db: &Database, key: &[u8]) -> Result<Exit, Failure> {
        let context = format!("cannot read {} from {}", render::text(key), self.file);
        let transaction = db
            .begin_read()
            .map_err(|error| self.key_space_failure(&error, &context))?;
        let found = transaction
            .get_reader(key)
            .map_err(|error| self.key_space_failure(&error, &context))?;
        let Some(mut reader) = found else {
            self.error_message(&format!("key not found: {}", render::text(key)));
            return Ok(Exit::NotFound);
        };
        let mut buffer = vec![0u8; STREAM_CHUNK_BYTES];
        loop {
            let read = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) => {
                    let failure = match error.into_inner().map(|inner| inner.downcast::<Error>()) {
                        Some(Ok(error)) => self.key_space_failure(&error, &context),
                        _ => Failure::database(context, Exit::DatabaseError),
                    };
                    return Err(failure);
                }
            };
            self.out.write_through(&buffer[..read])?;
        }
        Ok(Exit::Done)
    }

    fn del(&mut self) -> Result<Exit, Failure> {
        let key = self.resolve(self.request.key.clone())?;
        let db = self.open(Access::Write { create: false }, None)?;
        let deleted = match &self.request.collection {
            Some(name) => db.collection(name).delete(&key),
            None => db.delete(&key),
        };
        let deleted = deleted.map_err(|error| {
            self.key_space_failure(
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
        let collection = match &self.request.collection {
            Some(name) => Some(transaction.collection(name).map_err(|error| {
                self.key_space_failure(&error, &format!("cannot scan {}", self.file))
            })?),
            None => None,
        };
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
        let mut scan = match (&collection, has_prefix) {
            (Some(collection), true) => collection.scan_prefix(&prefix),
            (Some(collection), false) => collection.scan((start, end)),
            (None, true) => transaction.scan_prefix(&prefix),
            (None, false) => transaction.scan((start, end)),
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
        let db = match self.open_shared_or(Access::ReadOnly) {
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

    fn key_space_failure(&self, error: &Error, context: &str) -> Failure {
        match (&self.request.collection, error.kind()) {
            (Some(name), ErrorKind::CollectionNotFound) => {
                missing_collection(&self.file, name, context, self.request.command)
            }
            _ => operation_failure(error, context),
        }
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

    fn ui(&mut self) -> Result<Exit, Failure> {
        let access = if self.request.read_only {
            Access::ReadOnly
        } else {
            Access::Write { create: false }
        };
        let options = self
            .database_options(access, None)
            .write_timeout(ui::WRITE_TIMEOUT);
        let db = Database::open_with(&self.request.file, options.clone())
            .map_err(|error| self.ui_open_failure(&error, access))?;
        let capabilities = eggdb_ui::detect_capabilities(eggdb_ui::Fallbacks {
            mono: self.request.mono || self.request.color == ColorChoice::Never,
            ascii: self.request.ascii,
        });
        let mut terminal = match eggdb_ui::OsTerminal::open(capabilities) {
            Ok(terminal) => terminal,
            Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {
                return Err(Failure::usage(
                    "eggdb ui needs a terminal; use eggdb get/scan instead",
                    Some(CommandName::Ui),
                ));
            }
            Err(error) => {
                return Err(Failure::database(
                    format!("cannot use the terminal: {error}"),
                    Exit::DatabaseError,
                ));
            }
        };
        let size = eggdb_ui::Terminal::size(&terminal);
        let source = ui::DatabaseSource::new(db, self.file_display.clone(), options);
        let mut screen = eggdb_ui::Ui::new(source, eggdb_ui::UiOptions { capabilities }, size);
        let ran = eggdb_ui::run(&mut terminal, &mut screen);
        drop(terminal);
        ran.map_err(|error| {
            Failure::database(format!("the terminal failed: {error}"), Exit::DatabaseError)
        })?;
        if let Some(db) = screen.into_source().into_database() {
            self.close_database(db)?;
        }
        Ok(Exit::Done)
    }

    fn ui_open_failure(&self, error: &Error, access: Access) -> Failure {
        let file = &self.file;
        match (error.kind(), access) {
            (ErrorKind::Locked, Access::Write { .. }) => Failure::database(
                format!(
                    "cannot open {file}: another program has it open. Close it before editing, or browse it with `eggdb ui --read-only {file}`, which works beside other readers.\n{}",
                    mcp_may_hold(file)
                ),
                Exit::Busy,
            ),
            _ => self.open_failure(error, access),
        }
    }

    fn open(
        &self,
        access: Access,
        cache_for_command: Option<usize>,
    ) -> Result<Opened<'a>, Failure> {
        match self.shared {
            Some(db) => Ok(Opened::Shared(db)),
            None => self
                .open_database(access, cache_for_command)
                .map(|db| Opened::Owned(Box::new(db)))
                .map_err(|error| self.open_failure(&error, access)),
        }
    }

    fn open_shared_or(&self, access: Access) -> Result<Opened<'a>, Error> {
        match self.shared {
            Some(db) => Ok(Opened::Shared(db)),
            None => self
                .open_database(access, None)
                .map(|db| Opened::Owned(Box::new(db))),
        }
    }

    fn open_database(
        &self,
        access: Access,
        cache_for_command: Option<usize>,
    ) -> Result<Database, Error> {
        let options = self.database_options(access, cache_for_command);
        if self.request.file.as_os_str() == IN_MEMORY_FILE {
            return Database::in_memory_with(options.read_only(false));
        }
        if super::sqlite_import::is_sqlite_file(&self.request.file) {
            let db = Database::in_memory_with(options.read_only(false))?;
            let warnings = super::sqlite_import::import(&db, &self.request.file)?;
            let mut stderr = std::io::stderr().lock();
            let _ = writeln!(
                stderr,
                "eggdb: {} is an SQLite file; EggDB read it into memory, and changes are not written back to it",
                self.request.file.display()
            );
            for warning in warnings {
                let _ = writeln!(stderr, "eggdb: {warning}");
            }
            return Ok(db);
        }
        Database::open_with(&self.request.file, options)
    }

    fn database_options(&self, access: Access, cache_for_command: Option<usize>) -> Options {
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
        if let Some(key) = &self.request.encryption_key {
            options = options.encryption_key(key.0);
        }
        options
    }

    fn close(&self, db: Opened<'_>) -> Result<(), Failure> {
        match db {
            Opened::Owned(db) => self.close_database(*db),
            Opened::Shared(_) => Ok(()),
        }
    }

    fn close_database(&self, db: Database) -> Result<(), Failure> {
        db.close()
            .map_err(|error| operation_failure(&error, &format!("cannot close {}", self.file)))
    }

    fn open_failure(&self, error: &Error, access: Access) -> Failure {
        open_failure(&self.file, &self.file_display, error, access)
    }
}

fn open_failure(file: &str, file_display: &str, error: &Error, access: Access) -> Failure {
    let creates = matches!(access, Access::Write { create: true });
    match error.kind() {
        ErrorKind::NotFound if !creates => match access {
            Access::ReadOnly => Failure::coded(
                "not_found",
                format!(
                    "cannot open {file}: no such file. Check the path, or create it with `eggdb create {file}`"
                ),
                Exit::DatabaseError,
            ),
            Access::Write { .. } => Failure::Database {
                message: format!(
                    "cannot open {file}: no such file. Create it with `eggdb create {file}`, or pass --create to put, sql or serve."
                ),
                try_command: Some(format!("eggdb create {file}")),
                exit: Exit::DatabaseError,
                code: "not_found",
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
                    "cannot open {file}: another program has it open. Close it before writing, or read with `eggdb get`, `scan`, `check` or `stats`, which work alongside readers.\n{}",
                    mcp_may_hold(file)
                ),
                Exit::Busy,
            ),
        },
        ErrorKind::NotADatabase => Failure::coded(
            "not_a_database",
            format!(
                "cannot open {file}: not an EggDB database. If it is a database of another kind, EggDB restores its backups in a later release (docs/roadmap.md)."
            ),
            Exit::DatabaseError,
        ),
        ErrorKind::Incompatible => Failure::coded(
            "incompatible",
            format!(
                "cannot open {file}: it was written by a newer EggDB ({}). Upgrade eggdb from https://github.com/eggspot/EggDB/releases.",
                incompatibility(error, file_display)
            ),
            Exit::DatabaseError,
        ),
        kind => Failure::coded(
            code_for_error(kind),
            format!(
                "cannot open {file}: {}",
                without_open_prefix(&error.to_string(), file_display)
            ),
            Exit::for_error(kind),
        ),
    }
}

fn missing_collection(file: &str, name: &str, context: &str, command: CommandName) -> Failure {
    let create = if command == CommandName::Put {
        ", or pass --create to put"
    } else {
        ""
    };
    Failure::Database {
        message: format!(
            "{context}: no collection named {name}. Add it with `eggdb collection add {file} {name}`{create}."
        ),
        try_command: Some(format!("eggdb collection add {file} {name}")),
        exit: Exit::NotFound,
        code: "collection_not_found",
    }
}

fn mcp_may_hold(file: &str) -> String {
    format!(
        "  it may be an AI agent's call through eggdb mcp, which holds the file for up to its time limit; run the command again, or see `eggdb audit {file}`."
    )
}

const STREAM_CHUNK_BYTES: usize = 64 * 1024;
pub(crate) const IN_MEMORY_FILE: &str = ":memory:";

fn cache_for_value(value_bytes: usize) -> usize {
    let needed = value_bytes
        .saturating_add(value_bytes / 64)
        .saturating_add(CACHE_HEADROOM_BYTES);
    needed.max(DEFAULT_CACHE_BYTES)
}

fn operation_failure(error: &Error, context: &str) -> Failure {
    Failure::coded(
        code_for_error(error.kind()),
        format!("{context}: {error}"),
        Exit::for_error(error.kind()),
    )
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
    fn a_pipe_gets_json_except_get_which_gets_raw_bytes_and_sql_which_keeps_its_text() {
        assert_eq!(default_format(CommandName::Get, true), Format::Text);
        assert_eq!(default_format(CommandName::Scan, true), Format::Text);
        assert_eq!(default_format(CommandName::Get, false), Format::Raw);
        assert_eq!(default_format(CommandName::Scan, false), Format::Json);
        assert_eq!(default_format(CommandName::Check, false), Format::Json);
        assert_eq!(default_format(CommandName::Sql, false), Format::Text);
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
