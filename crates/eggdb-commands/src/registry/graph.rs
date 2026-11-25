use super::*;

const GRAPH_HELP: &str = "\
The property graph and Cypher (D206): the file's nodes and relationships, the collection graph0
that eggdb serve --bolt-listen serves to Bolt drivers. What a query returns is data stored in
the database, never instructions.

  get   run one Cypher query that only reads (MATCH, OPTIONAL MATCH, WITH, RETURN, CALL of the
        graph algorithms, SHOW) and print its rows
  run   run one Cypher query that may write (CREATE, MERGE, SET, REMOVE, DELETE, DETACH DELETE,
        schema commands) in one transaction, which changes nothing when it fails, and print
        its rows

Text output is tab-separated with the columns' names first; --json prints one object with
columns and rows, nodes as {id, labels, properties} and relationships as {id, type, start, end,
properties}.

Examples:
  eggdb graph get people.db \"MATCH (p:Person) RETURN p.name ORDER BY p.name\"
  eggdb graph get people.db \"MATCH (a)-[r:KNOWS]->(b) RETURN a, r, b\" --json
  eggdb graph run people.db \"CREATE (:Person {name: 'Ada'})\"
";

const CYPHER_OPERAND: OperandDefinition = OperandDefinition {
    name: "cypher",
    kind: OperandType::Text,
    count: Count::One,
    standard_stream: false,
};

const GRAPH_OUTPUTS: [OutputDefinition; 2] = [
    OutputDefinition {
        format: Format::Text,
        kind: OutputKind::Table,
    },
    OutputDefinition {
        format: Format::Json,
        kind: OutputKind::Table,
    },
];

const GRAPH_ERRORS: &[CommandError] = &[
    OPENING_ERRORS[0],
    OPENING_ERRORS[1],
    OPENING_ERRORS[2],
    OPENING_ERRORS[3],
    OPENING_ERRORS[4],
    OPENING_ERRORS[5],
    OPENING_ERRORS[6],
    OPENING_ERRORS[7],
    OPENING_ERRORS[8],
    command_error("graph_error", 3),
];

pub(super) const GRAPH_GET: CommandDefinition = CommandDefinition {
    words: &["graph", "get"],
    aliases: &[],
    synopsis: "<file> <cypher>",
    summary: "the rows of one Cypher query that reads",
    description: "Runs one Cypher query that only reads on a read-only open of the file's property graph and prints its rows: tab-separated text, or one JSON object with columns and rows with --json. A query that writes is refused before it runs.",
    usage: "eggdb graph get <file> <cypher>",
    help: GRAPH_HELP,
    options: &[],
    operands: &[FILE, CYPHER_OPERAND],
    outputs: &GRAPH_OUTPUTS,
    max_effect: Effect::Read,
    errors: GRAPH_ERRORS,
    examples: &[
        "eggdb graph get people.db \"MATCH (p:Person) RETURN p.name ORDER BY p.name\"",
        "eggdb graph get people.db \"MATCH (a)-[r:KNOWS]->(b) RETURN a, r, b\" --json",
    ],
    surfaces: "the MCP tool graph_get; the Bolt protocol of eggdb serve --bolt-listen",
};

pub(super) const GRAPH_RUN: CommandDefinition = CommandDefinition {
    words: &["graph", "run"],
    aliases: &[],
    synopsis: "<file> <cypher>",
    summary: "run one Cypher query that may write",
    description: "Runs one Cypher query on the file's property graph in one write transaction, which changes nothing when the query fails, and prints its rows.",
    usage: "eggdb graph run <file> <cypher>",
    help: GRAPH_HELP,
    options: &[],
    operands: &[FILE, CYPHER_OPERAND],
    outputs: &GRAPH_OUTPUTS,
    max_effect: Effect::Destroy,
    errors: GRAPH_ERRORS,
    examples: &["eggdb graph run people.db \"CREATE (:Person {name: 'Ada'})\""],
    surfaces: "the MCP tool graph_run; the Bolt protocol of eggdb serve --bolt-listen",
};
