//! EggDB's RDF store (D38, D886): triples and quads kept on EggDB's key-value transactions
//! through [`eggdb_keyspace::Keyspace`], read from N-Triples, N-Quads, Turtle and TriG, and
//! written out as N-Quads. Everything is written from the W3C recommendations (RDF 1.1
//! Concepts, N-Triples, N-Quads, Turtle, TriG); no other RDF store's code is used or read.

mod store;
mod syntax;
mod term;

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
pub(crate) fn writing<T>(
    keyspace: &dyn Keyspace,
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
pub(crate) fn reading<T>(
    keyspace: &dyn Keyspace,
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
pub fn load(
    keyspace: &dyn Keyspace,
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
pub fn export(keyspace: &dyn Keyspace, db: u32) -> RdfResult<String> {
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
}
