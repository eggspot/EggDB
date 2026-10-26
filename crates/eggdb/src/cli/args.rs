use super::byte_argument::ByteArgument;
use super::failure::Failure;
use super::render;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

const RESERVED_COMMANDS: [&str; 3] = ["shell", "backup", "compact"];
const SMALLEST_PAGE_SIZE: usize = 4096;
const LARGEST_PAGE_SIZE: usize = 65536;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandName {
    Create,
    Put,
    Get,
    Del,
    Scan,
    Check,
    Stats,
    Ui,
    Export,
    Import,
}

impl CommandName {
    fn from_name(name: &[u8]) -> Option<Self> {
        Some(match name {
            b"create" => Self::Create,
            b"put" => Self::Put,
            b"get" => Self::Get,
            b"del" | b"delete" => Self::Del,
            b"scan" => Self::Scan,
            b"check" => Self::Check,
            b"stats" => Self::Stats,
            b"ui" => Self::Ui,
            b"export" => Self::Export,
            b"import" => Self::Import,
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
            Self::Ui => "ui",
            Self::Export => "export",
            Self::Import => "import",
        }
    }

    fn takes(self, option: OptionName) -> bool {
        match option {
            OptionName::Cache | OptionName::Help | OptionName::Version => true,
            OptionName::Color => true,
            OptionName::Format => self != Self::Ui,
            OptionName::Mono | OptionName::Ascii | OptionName::ReadOnly => self == Self::Ui,
            OptionName::PageSize => matches!(self, Self::Create | Self::Import),
            OptionName::Create => matches!(self, Self::Put | Self::Import),
            OptionName::OnConflict | OptionName::DryRun => self == Self::Import,
            OptionName::Prefix
            | OptionName::From
            | OptionName::To
            | OptionName::Reverse
            | OptionName::Limit
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
    Color,
    Cache,
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
}

impl OptionName {
    fn from_long(name: &[u8]) -> Option<Self> {
        Some(match name {
            b"format" => Self::Format,
            b"color" => Self::Color,
            b"cache" => Self::Cache,
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
            _ => return None,
        })
    }

    fn takes_value(self) -> bool {
        !matches!(
            self,
            Self::Help
                | Self::Version
                | Self::Create
                | Self::Reverse
                | Self::KeysOnly
                | Self::Mono
                | Self::Ascii
                | Self::ReadOnly
                | Self::DryRun
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Invocation {
    Help(Option<CommandName>),
    NoArguments,
    Version,
    Run(Box<Request>),
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
    pub(crate) page_size: Option<usize>,
    pub(crate) create: bool,
    pub(crate) scan: ScanOptions,
    pub(crate) mono: bool,
    pub(crate) ascii: bool,
    pub(crate) read_only: bool,
    pub(crate) export_file: Option<ExportFile>,
    pub(crate) on_conflict: ConflictPolicy,
    pub(crate) dry_run: bool,
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
    if has(OptionName::Help) {
        return match first {
            Some(b"help" | b"version") | None => Ok(Invocation::Help(None)),
            Some(name) if command.is_none() => Err(unknown_command(name)),
            _ => Ok(Invocation::Help(command)),
        };
    }
    if has(OptionName::Version) {
        return Ok(Invocation::Version);
    }
    let Some(first) = first else {
        return Err(Failure::usage(
            "no command given. Commands: create, put, get, del, scan, check, stats, ui, export, import, help, version.",
            None,
        ));
    };
    match first {
        b"help" => return parse_help(&tokens.positionals[1..]),
        b"version" if tokens.positionals.len() == 1 => return Ok(Invocation::Version),
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

fn parse_help(arguments: &[&OsStr]) -> Result<Invocation, Failure> {
    match arguments {
        [] => Ok(Invocation::Help(None)),
        [name] => {
            let name = name.as_encoded_bytes();
            match CommandName::from_name(name) {
                Some(command) => Ok(Invocation::Help(Some(command))),
                None if name == b"help" || name == b"version" => Ok(Invocation::Help(None)),
                None if is_reserved(name) => Err(unavailable(name)),
                None => Err(unknown_command(name)),
            }
        }
        [_, extra @ ..] => Err(too_many_arguments(extra, None)),
    }
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
        page_size: None,
        create: false,
        scan: ScanOptions::default(),
        mono: false,
        ascii: false,
        read_only: false,
        export_file: None,
        on_conflict: ConflictPolicy::Error,
        dry_run: false,
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
            OptionName::Color => request.color = parse_color(value).map_err(usage)?,
            OptionName::Cache => {
                request.cache_bytes = Some(parse_size(&option.spelled, value).map_err(usage)?);
            }
            OptionName::PageSize => {
                request.page_size = Some(parse_page_size(&option.spelled, value).map_err(usage)?);
            }
            OptionName::Create => request.create = true,
            OptionName::Prefix => request.scan.prefix = Some(byte_argument()?),
            OptionName::From => request.scan.from = Some(byte_argument()?),
            OptionName::To => request.scan.to = Some(byte_argument()?),
            OptionName::Reverse => request.scan.reverse = true,
            OptionName::Limit => {
                request.scan.limit = Some(parse_limit(&option.spelled, value).map_err(usage)?);
            }
            OptionName::KeysOnly => request.scan.keys_only = true,
            OptionName::Mono => request.mono = true,
            OptionName::Ascii => request.ascii = true,
            OptionName::ReadOnly => request.read_only = true,
            OptionName::OnConflict => {
                request.on_conflict = parse_on_conflict(value).map_err(usage)?
            }
            OptionName::DryRun => request.dry_run = true,
            OptionName::Help | OptionName::Version => {}
        }
    }
    if request.scan.prefix.is_some() && (request.scan.from.is_some() || request.scan.to.is_some()) {
        return Err(usage(
            "--prefix cannot be combined with --from or --to".to_owned(),
        ));
    }
    let arguments = &tokens.positionals[1..];
    let (least, most) = match command {
        CommandName::Create
        | CommandName::Scan
        | CommandName::Check
        | CommandName::Stats
        | CommandName::Ui => (1, 1),
        CommandName::Export | CommandName::Import => (1, 2),
        CommandName::Get | CommandName::Del => (2, 2),
        CommandName::Put => (2, 3),
    };
    if arguments.len() < least {
        return Err(usage(missing_arguments(command, arguments.len())));
    }
    if arguments.len() > most {
        return Err(too_many_arguments(&arguments[most..], Some(command)));
    }
    request.file = PathBuf::from(arguments[0]);
    if matches!(command, CommandName::Export | CommandName::Import) {
        request.export_file = arguments.get(1).map(|argument| export_file(argument));
        return Ok(request);
    }
    let byte_argument = |argument: &&OsStr| {
        ByteArgument::parse(argument).map_err(|message| Failure::usage(message, Some(command)))
    };
    request.key = arguments.get(1).map(byte_argument).transpose()?;
    request.value = arguments.get(2).map(byte_argument).transpose()?;
    Ok(request)
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
        (CommandName::Get | CommandName::Del, 0) => "a file and a key",
        (CommandName::Get | CommandName::Del, _) => "a key",
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

fn is_reserved(name: &[u8]) -> bool {
    RESERVED_COMMANDS
        .iter()
        .any(|reserved| reserved.as_bytes() == name)
}

fn unavailable(name: &[u8]) -> Failure {
    Failure::Unavailable {
        message: format!(
            "eggdb {} is not in this release yet; see docs/roadmap.md",
            render::text(name)
        ),
    }
}

fn unknown_command(name: &[u8]) -> Failure {
    if is_reserved(name) {
        return unavailable(name);
    }
    Failure::usage(
        format!(
            "unknown command '{}'. Commands: create, put, get, del, scan, check, stats, ui, export, import, help, version.",
            render::text(name)
        ),
        None,
    )
}

fn unknown_option(spelled: &str, command: CommandName) -> Failure {
    Failure::usage(
        format!("unknown option '{spelled}' for eggdb {}", command.name()),
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
        ByteArgument, ColorChoice, CommandName, Format, Invocation, Request, ScanOptions, parse,
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
            Err(Failure::Usage { message, .. } | Failure::Unavailable { message }) => message,
            other => panic!("{words}: {other:?}"),
        }
    }

    fn bytes(text: &str) -> Option<ByteArgument> {
        Some(ByteArgument::Bytes(text.as_bytes().to_vec()))
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
            "unknown command 'frobnicate'. Commands: create, put, get, del, scan, check, stats, ui, export, import, help, version."
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
            "unknown command 'frob'. Commands: create, put, get, del, scan, check, stats, ui, export, import, help, version."
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
            "unknown command '0x66721b5b33316d6f62'. Commands: create, put, get, del, scan, check, stats, ui, export, import, help, version."
        );
        assert_eq!(
            usage_message("scan a -F j\u{7}"),
            "--format must be text, json, hex or raw, not '0x6a07'"
        );
    }

    #[test]
    fn reserved_commands_point_to_the_roadmap() {
        for name in ["shell", "backup", "compact"] {
            assert_eq!(
                usage_message(&format!("{name} app.db")),
                format!("eggdb {name} is not in this release yet; see docs/roadmap.md")
            );
            assert_eq!(
                usage_message(&format!("help {name}")),
                format!("eggdb {name} is not in this release yet; see docs/roadmap.md")
            );
        }
    }

    #[test]
    fn hex_mistakes_are_usage_errors() {
        assert_eq!(
            usage_message("get a 0x4"),
            "hex needs an even number of digits: 0x4"
        );
    }
}
