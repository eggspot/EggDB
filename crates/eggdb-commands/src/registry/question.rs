use super::*;

const QUESTION_HELP: &str = "\
Text-to-SQL (D31, D205, D779): asks a language model for one query that answers a question,
from the tables and columns the database shows, and prints the SQL without running it.

  get   ask <model> at <endpoint> (an OpenAI-compatible chat-completions URL) for one read-only
        query in --dialect (postgresql by default); PostgreSQL answers are checked with
        EXPLAIN, and a problem comes back with its SQLSTATE

The endpoint must be allowed by EGGDB_MODEL_ENDPOINTS (comma-separated URL prefixes; plain
HTTP only to loopback, for local models); the database needs CREATE EXTENSION eggdb_ai. The
answer is a model's text: read it before you run it.

Examples:
  eggdb question get shop.db http://127.0.0.1:11434/v1 llama3 \"which customers spent most last month?\"
  eggdb question get shop.db http://127.0.0.1:11434/v1 llama3 \"orders per day\" --dialect mysql --json
";

const QUESTION_ERRORS: &[CommandError] = &[
    OPENING_ERRORS[0],
    OPENING_ERRORS[1],
    OPENING_ERRORS[2],
    OPENING_ERRORS[3],
    OPENING_ERRORS[4],
    OPENING_ERRORS[5],
    OPENING_ERRORS[6],
    OPENING_ERRORS[7],
    OPENING_ERRORS[8],
    command_error("sql_error", 3),
];

const QUESTION_OUTPUTS: [OutputDefinition; 2] = [
    OutputDefinition {
        format: Format::Text,
        kind: OutputKind::Table,
    },
    OutputDefinition {
        format: Format::Json,
        kind: OutputKind::Table,
    },
];

const fn text_operand(name: &'static str) -> OperandDefinition {
    OperandDefinition {
        name,
        kind: OperandType::Text,
        count: Count::One,
        standard_stream: false,
    }
}

pub(super) const QUESTION_GET: CommandDefinition = CommandDefinition {
    words: &["question", "get"],
    aliases: &[],
    synopsis: "<file> <endpoint> <model> <question>",
    summary: "the SQL a language model writes for a question, not run",
    description: "Asks a language model at an allowed OpenAI-compatible endpoint for one read-only query that answers a question about the database, in a dialect, and prints the SQL with whether it was checked and any problem; it never runs the query.",
    usage: "eggdb question get <file> <endpoint> <model> <question> [--dialect <name>]",
    help: QUESTION_HELP,
    options: &[OptionDefinition {
        default: Some("postgresql"),
        ..valued(
            "dialect",
            "name",
            "postgresql (default), mysql, sqlite, oracle or sqlserver",
        )
    }],
    operands: &[
        FILE,
        text_operand("endpoint"),
        text_operand("model"),
        text_operand("question"),
    ],
    outputs: &QUESTION_OUTPUTS,
    max_effect: Effect::Read,
    errors: QUESTION_ERRORS,
    examples: &[
        "eggdb question get shop.db http://127.0.0.1:11434/v1 llama3 \"which customers spent most last month?\"",
        "eggdb question get shop.db http://127.0.0.1:11434/v1 llama3 \"orders per day\" --dialect mysql --json",
    ],
    surfaces: "the MCP tool question_get; the SQL function eggdb_ai.text_to_sql",
};
