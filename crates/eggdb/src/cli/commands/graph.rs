use super::{Access, Session};
use crate::cli::args::{CommandName, Format, GraphVerb};
use crate::cli::failure::{Exit, Failure};
use eggdb::Database;
use eggdb_graph::{GraphResult, QueryResult};

impl Session<'_> {
    pub(super) fn graph_command(&mut self) -> Result<Exit, Failure> {
        let Some((verb, cypher)) = self.request.graph.clone() else {
            return Err(Failure::usage("graph needs get or run", Some(CommandName::Graph)));
        };
        let access = match verb {
            GraphVerb::Get => Access::ReadOnly,
            GraphVerb::Run => Access::Write { create: false },
        };
        let db = self.open(access, None)?;
        let outcome = run_cypher(&db, &cypher, verb == GraphVerb::Get);
        self.close(db)?;
        match outcome {
            Ok(result) => {
                let written = match self.format {
                    Format::Json => eggdb_graph::result_json(&result),
                    _ => tab_separated(&result),
                };
                self.out.text(&written);
                self.out.text("\n");
                Ok(Exit::Done)
            }
            Err(error) => Err(Failure::coded(
                "graph_error",
                format!("Cypher: {error}"),
                Exit::DatabaseError,
            )),
        }
    }
}

pub(super) fn run_cypher(db: &Database, cypher: &str, read_only: bool) -> GraphResult<QueryResult> {
    let keyspace = super::serve_resp::DatabaseKeyspace {
        db,
        prefix: super::serve_resp::GRAPH_COLLECTIONS,
    };
    eggdb_graph::run(&keyspace, cypher, &[], read_only)
}

fn tab_separated(result: &QueryResult) -> String {
    let mut lines = vec![result.columns.iter().map(|column| cell(column)).collect::<Vec<_>>().join("\t")];
    for row in &result.rows {
        lines.push(
            row.iter()
                .map(|value| cell(&eggdb_graph::display(value)))
                .collect::<Vec<_>>()
                .join("\t"),
        );
    }
    lines.join("\n")
}

fn cell(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}
