use super::copy_io::bulk_cache;
use super::sql::exit_for;
use super::{Access, Session, open_failure};
use crate::cli::args::{CommandName, ExportFile, Format};
use crate::cli::failure::{Exit, Failure};
use crate::cli::output::{Output, Style};
use crate::cli::render::{self, JsonObject};
use eggdb::sql::Store as DatabaseStore;
use eggdb::{Database, Durability, Options};
use eggdb_platform::{FileSystem, OsFileSystem};
use eggdb_sql::{
    Dialect, IncludeBase, IncludeOpener, IncludeRequest, RestoreOptions, RestoreProgress,
    RestoreReport, SqlError, SqlResult, SqlState,
};
use std::cell::RefCell;
use std::ffi::OsString;
use std::fs::File;
use std::io::{BufRead, BufReader, Cursor, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const STAGING_SUFFIX: &str = ".restoring";
const READ_BUFFER_BYTES: usize = 64 * 1024;
const HEAD_BYTES: usize = 64 * 1024;
const SHOWN_TABLES: usize = 20;
const SHOWN_DESCRIPTION_CHARS: usize = 60;
const PROGRESS_EVERY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Archive {
    Custom,
    Directory,
    Tar { pg_dump: bool },
    Compressed(&'static str),
    Binary { kind: BinaryKind },
    Utf16Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BinaryKind {
    TapeBackup,
    Zip,
    NulBytes,
}

const SQLSERVER_FILE_WORDS: [(&str, &str); 7] = [
    ("bak", "a SQL Server backup"),
    ("trn", "a SQL Server log backup"),
    ("bacpac", "a BACPAC package"),
    ("dacpac", "a DACPAC package"),
    ("mdf", "a SQL Server data file"),
    ("ndf", "a SQL Server data file"),
    ("ldf", "a SQL Server log file"),
];

const SCRIPT_ROUTE: &str = "Restore it into a SQL Server you run, script it from there (SSMS: Tasks, Generate Scripts, Schema and data), then restore the script (docs/migrate.md).";

const ORACLE_ROUTE: &str = "Write its DDL with impdp ... SQLFILE= and its rows with SQLcl's UNLOAD, then restore the DDL and import the CSV (docs/migrate.md#oracle).";

pub(super) struct FileIncludes {
    scripts: RefCell<Vec<Option<PathBuf>>>,
}

impl FileIncludes {
    pub(super) fn new(main: Option<PathBuf>) -> Self {
        Self {
            scripts: RefCell::new(vec![main]),
        }
    }

    fn base(&self, relative_to: IncludeBase) -> Option<PathBuf> {
        let IncludeBase::IncludingScript(index) = relative_to else {
            return None;
        };
        let scripts = self.scripts.borrow();
        let script = scripts.get(usize::try_from(index).ok()?)?.as_ref()?;
        script
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
    }
}

impl IncludeOpener for FileIncludes {
    fn open<'s>(&'s self, request: &IncludeRequest<'_>) -> SqlResult<Box<dyn BufRead + 's>> {
        let path = match self.base(request.relative_to) {
            Some(directory) => directory.join(request.name),
            None => PathBuf::from(request.name),
        };
        let failure = |what: String| {
            SqlError::new(
                SqlState::IO_ERROR,
                format!(
                    "line {}: cannot include {}: {what}",
                    request.line, request.name
                ),
            )
        };
        let file = File::open(&path).map_err(|error| failure(error.to_string()))?;
        let metadata = file
            .metadata()
            .map_err(|error| failure(error.to_string()))?;
        if !metadata.is_file() {
            return Err(failure(
                "it is not a regular file (EggDB includes only script files)".to_owned(),
            ));
        }
        let mut scripts = self.scripts.borrow_mut();
        if let Ok(index) = usize::try_from(request.script) {
            if scripts.len() <= index {
                scripts.resize(index + 1, None);
            }
            scripts[index] = Some(path);
        }
        Ok(Box::new(BufReader::with_capacity(READ_BUFFER_BYTES, file)))
    }
}

struct Outcome<'r> {
    report: &'r RestoreReport,
    file: &'r str,
    file_display: &'r str,
    dump_shown: &'r str,
    dump_display: &'r str,
    dialect: Dialect,
    created: bool,
    left_nothing: bool,
    dry_run: bool,
    elapsed: Duration,
}

impl Session<'_> {
    pub(super) fn restore(&mut self) -> Result<Exit, Failure> {
        let usage = |message: String| Failure::usage(message, Some(CommandName::Restore));
        let dump = self.request.restore.dump.clone();
        let (path, dump_shown, dump_display) = match dump {
            Some(ExportFile::Path(path)) => {
                let display = path.display().to_string();
                let shown = render::text(display.as_bytes());
                (Some(path), shown, display)
            }
            Some(ExportFile::Standard) | None if self.stdin_is_terminal => {
                return Err(usage(
                    "restore needs a dump: give the file as the second argument, or pipe it to stdin"
                        .to_owned(),
                ));
            }
            Some(ExportFile::Standard) | None => (None, "stdin".to_owned(), "-".to_owned()),
        };
        if let Some(path) = &path
            && path.extension().is_some_and(|extension| extension == "aof")
        {
            return self.restore_aof(path);
        }
        if let Some(path) = &path
            && let Some(layout) = super::restore_mongodump::layout_of(path)
        {
            return self.restore_mongodump(path, layout);
        }
        let mut file = None;
        let mut size = 0;
        if let Some(path) = &path {
            if path.is_dir() {
                if path.join("toc.dat").is_file() {
                    return Err(archive_failure(
                        Archive::Directory,
                        &dump_shown,
                        true,
                        None,
                        &self.file,
                        self.request.dialect,
                    ));
                }
                return Err(usage(format!(
                    "cannot restore {dump_shown}: it is a directory, not a dump"
                )));
            }
            let opened = File::open(path)
                .map_err(|error| usage(format!("cannot read {dump_shown}: {error}")))?;
            size = opened
                .metadata()
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            file = Some(opened);
        }
        let raw: &mut dyn Read = match &mut file {
            Some(file) => file,
            None => &mut *self.stdin,
        };
        let head =
            read_head(raw).map_err(|error| usage(format!("cannot read {dump_shown}: {error}")))?;
        let dialect = self.request.dialect;
        if let Some(archive) = archive_of(&head, dialect) {
            let extension = path
                .as_ref()
                .and_then(|path| path.extension())
                .and_then(|extension| extension.to_str())
                .map(str::to_ascii_lowercase);
            return Err(archive_failure(
                archive,
                &dump_shown,
                path.is_some(),
                extension.as_deref(),
                &self.file,
                dialect,
            ));
        }
        let mut input = BufReader::with_capacity(READ_BUFFER_BYTES, Cursor::new(head).chain(raw));
        let target = self.request.file.clone();
        let exists = std::fs::symlink_metadata(&target).is_ok();
        let dry_run = self.request.dry_run;
        if !exists && !self.request.create {
            let flag = if dry_run { " --dry-run" } else { "" };
            return Err(Failure::Database {
                message: format!(
                    "cannot open {}: no such file. Pass --create to restore into a new file.",
                    self.file
                ),
                try_command: Some(format!(
                    "eggdb restore --create {} {}{flag}",
                    self.file, dump_shown
                )),
                exit: Exit::DatabaseError,
                code: "not_found",
            });
        }
        let staging = (!exists || dry_run).then(|| staging_path(&target));
        if let Some(staging) = &staging
            && std::fs::symlink_metadata(staging).is_ok()
        {
            let staging_shown = render::text(staging.display().to_string().as_bytes());
            if exists {
                return Err(Failure::coded(
                    "already_exists",
                    format!(
                        "cannot copy {} for the dry run: {staging_shown} exists, left by a restore that did not finish or one still running. Remove it and try again.",
                        self.file
                    ),
                    Exit::DatabaseError,
                ));
            }
            return Err(Failure::database(
                format!(
                    "cannot create {}: {staging_shown} exists, left by a restore that did not finish or one still running. Remove it and restore again.",
                    self.file
                ),
                Exit::DatabaseError,
            ));
        }
        if let Some(staging) = staging.as_ref().filter(|_| exists) {
            copy_for_dry_run(
                &self.file,
                &self.file_display,
                &target,
                self.request.cache_bytes,
                staging,
            )?;
        }
        let open_path = staging.clone().unwrap_or_else(|| target.clone());
        let cache = self.request.cache_bytes.unwrap_or_else(|| bulk_cache(size));
        let options = Options::new()
            .cache_bytes(cache)
            .durability(Durability::Normal)
            .create_if_missing(!exists);
        let db = Database::open_with(&open_path, options).map_err(|error| {
            if exists && let Some(staging) = &staging {
                let _ = std::fs::remove_file(staging);
            }
            let display = open_path.display().to_string();
            open_failure(
                &render::text(display.as_bytes()),
                &display,
                &error,
                Access::Write { create: !exists },
            )
        })?;
        let started = Instant::now();
        let mut progress = Progress::new(self.stderr_is_terminal, &dump_shown);
        let includes = FileIncludes::new(path.clone());
        let defines = self.request.restore.defines.clone();
        let options = RestoreOptions {
            keep_going: self.request.restore.keep_going,
            includes: Some(&includes),
            defines: &defines,
        };
        let mut report = run_restore(
            &db,
            &mut input,
            dialect,
            options,
            &mut progress,
            &mut self.messages,
        );
        match report.scripts.first_mut() {
            Some(first) => first.clone_from(&dump_display),
            None => report.scripts.push(dump_display.clone()),
        }
        progress.clear(&mut self.messages);
        let elapsed = started.elapsed();
        let stopped = report.error.is_some();
        let closed = db.close();
        let unfinished = stopped || closed.is_err();
        let left_nothing = !exists && unfinished;
        if let Some(staging) = &staging {
            if dry_run || unfinished {
                let _ = std::fs::remove_file(staging);
            } else {
                self.move_into_place(staging, &target)?;
            }
        }
        if let Err(error) = closed {
            return Err(Failure::database(
                format!("cannot close {}: {error}", self.file),
                Exit::for_error(error.kind()),
            ));
        }
        let file = self.file.clone();
        let file_display = self.file_display.clone();
        let outcome = Outcome {
            report: &report,
            file: &file,
            file_display: &file_display,
            dump_shown: &dump_shown,
            dump_display: &dump_display,
            dialect,
            created: !exists,
            left_nothing,
            dry_run,
            elapsed,
        };
        self.print_report(&outcome)?;
        if let Some(error) = &report.error {
            error_block(&mut self.messages, &error.error);
            if dry_run {
                let after = if exists {
                    format!(
                        "A real run would leave the {} before it in {} and run nothing after it.",
                        statements(error.statements_before),
                        self.file
                    )
                } else {
                    "A real run would create nothing.".to_owned()
                };
                return Err(Failure::database(
                    format!(
                        "cannot restore {dump_shown} into {} (dry run): line {}: {}. {after} Fix the dump, or add --keep-going to run on and list every error.",
                        self.file,
                        render::count(error.line),
                        error.error.message()
                    ),
                    exit_for(&error.error),
                ));
            }
            let after = if left_nothing {
                format!(
                    "Nothing was created; {} does not exist. Fix the dump, or add --keep-going to run on and list every error.",
                    self.file
                )
            } else {
                format!(
                    "The {} before it are in {}; nothing after it ran. Restore a fixed dump into a new file with --create, or add --keep-going to run on and list every error.",
                    statements(error.statements_before),
                    self.file
                )
            };
            return Err(Failure::database(
                format!(
                    "cannot restore {dump_shown} into {}: line {}: {}. {after}",
                    self.file,
                    render::count(error.line),
                    error.error.message()
                ),
                exit_for(&error.error),
            ));
        }
        if report.error_count() > 0 {
            let into = match (dry_run, exists) {
                (false, _) => self.file.clone(),
                (true, true) => format!("a copy of {}", self.file),
                (true, false) => format!("a scratch file for {}", self.file),
            };
            return Err(Failure::database(
                format!(
                    "restored {dump_shown} into {into} with {} ignored (--keep-going); the report lists them.",
                    counted(report.error_count(), "error", "errors")
                ),
                Exit::DatabaseError,
            ));
        }
        Ok(Exit::Done)
    }

    fn move_into_place(&mut self, staging: &Path, target: &Path) -> Result<(), Failure> {
        let staging_shown = render::text(staging.display().to_string().as_bytes());
        if std::fs::symlink_metadata(target).is_ok() {
            return Err(Failure::database(
                format!(
                    "cannot restore into {}: a file appeared there while the restore ran. The restored database is {staging_shown}; rename it yourself.",
                    self.file
                ),
                Exit::DatabaseError,
            ));
        }
        let file_system = OsFileSystem::new();
        file_system.rename(staging, target).map_err(|error| {
            Failure::database(
                format!(
                    "cannot rename {staging_shown} to {}: {error}. The restored database is {staging_shown}.",
                    self.file
                ),
                Exit::DatabaseError,
            )
        })?;
        file_system.sync_dir(directory_of(target)).map_err(|error| {
            Failure::database(
                format!(
                    "restored into {}, but cannot sync its directory, so a power loss could still undo the rename: {error}",
                    self.file
                ),
                Exit::DatabaseError,
            )
        })
    }

    fn print_report(&mut self, outcome: &Outcome<'_>) -> Result<(), Failure> {
        let Some(path) = self.request.restore.report.clone() else {
            match self.format {
                Format::Json => self.out.text(&json_report(outcome)),
                _ => text_report(&mut self.out, outcome),
            }
            return Ok(());
        };
        let json = match self.request.format {
            Some(format) => format == Format::Json,
            None => path
                .extension()
                .is_some_and(|extension| extension == "json"),
        };
        let text = if json {
            json_report(outcome)
        } else {
            let mut bytes = Vec::new();
            let mut plain = Output::new(&mut bytes, false);
            text_report(&mut plain, outcome);
            let _ = plain.finish();
            drop(plain);
            String::from_utf8_lossy(&bytes).into_owned()
        };
        verdict_line(&mut self.out, outcome);
        std::fs::write(&path, text).map_err(|error| {
            Failure::database(
                format!(
                    "cannot write the report to {}: {error}. The restore itself finished; see the exit code.",
                    render::text(path.display().to_string().as_bytes())
                ),
                Exit::DatabaseError,
            )
        })
    }
}

fn copy_for_dry_run(
    file: &str,
    file_display: &str,
    source: &Path,
    cache_bytes: Option<usize>,
    copy: &Path,
) -> Result<(), Failure> {
    let mut options = Options::new().read_only(true).create_if_missing(false);
    if let Some(cache_bytes) = cache_bytes {
        options = options.cache_bytes(cache_bytes);
    }
    let db = Database::open_with(source, options)
        .map_err(|error| open_failure(file, file_display, &error, Access::ReadOnly))?;
    let copied = db.backup_to(copy);
    let _ = db.close();
    copied.map(|_| ()).map_err(|error| {
        let _ = std::fs::remove_file(copy);
        Failure::database(
            format!(
                "cannot copy {file} for the dry run: {error}. Free space next to the file, or check the directory's permissions."
            ),
            Exit::for_error(error.kind()),
        )
    })
}

fn run_restore(
    db: &Database,
    input: &mut dyn std::io::BufRead,
    dialect: Dialect,
    options: RestoreOptions<'_>,
    progress: &mut Progress,
    messages: &mut Output<'_>,
) -> RestoreReport {
    let store = DatabaseStore(db);
    let mut session = store.session(dialect);
    let report = eggdb_sql::restore_dump(
        &mut session,
        input,
        options,
        &mut |step: &RestoreProgress| progress.tick(messages, step),
    );
    let _ = session.close();
    report
}

fn error_block(messages: &mut Output<'_>, error: &eggdb_sql::SqlError) {
    messages.styled(Style::Red, "ERROR:");
    messages.text(&format!("  {}\n", error.message()));
    for (label, text) in [
        ("DETAIL", error.detail()),
        ("HINT", error.hint()),
        ("CONTEXT", error.context()),
    ] {
        if let Some(text) = text {
            messages.text(&format!("{label}:  {text}\n"));
        }
    }
    messages.text(&format!("SQLSTATE:  {}\n", error.state()));
}

fn read_head(input: &mut dyn Read) -> std::io::Result<Vec<u8>> {
    let mut head = vec![0; HEAD_BYTES];
    let mut filled = 0;
    while filled < HEAD_BYTES {
        match input.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    head.truncate(filled);
    Ok(head)
}

fn archive_of(head: &[u8], dialect: Dialect) -> Option<Archive> {
    if head.starts_with(b"PGDMP") {
        return Some(Archive::Custom);
    }
    if head.get(257..262) == Some(b"ustar") {
        return Some(Archive::Tar {
            pg_dump: head.starts_with(b"toc.dat\0"),
        });
    }
    if head.starts_with(&[0x1f, 0x8b]) {
        return Some(Archive::Compressed("gzip"));
    }
    if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return Some(Archive::Compressed("zstd"));
    }
    if head.starts_with(&[0x04, 0x22, 0x4d, 0x18]) {
        return Some(Archive::Compressed("lz4"));
    }
    if head.starts_with(b"TAPE") {
        return Some(Archive::Binary {
            kind: BinaryKind::TapeBackup,
        });
    }
    if head.starts_with(b"PK\x03\x04") {
        return Some(Archive::Binary {
            kind: BinaryKind::Zip,
        });
    }
    if head.starts_with(b"\xFF\xFE") || head.starts_with(b"\xFE\xFF") {
        return (dialect == Dialect::PostgreSql).then_some(Archive::Utf16Text);
    }
    if head.contains(&0) {
        return Some(Archive::Binary {
            kind: BinaryKind::NulBytes,
        });
    }
    None
}

fn sqlserver_file_words(extension: Option<&str>) -> Option<&'static str> {
    let extension = extension?;
    SQLSERVER_FILE_WORDS
        .iter()
        .find(|(known, _)| *known == extension)
        .map(|(_, words)| *words)
}

fn oracle_binary_failure(shown: &str, extension: Option<&str>, file: &str) -> Failure {
    let what = match extension {
        Some("dmp") => "an Oracle Data Pump or exp dump file",
        _ => "a binary file, perhaps an RMAN backup",
    };
    Failure::Database {
        message: format!(
            "cannot restore {shown}: it is {what}, which eggdb restore does not read. {ORACLE_ROUTE}"
        ),
        try_command: Some(format!(
            "impdp system DIRECTORY=dp DUMPFILE=<file.dmp> SCHEMAS=<schema> SQLFILE=tables.sql EXCLUDE=REF_CONSTRAINT,STATISTICS, then eggdb restore --dialect oracle --create {file} tables.sql"
        )),
        exit: Exit::Usage,
        code: crate::cli::failure::USAGE_CODE,
    }
}

fn binary_failure(
    kind: BinaryKind,
    shown: &str,
    named: bool,
    extension: Option<&str>,
    file: &str,
) -> Failure {
    let target = if named { shown } else { "<script.sql>" };
    let script_command = format!("eggdb restore --dialect sqlserver --create {file} <script.sql>");
    let words = sqlserver_file_words(extension);
    let (what, advice, try_command) = match kind {
        BinaryKind::TapeBackup => (
            format!(
                "{} (it starts with TAPE)",
                words.unwrap_or("a SQL Server backup")
            ),
            SCRIPT_ROUTE.to_owned(),
            Some(script_command),
        ),
        BinaryKind::Zip if matches!(extension, Some("bacpac" | "dacpac")) => (
            format!("{} (a zip archive)", words.unwrap_or("a BACPAC or DACPAC package")),
            "Import it into a SQL Server you run with sqlpackage /Action:Import first, script it from there (SSMS: Tasks, Generate Scripts, Schema and data), then restore the script (docs/migrate.md).".to_owned(),
            Some(script_command),
        ),
        BinaryKind::Zip => (
            "a zip archive, not a script".to_owned(),
            "Unpack it first.".to_owned(),
            Some(format!("unzip -l {target}")),
        ),
        BinaryKind::NulBytes if words.is_some() => (
            words.unwrap_or_default().to_owned(),
            "Attach it to a SQL Server you run, script it from there (SSMS: Tasks, Generate Scripts, Schema and data), then restore the script (docs/migrate.md).".to_owned(),
            Some(script_command),
        ),
        BinaryKind::NulBytes => (
            "binary data (it holds NUL bytes), not a script".to_owned(),
            "If it is a backup, restore it into the database that made it and script it from there (docs/migrate.md).".to_owned(),
            None,
        ),
    };
    Failure::Database {
        message: format!(
            "cannot restore {shown}: it is {what}, which eggdb restore does not read. {advice}"
        ),
        try_command,
        exit: Exit::Usage,
        code: crate::cli::failure::USAGE_CODE,
    }
}

fn archive_failure(
    archive: Archive,
    shown: &str,
    named: bool,
    extension: Option<&str>,
    file: &str,
    dialect: Dialect,
) -> Failure {
    let target = if named { shown } else { "<archive>" };
    let (what, try_command) = match archive {
        Archive::Binary { .. } if dialect == Dialect::Oracle => {
            return oracle_binary_failure(shown, extension, file);
        }
        Archive::Binary { kind } => return binary_failure(kind, shown, named, extension, file),
        Archive::Utf16Text => {
            let target = if named { shown } else { "<script.sql>" };
            return Failure::Database {
                message: format!(
                    "cannot restore {shown}: it is UTF-16 text, which pg_dump never writes and SSMS Generate Scripts does. Add --dialect sqlserver."
                ),
                try_command: Some(format!(
                    "eggdb restore --dialect sqlserver --create {file} {target}"
                )),
                exit: Exit::Usage,
                code: crate::cli::failure::USAGE_CODE,
            };
        }
        Archive::Custom => (
            "a pg_dump custom-format archive (it starts with PGDMP)".to_owned(),
            format!("pg_restore -f dump.sql {target}"),
        ),
        Archive::Directory => (
            "a pg_dump directory-format archive (it holds toc.dat)".to_owned(),
            format!("pg_restore -f dump.sql {target}"),
        ),
        Archive::Tar { pg_dump: true } => (
            "a pg_dump tar-format archive (its first entry is toc.dat)".to_owned(),
            format!("pg_restore -f dump.sql {target}"),
        ),
        Archive::Tar { pg_dump: false } => (
            "a tar archive, not a SQL dump".to_owned(),
            format!("tar -tf {target}"),
        ),
        Archive::Compressed(tool) => (
            format!("compressed with {tool}"),
            format!("{tool} -dc {target} | eggdb restore <file>"),
        ),
    };
    let advice = match archive {
        Archive::Compressed(_) => "Decompress it first, as below.",
        _ => "Convert it to SQL first (docs/migrate.md).",
    };
    Failure::Database {
        message: format!(
            "cannot restore {shown}: it is {what}, which eggdb restore does not read. {advice}"
        ),
        try_command: Some(try_command),
        exit: Exit::Usage,
        code: crate::cli::failure::USAGE_CODE,
    }
}

fn staging_path(target: &Path) -> PathBuf {
    let mut name = OsString::from(target.as_os_str());
    name.push(STAGING_SUFFIX);
    PathBuf::from(name)
}

fn directory_of(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn counted(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", render::count(n), if n == 1 { one } else { many })
}

fn statements(n: u64) -> String {
    counted(n, "statement", "statements")
}

fn plural(kind: &str, n: u64) -> String {
    if n == 1 {
        return kind.to_owned();
    }
    match kind {
        "policy" => "policies".to_owned(),
        "row-level security" | "statistics" => kind.to_owned(),
        kind if kind.ends_with('s') => kind.to_owned(),
        kind => format!("{kind}s"),
    }
}

fn seconds(elapsed: Duration) -> String {
    format!("{:.1} s", elapsed.as_secs_f64())
}

fn kinds_summary(report: &RestoreReport) -> String {
    let mut kinds: Vec<(&str, u64)> = Vec::new();
    for object in &report.skipped {
        match kinds.iter_mut().find(|(kind, _)| *kind == object.kind) {
            Some((_, count)) => *count += 1,
            None => kinds.push((object.kind.as_str(), 1)),
        }
    }
    kinds
        .iter()
        .map(|(kind, count)| format!("{} {count}", plural(kind, *count)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn ignored_total(report: &RestoreReport) -> u64 {
    let ignored = &report.ignored;
    ignored.owner_to
        + ignored.grant
        + ignored.revoke
        + ignored.set_statements
        + ignored.other.iter().map(|(_, count)| count).sum::<u64>()
        + ignored.meta_commands.len() as u64
        + ignored.meta_commands_not_listed
}

fn source_word(dialect: Dialect) -> &'static str {
    if dialect == Dialect::PostgreSql {
        "dump"
    } else {
        "script"
    }
}

fn client_commands_word(dialect: Dialect) -> &'static str {
    if dialect == Dialect::PostgreSql {
        "meta-commands"
    } else {
        "client commands"
    }
}

fn ignored_summary(report: &RestoreReport, dialect: Dialect) -> String {
    let ignored = &report.ignored;
    let mut parts = Vec::new();
    for (label, count) in [
        ("OWNER TO", ignored.owner_to),
        ("GRANT", ignored.grant),
        ("REVOKE", ignored.revoke),
        ("SET", ignored.set_statements),
    ] {
        if count > 0 {
            parts.push(format!("{label} {count}"));
        }
    }
    for (label, count) in &ignored.other {
        parts.push(format!("{label} {count}"));
    }
    let meta = ignored.meta_commands.len() as u64 + ignored.meta_commands_not_listed;
    if meta > 0 {
        parts.push(format!("{} {meta}", client_commands_word(dialect)));
    }
    parts.join(", ")
}

fn total_tables(report: &RestoreReport) -> u64 {
    report.tables + report.tables_skipped
}

fn total_rows(report: &RestoreReport) -> u64 {
    report.rows + report.rows_skipped
}

fn verdict(outcome: &Outcome<'_>) -> (&'static str, Style, String) {
    let report = outcome.report;
    let file = outcome.file;
    if let Some(error) = &report.error {
        let after = if outcome.dry_run && outcome.created {
            format!("A real run would create nothing; {file} does not exist.")
        } else if outcome.dry_run {
            format!(
                "A real run would leave the {} before it in {file} and run nothing after it.",
                statements(error.statements_before)
            )
        } else if outcome.left_nothing {
            format!("Nothing was created; {file} does not exist.")
        } else {
            format!(
                "The {} before it are in {file}; nothing after it ran.",
                statements(error.statements_before)
            )
        };
        return (
            "failed:",
            Style::Red,
            format!(
                "line {}: {}. {after}",
                render::count(error.line),
                error.error.message()
            ),
        );
    }
    if report.error_count() > 0 {
        let count = report.error_count();
        let not_restored = if report.rows_skipped > 0 || report.tables_skipped > 0 {
            format!(
                " but the {} and {} of skipped tables",
                counted(report.tables_skipped, "table", "tables"),
                counted(report.rows_skipped, "row", "rows")
            )
        } else {
            String::new()
        };
        let holds = if outcome.dry_run {
            "would hold"
        } else {
            "holds"
        };
        return (
            "errors ignored:",
            Style::Red,
            format!(
                "{} failed and {} skipped (--keep-going), listed above; {file} {holds} everything else{not_restored}.",
                statements(count),
                if count == 1 { "was" } else { "were" }
            ),
        );
    }
    let skipped = report.skipped_count();
    let consequences = report.consequence_count();
    let differences = report.difference_count();
    let source = source_word(outcome.dialect);
    let listed = if differences > 0 {
        format!(
            "{} skipped, {} and {}, listed above.",
            counted(skipped, "object", "objects"),
            counted(consequences, "consequence", "consequences"),
            counted(differences, "known difference", "known differences")
        )
    } else {
        format!(
            "{} skipped and {}, listed above.",
            counted(skipped, "object", "objects"),
            counted(consequences, "consequence", "consequences")
        )
    };
    let (are, is) = if outcome.dry_run {
        ("would be", "would be")
    } else {
        ("are", "is")
    };
    let text = if report.tables_skipped > 0 || report.rows_skipped > 0 {
        format!(
            "{} of {} tables and {} of {} rows {are} in {file}; {listed}",
            render::count(report.tables),
            render::count(total_tables(report)),
            render::count(report.rows),
            render::count(total_rows(report)),
        )
    } else if skipped > 0 || consequences > 0 || differences > 0 {
        format!("every table and row of the {source} {is} in {file}; {listed}")
    } else {
        format!("every table and row of the {source} {is} in {file}.")
    };
    ("ok:", Style::Green, text)
}

fn verdict_line(out: &mut Output<'_>, outcome: &Outcome<'_>) {
    let (label, style, text) = verdict(outcome);
    out.styled(style, label);
    out.text(" ");
    out.text(&text);
    out.text("\n");
}

fn cut(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        return text.to_owned();
    }
    let mut shown: String = text.chars().take(chars - 1).collect();
    shown.push('…');
    shown
}

fn fact(out: &mut Output<'_>, label: &str, value: &str) {
    out.text(&format!("  {label:<16}{value}\n"));
}

fn place(report: &RestoreReport, script: u32, line: u64) -> String {
    let at = if line == 0 {
        "end".to_owned()
    } else {
        format!("line {}", render::count(line))
    };
    match usize::try_from(script)
        .ok()
        .filter(|&index| index > 0)
        .and_then(|index| report.scripts.get(index))
    {
        Some(name) => format!("{} {at}", render::text(name.as_bytes())),
        None => at,
    }
}

fn listed_lines(out: &mut Output<'_>, rows: &[(String, String, String)], more: u64, what: &str) {
    let number_width = rows
        .iter()
        .map(|(place, _, _)| place.chars().count())
        .max()
        .unwrap_or(1);
    let description_width = rows
        .iter()
        .map(|(_, description, _)| description.chars().count())
        .max()
        .unwrap_or(0)
        .min(SHOWN_DESCRIPTION_CHARS);
    for (place, description, note) in rows {
        let description = cut(description, SHOWN_DESCRIPTION_CHARS);
        let fill = description_width.saturating_sub(description.chars().count());
        out.text(&format!(
            "  {place:<number_width$}  {description}{}   {note}\n",
            " ".repeat(fill)
        ));
    }
    if more > 0 {
        out.text(&format!(
            "  … and {} more {what} (the report keeps the first {})\n",
            render::count(more),
            render::count(eggdb_sql::MAX_LISTED as u64)
        ));
    }
}

fn text_report(out: &mut Output<'_>, outcome: &Outcome<'_>) {
    let report = outcome.report;
    let into = match (outcome.dry_run, outcome.created) {
        (false, _) => outcome.file.to_owned(),
        (true, false) => format!("a copy of {}, then removed the copy", outcome.file),
        (true, true) => format!("a scratch file for {}, then removed it", outcome.file),
    };
    out.text(&format!(
        "restored {} into {into} ({}, {})\n",
        outcome.dump_shown,
        statements(report.statements),
        seconds(outcome.elapsed)
    ));
    fact(out, "schemas", &render::count(report.schemas));
    fact(
        out,
        "tables",
        &format!(
            "{}, {}",
            render::count(report.tables),
            counted(report.rows, "row", "rows")
        ),
    );
    for (label, count) in [
        ("views", report.views),
        ("sequences", report.sequences),
        ("indexes", report.indexes),
        ("constraints", report.constraints),
        ("comments", report.comments),
    ] {
        fact(out, label, &render::count(count));
    }
    let skipped = report.skipped_count();
    fact(
        out,
        "skipped",
        &if skipped == 0 {
            "0".to_owned()
        } else {
            format!("{} ({})", render::count(skipped), kinds_summary(report))
        },
    );
    fact(
        out,
        "consequences",
        &render::count(report.consequence_count()),
    );
    if outcome.dialect != Dialect::PostgreSql {
        fact(
            out,
            "differences",
            &render::count(report.difference_count()),
        );
    }
    let ignored = ignored_total(report);
    fact(
        out,
        "ignored",
        &if ignored == 0 {
            "0".to_owned()
        } else {
            format!(
                "{} ({})",
                render::count(ignored),
                ignored_summary(report, outcome.dialect)
            )
        },
    );
    if !report.rows_per_table.is_empty() {
        out.text("rows per table\n");
        let mut tables: Vec<_> = report.rows_per_table.iter().collect();
        tables.sort_by(|a, b| b.rows.cmp(&a.rows).then_with(|| a.table.cmp(&b.table)));
        let name_width = tables
            .iter()
            .take(SHOWN_TABLES)
            .map(|table| table.table.chars().count())
            .max()
            .unwrap_or(0);
        let rows_width = tables
            .iter()
            .take(SHOWN_TABLES)
            .map(|table| render::count(table.rows).len())
            .max()
            .unwrap_or(0);
        for table in tables.iter().take(SHOWN_TABLES) {
            out.text(&format!(
                "  {:<name_width$}  {:>rows_width$}\n",
                table.table,
                render::count(table.rows)
            ));
        }
        let hidden = tables.len().saturating_sub(SHOWN_TABLES) as u64 + report.tables_not_listed;
        if hidden > 0 {
            let hidden_rows: u64 = tables
                .iter()
                .skip(SHOWN_TABLES)
                .map(|table| table.rows)
                .sum();
            out.text(&format!(
                "  … and {} more tables ({}); --format json lists every table\n",
                render::count(hidden),
                counted(hidden_rows, "row", "rows")
            ));
        }
    }
    if !report.skipped.is_empty() {
        out.text("skipped (not in EggDB yet; docs/roadmap.md)\n");
        let rows: Vec<(String, String, String)> = report
            .skipped
            .iter()
            .map(|object| {
                let mut description = format!("{} {}", object.kind, object.name);
                if let Some(detail) = &object.detail
                    && detail != "not supported yet"
                {
                    description.push_str(&format!(" ({detail})"));
                }
                if let Some(rows) = object.rows.filter(|&rows| rows > 0) {
                    description.push_str(&format!(", {}", counted(rows, "row", "rows")));
                }
                (
                    place(report, object.script, object.line),
                    description,
                    object.reason.clone(),
                )
            })
            .collect();
        listed_lines(out, &rows, report.skipped_not_listed, "objects");
    }
    if !report.consequences.is_empty() {
        out.text("consequences (fail only because of a skipped object)\n");
        let rows: Vec<(String, String, String)> = report
            .consequences
            .iter()
            .map(|consequence| {
                (
                    place(report, consequence.script, consequence.line),
                    consequence.statement.clone(),
                    format!("uses {}", consequence.uses),
                )
            })
            .collect();
        listed_lines(out, &rows, report.consequences_not_listed, "consequences");
    }
    if !report.differences.is_empty() {
        out.text("known differences (restored, but not as the source had it)\n");
        let rows: Vec<(String, String, String)> = report
            .differences
            .iter()
            .map(|difference| {
                (
                    place(report, difference.script, difference.line),
                    difference.object.clone(),
                    difference.difference.clone(),
                )
            })
            .collect();
        listed_lines(out, &rows, report.differences_not_listed, "differences");
    }
    let failures: Vec<(String, String, String)> = report
        .errors
        .iter()
        .chain(report.error.iter())
        .map(|error| {
            (
                place(report, error.script, error.line),
                error.statement.clone(),
                error.error.message().to_owned(),
            )
        })
        .collect();
    if !failures.is_empty() {
        out.text(if report.error.is_some() && report.errors.is_empty() {
            "error\n"
        } else {
            "errors (ignored with --keep-going)\n"
        });
        listed_lines(out, &failures, report.errors_not_listed, "errors");
    }
    if ignored > 0 {
        out.text("ignored\n");
        ignored_lines(out, report, outcome.dialect);
    }
    verdict_line(out, outcome);
}

fn ignored_lines(out: &mut Output<'_>, report: &RestoreReport, dialect: Dialect) {
    let ignored = &report.ignored;
    let mut lines: Vec<(String, String)> = Vec::new();
    if ignored.owner_to > 0 {
        lines.push((
            "OWNER TO".to_owned(),
            format!("{} (EggDB has no roles yet)", statements(ignored.owner_to)),
        ));
    }
    if ignored.grant > 0 {
        lines.push(("GRANT".to_owned(), statements(ignored.grant)));
    }
    if ignored.revoke > 0 {
        lines.push(("REVOKE".to_owned(), statements(ignored.revoke)));
    }
    if ignored.set_statements > 0 {
        lines.push((
            "SET".to_owned(),
            format!(
                "{}: {} (not EggDB parameters)",
                statements(ignored.set_statements),
                ignored.set.join(", ")
            ),
        ));
    }
    for (kind, count) in &ignored.other {
        lines.push((kind.clone(), statements(*count)));
    }
    let mut commands: Vec<(&str, u32, Vec<u64>, &str)> = Vec::new();
    for note in &ignored.meta_commands {
        match commands.iter_mut().find(|(command, script, _, reason)| {
            *command == note.command && *script == note.script && *reason == note.reason
        }) {
            Some((_, _, lines, _)) => lines.push(note.line),
            None => commands.push((
                note.command.as_str(),
                note.script,
                vec![note.line],
                note.reason.as_str(),
            )),
        }
    }
    for (command, script, numbers, reason) in commands {
        let shown: Vec<String> = numbers.iter().take(10).map(|n| render::count(*n)).collect();
        let more = if numbers.len() > 10 { ", …" } else { "" };
        let in_script = usize::try_from(script)
            .ok()
            .filter(|&index| index > 0)
            .and_then(|index| report.scripts.get(index))
            .map(|name| format!("{} ", render::text(name.as_bytes())))
            .unwrap_or_default();
        let where_ = format!(
            "{in_script}{} {}{more}",
            if numbers.len() == 1 { "line" } else { "lines" },
            shown.join(", ")
        );
        let text = if reason == "never run" {
            format!("{where_}, never run")
        } else {
            format!("{where_} ({reason})")
        };
        lines.push((command.to_owned(), text));
    }
    if ignored.meta_commands_not_listed > 0 {
        let label = if dialect == Dialect::PostgreSql {
            "\\…"
        } else {
            ":…"
        };
        lines.push((
            label.to_owned(),
            format!(
                "{} more {}, never run",
                render::count(ignored.meta_commands_not_listed),
                client_commands_word(dialect)
            ),
        ));
    }
    let width = lines
        .iter()
        .map(|(label, _)| label.chars().count())
        .max()
        .unwrap_or(0)
        .max(10);
    for (label, text) in lines {
        out.text(&format!("  {label:<width$}  {text}\n"));
    }
}

fn json_string_list(items: &[String]) -> String {
    let parts: Vec<String> = items.iter().map(|item| render::json_string(item)).collect();
    format!("[{}]", parts.join(","))
}

fn json_error(error: &eggdb_sql::RestoreError, stopped: bool) -> String {
    let mut object = JsonObject::new()
        .number("line", error.line)
        .string("statement", &error.statement)
        .string("error", error.error.message())
        .string("sqlstate", error.error.state().code())
        .number("script", u64::from(error.script));
    if stopped {
        object = object.number("committed_statements", error.statements_before);
    }
    object.object()
}

fn json_report(outcome: &Outcome<'_>) -> String {
    let report = outcome.report;
    let rows_per_table: Vec<String> = {
        let mut tables: Vec<_> = report.rows_per_table.iter().collect();
        tables.sort_by(|a, b| b.rows.cmp(&a.rows).then_with(|| a.table.cmp(&b.table)));
        tables
            .iter()
            .map(|table| {
                JsonObject::new()
                    .string("table", &table.table)
                    .number("rows", table.rows)
                    .object()
            })
            .collect()
    };
    let skipped: Vec<String> = report
        .skipped
        .iter()
        .map(|object| {
            let mut json = JsonObject::new()
                .number("line", object.line)
                .string("kind", &object.kind)
                .string("name", &object.name)
                .string("reason", &object.reason)
                .number("script", u64::from(object.script));
            if let Some(detail) = &object.detail {
                json = json.string("detail", detail);
            }
            if let Some(rows) = object.rows {
                json = json.number("rows", rows);
            }
            json.object()
        })
        .collect();
    let consequences: Vec<String> = report
        .consequences
        .iter()
        .map(|consequence| {
            JsonObject::new()
                .number("line", consequence.line)
                .string("statement", &consequence.statement)
                .string("uses", &consequence.uses)
                .string("error", consequence.error.message())
                .string("sqlstate", consequence.error.state().code())
                .number("script", u64::from(consequence.script))
                .object()
        })
        .collect();
    let ignored = &report.ignored;
    let meta_commands: Vec<String> = ignored
        .meta_commands
        .iter()
        .map(|note| {
            JsonObject::new()
                .number("line", note.line)
                .string("command", &note.command)
                .string("reason", &note.reason)
                .number("script", u64::from(note.script))
                .object()
        })
        .collect();
    let other: Vec<String> = ignored
        .other
        .iter()
        .map(|(kind, count)| {
            JsonObject::new()
                .string("kind", kind)
                .number("statements", *count)
                .object()
        })
        .collect();
    let ignored_json = JsonObject::new()
        .number("owner_to", ignored.owner_to)
        .number("grant", ignored.grant)
        .number("revoke", ignored.revoke)
        .raw("set", &json_string_list(&ignored.set))
        .number("set_statements", ignored.set_statements)
        .raw("other", &format!("[{}]", other.join(",")))
        .raw("meta_commands", &format!("[{}]", meta_commands.join(",")))
        .number("meta_commands_not_listed", ignored.meta_commands_not_listed)
        .object();
    let errors: Vec<String> = report
        .errors
        .iter()
        .map(|error| json_error(error, false))
        .collect();
    let differences: Vec<String> = report
        .differences
        .iter()
        .map(|difference| {
            JsonObject::new()
                .number("line", difference.line)
                .string("object", &difference.object)
                .string("difference", &difference.difference)
                .number("script", u64::from(difference.script))
                .object()
        })
        .collect();
    let (_, _, verdict_text) = verdict(outcome);
    let mut object = JsonObject::new()
        .boolean("ok", report.succeeded())
        .string("file", outcome.file_display)
        .string("dump", outcome.dump_display)
        .raw("scripts", &json_string_list(&report.scripts))
        .boolean("created", outcome.created && !outcome.left_nothing);
    if outcome.dry_run {
        object = object.boolean("dry_run", true);
    }
    object
        .string("dialect", outcome.dialect.name())
        .number("statements", report.statements)
        .number(
            "elapsed_ms",
            u64::try_from(outcome.elapsed.as_millis()).unwrap_or(u64::MAX),
        )
        .number("schemas", report.schemas)
        .number("tables", report.tables)
        .number("rows", report.rows)
        .number("views", report.views)
        .number("sequences", report.sequences)
        .number("indexes", report.indexes)
        .number("constraints", report.constraints)
        .number("comments", report.comments)
        .number("tables_skipped", report.tables_skipped)
        .number("rows_skipped", report.rows_skipped)
        .raw("rows_per_table", &format!("[{}]", rows_per_table.join(",")))
        .number("tables_not_listed", report.tables_not_listed)
        .raw("skipped", &format!("[{}]", skipped.join(",")))
        .number("skipped_not_listed", report.skipped_not_listed)
        .raw("consequences", &format!("[{}]", consequences.join(",")))
        .number("consequences_not_listed", report.consequences_not_listed)
        .raw("differences", &format!("[{}]", differences.join(",")))
        .number("differences_not_listed", report.differences_not_listed)
        .raw("ignored", &ignored_json)
        .raw("errors", &format!("[{}]", errors.join(",")))
        .number("errors_not_listed", report.errors_not_listed)
        .raw(
            "error",
            &report
                .error
                .as_ref()
                .map_or_else(|| "null".to_owned(), |error| json_error(error, true)),
        )
        .string("verdict", &verdict_text)
        .line()
}

struct Progress {
    enabled: bool,
    dump: String,
    last: Instant,
    width: usize,
}

impl Progress {
    fn new(enabled: bool, dump: &str) -> Self {
        Self {
            enabled,
            dump: dump.to_owned(),
            last: Instant::now(),
            width: 0,
        }
    }

    fn tick(&mut self, messages: &mut Output<'_>, step: &RestoreProgress) {
        if !self.enabled || self.last.elapsed() < PROGRESS_EVERY {
            return;
        }
        self.last = Instant::now();
        let line = format!(
            "restoring {}: line {}, {}, {}…",
            self.dump,
            render::count(step.line),
            counted(step.tables, "table", "tables"),
            counted(step.rows, "row", "rows")
        );
        let width = line.chars().count();
        messages.text("\r");
        messages.text(&line);
        messages.text(&" ".repeat(self.width.saturating_sub(width)));
        self.width = width;
        let _ = messages.finish();
    }

    fn clear(&mut self, messages: &mut Output<'_>) {
        if self.width == 0 {
            return;
        }
        messages.text("\r");
        messages.text(&" ".repeat(self.width));
        messages.text("\r");
        self.width = 0;
        let _ = messages.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::{Archive, BinaryKind, archive_of, cut, plural};
    use eggdb_sql::Dialect;

    #[test]
    fn archives_are_known_by_their_first_bytes() {
        let postgresql = |head: &[u8]| archive_of(head, Dialect::PostgreSql);
        assert_eq!(postgresql(b"PGDMP\x01\x10"), Some(Archive::Custom));
        let mut tar = vec![0u8; 512];
        tar[..8].copy_from_slice(b"toc.dat\0");
        tar[257..262].copy_from_slice(b"ustar");
        assert_eq!(postgresql(&tar), Some(Archive::Tar { pg_dump: true }));
        tar[..8].copy_from_slice(b"other\0\0\0");
        assert_eq!(postgresql(&tar), Some(Archive::Tar { pg_dump: false }));
        assert_eq!(
            postgresql(&[0x1f, 0x8b, 8, 0]),
            Some(Archive::Compressed("gzip"))
        );
        assert_eq!(postgresql(b"--\n-- PostgreSQL database dump\n"), None);
        assert_eq!(postgresql(b""), None);
        assert_eq!(postgresql(b"PGDM"), None);
    }

    #[test]
    fn sql_server_files_that_are_not_scripts_are_known_by_their_content() {
        for dialect in [Dialect::PostgreSql, Dialect::SqlServer] {
            assert_eq!(
                archive_of(b"TAPE\0\0\x03\0", dialect),
                Some(Archive::Binary {
                    kind: BinaryKind::TapeBackup
                })
            );
            assert_eq!(
                archive_of(b"PK\x03\x04\x14\0", dialect),
                Some(Archive::Binary {
                    kind: BinaryKind::Zip
                })
            );
            assert_eq!(
                archive_of(b"\x01\x0f\0\0\0\0data", dialect),
                Some(Archive::Binary {
                    kind: BinaryKind::NulBytes
                })
            );
            assert_eq!(archive_of(b"CREATE TABLE t (a int);\n", dialect), None);
        }
        let utf16 = b"\xFF\xFEU\0S\0E\0";
        assert_eq!(
            archive_of(utf16, Dialect::PostgreSql),
            Some(Archive::Utf16Text)
        );
        assert_eq!(archive_of(utf16, Dialect::SqlServer), None);
    }

    #[test]
    fn long_text_is_cut_with_an_ellipsis_and_kinds_are_pluralized() {
        assert_eq!(cut("abcdef", 4), "abc…");
        assert_eq!(cut("abc", 4), "abc");
        assert_eq!(plural("function", 2), "functions");
        assert_eq!(plural("policy", 3), "policies");
        assert_eq!(plural("materialized view", 2), "materialized views");
        assert_eq!(plural("trigger", 1), "trigger");
    }
}
