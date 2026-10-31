use super::byte_argument::ByteArgument;
use super::failure::Failure;
use super::render;
use eggdb_commands::{CursorList, CursorRefusal, check_cursor, decode_cursor, shown_cursor};
use std::ffi::{OsStr, OsString};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

const SMALLEST_PAGE_SIZE: usize = 4096;
const LARGEST_PAGE_SIZE: usize = 65536;
const MOST_TIME_LIMIT_SECONDS: u64 = 300;
const MOST_CALL_GAP_MS: u64 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandName {
    Create,
    Put,
    Get,
    Del,
    Scan,
    Check,
    Stats,
    Backup,
    Compact,
    Ui,
    Export,
    Import,
    Sql,
    Serve,
    Shell,
    Restore,
    Follow,
    Mcp,
    Audit,
    Collection,
    Ttl,
    Expire,
    Persist,
    Incr,
    Decr,
    Cas,
    Explain,
    Table,
    Row,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CollectionAction {
    List,
    Add,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TableAction {
    List,
    Show,
    Sample,
}

impl TableAction {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Show => "show",
            Self::Sample => "sample",
        }
    }
}

impl CommandName {
    pub(crate) const ALL: [Self; 28] = [
        Self::Create,
        Self::Put,
        Self::Get,
        Self::Del,
        Self::Scan,
        Self::Ttl,
        Self::Expire,
        Self::Persist,
        Self::Incr,
        Self::Decr,
        Self::Cas,
        Self::Check,
        Self::Stats,
        Self::Backup,
        Self::Compact,
        Self::Ui,
        Self::Export,
        Self::Import,
        Self::Sql,
        Self::Explain,
        Self::Table,
        Self::Row,
        Self::Serve,
        Self::Shell,
        Self::Restore,
        Self::Follow,
        Self::Collection,
        Self::Mcp,
        Self::Audit,
    ];

    pub(crate) fn named(name: &str) -> Option<Self> {
        Self::from_name(name.as_bytes())
    }

    fn from_name(name: &[u8]) -> Option<Self> {
        Some(match name {
            b"create" => Self::Create,
            b"put" => Self::Put,
            b"get" => Self::Get,
            b"del" | b"delete" => Self::Del,
            b"scan" => Self::Scan,
            b"check" => Self::Check,
            b"stats" => Self::Stats,
            b"backup" => Self::Backup,
            b"compact" => Self::Compact,
            b"ui" => Self::Ui,
            b"export" => Self::Export,
            b"import" => Self::Import,
            b"sql" => Self::Sql,
            b"serve" => Self::Serve,
            b"shell" => Self::Shell,
            b"restore" => Self::Restore,
            b"follow" => Self::Follow,
            b"mcp" => Self::Mcp,
            b"audit" => Self::Audit,
            b"collection" => Self::Collection,
            b"ttl" => Self::Ttl,
            b"expire" => Self::Expire,
            b"persist" => Self::Persist,
            b"incr" => Self::Incr,
            b"decr" => Self::Decr,
            b"cas" => Self::Cas,
            b"explain" => Self::Explain,
            b"table" => Self::Table,
            b"row" => Self::Row,
            _ => return None,
        })
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Put => "put",
            Self::Get => "get",
            Self::Del => "del",
            Self::Scan => "scan",
            Self::Check => "check",
            Self::Stats => "stats",
            Self::Backup => "backup",
            Self::Compact => "compact",
            Self::Ui => "ui",
            Self::Export => "export",
            Self::Import => "import",
            Self::Sql => "sql",
            Self::Serve => "serve",
            Self::Shell => "shell",
            Self::Restore => "restore",
            Self::Follow => "follow",
            Self::Mcp => "mcp",
            Self::Audit => "audit",
            Self::Collection => "collection",
            Self::Ttl => "ttl",
            Self::Expire => "expire",
            Self::Persist => "persist",
            Self::Incr => "incr",
            Self::Decr => "decr",
            Self::Cas => "cas",
            Self::Explain => "explain",
            Self::Table => "table",
            Self::Row => "row",
        }
    }

    pub(crate) fn is_noun(self) -> bool {
        matches!(self, Self::Collection | Self::Table | Self::Row)
    }

    pub(crate) fn takes_format_option(self) -> bool {
        self.takes(OptionName::Format)
    }

    fn takes(self, option: OptionName) -> bool {
        match option {
            OptionName::Cache | OptionName::KeyFile | OptionName::Help | OptionName::Version => true,
            OptionName::Color => true,
            OptionName::Format | OptionName::Json => {
                !matches!(self, Self::Ui | Self::Shell | Self::Mcp)
            }
            OptionName::Mono | OptionName::Ascii => self == Self::Ui,
            OptionName::ReadOnly => matches!(self, Self::Ui | Self::Serve | Self::Shell | Self::Sql),
            OptionName::PageSize => matches!(self, Self::Create | Self::Import),
            OptionName::Create => {
                matches!(
                    self,
                    Self::Put
                        | Self::Import
                        | Self::Sql
                        | Self::Serve
                        | Self::Shell
                        | Self::Restore
                        | Self::Follow
                )
            }
            OptionName::Listen
            | OptionName::MysqlListen
            | OptionName::TdsListen
            | OptionName::MetricsListen
            | OptionName::McpListen
            | OptionName::RespListen
            | OptionName::MemcachedListen
            | OptionName::EsListen
            | OptionName::MongoListen
            | OptionName::RespMaxMemory
            | OptionName::RespEviction
            | OptionName::McpTokenFile
            | OptionName::LogBatches
            | OptionName::User
            | OptionName::Database
            | OptionName::MaxConnections
            | OptionName::SocketDir
            | OptionName::MysqlSocket
            | OptionName::TlsCertificate
            | OptionName::AuthRules
            | OptionName::TlsKey
            | OptionName::TlsSelfSigned
            | OptionName::TlsMinVersion => self == Self::Serve,
            OptionName::PasswordFile => matches!(self, Self::Serve | Self::Follow),
            OptionName::Publication | OptionName::Poll | OptionName::Until => self == Self::Follow,
            OptionName::Command => matches!(self, Self::Sql | Self::Shell | Self::Explain),
            OptionName::Dialect => matches!(
                self,
                Self::Sql | Self::Shell | Self::Restore | Self::Import | Self::Explain
            ),
            OptionName::Define => matches!(self, Self::Restore | Self::Sql),
            OptionName::KeepGoing => matches!(self, Self::Shell | Self::Restore),
            OptionName::Report => self == Self::Restore,
            OptionName::Mode => self == Self::Shell,
            OptionName::DryRun => matches!(self, Self::Import | Self::Restore),
            OptionName::Table | OptionName::Parquet => matches!(self, Self::Import | Self::Export),
            OptionName::OnConflict
            | OptionName::Csv
            | OptionName::Header
            | OptionName::Delimiter
            | OptionName::Null
            | OptionName::RowTerminator
            | OptionName::KeepNulls => self == Self::Import,
            OptionName::Limit => matches!(self, Self::Scan | Self::Audit | Self::Table | Self::Row),
            OptionName::After => matches!(self, Self::Table | Self::Row),
            OptionName::Schema => matches!(self, Self::Table | Self::Follow),
            OptionName::Collection => matches!(
                self,
                Self::Get
                    | Self::Put
                    | Self::Del
                    | Self::Scan
                    | Self::Export
                    | Self::Import
                    | Self::Ttl
                    | Self::Expire
                    | Self::Persist
                    | Self::Incr
                    | Self::Decr
                    | Self::Cas
            ),
            OptionName::Ttl => self == Self::Put,
            OptionName::IfAbsent | OptionName::Delete => self == Self::Cas,
            OptionName::TimeLimit
            | OptionName::AllowWrite
            | OptionName::Tables
            | OptionName::CallGap => self == Self::Mcp,
            OptionName::Prefix
            | OptionName::From
            | OptionName::To
            | OptionName::Reverse
            | OptionName::KeysOnly => self == Self::Scan,
        }
    }

    fn takes_format(self, format: Format) -> bool {
        match format {
            Format::Text | Format::Json => true,
            Format::Hex | Format::Raw => matches!(self, Self::Get | Self::Scan),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    Text,
    Json,
    Hex,
    Raw,
}

impl Format {
    fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Json => "json",
            Self::Hex => "hex",
            Self::Raw => "raw",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ConflictPolicy {
    #[default]
    Error,
    Overwrite,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExportFile {
    Standard,
    Path(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ColorChoice {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OptionName {
    Format,
    Json,
    Color,
    Cache,
    KeyFile,
    Help,
    Version,
    PageSize,
    Create,
    Prefix,
    From,
    To,
    Reverse,
    Limit,
    KeysOnly,
    Mono,
    Ascii,
    ReadOnly,
    OnConflict,
    DryRun,
    Command,
    Listen,
    MysqlListen,
    TdsListen,
    MetricsListen,
    McpListen,
    RespListen,
    MemcachedListen,
    EsListen,
    MongoListen,
    RespMaxMemory,
    RespEviction,
    McpTokenFile,
    LogBatches,
    Dialect,
    User,
    PasswordFile,
    Database,
    MaxConnections,
    SocketDir,
    MysqlSocket,
    TlsCertificate,
    AuthRules,
    TlsKey,
    TlsSelfSigned,
    TlsMinVersion,
    Publication,
    Poll,
    Until,
    KeepGoing,
    Mode,
    Csv,
    Parquet,
    Table,
    Header,
    Delimiter,
    Null,
    RowTerminator,
    KeepNulls,
    Report,
    TimeLimit,
    AllowWrite,
    Tables,
    CallGap,
    Define,
    Collection,
    Ttl,
    IfAbsent,
    Delete,
    Schema,
    After,
}

impl OptionName {
    fn from_long(name: &[u8]) -> Option<Self> {
        Some(match name {
            b"format" => Self::Format,
            b"json" => Self::Json,
            b"color" => Self::Color,
            b"cache" => Self::Cache,
            b"key-file" => Self::KeyFile,
            b"help" => Self::Help,
            b"version" => Self::Version,
            b"page-size" => Self::PageSize,
            b"create" => Self::Create,
            b"prefix" => Self::Prefix,
            b"from" => Self::From,
            b"to" => Self::To,
            b"reverse" => Self::Reverse,
            b"limit" => Self::Limit,
            b"keys-only" => Self::KeysOnly,
            b"mono" => Self::Mono,
            b"ascii" => Self::Ascii,
            b"read-only" => Self::ReadOnly,
            b"on-conflict" => Self::OnConflict,
            b"dry-run" => Self::DryRun,
            b"command" => Self::Command,
            b"listen" => Self::Listen,
            b"mysql-listen" => Self::MysqlListen,
            b"tds-listen" => Self::TdsListen,
            b"metrics-listen" => Self::MetricsListen,
            b"mcp-listen" => Self::McpListen,
            b"resp-listen" => Self::RespListen,
            b"memcached-listen" => Self::MemcachedListen,
            b"es-listen" => Self::EsListen,
            b"mongo-listen" => Self::MongoListen,
            b"resp-max-memory" => Self::RespMaxMemory,
            b"resp-eviction" => Self::RespEviction,
            b"mcp-token-file" => Self::McpTokenFile,
            b"log-batches" => Self::LogBatches,
            b"dialect" => Self::Dialect,
            b"user" => Self::User,
            b"password-file" => Self::PasswordFile,
            b"database" => Self::Database,
            b"max-connections" => Self::MaxConnections,
            b"socket-dir" => Self::SocketDir,
            b"mysql-socket" => Self::MysqlSocket,
            b"tls-certificate" => Self::TlsCertificate,
            b"auth-rules" => Self::AuthRules,
            b"tls-key" => Self::TlsKey,
            b"tls-self-signed" => Self::TlsSelfSigned,
            b"tls-min-version" => Self::TlsMinVersion,
            b"publication" => Self::Publication,
            b"poll" => Self::Poll,
            b"until" => Self::Until,
            b"keep-going" => Self::KeepGoing,
            b"mode" => Self::Mode,
            b"csv" => Self::Csv,
            b"parquet" => Self::Parquet,
            b"table" => Self::Table,
            b"header" => Self::Header,
            b"delimiter" => Self::Delimiter,
            b"null" => Self::Null,
            b"row-terminator" => Self::RowTerminator,
            b"keep-nulls" => Self::KeepNulls,
            b"report" => Self::Report,
            b"time-limit" => Self::TimeLimit,
            b"allow-write" => Self::AllowWrite,
            b"tables" => Self::Tables,
            b"call-gap" => Self::CallGap,
            b"define" => Self::Define,
            b"collection" => Self::Collection,
            b"ttl" => Self::Ttl,
            b"if-absent" => Self::IfAbsent,
            b"delete" => Self::Delete,
            b"schema" => Self::Schema,
            b"after" => Self::After,
            _ => return None,
        })
    }

    fn from_short(letter: u8) -> Option<Self> {
        Some(match letter {
            b'F' => Self::Format,
            b'h' => Self::Help,
            b'V' => Self::Version,
            b'p' => Self::Prefix,
            b'r' => Self::Reverse,
            b'n' => Self::Limit,
            b'k' => Self::KeysOnly,
            b'c' => Self::Command,
            b'C' => Self::Collection,
            _ => return None,
        })
    }

    fn takes_value(self) -> bool {
        !matches!(
            self,
            Self::Help
                | Self::Json
                | Self::Version
                | Self::Create
                | Self::Reverse
                | Self::KeysOnly
                | Self::Mono
                | Self::Ascii
                | Self::ReadOnly
                | Self::DryRun
                | Self::KeepGoing
                | Self::Header
                | Self::KeepNulls
                | Self::AllowWrite
                | Self::LogBatches
                | Self::TlsSelfSigned
                | Self::IfAbsent
                | Self::Delete
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Invocation {
    Help(Option<CommandName>),
    HelpJson(Option<CommandName>, Option<&'static str>),
    NoArguments,
    Version,
    VersionJson,
    Run(Box<Request>),
}

/// A database key read from `--key-file`: 64 hexadecimal digits, 32 bytes (D187).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct EncryptionKey(pub(crate) [u8; 32]);

impl std::fmt::Debug for EncryptionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EncryptionKey(..)")
    }
}

fn read_key_file(path: &str) -> Result<EncryptionKey, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read --key-file {path}: {error}"))?;
    let digits = text.trim();
    let wrong = || format!("--key-file {path} must hold 64 hexadecimal digits (32 bytes), such as from: openssl rand -hex 32");
    if digits.len() != 64 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(wrong());
    }
    let mut key = [0u8; 32];
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&digits[2 * index..2 * index + 2], 16).map_err(|_| wrong())?;
    }
    Ok(EncryptionKey(key))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) command: CommandName,
    pub(crate) file: PathBuf,
    pub(crate) key: Option<ByteArgument>,
    pub(crate) value: Option<ByteArgument>,
    pub(crate) format: Option<Format>,
    pub(crate) color: ColorChoice,
    pub(crate) cache_bytes: Option<usize>,
    pub(crate) encryption_key: Option<EncryptionKey>,
    pub(crate) page_size: Option<usize>,
    pub(crate) create: bool,
    pub(crate) scan: ScanOptions,
    pub(crate) mono: bool,
    pub(crate) ascii: bool,
    pub(crate) read_only: bool,
    pub(crate) export_file: Option<ExportFile>,
    pub(crate) on_conflict: ConflictPolicy,
    pub(crate) dry_run: bool,
    pub(crate) sql: Option<SqlSource>,
    pub(crate) dialect: eggdb_sql::Dialect,
    pub(crate) serve: ServeOptions,
    pub(crate) shell: ShellOptions,
    pub(crate) csv: CsvOptions,
    pub(crate) restore: RestoreOptions,
    pub(crate) follow: FollowOptions,
    pub(crate) time_limit: Option<u64>,
    pub(crate) allow_write: bool,
    pub(crate) tables: Vec<String>,
    pub(crate) call_gap_ms: Option<u64>,
    pub(crate) collection: Option<String>,
    pub(crate) collection_action: Option<CollectionAction>,
    pub(crate) table_action: Option<TableAction>,
    pub(crate) table: Option<OsString>,
    pub(crate) schema: Option<String>,
    pub(crate) after: Option<String>,
    pub(crate) ttl: Option<Duration>,
    pub(crate) step: Option<u64>,
    pub(crate) swap: SwapOptions,
    pub(crate) copy: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SwapOptions {
    pub(crate) expected: Option<ByteArgument>,
    pub(crate) if_absent: bool,
    pub(crate) delete: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RestoreOptions {
    pub(crate) dump: Option<ExportFile>,
    pub(crate) report: Option<PathBuf>,
    pub(crate) keep_going: bool,
    pub(crate) defines: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FollowOptions {
    pub(crate) source: String,
    pub(crate) schemas: Vec<String>,
    pub(crate) publication: Option<String>,
    pub(crate) poll_seconds: Option<u64>,
    pub(crate) until_caught_up: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CsvOptions {
    pub(crate) source: Option<ExportFile>,
    pub(crate) table: Option<String>,
    pub(crate) header: bool,
    pub(crate) delimiter: Option<String>,
    pub(crate) null: Option<String>,
    pub(crate) row_terminator: Option<String>,
    pub(crate) keep_nulls: bool,
    pub(crate) parquet: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ShellOptions {
    pub(crate) commands: Vec<String>,
    pub(crate) keep_going: bool,
    pub(crate) mode: Option<ResultMode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResultMode {
    Table,
    Expanded,
    Csv,
}

impl ResultMode {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "table" => Self::Table,
            "expanded" => Self::Expanded,
            "csv" => Self::Csv,
            _ => return None,
        })
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::Expanded => "expanded",
            Self::Csv => "csv",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ServeOptions {
    pub(crate) listen: Option<SocketAddr>,
    pub(crate) mysql_listen: Option<SocketAddr>,
    pub(crate) tds_listen: Option<SocketAddr>,
    pub(crate) metrics_listen: Option<SocketAddr>,
    pub(crate) mcp_listen: Option<SocketAddr>,
    pub(crate) mcp_token_file: Option<PathBuf>,
    pub(crate) resp_listen: Option<SocketAddr>,
    pub(crate) memcached_listen: Option<SocketAddr>,
    pub(crate) es_listen: Option<SocketAddr>,
    pub(crate) mongo_listen: Option<SocketAddr>,
    pub(crate) resp_max_memory: Option<u64>,
    pub(crate) resp_eviction: Option<eggdb_resp::EvictionPolicy>,
    pub(crate) log_batches: bool,
    pub(crate) user: Option<String>,
    pub(crate) password_file: Option<ExportFile>,
    pub(crate) database: Option<String>,
    pub(crate) max_connections: Option<usize>,
    pub(crate) socket_dir: Option<PathBuf>,
    pub(crate) mysql_socket: Option<PathBuf>,
    pub(crate) tls_certificate: Option<PathBuf>,
    pub(crate) auth_rules: Option<PathBuf>,
    pub(crate) tls_key: Option<PathBuf>,
    pub(crate) tls_self_signed: bool,
    pub(crate) tls_min_version: Option<eggdb_tls::MinimumVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SqlSource {
    Command(String),
    Script(PathBuf),
    Stdin,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ScanOptions {
    pub(crate) prefix: Option<ByteArgument>,
    pub(crate) from: Option<ByteArgument>,
    pub(crate) to: Option<ByteArgument>,
    pub(crate) reverse: bool,
    pub(crate) limit: Option<u64>,
    pub(crate) keys_only: bool,
}

struct GivenOption<'a> {
    name: OptionName,
    spelled: String,
    value: Option<&'a OsStr>,
}

struct Tokens<'a> {
    positionals: Vec<&'a OsStr>,
    options: Vec<GivenOption<'a>>,
    unknown: Option<String>,
    missing_value: Option<String>,
}

pub(crate) fn color_choice_of(args: &[OsString]) -> ColorChoice {
    tokenize(args)
        .options
        .iter()
        .filter(|option| option.name == OptionName::Color)
        .filter_map(|option| parse_color(option.value?).ok())
        .next_back()
        .unwrap_or_default()
}

pub(crate) fn json_errors_requested(args: &[OsString]) -> bool {
    let tokens = tokenize(args);
    if !asks_for_json(&tokens) {
        return false;
    }
    let first = tokens
        .positionals
        .first()
        .map(|name| name.as_encoded_bytes());
    first
        .and_then(CommandName::from_name)
        .is_none_or(CommandName::takes_format_option)
}

fn asks_for_json(tokens: &Tokens<'_>) -> bool {
    tokens
        .options
        .iter()
        .rev()
        .find_map(|option| match option.name {
            OptionName::Json => Some(true),
            OptionName::Format => {
                Some(option.value.is_some_and(|value| value.as_encoded_bytes() == b"json"))
            }
            _ => None,
        })
        .unwrap_or(false)
}

pub(crate) fn parse(args: &[OsString]) -> Result<Invocation, Failure> {
    if args.is_empty() {
        return Ok(Invocation::NoArguments);
    }
    let tokens = tokenize(args);
    let first = tokens
        .positionals
        .first()
        .map(|name| name.as_encoded_bytes());
    let command = first.and_then(CommandName::from_name);
    let has = |name| tokens.options.iter().any(|option| option.name == name);
    let json = asks_for_json(&tokens);
    let help = |command: Option<CommandName>, verb: Option<&'static str>| {
        if json {
            Invocation::HelpJson(command, verb)
        } else {
            Invocation::Help(command)
        }
    };
    if has(OptionName::Help) {
        return match (first, command) {
            (Some(b"help" | b"version") | None, _) => Ok(help(None, None)),
            (Some(name), None) => Err(unknown_command(name)),
            (_, Some(command)) => {
                let verb = tokens
                    .positionals
                    .get(1)
                    .and_then(|verb| verb_of(command, verb));
                Ok(help(Some(command), verb))
            }
        };
    }
    let version = if json {
        Invocation::VersionJson
    } else {
        Invocation::Version
    };
    if has(OptionName::Version) {
        return Ok(version);
    }
    let Some(first) = first else {
        if json {
            return Ok(help(None, None));
        }
        return Err(Failure::usage(
            "no command given. Commands: create, put, get, del, scan, ttl, expire, persist, incr, decr, cas, check, stats, backup, compact, ui, export, import, sql, explain, table, row, serve, shell, restore, mcp, audit, collection, help, version.",
            None,
        ));
    };
    match first {
        b"help" => {
            return parse_help(&tokens.positionals[1..]).map(|(command, verb)| help(command, verb));
        }
        b"version" if tokens.positionals.len() == 1 => return Ok(version),
        b"version" => return Err(too_many_arguments(&tokens.positionals[1..], None)),
        _ => {}
    }
    let Some(command) = command else {
        return Err(unknown_command(first));
    };
    if let Some(spelled) = tokens.unknown {
        return Err(unknown_option(&spelled, command));
    }
    if let Some(spelled) = tokens.missing_value {
        return Err(Failure::usage(
            format!("{spelled} needs a value"),
            Some(command),
        ));
    }
    build_request(command, &tokens).map(|request| Invocation::Run(Box::new(request)))
}

fn tokenize(args: &[OsString]) -> Tokens<'_> {
    let mut tokens = Tokens {
        positionals: Vec::new(),
        options: Vec::new(),
        unknown: None,
        missing_value: None,
    };
    let mut rest = args.iter().map(OsString::as_os_str);
    let mut options_ended = false;
    while let Some(arg) = rest.next() {
        let bytes = arg.as_encoded_bytes();
        if options_ended || bytes == b"-" || !bytes.starts_with(b"-") {
            tokens.positionals.push(arg);
            continue;
        }
        if bytes == b"--" {
            options_ended = true;
            continue;
        }
        let (name, attached) = split_option(arg);
        let spelled = render::text(name.as_encoded_bytes());
        let known = match name.as_encoded_bytes() {
            [b'-', b'-', long @ ..] => OptionName::from_long(long),
            [b'-', letter] => OptionName::from_short(*letter),
            _ => None,
        };
        let Some(known) = known else {
            tokens.unknown.get_or_insert(spelled);
            continue;
        };
        let value = match (known.takes_value(), attached) {
            (true, Some(value)) => Some(value),
            (true, None) => {
                let next = rest.next();
                if next.is_none() {
                    tokens.missing_value.get_or_insert(spelled.clone());
                }
                next
            }
            (false, Some(_)) => {
                tokens.unknown.get_or_insert(format!("{spelled}=..."));
                continue;
            }
            (false, None) => None,
        };
        tokens.options.push(GivenOption {
            name: known,
            spelled,
            value,
        });
    }
    tokens
}

fn split_option(arg: &OsStr) -> (&OsStr, Option<&OsStr>) {
    let Some(text) = arg.to_str() else {
        return (arg, None);
    };
    match text.split_once('=') {
        Some((name, value)) if text.starts_with("--") => {
            (OsStr::new(name), Some(OsStr::new(value)))
        }
        _ => (arg, None),
    }
}

type HelpTopic = (Option<CommandName>, Option<&'static str>);

fn parse_help(arguments: &[&OsStr]) -> Result<HelpTopic, Failure> {
    let Some((name, rest)) = arguments.split_first() else {
        return Ok((None, None));
    };
    let name = name.as_encoded_bytes();
    let command = match CommandName::from_name(name) {
        Some(command) => command,
        None if !rest.is_empty() => return Err(too_many_arguments(rest, None)),
        None if name == b"help" || name == b"version" => return Ok((None, None)),
        None => return Err(unknown_command(name)),
    };
    match rest {
        [] => Ok((Some(command), None)),
        [verb, extra @ ..] if command.is_noun() => {
            let Some(verb) = verb_of(command, verb) else {
                return Err(noun_needs_a_verb(command));
            };
            if extra.is_empty() {
                Ok((Some(command), Some(verb)))
            } else {
                Err(too_many_arguments(extra, None))
            }
        }
        extra => Err(too_many_arguments(extra, None)),
    }
}

fn verb_of(noun: CommandName, verb: &OsStr) -> Option<&'static str> {
    let verb = verb.to_str()?;
    let definition = eggdb_commands::command(&[noun.name(), verb])?;
    definition.words.get(1).copied()
}

fn noun_needs_a_verb(noun: CommandName) -> Failure {
    let message = match noun {
        CommandName::Table => TABLE_NEEDS_A_VERB,
        CommandName::Row => ROW_NEEDS_A_VERB,
        _ => COLLECTION_NEEDS_A_VERB,
    };
    Failure::usage(message, Some(noun))
}

fn build_request(command: CommandName, tokens: &Tokens<'_>) -> Result<Request, Failure> {
    let usage = |message: String| Failure::usage(message, Some(command));
    let mut request = Request {
        command,
        file: PathBuf::new(),
        key: None,
        value: None,
        format: None,
        color: ColorChoice::Auto,
        cache_bytes: None,
        encryption_key: None,
        page_size: None,
        create: false,
        scan: ScanOptions::default(),
        mono: false,
        ascii: false,
        read_only: false,
        export_file: None,
        on_conflict: ConflictPolicy::Error,
        dry_run: false,
        sql: None,
        dialect: eggdb_sql::Dialect::PostgreSql,
        serve: ServeOptions::default(),
        shell: ShellOptions::default(),
        csv: CsvOptions::default(),
        restore: RestoreOptions::default(),
        follow: FollowOptions::default(),
        time_limit: None,
        allow_write: false,
        tables: Vec::new(),
        call_gap_ms: None,
        collection: None,
        collection_action: None,
        table_action: None,
        table: None,
        schema: None,
        after: None,
        ttl: None,
        step: None,
        swap: SwapOptions::default(),
        copy: None,
    };
    for option in &tokens.options {
        if !command.takes(option.name) {
            return Err(unknown_option(&option.spelled, command));
        }
        let value = option.value.unwrap_or_default();
        let byte_argument =
            || ByteArgument::parse(value).map_err(|message| Failure::usage(message, Some(command)));
        match option.name {
            OptionName::Format => {
                let format = parse_format(value).map_err(usage)?;
                if !command.takes_format(format) {
                    return Err(usage(format!(
                        "--format {} is for get and scan; eggdb {} prints text or json",
                        format.name(),
                        command.name()
                    )));
                }
                request.format = Some(format);
            }
            OptionName::Json => request.format = Some(Format::Json),
            OptionName::Color => request.color = parse_color(value).map_err(usage)?,
            OptionName::Cache => {
                request.cache_bytes = Some(parse_size(&option.spelled, value).map_err(usage)?);
            }
            OptionName::KeyFile => {
                request.encryption_key = Some(read_key_file(value).map_err(usage)?);
            }
            OptionName::PageSize => {
                request.page_size = Some(parse_page_size(&option.spelled, value).map_err(usage)?);
            }
            OptionName::Create => request.create = true,
            OptionName::Prefix => request.scan.prefix = Some(byte_argument()?),
            OptionName::From => request.scan.from = Some(byte_argument()?),
            OptionName::To => request.scan.to = Some(byte_argument()?),
            OptionName::Reverse => request.scan.reverse = true,
            OptionName::After => {}
            OptionName::Limit if command.is_noun() => {}
            OptionName::Limit => {
                request.scan.limit = Some(parse_limit(&option.spelled, value).map_err(usage)?);
            }
            OptionName::KeysOnly => request.scan.keys_only = true,
            OptionName::Mono => request.mono = true,
            OptionName::Ascii => request.ascii = true,
            OptionName::ReadOnly => request.read_only = true,
            OptionName::AllowWrite => request.allow_write = true,
            OptionName::LogBatches => request.serve.log_batches = true,
            OptionName::TlsSelfSigned => request.serve.tls_self_signed = true,
            OptionName::OnConflict => {
                request.on_conflict = parse_on_conflict(value).map_err(usage)?
            }
            OptionName::DryRun => request.dry_run = true,
            OptionName::Command if command == CommandName::Shell => {
                let text = value
                    .to_str()
                    .ok_or_else(|| usage(format!("{} must be UTF-8 text", option.spelled)))?;
                request.shell.commands.push(text.to_owned());
            }
            OptionName::KeepGoing if command == CommandName::Restore => {
                request.restore.keep_going = true;
            }
            OptionName::KeepGoing => request.shell.keep_going = true,
            OptionName::Report => request.restore.report = Some(PathBuf::from(value)),
            OptionName::Dialect if command == CommandName::Restore => {
                request.dialect = check_restore_dialect(value).map_err(usage)?;
            }
            OptionName::Dialect if command == CommandName::Import => {
                request.dialect = check_import_dialect(value).map_err(usage)?;
            }
            OptionName::Define => {
                let definition = parse_define(&option.spelled, value).map_err(usage)?;
                request.restore.defines.push(definition);
            }
            OptionName::Mode => {
                let name = value.to_str().unwrap_or_default();
                request.shell.mode = Some(ResultMode::parse(name).ok_or_else(|| {
                    usage(format!(
                        "{} takes table, expanded or csv, not '{}'",
                        option.spelled,
                        shown(value)
                    ))
                })?);
            }
            OptionName::Command => {
                if request.sql.is_some() {
                    return Err(usage(
                        "give -c once; put several statements in it, separated by semicolons"
                            .to_owned(),
                    ));
                }
                let text = value
                    .to_str()
                    .ok_or_else(|| usage(format!("{} must be UTF-8 text", option.spelled)))?;
                request.sql = Some(SqlSource::Command(text.to_owned()));
            }
            OptionName::Listen => {
                request.serve.listen =
                    Some(parse_listen(&option.spelled, value, 5432).map_err(usage)?);
            }
            OptionName::MysqlListen => {
                request.serve.mysql_listen =
                    Some(parse_listen(&option.spelled, value, 3306).map_err(usage)?);
            }
            OptionName::TdsListen => {
                request.serve.tds_listen =
                    Some(parse_listen(&option.spelled, value, 1433).map_err(usage)?);
            }
            OptionName::MetricsListen => {
                request.serve.metrics_listen =
                    Some(parse_listen(&option.spelled, value, 9187).map_err(usage)?);
            }
            OptionName::McpListen => {
                request.serve.mcp_listen =
                    Some(parse_listen(&option.spelled, value, 8765).map_err(usage)?);
            }
            OptionName::McpTokenFile => request.serve.mcp_token_file = Some(PathBuf::from(value)),
            OptionName::RespMaxMemory => {
                request.serve.resp_max_memory = Some(parse_size(&option.spelled, value).map_err(usage)? as u64);
            }
            OptionName::RespEviction => {
                let name = value.to_str().unwrap_or_default();
                request.serve.resp_eviction = Some(eggdb_resp::EvictionPolicy::named(name).ok_or_else(|| {
                    usage(format!(
                        "{} {name}: use noeviction, allkeys-lru, allkeys-lfu, allkeys-random, volatile-lru, volatile-lfu, volatile-random or volatile-ttl",
                        option.spelled
                    ))
                })?);
            }
            OptionName::MemcachedListen => {
                request.serve.memcached_listen =
                    Some(parse_listen(&option.spelled, value, 11211).map_err(usage)?);
            }
            OptionName::MongoListen => {
                request.serve.mongo_listen =
                    Some(parse_listen(&option.spelled, value, 27017).map_err(usage)?);
            }
            OptionName::EsListen => {
                request.serve.es_listen =
                    Some(parse_listen(&option.spelled, value, 9200).map_err(usage)?);
            }
            OptionName::RespListen => {
                request.serve.resp_listen =
                    Some(parse_listen(&option.spelled, value, 6379).map_err(usage)?);
            }
            OptionName::Dialect => {
                let name = value.to_str().unwrap_or_default();
                request.dialect = eggdb_sql::Dialect::named(name).ok_or_else(|| {
                    usage(format!(
                        "{} takes postgresql, mysql, sqlite, oracle or sqlserver, not '{}'",
                        option.spelled,
                        shown(value)
                    ))
                })?;
            }
            OptionName::User => {
                request.serve.user = Some(parse_name(&option.spelled, value).map_err(usage)?);
            }
            OptionName::Database => {
                request.serve.database = Some(parse_name(&option.spelled, value).map_err(usage)?);
            }
            OptionName::PasswordFile => request.serve.password_file = Some(export_file(value)),
            OptionName::SocketDir | OptionName::MysqlSocket => {
                if !eggdb_platform::local_socket::LOCAL_SOCKETS_SUPPORTED {
                    return Err(usage(format!(
                        "{} is not available on Windows yet; connect over --listen instead",
                        option.spelled
                    )));
                }
                if option.name == OptionName::SocketDir {
                    request.serve.socket_dir = Some(PathBuf::from(value));
                } else {
                    request.serve.mysql_socket = Some(PathBuf::from(value));
                }
            }
            OptionName::TlsCertificate => request.serve.tls_certificate = Some(PathBuf::from(value)),
            OptionName::AuthRules => request.serve.auth_rules = Some(PathBuf::from(value)),
            OptionName::TlsKey => request.serve.tls_key = Some(PathBuf::from(value)),
            OptionName::TlsMinVersion => {
                request.serve.tls_min_version = Some(match value.as_encoded_bytes() {
                    b"1.2" => eggdb_tls::MinimumVersion::Tls12,
                    b"1.3" => eggdb_tls::MinimumVersion::Tls13,
                    _ => {
                        return Err(usage(format!(
                            "{} needs 1.2 or 1.3, not {}",
                            option.spelled,
                            shown(value)
                        )));
                    }
                });
            }
            OptionName::MaxConnections => {
                let count = parse_whole_number(value.as_encoded_bytes())
                    .filter(|&count| count >= 1)
                    .and_then(|count| usize::try_from(count).ok())
                    .ok_or_else(|| {
                        usage(format!(
                            "{} needs a whole number of at least 1, not '{}'",
                            option.spelled,
                            shown(value)
                        ))
                    })?;
                request.serve.max_connections = Some(count);
            }
            OptionName::Csv => request.csv.source = Some(export_file(value)),
            OptionName::Parquet => request.csv.parquet = Some(PathBuf::from(value)),
            OptionName::Header => request.csv.header = true,
            OptionName::KeepNulls => request.csv.keep_nulls = true,
            OptionName::Table
            | OptionName::Delimiter
            | OptionName::Null
            | OptionName::RowTerminator => {
                let text = value
                    .to_str()
                    .ok_or_else(|| usage(format!("{} must be UTF-8 text", option.spelled)))?
                    .to_owned();
                match option.name {
                    OptionName::Table => request.csv.table = Some(text),
                    OptionName::Delimiter => request.csv.delimiter = Some(text),
                    OptionName::RowTerminator => request.csv.row_terminator = Some(text),
                    _ => request.csv.null = Some(text),
                }
            }
            OptionName::TimeLimit => {
                request.time_limit = Some(
                    parse_whole_number(value.as_encoded_bytes())
                        .filter(|seconds| (1..=MOST_TIME_LIMIT_SECONDS).contains(seconds))
                        .ok_or_else(|| {
                            usage(format!(
                                "{} must be a whole number of seconds from 1 to {MOST_TIME_LIMIT_SECONDS}, not '{}'",
                                option.spelled,
                                shown(value)
                            ))
                        })?,
                );
            }
            OptionName::Tables => {
                for name in parse_table_names(&option.spelled, value).map_err(usage)? {
                    if !request.tables.contains(&name) {
                        request.tables.push(name);
                    }
                }
            }
            OptionName::CallGap => {
                request.call_gap_ms = Some(
                    parse_whole_number(value.as_encoded_bytes())
                        .filter(|milliseconds| *milliseconds <= MOST_CALL_GAP_MS)
                        .ok_or_else(|| {
                            usage(format!(
                                "{} must be a whole number of milliseconds from 0 to {MOST_CALL_GAP_MS}, not '{}'",
                                option.spelled,
                                shown(value)
                            ))
                        })?,
                );
            }
            OptionName::Collection => {
                request.collection =
                    Some(parse_collection_name(Some(option.spelled.as_str()), value).map_err(usage)?);
            }
            OptionName::Ttl => {
                request.ttl = Some(parse_ttl_option(&option.spelled, value).map_err(usage)?);
            }
            OptionName::Schema if command == CommandName::Follow => {
                for name in value.to_str().unwrap_or_default().split(',') {
                    let name = name.trim();
                    if name.is_empty() {
                        return Err(usage(format!(
                            "{} needs schema names separated by commas, not {}",
                            option.spelled,
                            shown(value)
                        )));
                    }
                    request.follow.schemas.push(name.to_owned());
                }
            }
            OptionName::Schema => {
                request.schema = Some(parse_name(&option.spelled, value).map_err(usage)?);
            }
            OptionName::Publication => {
                request.follow.publication = Some(parse_name(&option.spelled, value).map_err(usage)?);
            }
            OptionName::Poll => {
                let seconds = parse_whole_number(value.as_encoded_bytes())
                    .filter(|&seconds| seconds >= 1)
                    .ok_or_else(|| {
                        usage(format!(
                            "{} needs a whole number of seconds, at least 1, not {}",
                            option.spelled,
                            shown(value)
                        ))
                    })?;
                request.follow.poll_seconds = Some(seconds);
            }
            OptionName::Until => {
                if value.as_encoded_bytes() != b"caught-up" {
                    return Err(usage(format!(
                        "{} takes caught-up, not {}",
                        option.spelled,
                        shown(value)
                    )));
                }
                request.follow.until_caught_up = true;
            }
            OptionName::IfAbsent => request.swap.if_absent = true,
            OptionName::Delete => request.swap.delete = true,
            OptionName::Help | OptionName::Version => {}
        }
    }
    if request.scan.prefix.is_some() && (request.scan.from.is_some() || request.scan.to.is_some()) {
        return Err(usage(
            "--prefix cannot be combined with --from or --to".to_owned(),
        ));
    }
    if request.allow_write && !request.tables.is_empty() {
        return Err(usage(
            "--tables goes with a read-only server in this release: drop --allow-write, or drop --tables to let the agent write every table"
                .to_owned(),
        ));
    }
    let arguments = &tokens.positionals[1..];
    if command == CommandName::Collection {
        return collection_request(request, arguments);
    }
    if command == CommandName::Table {
        return table_request(request, tokens, arguments);
    }
    if command == CommandName::Row {
        return row_request(request, tokens, arguments);
    }
    if request.swap.if_absent && request.swap.delete {
        return Err(usage(
            "--if-absent with --delete would change nothing: an absent key cannot be deleted"
                .to_owned(),
        ));
    }
    let swap_arguments = 2 + usize::from(!request.swap.if_absent) + usize::from(!request.swap.delete);
    let (least, most) = match command {
        CommandName::Create
        | CommandName::Scan
        | CommandName::Check
        | CommandName::Stats
        | CommandName::Compact
        | CommandName::Ui
        | CommandName::Serve
        | CommandName::Shell
        | CommandName::Mcp
        | CommandName::Audit
        | CommandName::Explain
        | CommandName::Collection
        | CommandName::Table
        | CommandName::Row => (1, 1),
        CommandName::Export | CommandName::Import | CommandName::Sql | CommandName::Restore => {
            (1, 2)
        }
        CommandName::Get | CommandName::Del | CommandName::Ttl | CommandName::Persist => (2, 2),
        CommandName::Put | CommandName::Incr | CommandName::Decr => (2, 3),
        CommandName::Expire => (3, 3),
        CommandName::Backup | CommandName::Follow => (2, 2),
        CommandName::Cas => (swap_arguments, swap_arguments),
    };
    if arguments.len() < least {
        return Err(usage(missing_arguments(command, arguments.len())));
    }
    if arguments.len() > most {
        return Err(too_many_arguments(&arguments[most..], Some(command)));
    }
    request.file = PathBuf::from(arguments[0]);
    if command == CommandName::Explain {
        let given = matches!(&request.sql, Some(SqlSource::Command(text)) if !text.trim().is_empty());
        if !given {
            return Err(usage(EXPLAIN_NEEDS_A_STATEMENT.to_owned()));
        }
        return Ok(request);
    }
    if command == CommandName::Sql {
        if request.read_only && request.create {
            return Err(usage(READ_ONLY_CANNOT_CREATE.to_owned()));
        }
        match (arguments.get(1), &request.sql) {
            (Some(_), Some(_)) => {
                return Err(usage(
                    "give the SQL with -c or as a script, not both".to_owned(),
                ));
            }
            (Some(script), None) => {
                request.sql = Some(match export_file(script) {
                    ExportFile::Standard => SqlSource::Stdin,
                    ExportFile::Path(path) => SqlSource::Script(path),
                });
            }
            (None, Some(_)) => {}
            (None, None) => request.sql = Some(SqlSource::Stdin),
        }
        if !request.restore.defines.is_empty() {
            if request.dialect != eggdb_sql::Dialect::Oracle {
                return Err(usage(DEFINE_NEEDS_ORACLE.to_owned()));
            }
            if matches!(request.sql, Some(SqlSource::Command(_))) {
                return Err(usage(
                    "--define goes with a script: -c runs its SQL as written, with no substitution"
                        .to_owned(),
                ));
            }
        }
        return Ok(request);
    }
    if command == CommandName::Restore {
        request.restore.dump = arguments.get(1).map(|argument| export_file(argument));
        if !request.restore.defines.is_empty() && request.dialect != eggdb_sql::Dialect::Oracle {
            return Err(usage(DEFINE_NEEDS_ORACLE.to_owned()));
        }
        return Ok(request);
    }
    if command == CommandName::Follow {
        if arguments[0].as_encoded_bytes() == b"-" {
            return Err(usage(
                "follow writes into a database file, and stdin is not one. Give a path.".to_owned(),
            ));
        }
        request.follow.source = arguments[1]
            .to_str()
            .ok_or_else(|| usage("the source URL must be UTF-8 text".to_owned()))?
            .to_owned();
        return Ok(request);
    }
    if command == CommandName::Backup {
        if arguments[1].as_encoded_bytes() == b"-" {
            return Err(usage(
                "backup writes a file it can sync and verify, and stdout is not one. Give a path."
                    .to_owned(),
            ));
        }
        request.copy = Some(PathBuf::from(arguments[1]));
        return Ok(request);
    }
    if command == CommandName::Compact {
        return Ok(request);
    }
    if matches!(command, CommandName::Export | CommandName::Import) {
        request.export_file = arguments.get(1).map(|argument| export_file(argument));
        check_csv_options(&request).map_err(usage)?;
        return Ok(request);
    }
    let byte_argument = |argument: &&OsStr| {
        ByteArgument::parse(argument).map_err(|message| Failure::usage(message, Some(command)))
    };
    request.key = arguments.get(1).map(byte_argument).transpose()?;
    match command {
        CommandName::Expire => {
            let time = arguments.get(2).copied().unwrap_or_default();
            request.ttl = Some(parse_expire_time(time, arguments[1]).map_err(usage)?);
        }
        CommandName::Incr | CommandName::Decr => {
            request.step = arguments
                .get(2)
                .map(|step| parse_step(command, step))
                .transpose()
                .map_err(usage)?;
        }
        CommandName::Cas => {
            let mut rest = arguments[2..].iter();
            if !request.swap.if_absent {
                request.swap.expected = rest.next().map(byte_argument).transpose()?;
            }
            if !request.swap.delete {
                request.value = rest.next().map(byte_argument).transpose()?;
            }
        }
        _ => request.value = arguments.get(2).map(byte_argument).transpose()?,
    }
    Ok(request)
}

const TIME_EXAMPLES: &str = "30s, 5m, 1h30m, 7d or 1500ms";

fn parse_ttl_option(spelled: &str, value: &OsStr) -> Result<Duration, String> {
    match eggdb_ui::parse_time_to_live(value.to_str().unwrap_or_default()) {
        Ok(ttl) => Ok(ttl),
        Err(eggdb_ui::TimeToLiveError::Zero) => Err(format!(
            "{spelled} 0 would remove the key as soon as it is stored; leave {spelled} out, or give a time above 0"
        )),
        Err(eggdb_ui::TimeToLiveError::NotATime) => Err(format!(
            "{spelled} needs a time such as {TIME_EXAMPLES}, not '{}'",
            shown(value)
        )),
    }
}

fn parse_expire_time(value: &OsStr, key: &OsStr) -> Result<Duration, String> {
    match eggdb_ui::parse_time_to_live(value.to_str().unwrap_or_default()) {
        Ok(ttl) => Ok(ttl),
        Err(eggdb_ui::TimeToLiveError::Zero) => Err(format!(
            "a time to live of 0 would remove {} now; use eggdb del",
            shown(key)
        )),
        Err(eggdb_ui::TimeToLiveError::NotATime) => Err(format!(
            "expire needs a time such as {TIME_EXAMPLES}, not '{}'",
            shown(value)
        )),
    }
}

fn parse_step(command: CommandName, value: &OsStr) -> Result<u64, String> {
    parse_whole_number(value.as_encoded_bytes())
        .filter(|&step| i64::try_from(step).is_ok())
        .ok_or_else(|| {
            format!(
                "{} needs a whole number from 0 to {}, not '{}'",
                command.name(),
                i64::MAX,
                shown(value)
            )
        })
}

fn collection_request(mut request: Request, arguments: &[&OsStr]) -> Result<Request, Failure> {
    let command = CommandName::Collection;
    let usage = |message: String| Failure::usage(message, Some(command));
    let Some(action) = arguments.first() else {
        return Err(usage(COLLECTION_NEEDS_A_VERB.to_owned()));
    };
    let action = match action.as_encoded_bytes() {
        b"list" => CollectionAction::List,
        b"add" => CollectionAction::Add,
        b"delete" | b"del" => CollectionAction::Delete,
        _ => return Err(usage(COLLECTION_NEEDS_A_VERB.to_owned())),
    };
    let (verb, needed) = match action {
        CollectionAction::List => ("list", 2),
        CollectionAction::Add => ("add", 3),
        CollectionAction::Delete => ("delete", 3),
    };
    if arguments.len() < needed {
        let needs = match (action, arguments.len()) {
            (CollectionAction::List, _) => "a file",
            (_, 1) => "a file and a collection name",
            _ => "a collection name",
        };
        return Err(usage(format!("eggdb collection {verb} needs {needs}")));
    }
    if arguments.len() > needed {
        return Err(too_many_arguments(&arguments[needed..], Some(command)));
    }
    request.collection_action = Some(action);
    request.file = PathBuf::from(arguments[1]);
    if let Some(name) = arguments.get(2) {
        request.collection = Some(parse_collection_name(None, name).map_err(usage)?);
    }
    Ok(request)
}

fn table_request(
    mut request: Request,
    tokens: &Tokens<'_>,
    arguments: &[&OsStr],
) -> Result<Request, Failure> {
    let command = CommandName::Table;
    let usage = |message: String| Failure::usage(message, Some(command));
    let action = match arguments.first().map(|action| action.as_encoded_bytes()) {
        Some(b"list") => TableAction::List,
        Some(b"show") => TableAction::Show,
        Some(b"sample") => TableAction::Sample,
        _ => return Err(usage(TABLE_NEEDS_A_VERB.to_owned())),
    };
    let foreign = tokens.options.iter().find(|option| match option.name {
        OptionName::Limit => action == TableAction::Show,
        OptionName::Schema | OptionName::After => action != TableAction::List,
        _ => false,
    });
    if let Some(option) = foreign {
        return Err(usage(format!(
            "unknown option '{}' for eggdb table {}",
            option.spelled,
            action.name()
        )));
    }
    let needed = if action == TableAction::List { 2 } else { 3 };
    if arguments.len() < needed {
        let needs = match (action, arguments.len()) {
            (TableAction::List, _) => "a file",
            (_, 1) => "a file and a table name",
            _ => "a table name",
        };
        return Err(usage(format!(
            "eggdb table {} needs {needs}",
            action.name()
        )));
    }
    if arguments.len() > needed {
        return Err(too_many_arguments(&arguments[needed..], Some(command)));
    }
    request.scan.limit = match given_limit(tokens) {
        Some((spelled, value)) if action == TableAction::Sample => {
            Some(parse_sample_rows(spelled, value).map_err(usage)?)
        }
        Some((spelled, value)) => Some(parse_page_limit(spelled, value).map_err(usage)?),
        None => None,
    };
    request.after = given_cursor(tokens, CursorList::Tables).map_err(usage)?;
    request.table_action = Some(action);
    request.file = PathBuf::from(arguments[1]);
    request.table = arguments.get(2).map(|name| name.to_os_string());
    Ok(request)
}

fn row_request(
    mut request: Request,
    tokens: &Tokens<'_>,
    arguments: &[&OsStr],
) -> Result<Request, Failure> {
    let command = CommandName::Row;
    let usage = |message: String| Failure::usage(message, Some(command));
    if arguments.first().map(|verb| verb.as_encoded_bytes()) != Some(b"list".as_slice()) {
        return Err(usage(ROW_NEEDS_A_VERB.to_owned()));
    }
    if arguments.len() < 3 {
        let needs = if arguments.len() == 1 {
            "a file and a table name"
        } else {
            "a table name"
        };
        return Err(usage(format!("eggdb row list needs {needs}")));
    }
    if arguments.len() > 3 {
        return Err(too_many_arguments(&arguments[3..], Some(command)));
    }
    request.scan.limit = given_limit(tokens)
        .map(|(spelled, value)| parse_page_limit(spelled, value))
        .transpose()
        .map_err(usage)?;
    request.after = given_cursor(tokens, CursorList::Rows).map_err(usage)?;
    request.file = PathBuf::from(arguments[1]);
    request.table = Some(arguments[2].to_os_string());
    Ok(request)
}

fn given_limit<'t>(tokens: &'t Tokens<'_>) -> Option<(&'t str, &'t OsStr)> {
    tokens
        .options
        .iter()
        .rev()
        .find(|option| option.name == OptionName::Limit)
        .map(|option| (option.spelled.as_str(), option.value.unwrap_or_default()))
}

fn given_cursor(tokens: &Tokens<'_>, list: CursorList) -> Result<Option<String>, String> {
    let Some(option) = tokens
        .options
        .iter()
        .rev()
        .find(|option| option.name == OptionName::After)
    else {
        return Ok(None);
    };
    let value = option.value.unwrap_or_default();
    let refused = |refusal: CursorRefusal| {
        refusal.message(&option.spelled, list, "", &shown_cursor(&shown(value)))
    };
    let Some(text) = value.to_str() else {
        return Err(refused(CursorRefusal::Malformed));
    };
    let checked = match list {
        CursorList::Tables => decode_cursor(list, "", text).map(|_| ()),
        CursorList::Rows => check_cursor(list, text),
    };
    checked.map_err(refused)?;
    Ok(Some(text.to_owned()))
}

fn parse_page_limit(option: &str, value: &OsStr) -> Result<u64, String> {
    parse_whole_number(value.as_encoded_bytes())
        .filter(|&count| count >= 1)
        .ok_or_else(|| {
            format!(
                "{option} needs a whole number of at least 1, not '{}'",
                shown(value)
            )
        })
}

fn parse_sample_rows(option: &str, value: &OsStr) -> Result<u64, String> {
    parse_whole_number(value.as_encoded_bytes())
        .filter(|rows| (1..=MOST_SAMPLE_ROWS).contains(rows))
        .ok_or_else(|| {
            format!(
                "{option} needs a whole number from 1 to {MOST_SAMPLE_ROWS}, not '{}'; for more rows use eggdb sql",
                shown(value)
            )
        })
}

const EXPLAIN_NEEDS_A_STATEMENT: &str = "eggdb explain needs a statement: give it with -c";
const READ_ONLY_CANNOT_CREATE: &str =
    "--read-only cannot be combined with --create: a read-only open never creates a file";
const TABLE_NEEDS_A_VERB: &str = "table needs list, show or sample";
const ROW_NEEDS_A_VERB: &str = "row needs list";
const COLLECTION_NEEDS_A_VERB: &str = "collection needs list, add or delete";
pub(crate) const MOST_SAMPLE_ROWS: u64 = 1_000;

const MOST_COLLECTION_NAME_BYTES: usize = 128;

pub(crate) fn is_collection_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MOST_COLLECTION_NAME_BYTES
        && !name.chars().any(|c| c.is_whitespace() || c.is_control())
}

pub(crate) fn bad_collection_name(option: Option<&str>, shown_name: &str) -> String {
    match option {
        Some(option) => format!(
            "{option} needs a name of 1 to {MOST_COLLECTION_NAME_BYTES} bytes with no spaces or control characters, not '{shown_name}'"
        ),
        None => format!(
            "a collection name is 1 to {MOST_COLLECTION_NAME_BYTES} bytes with no spaces or control characters, not '{shown_name}'"
        ),
    }
}

fn parse_collection_name(option: Option<&str>, value: &OsStr) -> Result<String, String> {
    match value.to_str() {
        Some(name) if is_collection_name(name) => Ok(name.to_owned()),
        _ => Err(bad_collection_name(option, &shown(value))),
    }
}

fn check_csv_options(request: &Request) -> Result<(), String> {
    let csv = &request.csv;
    if csv.parquet.is_some() {
        if csv.source.is_some() {
            return Err("give --csv or --parquet, not both".to_owned());
        }
        if request.export_file.is_some() {
            return Err("give the Parquet file with --parquet <file>, not as a second argument".to_owned());
        }
        if csv.table.is_none() {
            return Err("--parquet needs --table <name>, the table to load or write".to_owned());
        }
        return Ok(());
    }
    if csv.source.is_none() {
        let given = [
            (csv.table.is_some(), "--table"),
            (csv.header, "--header"),
            (csv.delimiter.is_some(), "--delimiter"),
            (csv.null.is_some(), "--null"),
            (csv.row_terminator.is_some(), "--row-terminator"),
            (csv.keep_nulls, "--keep-nulls"),
        ];
        if let Some((_, option)) = given.iter().find(|(given, _)| *given) {
            return Err(format!("{option} goes with --csv <file>"));
        }
        if request.command == CommandName::Import
            && request.dialect != eggdb_sql::Dialect::PostgreSql
        {
            return Err("--dialect goes with --csv <file>".to_owned());
        }
        return Ok(());
    }
    if request.export_file.is_some() {
        return Err("give the CSV with --csv <file>, not as a second argument".to_owned());
    }
    if request.collection.is_some() {
        return Err(
            "--collection is for eggdb-jsonl imports; --csv loads rows into a SQL table".to_owned(),
        );
    }
    if csv.table.is_none() {
        return Err("--csv needs --table <name>, the table to load the rows into".to_owned());
    }
    if request.create {
        return Err(
            "--csv loads into a table that exists; a new file has none, so create it first with eggdb sql --create"
                .to_owned(),
        );
    }
    let bcp = request.dialect == eggdb_sql::Dialect::SqlServer;
    if bcp && csv.null.is_some() {
        return Err(
            "--null is not for --dialect sqlserver: bcp character files mark NULL with an empty field"
                .to_owned(),
        );
    }
    if !bcp && (csv.row_terminator.is_some() || csv.keep_nulls) {
        return Err(
            "--row-terminator and --keep-nulls are for bcp character files, with --dialect sqlserver"
                .to_owned(),
        );
    }
    if request.on_conflict != ConflictPolicy::Error || request.page_size.is_some() {
        return Err(
            "--on-conflict and --page-size are for eggdb-jsonl imports, not --csv".to_owned(),
        );
    }
    Ok(())
}

const RESTORE_DIALECTS: [(&str, eggdb_sql::Dialect); 7] = [
    ("postgresql", eggdb_sql::Dialect::PostgreSql),
    ("sqlserver", eggdb_sql::Dialect::SqlServer),
    ("mssql", eggdb_sql::Dialect::SqlServer),
    ("tsql", eggdb_sql::Dialect::SqlServer),
    ("oracle", eggdb_sql::Dialect::Oracle),
    ("mysql", eggdb_sql::Dialect::MySql),
    ("mariadb", eggdb_sql::Dialect::MySql),
];

fn check_restore_dialect(value: &OsStr) -> Result<eggdb_sql::Dialect, String> {
    let name = value.to_str().unwrap_or_default();
    if let Some((_, dialect)) = RESTORE_DIALECTS.iter().find(|(known, _)| *known == name) {
        return Ok(*dialect);
    }
    Err(format!(
        "--dialect must be postgresql, sqlserver, oracle or mysql, not '{}'",
        shown(value)
    ))
}

fn check_import_dialect(value: &OsStr) -> Result<eggdb_sql::Dialect, String> {
    match value.to_str().unwrap_or_default() {
        "postgresql" => Ok(eggdb_sql::Dialect::PostgreSql),
        "oracle" => Ok(eggdb_sql::Dialect::Oracle),
        "sqlserver" | "mssql" | "tsql" => Ok(eggdb_sql::Dialect::SqlServer),
        _ => Err(format!(
            "--dialect for import --csv must be postgresql, oracle or sqlserver, not '{}'",
            shown(value)
        )),
    }
}

const MAX_DEFINE_VALUE_BYTES: usize = 240;

const DEFINE_NEEDS_ORACLE: &str =
    "--define sets SQL*Plus substitution variables, so it goes with --dialect oracle";

fn parse_define(spelled: &str, value: &OsStr) -> Result<(String, String), String> {
    let text = value
        .to_str()
        .ok_or_else(|| format!("{spelled} must be UTF-8 text"))?;
    let Some((name, definition)) = text.split_once('=') else {
        return Err(format!(
            "{spelled} takes name=value, such as {spelled} pass=secret"
        ));
    };
    let name = name.trim();
    if name.is_empty()
        || !name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '$' | '#')
        })
    {
        return Err(format!(
            "{spelled} needs a variable name of letters, digits, _, $ and #, not '{name}'"
        ));
    }
    if definition.len() > MAX_DEFINE_VALUE_BYTES {
        return Err(format!(
            "the value of {spelled} {name} is longer than {MAX_DEFINE_VALUE_BYTES} bytes, SQL*Plus's DEFINE limit"
        ));
    }
    Ok((name.to_owned(), definition.to_owned()))
}

fn export_file(argument: &OsStr) -> ExportFile {
    if argument.as_encoded_bytes() == b"-" {
        ExportFile::Standard
    } else {
        ExportFile::Path(PathBuf::from(argument))
    }
}

fn missing_arguments(command: CommandName, given: usize) -> String {
    let needs = match (command, given) {
        (CommandName::Put, 0) => "a file and a key, and a value or stdin",
        (CommandName::Put, _) => "a key, and a value or stdin",
        (
            CommandName::Get
            | CommandName::Del
            | CommandName::Ttl
            | CommandName::Persist
            | CommandName::Incr
            | CommandName::Decr,
            0,
        ) => "a file and a key",
        (
            CommandName::Get
            | CommandName::Del
            | CommandName::Ttl
            | CommandName::Persist
            | CommandName::Incr
            | CommandName::Decr,
            _,
        ) => "a key",
        (CommandName::Follow, 0) => "a file and the source, such as postgres://user@host/database",
        (CommandName::Follow, _) => "the source, such as postgres://user@host/database",
        (CommandName::Backup, 0) => "a file and the path of the copy",
        (CommandName::Backup, _) => "the path of the copy",
        (CommandName::Expire, 0) => "a file, a key and a time such as 30s",
        (CommandName::Expire, 1) => "a key and a time such as 30s",
        (CommandName::Expire, _) => "a time such as 30s, 5m, 1h30m, 7d or 1500ms",
        (CommandName::Cas, 0) => {
            "a file, a key, the value expected (or --if-absent) and the new value (or --delete)"
        }
        (CommandName::Cas, 1) => {
            "a key, the value expected (or --if-absent) and the new value (or --delete)"
        }
        (CommandName::Cas, _) => {
            "the value expected (or --if-absent) and the new value (or --delete)"
        }
        _ => "a file",
    };
    format!("eggdb {} needs {needs}", command.name())
}

fn too_many_arguments(extra: &[&OsStr], command: Option<CommandName>) -> Failure {
    let shown = extra
        .iter()
        .map(|argument| format!("'{}'", shown(argument)))
        .collect::<Vec<_>>()
        .join(" ");
    let what = command.map_or("eggdb".to_owned(), |command| {
        format!("eggdb {}", command.name())
    });
    Failure::usage(format!("too many arguments for {what}: {shown}"), command)
}

fn shown(argument: &OsStr) -> String {
    render::text(argument.as_encoded_bytes())
}

fn unknown_command(name: &[u8]) -> Failure {
    Failure::usage(
        format!(
            "unknown command '{}'. Commands: create, put, get, del, scan, ttl, expire, persist, incr, decr, cas, check, stats, backup, compact, ui, export, import, sql, explain, table, row, serve, shell, restore, mcp, audit, collection, help, version.",
            render::text(name)
        ),
        None,
    )
}

fn unknown_option(spelled: &str, command: CommandName) -> Failure {
    let negative_step = spelled
        .strip_prefix('-')
        .is_some_and(|digits| parse_whole_number(digits.as_bytes()).is_some());
    let hint = match command {
        CommandName::Incr if negative_step => "; to subtract, use eggdb decr",
        CommandName::Decr if negative_step => "; to add, use eggdb incr",
        _ => "",
    };
    Failure::usage(
        format!("unknown option '{spelled}' for eggdb {}{hint}", command.name()),
        Some(command),
    )
}

fn parse_format(value: &OsStr) -> Result<Format, String> {
    match value.as_encoded_bytes() {
        b"text" => Ok(Format::Text),
        b"json" => Ok(Format::Json),
        b"hex" => Ok(Format::Hex),
        b"raw" => Ok(Format::Raw),
        _ => Err(format!(
            "--format must be text, json, hex or raw, not '{}'",
            shown(value)
        )),
    }
}

fn parse_listen(option: &str, value: &OsStr, port: u16) -> Result<SocketAddr, String> {
    let refused = || {
        format!(
            "{option} needs an address and port such as 127.0.0.1:{port}, [::1]:{port} or {}, not '{}'",
            port + 1,
            shown(value)
        )
    };
    let text = value.to_str().ok_or_else(refused)?;
    if let Ok(port) = text.parse::<u16>() {
        return Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
    }
    if let Some(port) = text.strip_prefix("localhost:") {
        let port = port.parse::<u16>().map_err(|_| refused())?;
        return Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
    }
    text.parse::<SocketAddr>().map_err(|_| refused())
}

fn parse_name(option: &str, value: &OsStr) -> Result<String, String> {
    match value.to_str() {
        Some(name) if !name.is_empty() && !name.chars().any(char::is_control) => {
            Ok(name.to_owned())
        }
        _ => Err(format!(
            "{option} needs a name without control characters, not '{}'",
            shown(value)
        )),
    }
}

fn parse_on_conflict(value: &OsStr) -> Result<ConflictPolicy, String> {
    match value.as_encoded_bytes() {
        b"error" => Ok(ConflictPolicy::Error),
        b"overwrite" => Ok(ConflictPolicy::Overwrite),
        b"skip" => Ok(ConflictPolicy::Skip),
        _ => Err(format!(
            "--on-conflict must be error, overwrite or skip, not '{}'",
            shown(value)
        )),
    }
}

fn parse_color(value: &OsStr) -> Result<ColorChoice, String> {
    match value.as_encoded_bytes() {
        b"auto" => Ok(ColorChoice::Auto),
        b"always" => Ok(ColorChoice::Always),
        b"never" => Ok(ColorChoice::Never),
        _ => Err(format!(
            "--color must be auto, always or never, not '{}'",
            shown(value)
        )),
    }
}

fn parse_size(option: &str, value: &OsStr) -> Result<usize, String> {
    let refused = || {
        format!(
            "{option} needs a size such as 4096, 64k, 16m or 1g, not '{}'",
            shown(value)
        )
    };
    let bytes = value.as_encoded_bytes();
    let (digits, shift) = match bytes.split_last() {
        Some((b'k' | b'K', digits)) => (digits, 10),
        Some((b'm' | b'M', digits)) => (digits, 20),
        Some((b'g' | b'G', digits)) => (digits, 30),
        _ => (bytes, 0),
    };
    let number = parse_whole_number(digits).ok_or_else(refused)?;
    let scaled = number.checked_mul(1 << shift).ok_or_else(refused)?;
    usize::try_from(scaled).map_err(|_| refused())
}

fn parse_page_size(option: &str, value: &OsStr) -> Result<usize, String> {
    let bytes = parse_size(option, value)?;
    if bytes.is_power_of_two() && (SMALLEST_PAGE_SIZE..=LARGEST_PAGE_SIZE).contains(&bytes) {
        return Ok(bytes);
    }
    Err(format!(
        "{option} must be a power of two from 4096 to 65536, not '{}'",
        shown(value)
    ))
}

fn parse_limit(option: &str, value: &OsStr) -> Result<u64, String> {
    parse_whole_number(value.as_encoded_bytes())
        .ok_or_else(|| format!("{option} needs a whole number, not '{}'", shown(value)))
}

fn parse_table_names(spelled: &str, value: &OsStr) -> Result<Vec<String>, String> {
    let text = value
        .to_str()
        .ok_or_else(|| format!("{spelled} must be UTF-8 text"))?;
    if text.trim().is_empty() {
        return Err(format!(
            "{spelled} needs at least one name, such as {spelled} users,orders"
        ));
    }
    let mut names = Vec::new();
    for name in text.split(',').map(str::trim) {
        if name.is_empty() {
            return Err(format!("{spelled} has an empty name in '{text}'"));
        }
        let parts: Vec<&str> = name.split('.').collect();
        if parts.len() > 2 || parts.iter().any(|part| part.is_empty()) {
            return Err(format!(
                "{spelled} takes name or schema.name, not '{name}'"
            ));
        }
        names.push(name.to_owned());
    }
    Ok(names)
}

fn parse_whole_number(digits: &[u8]) -> Option<u64> {
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    digits.iter().try_fold(0u64, |number, digit| {
        number.checked_mul(10)?.checked_add(u64::from(digit - b'0'))
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ByteArgument, ColorChoice, CommandName, CsvOptions, ExportFile, Format, Invocation,
        Request, ScanOptions, parse,
    };
    use crate::cli::failure::Failure;
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn parse_words(words: &str) -> Result<Invocation, Failure> {
        let args: Vec<OsString> = words.split_whitespace().map(OsString::from).collect();
        parse(&args)
    }

    fn request(words: &str) -> Request {
        match parse_words(words) {
            Ok(Invocation::Run(request)) => *request,
            other => panic!("{words}: {other:?}"),
        }
    }

    fn usage_message(words: &str) -> String {
        match parse_words(words) {
            Err(Failure::Usage { message, .. }) => message,
            other => panic!("{words}: {other:?}"),
        }
    }

    fn bytes(text: &str) -> Option<ByteArgument> {
        Some(ByteArgument::Bytes(text.as_bytes().to_vec()))
    }

    #[test]
    fn mcp_takes_repeated_table_lists_and_a_call_gap() {
        let limited = request(
            "mcp app.db --tables users,orders --tables archive.orders,users --call-gap 250",
        );
        assert_eq!(limited.command, CommandName::Mcp);
        assert_eq!(limited.tables, vec!["users", "orders", "archive.orders"]);
        assert_eq!(limited.call_gap_ms, Some(250));
        assert!(!limited.allow_write);
        let plain = request("mcp app.db");
        assert!(plain.tables.is_empty());
        assert_eq!(plain.call_gap_ms, None);
        assert_eq!(request("mcp app.db --call-gap 0").call_gap_ms, Some(0));
        assert_eq!(request("mcp app.db --call-gap 10000").call_gap_ms, Some(10_000));
    }

    #[test]
    fn mcp_refuses_tables_with_writes_and_malformed_names_and_gaps() {
        assert_eq!(
            usage_message("mcp app.db --allow-write --tables users"),
            "--tables goes with a read-only server in this release: drop --allow-write, or drop --tables to let the agent write every table"
        );
        assert_eq!(
            usage_message("mcp app.db --tables users,,orders"),
            "--tables has an empty name in 'users,,orders'"
        );
        assert_eq!(
            usage_message("mcp app.db --tables a.b.c"),
            "--tables takes name or schema.name, not 'a.b.c'"
        );
        assert_eq!(
            usage_message("mcp app.db --tables .orders"),
            "--tables takes name or schema.name, not '.orders'"
        );
        assert_eq!(
            usage_message("mcp app.db --tables ,"),
            "--tables has an empty name in ','"
        );
        assert_eq!(
            usage_message("mcp app.db --call-gap 20s"),
            "--call-gap must be a whole number of milliseconds from 0 to 10000, not '20s'"
        );
        assert_eq!(
            usage_message("mcp app.db --call-gap 10001"),
            "--call-gap must be a whole number of milliseconds from 0 to 10000, not '10001'"
        );
        assert!(usage_message("get app.db k --tables users").contains("--tables"));
    }

    #[test]
    fn import_csv_takes_a_table_and_the_csv_options() {
        let csv =
            request("import app.db --csv data.csv --table users --header --delimiter ; --null NA");
        assert_eq!(
            csv.csv,
            CsvOptions {
                source: Some(ExportFile::Path(PathBuf::from("data.csv"))),
                table: Some("users".into()),
                header: true,
                delimiter: Some(";".into()),
                null: Some("NA".into()),
                row_terminator: None,
                keep_nulls: false,
            }
        );
        let bcp = request(
            "import --dialect sqlserver app.db --csv items.dat --table dbo.items --delimiter , --row-terminator \\r\\n --keep-nulls",
        );
        assert_eq!(bcp.dialect, eggdb_sql::Dialect::SqlServer);
        assert_eq!(bcp.csv.row_terminator.as_deref(), Some("\\r\\n"));
        assert!(bcp.csv.keep_nulls);
        assert!(
            usage_message("import --dialect sqlserver app.db --csv d.dat --table t --null NA")
                .contains("--null")
        );
        assert!(
            usage_message("import app.db --csv d.csv --table t --keep-nulls")
                .contains("--dialect sqlserver")
        );
        assert_eq!(
            request("import app.db --csv - --table t").csv.source,
            Some(ExportFile::Standard)
        );
        assert_eq!(
            usage_message("import app.db --csv data.csv"),
            "--csv needs --table <name>, the table to load the rows into"
        );
        assert_eq!(
            usage_message("import app.db --table t"),
            "--table goes with --csv <file>"
        );
        assert_eq!(
            usage_message("import app.db in.csv --csv data.csv --table t"),
            "give the CSV with --csv <file>, not as a second argument"
        );
        assert!(
            usage_message("import app.db --csv d.csv --table t --dry-run").contains("--dry-run")
        );
        assert!(usage_message("import app.db --csv d.csv --table t --create").contains("--create"));
        assert!(usage_message("sql app.db --csv d.csv").contains("--csv"));
    }

    #[test]
    fn no_arguments_is_its_own_case() {
        assert_eq!(parse(&[]), Ok(Invocation::NoArguments));
    }

    #[test]
    fn help_and_version_have_every_spelling() {
        for words in ["--help", "-h", "help"] {
            assert_eq!(parse_words(words), Ok(Invocation::Help(None)), "{words}");
        }
        for words in ["help scan", "scan --help", "scan app.db -h", "help delete"] {
            assert!(
                matches!(parse_words(words), Ok(Invocation::Help(Some(_)))),
                "{words}"
            );
        }
        for words in ["--version", "-V", "version", "get app.db k -V"] {
            assert_eq!(parse_words(words), Ok(Invocation::Version), "{words}");
        }
    }

    #[test]
    fn the_command_comes_first_and_options_go_anywhere_after_it() {
        let parsed = request("scan --prefix user: app.db -r -n 1 -k --format=json --color never");
        assert_eq!(parsed.command, CommandName::Scan);
        assert_eq!(parsed.file, PathBuf::from("app.db"));
        assert_eq!(parsed.format, Some(Format::Json));
        assert_eq!(parsed.color, ColorChoice::Never);
        assert_eq!(
            parsed.scan,
            ScanOptions {
                prefix: bytes("user:"),
                reverse: true,
                limit: Some(1),
                keys_only: true,
                ..ScanOptions::default()
            }
        );
    }

    #[test]
    fn put_takes_an_optional_value_and_delete_is_del() {
        let put = request("put app.db user:1 Ada --create");
        assert_eq!(
            (put.key, put.value, put.create),
            (bytes("user:1"), bytes("Ada"), true)
        );
        assert_eq!(request("put app.db photo").value, None);
        assert_eq!(request("delete app.db k").command, CommandName::Del);
    }

    #[test]
    fn a_double_dash_ends_options() {
        let parsed = request("put app.db -- -k --v");
        assert_eq!((parsed.key, parsed.value), (bytes("-k"), bytes("--v")));
        assert_eq!(request("get app.db -").key, bytes("-"));
    }

    #[test]
    fn option_values_may_start_with_a_dash() {
        assert_eq!(request("scan app.db --from -a").scan.from, bytes("-a"));
    }

    #[test]
    fn sizes_take_suffixes() {
        assert_eq!(request("get a k --cache 64m").cache_bytes, Some(64 << 20));
        assert_eq!(request("get a k --cache=4096").cache_bytes, Some(4096));
        assert_eq!(request("create a --page-size 16k").page_size, Some(16384));
        assert_eq!(
            usage_message("get a k --cache 12q"),
            "--cache needs a size such as 4096, 64k, 16m or 1g, not '12q'"
        );
        assert_eq!(
            usage_message("create a --page-size 5000"),
            "--page-size must be a power of two from 4096 to 65536, not '5000'"
        );
    }

    #[test]
    fn usage_errors_use_the_designed_wording() {
        assert_eq!(
            usage_message("frobnicate a"),
            "unknown command 'frobnicate'. Commands: create, put, get, del, scan, ttl, expire, persist, incr, decr, cas, check, stats, backup, compact, ui, export, import, sql, explain, table, row, serve, shell, restore, mcp, audit, collection, help, version."
        );
        assert_eq!(
            usage_message("get a k --prefix x"),
            "unknown option '--prefix' for eggdb get"
        );
        assert_eq!(
            usage_message("scan a --bogus"),
            "unknown option '--bogus' for eggdb scan"
        );
        assert_eq!(
            usage_message("scan a -F yaml"),
            "--format must be text, json, hex or raw, not 'yaml'"
        );
        assert_eq!(
            usage_message("scan a -p x --to y"),
            "--prefix cannot be combined with --from or --to"
        );
        assert_eq!(usage_message("get a"), "eggdb get needs a key");
        assert_eq!(usage_message("check"), "eggdb check needs a file");
        assert_eq!(
            usage_message("get a k extra"),
            "too many arguments for eggdb get: 'extra'"
        );
        assert_eq!(usage_message("scan a -n"), "-n needs a value");
        assert_eq!(
            usage_message("scan a -n x"),
            "-n needs a whole number, not 'x'"
        );
        assert_eq!(
            usage_message("check a -F hex"),
            "--format hex is for get and scan; eggdb check prints text or json"
        );
        assert_eq!(
            usage_message("scan a --reverse=yes"),
            "unknown option '--reverse=...' for eggdb scan"
        );
    }

    #[test]
    fn help_for_an_unknown_command_is_the_unknown_command_error() {
        assert_eq!(
            usage_message("frob --help"),
            "unknown command 'frob'. Commands: create, put, get, del, scan, ttl, expire, persist, incr, decr, cas, check, stats, backup, compact, ui, export, import, sql, explain, table, row, serve, shell, restore, mcp, audit, collection, help, version."
        );
        assert_eq!(
            parse_words("ui -h"),
            Ok(Invocation::Help(Some(CommandName::Ui)))
        );
        assert_eq!(parse_words("version --help"), Ok(Invocation::Help(None)));
    }

    #[test]
    fn control_characters_in_quoted_arguments_are_shown_as_hex() {
        assert_eq!(
            usage_message("fr\u{1b}[31mob"),
            "unknown command '0x66721b5b33316d6f62'. Commands: create, put, get, del, scan, ttl, expire, persist, incr, decr, cas, check, stats, backup, compact, ui, export, import, sql, explain, table, row, serve, shell, restore, mcp, audit, collection, help, version."
        );
        assert_eq!(
            usage_message("scan a -F j\u{7}"),
            "--format must be text, json, hex or raw, not '0x6a07'"
        );
    }

    #[test]
    fn backup_takes_a_file_and_the_path_of_its_copy_and_compact_a_file() {
        let backup = request("backup app.db app.bak");
        assert_eq!(backup.command, CommandName::Backup);
        assert_eq!(backup.file, PathBuf::from("app.db"));
        assert_eq!(backup.copy, Some(PathBuf::from("app.bak")));
        let compact = request("compact app.db --format json");
        assert_eq!(compact.command, CommandName::Compact);
        assert_eq!(compact.file, PathBuf::from("app.db"));
        assert_eq!(compact.format, Some(Format::Json));
        assert_eq!(
            usage_message("backup app.db"),
            "eggdb backup needs the path of the copy"
        );
        assert_eq!(
            usage_message("backup app.db -"),
            "backup writes a file it can sync and verify, and stdout is not one. Give a path."
        );
        assert_eq!(
            usage_message("compact app.db extra"),
            "too many arguments for eggdb compact: 'extra'"
        );
        assert_eq!(
            usage_message("backup app.db app.bak -F hex"),
            "--format hex is for get and scan; eggdb backup prints text or json"
        );
    }

    #[test]
    fn collection_takes_a_verb_a_file_and_a_name() {
        use super::CollectionAction;
        let list = request("collection list app.db");
        assert_eq!(list.command, CommandName::Collection);
        assert_eq!(list.collection_action, Some(CollectionAction::List));
        assert_eq!(list.file, PathBuf::from("app.db"));
        assert_eq!(list.collection, None);
        let add = request("collection add app.db sessions --format json");
        assert_eq!(add.collection_action, Some(CollectionAction::Add));
        assert_eq!(add.collection.as_deref(), Some("sessions"));
        assert_eq!(add.format, Some(Format::Json));
        for verb in ["delete", "del"] {
            let deleted = request(&format!("collection {verb} app.db cache"));
            assert_eq!(deleted.collection_action, Some(CollectionAction::Delete));
        }
        assert_eq!(
            usage_message("collection"),
            "collection needs list, add or delete"
        );
        assert_eq!(
            usage_message("collection rename app.db a"),
            "collection needs list, add or delete"
        );
        assert_eq!(usage_message("collection list"), "eggdb collection list needs a file");
        assert_eq!(
            usage_message("collection add"),
            "eggdb collection add needs a file and a collection name"
        );
        assert_eq!(
            usage_message("collection add app.db"),
            "eggdb collection add needs a collection name"
        );
        assert_eq!(
            usage_message("collection list app.db extra"),
            "too many arguments for eggdb collection: 'extra'"
        );
        assert!(usage_message("collection add app.db a -F hex").contains("--format hex"));
    }

    #[test]
    fn the_collection_option_goes_on_the_commands_that_read_or_write_keys() {
        for words in [
            "get app.db k -C users",
            "put app.db k v --collection users",
            "del app.db k -C users",
            "scan app.db -C users -p u:",
            "export app.db out.jsonl -C users",
            "import app.db in.jsonl --collection=users",
        ] {
            assert_eq!(request(words).collection.as_deref(), Some("users"), "{words}");
        }
        for command in ["check", "stats", "create", "ui", "sql"] {
            assert_eq!(
                usage_message(&format!("{command} app.db -C users")),
                format!("unknown option '-C' for eggdb {command}")
            );
        }
        let long = "n".repeat(129);
        let bad = [String::from(""), long, String::from("tab\there")];
        for name in &bad {
            let args = [
                OsString::from("get"),
                OsString::from("app.db"),
                OsString::from("k"),
                OsString::from("-C"),
                OsString::from(name),
            ];
            match parse(&args) {
                Err(Failure::Usage { message, .. }) => {
                    assert!(message.starts_with("-C "), "{message}");
                }
                other => panic!("{name:?}: {other:?}"),
            }
        }
        assert!(
            usage_message("import app.db --csv d.csv --table t -C users").contains("--collection")
        );
    }

    #[test]
    fn table_takes_a_verb_a_file_and_a_table_and_explain_takes_a_statement() {
        use super::{SqlSource, TableAction};
        let list = request("table list app.db --schema sales --json");
        assert_eq!(list.command, CommandName::Table);
        assert_eq!(list.table_action, Some(TableAction::List));
        assert_eq!(list.schema.as_deref(), Some("sales"));
        assert_eq!(list.format, Some(Format::Json));
        let sample = request("table sample app.db users -n 50");
        assert_eq!(sample.table_action, Some(TableAction::Sample));
        assert_eq!(sample.table, Some(OsString::from("users")));
        assert_eq!(sample.scan.limit, Some(50));
        assert_eq!(request("table show app.db sales.totals").file, PathBuf::from("app.db"));
        assert_eq!(usage_message("table"), "table needs list, show or sample");
        assert_eq!(usage_message("table frob app.db"), "table needs list, show or sample");
        assert_eq!(usage_message("table list"), "eggdb table list needs a file");
        assert_eq!(
            usage_message("table show"),
            "eggdb table show needs a file and a table name"
        );
        assert_eq!(usage_message("table sample app.db"), "eggdb table sample needs a table name");
        assert_eq!(request("table list app.db -n 5").scan.limit, Some(5));
        assert_eq!(request("table list app.db --limit 5000").scan.limit, Some(5000));
        assert_eq!(
            usage_message("table show app.db users -n 5"),
            "unknown option '-n' for eggdb table show"
        );
        assert_eq!(
            usage_message("table sample app.db users --after t1.811c9dc5"),
            "unknown option '--after' for eggdb table sample"
        );
        assert_eq!(
            request("table list app.db --after t1.811c9dc5").after.as_deref(),
            Some("t1.811c9dc5")
        );
        assert_eq!(
            usage_message("table show app.db users --schema x"),
            "unknown option '--schema' for eggdb table show"
        );
        assert_eq!(
            usage_message("table sample app.db users --limit 5000"),
            "--limit needs a whole number from 1 to 1000, not '5000'; for more rows use eggdb sql"
        );
        assert_eq!(
            usage_message("table sample app.db users -n 0"),
            "-n needs a whole number from 1 to 1000, not '0'; for more rows use eggdb sql"
        );
        let explain = request("explain app.db --dialect sqlite -c select");
        assert_eq!(explain.command, CommandName::Explain);
        assert_eq!(explain.dialect, eggdb_sql::Dialect::Sqlite);
        assert_eq!(explain.sql, Some(SqlSource::Command("select".to_owned())));
        assert_eq!(
            usage_message("explain app.db"),
            "eggdb explain needs a statement: give it with -c"
        );
        assert!(usage_message("explain app.db -F hex -c x").contains("--format hex"));
    }

    #[test]
    fn row_list_takes_a_file_a_table_a_page_size_and_a_cursor_of_its_own_list() {
        let page = request("row list app.db orders -n 2 --after r1.818549630180000002 --json");
        assert_eq!(page.command, CommandName::Row);
        assert_eq!(page.file, PathBuf::from("app.db"));
        assert_eq!(page.table, Some(OsString::from("orders")));
        assert_eq!(page.scan.limit, Some(2));
        assert_eq!(page.after.as_deref(), Some("r1.818549630180000002"));
        assert_eq!(page.format, Some(Format::Json));
        let all = request("row list app.db sales.orders");
        assert_eq!((all.scan.limit, all.after), (None, None));
        assert_eq!(usage_message("row"), "row needs list");
        assert_eq!(usage_message("row show app.db orders"), "row needs list");
        assert_eq!(usage_message("row list"), "eggdb row list needs a file and a table name");
        assert_eq!(usage_message("row list app.db"), "eggdb row list needs a table name");
        assert_eq!(
            usage_message("row list app.db orders extra"),
            "too many arguments for eggdb row: 'extra'"
        );
        assert_eq!(
            usage_message("row list app.db orders --limit 0"),
            "--limit needs a whole number of at least 1, not '0'"
        );
        assert_eq!(
            usage_message("row list app.db orders -n x"),
            "-n needs a whole number of at least 1, not 'x'"
        );
        assert_eq!(
            usage_message("row list app.db orders --after 42"),
            "--after needs the cursor printed at the end of the page before, not '42'. Leave --after out for the first page."
        );
        assert_eq!(
            usage_message("row list app.db orders --after t1.811c9dc5"),
            "--after got a cursor of eggdb table list ('t1.811c9dc5'), which eggdb row list cannot use. Page each list with the cursors its own pages print."
        );
        assert_eq!(
            usage_message("table list app.db --after r1.818549630180000002"),
            "--after got a cursor of eggdb row list ('r1.818549630180000002'), which eggdb table list cannot use. Page each list with the cursors its own pages print."
        );
        assert_eq!(
            usage_message("row list app.db orders --schema x"),
            "unknown option '--schema' for eggdb row"
        );
        assert_eq!(
            parse_words("help row list --json"),
            Ok(Invocation::HelpJson(Some(CommandName::Row), Some("list")))
        );
    }

    #[test]
    fn sql_opens_read_only_with_read_only_but_never_creates_then() {
        assert!(request("sql app.db --read-only -c select").read_only);
        assert_eq!(
            usage_message("sql app.db --read-only --create -c select"),
            "--read-only cannot be combined with --create: a read-only open never creates a file"
        );
    }

    #[test]
    fn help_names_a_noun_and_one_of_its_verbs() {
        for words in ["help table", "help table list", "help collection del", "table --help"] {
            assert!(
                matches!(parse_words(words), Ok(Invocation::Help(Some(_)))),
                "{words}"
            );
        }
        assert_eq!(
            parse_words("help table sample --json"),
            Ok(Invocation::HelpJson(Some(CommandName::Table), Some("sample")))
        );
        assert_eq!(
            parse_words("table show --help --json"),
            Ok(Invocation::HelpJson(Some(CommandName::Table), Some("show")))
        );
        assert_eq!(usage_message("help table frob"), "table needs list, show or sample");
        assert_eq!(
            usage_message("help scan extra"),
            "too many arguments for eggdb: 'extra'"
        );
        assert_eq!(parse_words("version --json"), Ok(Invocation::VersionJson));
    }

    #[test]
    fn hex_mistakes_are_usage_errors() {
        assert_eq!(
            usage_message("get a 0x4"),
            "hex needs an even number of digits: 0x4"
        );
    }
}
