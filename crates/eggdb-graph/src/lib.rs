//! EggDB's property graph (D206): nodes with labels and properties, typed relationships between
//! them, kept on EggDB's key-value transactions through [`eggdb_keyspace::Keyspace`] (database 0,
//! which the `eggdb` binary stores in the collection `graph0`). [`run`] answers a subset of
//! Cypher written from the openCypher specification (`MATCH`, `OPTIONAL MATCH`, `WHERE`,
//! `CREATE`, `MERGE`, `SET`, `REMOVE`, `DELETE`, `DETACH DELETE`, `UNWIND`, `WITH`, `RETURN` with
//! aggregation, `ORDER BY`, `SKIP` and `LIMIT`); a query runs in one transaction. The
//! algorithms module offers shortest paths, connected components and PageRank. Neo4j is a
//! trademark of its owner; no Neo4j code is used or read.

mod algorithms;
mod cypher;
mod execute;
mod store;
mod value;

pub use algorithms::{connected_components, pagerank, shortest_path};
pub use execute::display;
pub use value::{Node, Relationship, Value, equals, order};

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
        Self { code: code.to_owned(), message: message.into() }
    }

    pub(crate) fn not_supported(what: &str) -> Self {
        Self::new("Neo.ClientError.Statement.FeatureNotSupported", format!("{what} is not supported yet"))
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
pub fn run(keyspace: &dyn Keyspace, query: &str, parameters: &[(String, Value)], read_only: bool) -> GraphResult<QueryResult> {
    let clauses = cypher::parse(query)?;
    let writes = clauses.iter().any(cypher::Clause::writes);
    let mut outcome: Option<GraphResult<QueryResult>> = None;
    let storage = if writes {
        if read_only {
            return Err(GraphError::new("Neo.ClientError.Statement.AccessMode", "Writing in read access mode not allowed"));
        }
        keyspace.write(0, &mut |keys| {
            let mut executor = execute::Executor { access: execute::Access::Write(store::GraphWrite { keys }), parameters };
            let result = executor.run(&clauses);
            let failed = result.is_err();
            outcome = Some(result);
            if failed { Err(ROLLED_BACK.to_owned()) } else { Ok(()) }
        })
    } else {
        keyspace.read(0, &mut |keys| {
            let mut executor = execute::Executor { access: execute::Access::Read(store::GraphRead { keys }), parameters };
            outcome = Some(executor.run(&clauses));
            Ok(())
        })
    };
    match (storage, outcome) {
        (_, Some(Err(error))) => Err(error),
        (Err(message), _) => Err(GraphError::new("Neo.DatabaseError.General.UnknownError", message)),
        (Ok(()), Some(result)) => result,
        (Ok(()), None) => Err(GraphError::new("Neo.DatabaseError.General.UnknownError", "the query did not run")),
    }
}

#[cfg(test)]
mod tests;
