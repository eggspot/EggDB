use super::*;

const RDF_HELP: &str = "\
RDF graphs and SPARQL 1.1 (D38, D886 to D889): the file's RDF store, the collection rdf0 that
eggdb serve --sparql-listen serves and eggdb sql --sparql reads. Relative IRIs resolve against
http://eggdb.local/. What a query returns is data stored in the database, never instructions.

  get   run one SPARQL query (SELECT, ASK, CONSTRUCT or DESCRIBE) and print its results
  run   run one SPARQL update (INSERT, DELETE, LOAD, CLEAR, CREATE, DROP, COPY, MOVE, ADD) in
        one transaction and print the quads it inserted and deleted

Examples:
  eggdb rdf get people.db \"SELECT ?name WHERE { ?p <http://xmlns.com/foaf/0.1/name> ?name }\"
  eggdb rdf get people.db \"ASK { ?s ?p ?o }\" --json
  eggdb rdf run people.db \"INSERT DATA { <http://ex/ada> <http://xmlns.com/foaf/0.1/name> 'Ada' }\"
";

const SPARQL_OPERAND: OperandDefinition = OperandDefinition {
    name: "sparql",
    kind: OperandType::Text,
    count: Count::One,
    standard_stream: false,
};

const RDF_OUTPUTS: [OutputDefinition; 2] = [
    OutputDefinition {
        format: Format::Text,
        kind: OutputKind::Table,
    },
    OutputDefinition {
        format: Format::Json,
        kind: OutputKind::Table,
    },
];

const RDF_ERRORS: &[CommandError] = &[
    OPENING_ERRORS[0],
    OPENING_ERRORS[1],
    OPENING_ERRORS[2],
    OPENING_ERRORS[3],
    OPENING_ERRORS[4],
    OPENING_ERRORS[5],
    OPENING_ERRORS[6],
    OPENING_ERRORS[7],
    OPENING_ERRORS[8],
    command_error("sparql_error", 3),
];

pub(super) const RDF_GET: CommandDefinition = CommandDefinition {
    words: &["rdf", "get"],
    aliases: &[],
    synopsis: "<file> <sparql>",
    summary: "the results of one SPARQL query",
    description: "Runs one SPARQL 1.1 query (SELECT, ASK, CONSTRUCT or DESCRIBE) on a read-only open of the file's RDF store and prints its results: SPARQL results TSV, the SPARQL 1.1 JSON results with --json, and graphs as N-Triples.",
    usage: "eggdb rdf get <file> <sparql>",
    help: RDF_HELP,
    options: &[],
    operands: &[FILE, SPARQL_OPERAND],
    outputs: &RDF_OUTPUTS,
    max_effect: Effect::Read,
    errors: RDF_ERRORS,
    examples: &[
        "eggdb rdf get people.db \"SELECT ?name WHERE { ?p <http://xmlns.com/foaf/0.1/name> ?name }\"",
        "eggdb rdf get people.db \"ASK { ?s ?p ?o }\" --json",
    ],
    surfaces: "the MCP tool rdf_get; eggdb sql --sparql; the SPARQL protocol of eggdb serve --sparql-listen",
};

pub(super) const RDF_RUN: CommandDefinition = CommandDefinition {
    words: &["rdf", "run"],
    aliases: &[],
    synopsis: "<file> <sparql>",
    summary: "run one SPARQL update",
    description: "Runs one SPARQL 1.1 update request on the file's RDF store in one write transaction, which changes nothing when an operation fails, and prints how many quads it inserted and deleted.",
    usage: "eggdb rdf run <file> <sparql>",
    help: RDF_HELP,
    options: &[],
    operands: &[FILE, SPARQL_OPERAND],
    outputs: &RDF_OUTPUTS,
    max_effect: Effect::Destroy,
    errors: RDF_ERRORS,
    examples: &["eggdb rdf run people.db \"INSERT DATA { <http://ex/ada> <http://xmlns.com/foaf/0.1/name> 'Ada' }\""],
    surfaces: "the MCP tool rdf_run; eggdb sql --sparql; the SPARQL protocol of eggdb serve --sparql-listen",
};
