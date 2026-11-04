//! EggDB's property graph (D206): nodes with labels and properties, typed relationships between
//! them, kept on EggDB's key-value transactions through [`eggdb_keyspace::Keyspace`] (database 0,
//! which the `eggdb` binary stores in the collection `graph0`). [`run`] answers a subset of
//! Cypher written from the openCypher specification (`MATCH`, `OPTIONAL MATCH`, `WHERE`,
//! `CREATE`, `MERGE` with `ON CREATE SET` and `ON MATCH SET`, `SET`, `REMOVE`, `DELETE`,
//! `DETACH DELETE`, `FOREACH`, `UNWIND`, `WITH`, `RETURN` with aggregation, `ORDER BY`, `SKIP`
//! and `LIMIT`, `UNION` and `UNION ALL`, named paths with `shortestPath()` and
//! `allShortestPaths()`, list and pattern comprehensions, and `CALL ... YIELD` of the graph
//! algorithms); a query runs in one transaction. The algorithms are shortest paths, connected components,
//! PageRank, label propagation communities, triangle counts and Jaccard node similarity. Neo4j is a
//! trademark of its owner; no Neo4j code is used or read.

mod algorithms;
mod cypher;
mod execute;
mod procedures;
mod store;
mod value;

pub use algorithms::{
    connected_components, label_propagation, node_similarity, pagerank, shortest_path,
    triangle_count,
};
pub use execute::display;
pub use value::{Node, Path, Relationship, Value, equals, order};

use eggdb_keyspace::Keyspace;

/// Why a query failed, with the status code Neo4j's documentation gives the same failure, so
/// Bolt drivers raise their usual exception.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphError {
    /// The status code, such as `Neo.ClientError.Statement.SyntaxError`.
    pub code: String,
    /// What went wrong.
    pub message: String,
}

impl GraphError {
    pub(crate) fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
        }
    }

    pub(crate) fn not_supported(what: &str) -> Self {
        Self::new(
            "Neo.ClientError.Statement.FeatureNotSupported",
            format!("{what} is not supported yet"),
        )
    }
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for GraphError {}

/// The result of a graph operation.
pub type GraphResult<T> = Result<T, GraphError>;

/// What a query returned.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueryResult {
    /// The column names.
    pub columns: Vec<String>,
    /// The rows, one value per column.
    pub rows: Vec<Vec<Value>>,
}

const ROLLED_BACK: &str = "the graph query was rolled back";

/// Runs a Cypher query with its `$parameters`, in a read transaction when it only reads and in
/// a write transaction otherwise; a failing query changes nothing. With `read_only`, a query
/// that writes fails before it runs.
pub fn run(
    keyspace: &dyn Keyspace,
    query: &str,
    parameters: &[(String, Value)],
    read_only: bool,
) -> GraphResult<QueryResult> {
    let parsed = cypher::parse(query)?;
    let writes = parsed.writes();
    let mut outcome: Option<GraphResult<QueryResult>> = None;
    let storage = if writes {
        if read_only {
            return Err(GraphError::new(
                "Neo.ClientError.Statement.AccessMode",
                "Writing in read access mode not allowed",
            ));
        }
        keyspace.write(0, &mut |keys| {
            let mut executor = execute::Executor {
                access: execute::Access::Write(store::GraphWrite { keys }),
                parameters,
                foreach_runs: 0,
                shortest_path_steps: std::cell::Cell::new(0),
                shortest_path_relationships_returned: std::cell::Cell::new(0),
            };
            let result = executor.run_query(&parsed);
            let failed = result.is_err();
            outcome = Some(result);
            if failed {
                Err(ROLLED_BACK.to_owned())
            } else {
                Ok(())
            }
        })
    } else {
        keyspace.read(0, &mut |keys| {
            let mut executor = execute::Executor {
                access: execute::Access::Read(store::GraphRead { keys }),
                parameters,
                foreach_runs: 0,
                shortest_path_steps: std::cell::Cell::new(0),
                shortest_path_relationships_returned: std::cell::Cell::new(0),
            };
            outcome = Some(executor.run_query(&parsed));
            Ok(())
        })
    };
    match (storage, outcome) {
        (_, Some(Err(error))) => Err(error),
        (Err(message), _) => Err(GraphError::new(
            "Neo.DatabaseError.General.UnknownError",
            message,
        )),
        (Ok(()), Some(result)) => result,
        (Ok(()), None) => Err(GraphError::new(
            "Neo.DatabaseError.General.UnknownError",
            "the query did not run",
        )),
    }
}

#[cfg(test)]
mod tests;
