use super::args::{CommandName, Format};
use super::commands::default_format;
use super::render::{JsonObject, json_string};
use eggdb_commands::{
    COMMANDS, COMMON_OPTIONS, Category, CommandDefinition, Effect, OptionDefinition, TOOLS,
    ToolDefinition,
};

pub(crate) const MAIN: &str = "\
eggdb: one database for every kind of data. This release: an embedded key-value store in one file,
with a first part of SQL, served over the PostgreSQL protocol.

Usage: eggdb <command> <file> [arguments] [options]

Commands:
  create <file>                 make an empty database (fails if the file exists)
  put    <file> <key> [value]   store a value; the value may come from stdin
  get    <file> <key>           print a value (exit 1 if the key is not there)
  del    <file> <key>           remove a key (exit 1 if it was not there)
  scan   <file>                 list entries in key order; see --prefix, --from, --to
  ttl    <file> <key>           print how long a key still lives (exit 1 if it is not there)
  expire <file> <key> <time>    give a key a time to live: 30s, 5m, 1h30m, 7d, 1500ms
  persist <file> <key>          remove a key's time to live, so it stays until deleted
  incr   <file> <key> [n]       add n (1) to a counter and print the new value
  decr   <file> <key> [n]       subtract n (1) from a counter and print the new value
  cas    <file> <key> <expected> <new>
                                replace a value only while it is <expected>; see eggdb help cas
  check  <file>                 read every page and report the file's health
  stats  <file>                 size, pages, last commit and log, without reading pages
  backup <file> <copy>          write a consistent copy of the file while readers keep working
  compact <file>                rewrite the file without free or leaked pages, in place
  ui     <file>                 browse and edit keys full screen in the terminal
  export <file> [out]           write every entry to a JSON-lines file, or to stdout
  import <file> [in]            load an export, or a CSV file into a table with --csv
  sql    <file> [script]        run SQL (PostgreSQL or MySQL) from -c, a script or stdin
  explain <file>                show the plan for one statement given with -c, without running it
  table  list|show|sample <file> [table]
                                tables and views: list them, show one, sample rows; see eggdb help table
  row    list <file> <table>    the rows of one table, as stored, a page at a time; see eggdb help row
  serve  <file>                 serve the file to PostgreSQL and MySQL clients; see --listen
  shell  <file>                 an interactive prompt for key-value commands and SQL; see -c
  restore <file> [dump]         restore a PostgreSQL dump, or a SQL Server or Oracle script
  collection list|add|delete <file> [name]
                                a named key space inside the file; see eggdb help collection
  mcp    <file>                 serve the file to an AI agent over MCP; read-only unless --allow-write
  audit  <file>                 list the calls AI agents made through eggdb mcp
  help   [command]              this text, or one command's help
  version                       print the version

Keys and values: text as typed, 0x.. for hex, @file for a file's bytes, @- for stdin,
=text for text that starts with 0x, @ or =.

Options for every command:
  -F, --format <text|json|hex|raw>   default: text on a terminal, json in a pipe (raw for get)
      --color <auto|always|never>    default: auto (a terminal, unless NO_COLOR is set)
      --cache <bytes>                page cache size, e.g. 64m (default fits the value)
  -h, --help                         help for eggdb or for one command
  -V, --version                      print the version

Exit codes: 0 ok, 1 not found, 2 usage, 3 database error, 4 damaged, 5 busy.
Guide: docs/cli.md in the repository, https://github.com/eggspot/EggDB
";

pub(crate) fn definitions(command: CommandName) -> Vec<&'static CommandDefinition> {
    COMMANDS
        .iter()
        .filter(|definition| definition.words.first() == Some(&command.name()))
        .collect()
}

pub(crate) fn usage_line(command: CommandName) -> String {
    definitions(command)
        .iter()
        .map(|definition| definition.usage)
        .collect::<Vec<_>>()
        .join("\n       ")
}

pub(crate) fn command_help(command: CommandName) -> String {
    let body = definitions(command)
        .first()
        .map_or("", |definition| definition.help);
    format!("Usage: {}\n\n{body}", usage_line(command))
}

pub(crate) fn registry_json(command: Option<CommandName>, verb: Option<&str>) -> String {
    let Some(command) = command else {
        return format!("{}\n", whole_registry_json());
    };
    definitions(command)
        .into_iter()
        .filter(|definition| verb.is_none_or(|verb| definition.words.get(1) == Some(&verb)))
        .map(|definition| format!("{}\n", command_json(definition)))
        .collect()
}

pub(crate) fn version_json() -> String {
    JsonObject::new().string("version", eggdb::VERSION).line()
}

fn whole_registry_json() -> String {
    let effects: Vec<String> = Effect::ALL
        .iter()
        .map(|effect| {
            JsonObject::new()
                .string("name", effect.name())
                .string("means", effect.meaning())
                .object()
        })
        .collect();
    let categories: Vec<String> = Category::ALL
        .iter()
        .map(|category| {
            JsonObject::new()
                .string("name", category.name())
                .string("means", category.meaning())
                .object()
        })
        .collect();
    let options: Vec<String> = COMMON_OPTIONS.iter().map(option_json).collect();
    let commands: Vec<String> = COMMANDS.iter().map(command_json).collect();
    let tools: Vec<String> = TOOLS.iter().map(tool_json).collect();
    JsonObject::new()
        .string("version", eggdb::VERSION)
        .raw("effects", &array(&effects))
        .raw("categories", &array(&categories))
        .raw("common_options", &array(&options))
        .raw("commands", &array(&commands))
        .raw("other_commands", "[]")
        .raw("tools", &array(&tools))
        .object()
}

fn array(items: &[String]) -> String {
    format!("[{}]", items.join(","))
}

fn strings(items: &[&str]) -> String {
    let items: Vec<String> = items.iter().map(|item| json_string(item)).collect();
    array(&items)
}

fn optional(text: Option<&str>) -> String {
    text.map_or_else(|| "null".to_owned(), json_string)
}

fn option_json(option: &OptionDefinition) -> String {
    let short = option.short.map(String::from);
    JsonObject::new()
        .raw("short", &optional(short.as_deref()))
        .string("long", option.long)
        .raw("value", &optional(option.value))
        .boolean("repeatable", option.repeatable)
        .raw("default", &optional(option.default))
        .string("help", option.help)
        .object()
}

fn default_formats(definition: &CommandDefinition) -> Option<(Format, Format)> {
    if definition.outputs.is_empty() {
        return None;
    }
    let named = definition
        .words
        .first()
        .and_then(|word| CommandName::named(word));
    Some(match named {
        Some(command) => (default_format(command, true), default_format(command, false)),
        None => (Format::Text, Format::Text),
    })
}

fn command_json(definition: &CommandDefinition) -> String {
    let options: Vec<String> = definition.options.iter().map(option_json).collect();
    let operands: Vec<String> = definition
        .operands
        .iter()
        .map(|operand| {
            JsonObject::new()
                .string("name", operand.name)
                .string("type", operand.kind.name())
                .string("count", operand.count.name())
                .boolean("standard_stream", operand.standard_stream)
                .object()
        })
        .collect();
    let outputs: Vec<String> = definition
        .outputs
        .iter()
        .map(|output| {
            let fields: Vec<String> = output
                .kind
                .fields()
                .iter()
                .map(|field| {
                    JsonObject::new()
                        .string("name", field.name)
                        .string("type", field.kind.name())
                        .object()
                })
                .collect();
            JsonObject::new()
                .string("format", output.format.name())
                .string("kind", output.kind.name())
                .raw("fields", &array(&fields))
                .object()
        })
        .collect();
    let formats = default_formats(definition).map_or_else(
        || "null".to_owned(),
        |(terminal, pipe)| {
            JsonObject::new()
                .string("terminal", format_name(terminal))
                .string("pipe", format_name(pipe))
                .object()
        },
    );
    let errors: Vec<String> = definition
        .errors
        .iter()
        .map(|error| {
            let category = eggdb_commands::category_of(error.code).unwrap_or(Category::Failed);
            JsonObject::new()
                .string("code", error.code)
                .string("category", category.name())
                .number("exit", u64::from(error.exit))
                .object()
        })
        .collect();
    JsonObject::new()
        .raw("words", &strings(definition.words))
        .raw("aliases", &strings(definition.aliases))
        .string("summary", definition.summary)
        .string("description", definition.description)
        .string("usage", definition.usage)
        .raw("options", &array(&options))
        .raw("operands", &array(&operands))
        .raw("outputs", &array(&outputs))
        .raw("default_format", &formats)
        .string("max_effect", definition.max_effect.name())
        .raw("errors", &array(&errors))
        .raw("examples", &strings(definition.examples))
        .string("surfaces", definition.surfaces)
        .object()
}

fn format_name(format: Format) -> &'static str {
    match format {
        Format::Text => "text",
        Format::Json => "json",
        Format::Hex => "hex",
        Format::Raw => "raw",
    }
}

fn tool_json(tool: &ToolDefinition) -> String {
    let arguments: Vec<String> = tool
        .arguments
        .iter()
        .map(|argument| {
            JsonObject::new()
                .string("name", argument.name)
                .string("type", argument.kind.json_type())
                .raw("maps_to", &optional(argument.maps_to.json_name().as_deref()))
                .object()
        })
        .collect();
    let mut fixed = JsonObject::new();
    let mut tokens = tool.fixed.iter().peekable();
    while let Some(option) = tokens.next() {
        let name = option.trim_start_matches('-');
        match tokens.next_if(|value| !value.starts_with("--")) {
            Some(value) => fixed = fixed.string(name, value),
            None => fixed = fixed.boolean(name, true),
        }
    }
    let annotations = tool.annotations();
    let annotations = JsonObject::new()
        .string("title", tool.title)
        .boolean("readOnlyHint", annotations.read_only)
        .boolean("destructiveHint", annotations.destructive)
        .boolean("idempotentHint", annotations.idempotent)
        .boolean("openWorldHint", annotations.open_world)
        .object();
    let command = tool.command.map(strings);
    JsonObject::new()
        .string("name", tool.name)
        .raw("command", &command.unwrap_or_else(|| "null".to_owned()))
        .raw("fixed", &fixed.object())
        .raw("arguments", &array(&arguments))
        .string("max_effect", tool.max_effect.name())
        .raw("grant", &optional(tool.grant.name()))
        .boolean("idempotent", tool.idempotent)
        .boolean("open_world", tool.open_world)
        .raw("confirm", &optional(tool.confirm.map(|check| check.name())))
        .raw("confirm_when", &optional(tool.confirm.map(|check| check.when())))
        .raw("annotations", &annotations)
        .object()
}
