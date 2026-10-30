//! EggDB's command model (D143): one registry of the `eggdb` commands and of the MCP tools
//! agents call, which the CLI, its help and the MCP server all read, so the help, the tools and
//! the docs cannot drift apart.
//!
//! - A [`CommandDefinition`] describes one command: its words, help, options, operands,
//!   outputs per format, the most it can do ([`Effect`]) and its errors with their exit codes.
//! - A [`ToolDefinition`] describes one MCP tool: its arguments and how each maps to its CLI
//!   twin, its effect, the grant it needs, and the MCP annotations that follow from those
//!   ([`ToolDefinition::annotations`]).
//! - Every error code EggDB gives has a [`Category`] shared with EggOS, so an agent or a
//!   script can rely on the category across both products ([`ERROR_CODES`]).
//!
//! - A paging cursor ([`encode_cursor`], [`decode_cursor`]) is what a list command prints and
//!   a list tool returns as `next_cursor`, so the next page starts after the last item (D145).
//!
//! The shared part of the model is EggOS's `docs/ux/command-model.md`; EggDB's tables are in
//! `docs/ux/command-model.md` in this repository. The registry is `'static` data with no
//! dependencies: nothing here runs a command.

mod cursor;
mod registry;

pub use cursor::{
    CursorList, CursorRefusal, LONGEST_CURSOR, check_cursor, decode_cursor, encode_cursor,
    shown_cursor, table_at, table_position,
};
pub use registry::{
    COMMANDS, COMMON_OPTIONS, CORE_VERBS, ERROR_CODES, KEPT_NAMES, NOUNS, SHIPPED_TOOL_NAMES,
    TOOLS,
};

/// The most a use of a command or a call of a tool can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// It reads and changes nothing.
    Read,
    /// It adds or replaces something, and destroys nothing.
    Change,
    /// It may remove or overwrite data that cannot be brought back.
    Destroy,
    /// It runs anything: any program or any statement.
    Control,
}

impl Effect {
    /// Every effect, from the least to the most.
    pub const ALL: [Self; 4] = [Self::Read, Self::Change, Self::Destroy, Self::Control];

    /// The effect's name in JSON and in the docs: `read`, `change`, `destroy` or `control`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Change => "change",
            Self::Destroy => "destroy",
            Self::Control => "control",
        }
    }

    /// What the effect means, in a few words an agent reads.
    pub const fn meaning(self) -> &'static str {
        match self {
            Self::Read => "changes nothing",
            Self::Change => "adds or replaces data",
            Self::Destroy => "removes data; needs confirmation where a check says so",
            Self::Control => "runs anything it is given",
        }
    }
}

/// The category of an error, shared by EggDB and EggOS: what an agent or a script can rely on
/// whatever the product's own error code is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// The command or call was given wrong arguments.
    Usage,
    /// What it names is not there.
    NotFound,
    /// What it would make is already there.
    Exists,
    /// Something else holds what it needs; try again later.
    Busy,
    /// It needs to write where only reading is allowed.
    ReadOnly,
    /// The agent needs a grant or a flag it was not given.
    NeedsGrant,
    /// It needs the user's confirmation first.
    NeedsConfirmation,
    /// The user said no, or the call was cancelled.
    Declined,
    /// The product refuses it by its rules.
    Refused,
    /// It is larger than a limit allows.
    TooLarge,
    /// It ran out of its time.
    Timeout,
    /// The data is damaged.
    Damaged,
    /// It failed for another reason, as the message says.
    Failed,
}

impl Category {
    /// Every category, in the order the docs list them.
    pub const ALL: [Self; 13] = [
        Self::Usage,
        Self::NotFound,
        Self::Exists,
        Self::Busy,
        Self::ReadOnly,
        Self::NeedsGrant,
        Self::NeedsConfirmation,
        Self::Declined,
        Self::Refused,
        Self::TooLarge,
        Self::Timeout,
        Self::Damaged,
        Self::Failed,
    ];

    /// The category's name in JSON: `usage`, `not_found`, `exists`, `busy`, `read_only`,
    /// `needs_grant`, `needs_confirmation`, `declined`, `refused`, `too_large`, `timeout`,
    /// `damaged` or `failed`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::NotFound => "not_found",
            Self::Exists => "exists",
            Self::Busy => "busy",
            Self::ReadOnly => "read_only",
            Self::NeedsGrant => "needs_grant",
            Self::NeedsConfirmation => "needs_confirmation",
            Self::Declined => "declined",
            Self::Refused => "refused",
            Self::TooLarge => "too_large",
            Self::Timeout => "timeout",
            Self::Damaged => "damaged",
            Self::Failed => "failed",
        }
    }

    /// What an agent or a script does about an error of this category, in a few words.
    pub const fn meaning(self) -> &'static str {
        match self {
            Self::Usage => "fix your arguments",
            Self::NotFound => "what you named is not there",
            Self::Exists => "it is already there",
            Self::Busy => "another program holds it; retry",
            Self::ReadOnly => "this session cannot write",
            Self::NeedsGrant => "the session lacks the grant",
            Self::NeedsConfirmation => "ask the user, then confirm",
            Self::Declined => "the user or client said no",
            Self::Refused => "EggDB will not do this as asked; a retry will not help",
            Self::TooLarge => "over a size limit",
            Self::Timeout => "ran past the time limit",
            Self::Damaged => "the file is damaged",
            Self::Failed => "something went wrong on EggDB's side; tell the user",
        }
    }
}

/// One of the core verbs: a fixed meaning and the effect it usually has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verb {
    /// The verb, such as `list`.
    pub name: &'static str,
    /// What it means.
    pub means: &'static str,
    /// The effect it usually has.
    pub usual_effect: Effect,
}

/// One of EggDB's error codes and its shared category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorCode {
    /// The code, such as `database_busy`; shipped codes never change.
    pub code: &'static str,
    /// Its shared category.
    pub category: Category,
    /// When it is given.
    pub when: &'static str,
}

/// An error a command can end with, and the exit code it ends with then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandError {
    /// The error code, one of [`ERROR_CODES`].
    pub code: &'static str,
    /// The exit code of the command with this error.
    pub exit: u8,
}

/// An option of a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptionDefinition {
    /// The short letter, such as `C` for `-C`.
    pub short: Option<char>,
    /// The long name, without its dashes, such as `collection`.
    pub long: &'static str,
    /// The value's placeholder without its brackets, such as `name`, or `None` for a flag.
    pub value: Option<&'static str>,
    /// Whether it may be given more than once.
    pub repeatable: bool,
    /// The value without the option, when there is one.
    pub default: Option<&'static str>,
    /// One line of help.
    pub help: &'static str,
}

/// The type of an operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperandType {
    /// A path on this machine.
    Path,
    /// UTF-8 text.
    Text,
    /// Bytes, written as text, `0x` hex, `@file`, `@-` or `=text`.
    Bytes,
    /// A whole number within a range.
    Integer {
        /// The smallest value.
        least: i64,
        /// The largest value.
        most: i64,
    },
    /// One of a list of words.
    Choice(&'static [&'static str]),
}

impl OperandType {
    /// The type's name in JSON: `path`, `text`, `bytes`, `integer` or `choice`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Text => "text",
            Self::Bytes => "bytes",
            Self::Integer { .. } => "integer",
            Self::Choice(_) => "choice",
        }
    }
}

/// How many times an operand is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Count {
    /// Once or not at all, `0..1`.
    Optional,
    /// Exactly once, `1`.
    One,
    /// Once or more, `1..`.
    OneOrMore,
}

impl Count {
    /// The count as the docs write it: `0..1`, `1` or `1..`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Optional => "0..1",
            Self::One => "1",
            Self::OneOrMore => "1..",
        }
    }
}

/// An operand of a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperandDefinition {
    /// Its name, such as `file`.
    pub name: &'static str,
    /// Its type.
    pub kind: OperandType,
    /// How many times it is given.
    pub count: Count,
    /// Whether `-`, or `@-` for bytes, stands for stdin, or for stdout where it is written to.
    pub standard_stream: bool,
}

/// The type of a field in a command's JSON output or a tool's result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    /// A JSON string of UTF-8 text.
    Text,
    /// Text when the bytes are UTF-8, otherwise the field's name with `_hex` and hex digits.
    Bytes,
    /// A JSON integer.
    Integer,
    /// `true` or `false`.
    Boolean,
    /// A time, RFC 3339 in UTC.
    Time,
    /// A JSON object.
    Object,
    /// A JSON array.
    Array,
}

impl FieldType {
    /// The type's name in JSON.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Bytes => "bytes",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
            Self::Time => "time",
            Self::Object => "object",
            Self::Array => "array",
        }
    }
}

/// A field of a command's JSON output or of a tool's result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldDefinition {
    /// The field's name.
    pub name: &'static str,
    /// Its type.
    pub kind: FieldType,
}

/// What a command writes to stdout in one format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    /// A list, one record per line in JSON, with these fields.
    Records(&'static [FieldDefinition]),
    /// One value, with these fields.
    Object(&'static [FieldDefinition]),
    /// SQL results, whose columns are known only when the statement runs.
    Table,
    /// Bytes as they are stored.
    Content,
    /// Nothing.
    Nothing,
}

impl OutputKind {
    /// The kind's name in JSON: `records`, `object`, `table`, `content` or `none`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Records(_) => "records",
            Self::Object(_) => "object",
            Self::Table => "table",
            Self::Content => "content",
            Self::Nothing => "none",
        }
    }

    /// The fields of records or of an object; none for the other kinds.
    pub const fn fields(self) -> &'static [FieldDefinition] {
        match self {
            Self::Records(fields) | Self::Object(fields) => fields,
            Self::Table | Self::Content | Self::Nothing => &[],
        }
    }
}

/// An output format of the `eggdb` command (`--format`, D90).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Text for people, the default on a terminal.
    Text,
    /// JSON for scripts, the default in a pipe for most commands.
    Json,
    /// Hexadecimal bytes.
    Hex,
    /// The bytes themselves.
    Raw,
}

impl Format {
    /// The format's name, as `--format` takes it.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Json => "json",
            Self::Hex => "hex",
            Self::Raw => "raw",
        }
    }
}

/// What a command writes in one format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputDefinition {
    /// The format.
    pub format: Format,
    /// What it writes in that format.
    pub kind: OutputKind,
}

/// One `eggdb` command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandDefinition {
    /// The words that name it after `eggdb`, such as `["get"]`.
    pub words: &'static [&'static str],
    /// Other spellings of its last word, such as `delete` for `del` and `del` for
    /// `collection delete`.
    pub aliases: &'static [&'static str],
    /// Its operands as the main help lists them, such as `<file> <key>`.
    pub synopsis: &'static str,
    /// One sentence, as the main help lists it.
    pub summary: &'static str,
    /// What it does and its limits, and that what it returns is data, never instructions.
    pub description: &'static str,
    /// Its one-line usage, as `eggdb help <command>` and usage errors print it.
    pub usage: &'static str,
    /// The text `eggdb help <command>` prints after the usage line; the commands of one noun
    /// share it, and it is empty for `help` and `version`, which the main help describes.
    pub help: &'static str,
    /// Its own options; [`COMMON_OPTIONS`] apply to it as well.
    pub options: &'static [OptionDefinition],
    /// Its operands, in order.
    pub operands: &'static [OperandDefinition],
    /// What it writes, per format.
    pub outputs: &'static [OutputDefinition],
    /// The most a person's use of it can do.
    pub max_effect: Effect,
    /// The errors it can end with, and their exit codes.
    pub errors: &'static [CommandError],
    /// Command lines that show it in use.
    pub examples: &'static [&'static str],
    /// Where its front-end in the GUI is, or why it has none.
    pub surfaces: &'static str,
}

impl CommandDefinition {
    /// The command's name: its words joined by spaces.
    pub fn name(&self) -> String {
        self.words.join(" ")
    }

    /// The exit code this command ends with for the error `code`, if it declares it.
    pub fn exit_for(&self, code: &str) -> Option<u8> {
        self.errors
            .iter()
            .find(|error| error.code == code)
            .map(|error| error.exit)
    }

    /// Whether it writes JSON with `--format json`.
    pub fn has_json(&self) -> bool {
        self.outputs
            .iter()
            .any(|output| output.format == Format::Json)
    }
}

/// What an agent needs before a tool is offered to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    /// Nothing: every session has the tool.
    Nothing,
    /// `eggdb mcp --allow-write`.
    AllowWrite,
    /// The key-value store, which every server grants unless it was started with `--tables`.
    KeyValueStore,
}

impl Grant {
    /// The grant's name in JSON, or `None` for no grant.
    pub const fn name(self) -> Option<&'static str> {
        match self {
            Self::Nothing => None,
            Self::AllowWrite => Some("allow_write"),
            Self::KeyValueStore => Some("key_value_store"),
        }
    }
}

/// What one `eggdb mcp` server grants its agent, from the options it was started with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grants {
    /// Started with `--allow-write`.
    pub allow_write: bool,
    /// Started without `--tables`, so the agent reaches the key-value store.
    pub key_value_store: bool,
}

/// The JSON type of a tool's argument, as its input schema declares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgumentType {
    /// A string.
    String,
    /// A whole number from `least` to `most`, `default` when it is left out.
    Integer {
        /// The smallest value.
        least: u64,
        /// The largest value.
        most: u64,
        /// The value when it is left out.
        default: u64,
    },
    /// `true` or `false`, `default` when it is left out.
    Boolean {
        /// The value when it is left out.
        default: bool,
    },
    /// The name of a SQL dialect, `postgresql` when it is left out.
    Dialect,
}

impl ArgumentType {
    /// The JSON Schema type: `string`, `integer` or `boolean`.
    pub const fn json_type(self) -> &'static str {
        match self {
            Self::String | Self::Dialect => "string",
            Self::Integer { .. } => "integer",
            Self::Boolean { .. } => "boolean",
        }
    }
}

/// Where a tool's argument goes on its CLI twin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgumentTarget {
    /// To this operand, as text.
    Operand(&'static str),
    /// To this operand, as `0x` and the argument's hex digits.
    OperandHex(&'static str),
    /// To this option, as text.
    Option(&'static str),
    /// To this option, as `0x` and the argument's hex digits.
    OptionHex(&'static str),
    /// To stdin.
    Stdin,
    /// A limit of the call that the CLI has no need of.
    CallLimit,
    /// The user's confirmation, which the CLI never asks for.
    Confirmation,
    /// Nowhere: the tool has no CLI twin yet.
    NoTwin,
}

impl ArgumentTarget {
    /// How `help --json` writes it: `option:<long>`, `operand:<name>` (hex arguments map to
    /// the same option or operand, which the CLI spells `0x…`), `stdin`, or `None` for an
    /// argument without a CLI counterpart.
    pub fn json_name(self) -> Option<String> {
        match self {
            Self::Operand(name) | Self::OperandHex(name) => Some(format!("operand:{name}")),
            Self::Option(name) | Self::OptionHex(name) => Some(format!("option:{name}")),
            Self::Stdin => Some("stdin".to_owned()),
            Self::CallLimit | Self::Confirmation | Self::NoTwin => None,
        }
    }
}

/// One argument of a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArgumentDefinition {
    /// Its name in the call's `arguments`.
    pub name: &'static str,
    /// Its JSON type.
    pub kind: ArgumentType,
    /// Its description in the input schema.
    pub description: &'static str,
    /// Whether every call must give it.
    pub required: bool,
    /// Where it goes on the CLI twin.
    pub maps_to: ArgumentTarget,
}

/// The per-call check that decides whether a call needs the user's confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmCheck {
    /// The statement is destructive (D121): `DROP`, `TRUNCATE`, a column dropped or retyped,
    /// or a `DELETE` or `UPDATE` whose `WHERE` names no column of its table.
    DestructiveStatement,
}

impl ConfirmCheck {
    /// The check's name in JSON.
    pub const fn name(self) -> &'static str {
        match self {
            Self::DestructiveStatement => "destructive_statement",
        }
    }

    /// When the check asks for confirmation, in one sentence people and agents read
    /// (`help --json`'s `confirm_when`).
    pub const fn when(self) -> &'static str {
        match self {
            Self::DestructiveStatement => {
                "the statement is destructive: DROP, TRUNCATE, a column dropped or retyped, or a DELETE or UPDATE whose WHERE names no column of its table"
            }
        }
    }
}

/// The MCP annotations of a tool, as `tools/list` sends them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Annotations {
    /// `readOnlyHint`.
    pub read_only: bool,
    /// `destructiveHint`.
    pub destructive: bool,
    /// `idempotentHint`.
    pub idempotent: bool,
    /// `openWorldHint`.
    pub open_world: bool,
}

/// One MCP tool of `eggdb mcp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolDefinition {
    /// The tool's name; new tools join their command's words with `_`.
    pub name: &'static str,
    /// Its title, in `tools/list` and its annotations.
    pub title: &'static str,
    /// Its description on a server started with `--allow-write`, or on every server when
    /// [`ToolDefinition::read_only_server_description`] is `None`.
    pub description: &'static str,
    /// Its description on a read-only server, when that differs.
    pub read_only_server_description: Option<&'static str>,
    /// The words of the command it runs, or `None` while it has no CLI twin.
    pub command: Option<&'static [&'static str]>,
    /// The options it always passes to that command.
    pub fixed: &'static [&'static str],
    /// Its arguments, in the order its input schema lists them.
    pub arguments: &'static [ArgumentDefinition],
    /// The most one call can do.
    pub max_effect: Effect,
    /// What an agent needs before the tool is offered.
    pub grant: Grant,
    /// Whether calling it twice with the same arguments does what calling it once does.
    pub idempotent: bool,
    /// Whether it reaches outside the database.
    pub open_world: bool,
    /// The per-call check for the user's confirmation, if any.
    pub confirm: Option<ConfirmCheck>,
    /// The fields of its result.
    pub result: &'static [FieldDefinition],
    /// The result's fields that its CLI twin does not print: the data notice, the per-call
    /// limits and what the call reports about itself.
    pub envelope: &'static [&'static str],
}

impl ToolDefinition {
    /// The annotations that follow from the tool's effect and fields: `read` is read-only;
    /// `change` is neither read-only nor destructive; `destroy` and `control` are destructive.
    pub const fn annotations(&self) -> Annotations {
        let (read_only, destructive) = match self.max_effect {
            Effect::Read => (true, false),
            Effect::Change => (false, false),
            Effect::Destroy | Effect::Control => (false, true),
        };
        Annotations {
            read_only,
            destructive,
            idempotent: self.idempotent,
            open_world: self.open_world,
        }
    }

    /// Whether a server that grants `grants` offers the tool: a tool that needs `--allow-write`
    /// is offered only by a server started with it, and one that reads the key-value store only
    /// by a server started without `--tables`.
    pub const fn offered(&self, grants: Grants) -> bool {
        match self.grant {
            Grant::Nothing => true,
            Grant::AllowWrite => grants.allow_write,
            Grant::KeyValueStore => grants.key_value_store,
        }
    }

    /// The tool's description on a server started with or without `--allow-write`.
    pub const fn description_for(&self, allow_write: bool) -> &'static str {
        match (allow_write, self.read_only_server_description) {
            (false, Some(description)) => description,
            _ => self.description,
        }
    }

    /// The names of its arguments, in order.
    pub fn argument_names(&self) -> Vec<&'static str> {
        self.arguments.iter().map(|argument| argument.name).collect()
    }
}

/// The command named by these words, or by them with an alias as the last word.
pub fn command(words: &[&str]) -> Option<&'static CommandDefinition> {
    let (last, first) = words.split_last()?;
    COMMANDS.iter().find(|command| {
        command.words == words
            || (command.words.len() == words.len()
                && command.words[..first.len()] == *first
                && command.aliases.contains(last))
    })
}

/// The tool of this name.
pub fn tool(name: &str) -> Option<&'static ToolDefinition> {
    TOOLS.iter().find(|tool| tool.name == name)
}

/// The category of an EggDB error code, or `None` for a code the registry does not know.
pub fn category_of(code: &str) -> Option<Category> {
    ERROR_CODES
        .iter()
        .find(|known| known.code == code)
        .map(|known| known.category)
}

#[cfg(test)]
mod tests;
