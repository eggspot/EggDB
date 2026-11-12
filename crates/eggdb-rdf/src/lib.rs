//! EggDB's RDF store (D38, D886): triples and quads kept on EggDB's key-value transactions
//! through [`eggdb_keyspace::Keyspace`], read from N-Triples, N-Quads, Turtle and TriG, and
//! written out as N-Quads, queried and changed with SPARQL 1.1 ([`query`], [`update`]), and
//! served over HTTP by [`Endpoint`] with the SPARQL 1.1 Protocol and Graph Store HTTP
//! Protocol, with `LOAD` fetching documents through the host's [`set_remote_fetch`]. Everything
//! is written from the W3C recommendations (RDF 1.1 Concepts, N-Triples,
//! N-Quads, Turtle, TriG, SPARQL 1.1 Query Language, Update, Query Results formats, Protocol
//! and Graph Store HTTP Protocol); no other RDF store's code is used or read.

mod protocol;
mod sparql_ast;
mod sparql_eval;
mod sparql_parser;
mod sparql_regex;
mod sparql_results;
mod sparql_update;
mod sparql_values;
mod store;
mod syntax;
mod term;

pub use protocol::{Endpoint, MAX_REQUEST_BYTES};

/// What fetches a document for SPARQL's `LOAD`: called with the IRI, it returns the
/// document's media type and text, or why it could not.
pub type RemoteFetch = dyn Fn(&str) -> Result<(String, String), String> + Send + Sync;

static REMOTE: std::sync::OnceLock<Box<RemoteFetch>> = std::sync::OnceLock::new();

/// Lets SPARQL's `LOAD` fetch documents through `fetch`, which decides what it may reach; the
/// first call wins. Without it `LOAD` is refused, so a query never makes the server fetch an
/// address on its own.
pub fn set_remote_fetch(fetch: Box<RemoteFetch>) {
    let _ = REMOTE.set(fetch);
}

pub(crate) fn fetch_remote(iri: &str) -> RdfResult<(String, String)> {
    let fetch = REMOTE.get().ok_or_else(|| {
        RdfError::not_supported("LOAD here: the server fetches no documents unless it was started to (EGGDB_SPARQL_LOAD)")
    })?;
    fetch(iri).map_err(|message| RdfError::invalid(format!("LOAD <{iri}>: {message}")))
}
pub use sparql_eval::QueryOutcome;
pub use sparql_update::UpdateSummary;
pub use sparql_results::{ResultFormat, write_results};
pub use syntax::{Format, parse};
pub use term::{
    Literal, Quad, RDF_FIRST, RDF_LANG_STRING, RDF_NIL, RDF_REST, RDF_TYPE, Term, XSD_BOOLEAN,
    XSD_DATE_TIME, XSD_DECIMAL, XSD_DOUBLE, XSD_INTEGER, XSD_STRING,
};

use eggdb_keyspace::Keyspace;
use std::collections::HashMap;

/// What kind of failure an [`RdfError`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RdfErrorKind {
    /// The text is not valid in its syntax.
    Syntax,
    /// The request is well formed but not allowed, such as a literal subject.
    Invalid,
    /// The request uses something not written yet.
    NotSupported,
    /// The key-value store failed.
    Storage,
}

/// Why an RDF operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RdfError {
    /// What kind of failure it is.
    pub kind: RdfErrorKind,
    /// What went wrong.
    pub message: String,
}

impl RdfError {
    pub(crate) fn syntax(message: impl Into<String>) -> Self {
        Self {
            kind: RdfErrorKind::Syntax,
            message: message.into(),
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: RdfErrorKind::Invalid,
            message: message.into(),
        }
    }

    pub(crate) fn not_supported(what: &str) -> Self {
        Self {
            kind: RdfErrorKind::NotSupported,
            message: format!("{what} is not supported yet"),
        }
    }

    pub(crate) fn storage(message: impl Into<String>) -> Self {
        Self {
            kind: RdfErrorKind::Storage,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for RdfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RdfError {}

/// The result of an RDF operation.
pub type RdfResult<T> = Result<T, RdfError>;

const ROLLED_BACK: &str = "the RDF change was rolled back";

/// Runs `body` in a write transaction on database `db`, keeping its error when it fails.
pub(crate) fn writing<T, K: Keyspace + ?Sized>(
    keyspace: &K,
    db: u32,
    body: &mut dyn FnMut(&mut store::RdfWrite<'_>) -> RdfResult<T>,
) -> RdfResult<T> {
    let mut outcome: Option<RdfResult<T>> = None;
    let stored = keyspace.write(db, &mut |keys| {
        let result = body(&mut store::RdfWrite { keys });
        let failed = result.is_err();
        outcome = Some(result);
        if failed { Err(ROLLED_BACK.to_owned()) } else { Ok(()) }
    });
    match (stored, outcome) {
        (_, Some(Err(error))) => Err(error),
        (Err(message), _) => Err(RdfError::storage(message)),
        (Ok(()), Some(result)) => result,
        (Ok(()), None) => Err(RdfError::storage("the change did not run")),
    }
}

/// Runs `body` on a snapshot of database `db`.
pub(crate) fn reading<T, K: Keyspace + ?Sized>(
    keyspace: &K,
    db: u32,
    body: &mut dyn FnMut(&store::RdfRead<'_>) -> RdfResult<T>,
) -> RdfResult<T> {
    let mut outcome: Option<RdfResult<T>> = None;
    let stored = keyspace.read(db, &mut |keys| {
        outcome = Some(body(&store::RdfRead { keys }));
        Ok(())
    });
    match (stored, outcome) {
        (_, Some(Err(error))) => Err(error),
        (Err(message), _) => Err(RdfError::storage(message)),
        (Ok(()), Some(result)) => result,
        (Ok(()), None) => Err(RdfError::storage("the read did not run")),
    }
}

/// Gives a document's blank nodes labels of their own, so two documents that both say `_:b0`
/// do not share a node.
pub(crate) fn scoped_blanks(write: &mut store::RdfWrite<'_>, quads: Vec<Quad>) -> RdfResult<Vec<Quad>> {
    let scope = write.next_number()?;
    let mut renamed: HashMap<String, String> = HashMap::new();
    let mut rename = |term: Term| match term {
        Term::Blank(label) => {
            let fresh = renamed
                .entry(label)
                .or_insert_with_key(|label| format!("s{scope}_{label}"))
                .clone();
            Term::Blank(fresh)
        }
        other => other,
    };
    Ok(quads
        .into_iter()
        .map(|quad| Quad {
            subject: rename(quad.subject),
            predicate: quad.predicate,
            object: rename(quad.object),
            graph: quad.graph.map(&mut rename),
        })
        .collect())
}

/// Reads `text` in `format` (relative IRIs against `base`) and stores its quads in database
/// `db`, in one transaction; `graph` puts the triples of the default graph into that named
/// graph. Returns how many quads were new.
pub fn load<K: Keyspace + ?Sized>(
    keyspace: &K,
    db: u32,
    text: &str,
    format: Format,
    base: &str,
    graph: Option<&Term>,
) -> RdfResult<usize> {
    let quads = parse(text, format, base)?;
    writing(keyspace, db, &mut |write| {
        let mut added = 0;
        for mut quad in scoped_blanks(write, quads.clone())? {
            if quad.graph.is_none() {
                quad.graph = graph.cloned();
            }
            if write.insert(&quad)? {
                added += 1;
            }
        }
        Ok(added)
    })
}

/// Every quad of database `db` in N-Quads, the default graph's first.
pub fn export<K: Keyspace + ?Sized>(keyspace: &K, db: u32) -> RdfResult<String> {
    reading(keyspace, db, &mut |read| {
        let mut out = String::new();
        for ids in read.matching(&store::IdPattern::default())? {
            let quad = read.quad_of(ids)?;
            out.push_str(&quad.subject.to_ntriples());
            out.push(' ');
            out.push_str(&quad.predicate.to_ntriples());
            out.push(' ');
            out.push_str(&quad.object.to_ntriples());
            if let Some(graph) = &quad.graph {
                out.push(' ');
                out.push_str(&graph.to_ntriples());
            }
            out.push_str(" .\n");
        }
        Ok(out)
    })
}

/// Runs a SPARQL 1.1 query (relative IRIs against `base`) on a snapshot of database `db`.
pub fn query<K: Keyspace + ?Sized>(keyspace: &K, db: u32, text: &str, base: &str) -> RdfResult<QueryOutcome> {
    query_in(keyspace, db, text, base, None)
}

/// Runs a SPARQL 1.1 query with the dataset the SPARQL protocol's `default-graph-uri` and
/// `named-graph-uri` name, which replaces the query's own `FROM` clauses when given.
pub fn query_in<K: Keyspace + ?Sized>(
    keyspace: &K,
    db: u32,
    text: &str,
    base: &str,
    dataset: Option<(Vec<String>, Vec<String>)>,
) -> RdfResult<QueryOutcome> {
    let mut parsed = sparql_parser::parse_query(text, base)?;
    if let Some((default, named)) = dataset {
        parsed.dataset = Some(sparql_ast::Dataset { default, named });
    }
    reading(keyspace, db, &mut |read| {
        let evaluator = sparql_eval::Evaluator::new(read, parsed.dataset.as_ref())?;
        evaluator.query(&parsed)
    })
}

/// Runs a SPARQL 1.1 update request in one write transaction on database `db`; a failing
/// operation undoes every operation of the request.
pub fn update<K: Keyspace + ?Sized>(keyspace: &K, db: u32, text: &str, base: &str) -> RdfResult<UpdateSummary> {
    let operations = sparql_parser::parse_update(text, base)?;
    writing(keyspace, db, &mut |write| sparql_update::run(write, &operations))
}

/// The triples of one graph (`None` for the default graph) as N-Triples, or `None` when a
/// named graph holds nothing.
pub fn export_graph<K: Keyspace + ?Sized>(keyspace: &K, db: u32, graph: Option<&Term>) -> RdfResult<Option<String>> {
    reading(keyspace, db, &mut |read| {
        let graph_id = match graph {
            None => store::DEFAULT_GRAPH,
            Some(term) => match read.id_of(term)? {
                Some(id) => id,
                None => return Ok(None),
            },
        };
        let quads = read.matching(&store::IdPattern {
            graph: Some(graph_id),
            ..store::IdPattern::default()
        })?;
        if quads.is_empty() && graph.is_some() {
            return Ok(None);
        }
        let mut out = String::new();
        for ids in quads {
            let quad = read.quad_of(ids)?;
            out.push_str(&format!(
                "{} {} {} .\n",
                quad.subject.to_ntriples(),
                quad.predicate.to_ntriples(),
                quad.object.to_ntriples()
            ));
        }
        Ok(Some(out))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_keyspace::MemoryKeyspace;

    #[test]
    fn documents_load_and_export_with_their_own_blank_nodes() {
        let keyspace = MemoryKeyspace::default();
        let document = "@prefix ex: <http://example.org/> . ex:a ex:knows _:x . _:x ex:name \"X\" .";
        assert_eq!(load(&keyspace, 0, document, Format::Turtle, "", None).unwrap(), 2);
        assert_eq!(load(&keyspace, 0, document, Format::Turtle, "", None).unwrap(), 2);
        let graph = Term::iri("http://example.org/g");
        assert_eq!(load(&keyspace, 0, "<http://a> <http://b> <http://c> .", Format::NTriples, "", Some(&graph)).unwrap(), 1);
        let exported = export(&keyspace, 0).unwrap();
        assert_eq!(exported.lines().count(), 5);
        assert!(exported.ends_with("<http://a> <http://b> <http://c> <http://example.org/g> .\n"), "{exported}");
        assert!(load(&keyspace, 0, "\"x\" <http://b> <http://c> .", Format::Turtle, "", None).is_err());
    }

    fn rows(outcome: QueryOutcome) -> Vec<Vec<Option<Term>>> {
        match outcome {
            QueryOutcome::Solutions { rows, .. } => rows,
            other => panic!("solutions, not {other:?}"),
        }
    }

    fn plain(text: &str) -> Option<Term> {
        Some(Term::Literal(Literal::string(text)))
    }

    #[test]
    fn sparql_queries_answer_over_loaded_data() {
        let keyspace = MemoryKeyspace::default();
        let data = r#"@prefix ex: <http://example.org/> .
            ex:ann a ex:Person ; ex:name "Ann" ; ex:age 31 ; ex:knows ex:bob, ex:cid .
            ex:bob a ex:Person ; ex:name "Bob" ; ex:age 17 ; ex:knows ex:cid .
            ex:cid a ex:Person ; ex:name "Cid" .
            ex:g1 { ex:ann ex:email "ann@example.org" }"#;
        load(&keyspace, 0, data, Format::TriG, "", None).unwrap();
        let adults = query(
            &keyspace,
            0,
            "PREFIX ex: <http://example.org/> SELECT ?name WHERE { ?p a ex:Person ; ex:name ?name ; ex:age ?age FILTER(?age >= 18) }",
            "",
        )
        .unwrap();
        assert_eq!(rows(adults), vec![vec![plain("Ann")]]);
        let counted = query(
            &keyspace,
            0,
            "PREFIX ex: <http://example.org/> SELECT ?name (COUNT(?friend) AS ?friends) WHERE { ?p ex:name ?name OPTIONAL { ?p ex:knows ?friend } } GROUP BY ?name ORDER BY DESC(?friends) ?name",
            "",
        )
        .unwrap();
        let counted = rows(counted);
        assert_eq!(counted.len(), 3);
        assert_eq!(counted[0][0], plain("Ann"));
        assert_eq!(counted[0][1], Some(Term::Literal(Literal::typed("2", XSD_INTEGER))));
        assert_eq!(counted[2][1], Some(Term::Literal(Literal::typed("0", XSD_INTEGER))));
        let reach = query(
            &keyspace,
            0,
            "PREFIX ex: <http://example.org/> SELECT DISTINCT ?who WHERE { ex:ann ex:knows+ ?who } ORDER BY ?who",
            "",
        )
        .unwrap();
        assert_eq!(rows(reach).len(), 2);
        let graph = query(
            &keyspace,
            0,
            "PREFIX ex: <http://example.org/> SELECT ?g ?mail WHERE { GRAPH ?g { ?p ex:email ?mail } }",
            "",
        )
        .unwrap();
        assert_eq!(rows(graph), vec![vec![Some(Term::iri("http://example.org/g1")), plain("ann@example.org")]]);
        assert_eq!(
            query(&keyspace, 0, "ASK { <http://example.org/bob> <http://example.org/knows> ?x }", "").unwrap(),
            QueryOutcome::Boolean(true)
        );
        let built = query(
            &keyspace,
            0,
            "PREFIX ex: <http://example.org/> CONSTRUCT { ?b ex:knownBy ?a } WHERE { ?a ex:knows ?b }",
            "",
        )
        .unwrap();
        let QueryOutcome::Graph(triples) = built else {
            panic!("a graph");
        };
        assert_eq!(triples.len(), 3);
        let missing = query(&keyspace, 0, "SELECT * WHERE { ?s <http://nothing> ?o }", "").unwrap();
        assert!(rows(missing).is_empty());
    }

    #[test]
    fn sparql_updates_change_the_store_in_one_transaction() {
        let keyspace = MemoryKeyspace::default();
        let summary = update(
            &keyspace,
            0,
            "PREFIX ex: <http://ex/> INSERT DATA { ex:a ex:age 30 . ex:b ex:age 40 . GRAPH ex:g { ex:a ex:tag \"x\" } }",
            "",
        )
        .unwrap();
        assert_eq!(summary.inserted, 3);
        let summary = update(
            &keyspace,
            0,
            "PREFIX ex: <http://ex/> DELETE { ?s ex:age ?old } INSERT { ?s ex:age ?new ; ex:note [ ex:was ?old ] } WHERE { ?s ex:age ?old BIND(?old + 1 AS ?new) }",
            "",
        )
        .unwrap();
        assert_eq!((summary.inserted, summary.deleted), (6, 2));
        let ages = query(&keyspace, 0, "SELECT ?a WHERE { ?s <http://ex/age> ?a } ORDER BY ?a", "").unwrap();
        assert_eq!(
            rows(ages),
            vec![
                vec![Some(Term::Literal(Literal::typed("31", XSD_INTEGER)))],
                vec![Some(Term::Literal(Literal::typed("41", XSD_INTEGER)))]
            ]
        );
        update(&keyspace, 0, "COPY GRAPH <http://ex/g> TO GRAPH <http://ex/h> ; CLEAR GRAPH <http://ex/g>", "").unwrap();
        assert_eq!(export_graph(&keyspace, 0, Some(&Term::iri("http://ex/g"))).unwrap(), None);
        assert!(export_graph(&keyspace, 0, Some(&Term::iri("http://ex/h"))).unwrap().is_some());
        let failed = update(&keyspace, 0, "INSERT DATA { <http://ex/z> <http://ex/p> 1 } ; CLEAR GRAPH <http://ex/nothing>", "");
        assert!(failed.is_err());
        let untouched = query(&keyspace, 0, "ASK { <http://ex/z> ?p ?o }", "").unwrap();
        assert_eq!(untouched, QueryOutcome::Boolean(false));
    }
}
