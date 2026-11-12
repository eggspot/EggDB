//! Quads on a key-value database (D886): every term gets a number in a dictionary, and each
//! quad is kept under three orders of those numbers (graph, subject, predicate, object; graph,
//! predicate, object, subject; graph, object, subject, predicate), so a pattern with any bound
//! positions reads one key range. The default graph is number 0.

use crate::term::{Quad, Term};
use crate::{RdfError, RdfResult};
use eggdb_keyspace::{KeysRead, KeysWrite};

const COUNTER: &[u8] = b"\x00next";
const TERM_ID: u8 = b'T';
const ID_TERM: u8 = b'D';
const BY_SUBJECT: u8 = b'S';
const BY_PREDICATE: u8 = b'P';
const BY_OBJECT: u8 = b'O';
const GRAPHS: u8 = b'G';

/// The default graph's number.
pub(crate) const DEFAULT_GRAPH: u64 = 0;

fn storage(message: String) -> RdfError {
    RdfError::storage(message)
}

fn term_key(term: &Term) -> Vec<u8> {
    let mut key = vec![TERM_ID];
    key.extend_from_slice(&term.to_bytes());
    key
}

fn id_key(id: u64) -> Vec<u8> {
    let mut key = vec![ID_TERM];
    key.extend_from_slice(&id.to_be_bytes());
    key
}

fn order_key(order: u8, ids: [u64; 4]) -> Vec<u8> {
    let mut key = Vec::with_capacity(33);
    key.push(order);
    for id in ids {
        key.extend_from_slice(&id.to_be_bytes());
    }
    key
}

fn graph_key(graph: u64) -> Vec<u8> {
    let mut key = vec![GRAPHS];
    key.extend_from_slice(&graph.to_be_bytes());
    key
}

fn ids_of(key: &[u8]) -> Option<[u64; 4]> {
    let mut ids = [0u64; 4];
    for (at, id) in ids.iter_mut().enumerate() {
        *id = u64::from_be_bytes(key.get(1 + at * 8..9 + at * 8)?.try_into().ok()?);
    }
    Some(ids)
}

/// A quad pattern by term numbers: `None` matches anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct IdPattern {
    pub(crate) graph: Option<u64>,
    pub(crate) subject: Option<u64>,
    pub(crate) predicate: Option<u64>,
    pub(crate) object: Option<u64>,
}

/// One quad by term numbers: graph, subject, predicate, object.
pub(crate) type IdQuad = [u64; 4];

/// Reads quads within a transaction.
pub(crate) struct RdfRead<'k> {
    pub(crate) keys: &'k dyn KeysRead,
}

impl RdfRead<'_> {
    /// The number of `term`, when it was ever stored.
    pub(crate) fn id_of(&self, term: &Term) -> RdfResult<Option<u64>> {
        Ok(self
            .keys
            .get(&term_key(term))
            .map_err(storage)?
            .and_then(|bytes| bytes.try_into().ok())
            .map(u64::from_be_bytes))
    }

    /// The term numbered `id`.
    pub(crate) fn term_of(&self, id: u64) -> RdfResult<Term> {
        let bytes = self
            .keys
            .get(&id_key(id))
            .map_err(storage)?
            .ok_or_else(|| RdfError::storage(format!("term {id} is missing from the dictionary")))?;
        Term::from_bytes(&bytes).ok_or_else(|| RdfError::storage(format!("term {id} is corrupt")))
    }

    /// The named graphs that hold quads.
    pub(crate) fn named_graphs(&self) -> RdfResult<Vec<u64>> {
        Ok(self
            .keys
            .scan(&[GRAPHS])
            .map_err(storage)?
            .into_iter()
            .filter_map(|(key, _)| Some(u64::from_be_bytes(key.get(1..9)?.try_into().ok()?)))
            .collect())
    }

    fn scan_order(&self, order: u8, bound: &[u64], pattern: &IdPattern, out: &mut Vec<IdQuad>) -> RdfResult<()> {
        let mut prefix = vec![order];
        for id in bound {
            prefix.extend_from_slice(&id.to_be_bytes());
        }
        for (key, _) in self.keys.scan(&prefix).map_err(storage)? {
            let Some(ids) = ids_of(&key) else {
                continue;
            };
            let quad = match order {
                BY_SUBJECT => ids,
                BY_PREDICATE => [ids[0], ids[3], ids[1], ids[2]],
                _ => [ids[0], ids[2], ids[3], ids[1]],
            };
            let fits = |wanted: Option<u64>, found: u64| wanted.is_none_or(|wanted| wanted == found);
            if fits(pattern.graph, quad[0])
                && fits(pattern.subject, quad[1])
                && fits(pattern.predicate, quad[2])
                && fits(pattern.object, quad[3])
            {
                out.push(quad);
            }
        }
        Ok(())
    }

    /// The quads matching `pattern` in one graph, read from the order whose leading positions
    /// the pattern binds.
    fn matching_in(&self, graph: u64, pattern: &IdPattern, out: &mut Vec<IdQuad>) -> RdfResult<()> {
        match (pattern.subject, pattern.predicate, pattern.object) {
            (Some(subject), Some(predicate), Some(object)) => {
                if self.keys.get(&order_key(BY_SUBJECT, [graph, subject, predicate, object])).map_err(storage)?.is_some() {
                    out.push([graph, subject, predicate, object]);
                }
                Ok(())
            }
            (Some(subject), Some(predicate), None) => self.scan_order(BY_SUBJECT, &[graph, subject, predicate], pattern, out),
            (Some(subject), None, Some(object)) => self.scan_order(BY_OBJECT, &[graph, object, subject], pattern, out),
            (Some(subject), None, None) => self.scan_order(BY_SUBJECT, &[graph, subject], pattern, out),
            (None, Some(predicate), Some(object)) => self.scan_order(BY_PREDICATE, &[graph, predicate, object], pattern, out),
            (None, Some(predicate), None) => self.scan_order(BY_PREDICATE, &[graph, predicate], pattern, out),
            (None, None, Some(object)) => self.scan_order(BY_OBJECT, &[graph, object], pattern, out),
            (None, None, None) => self.scan_order(BY_SUBJECT, &[graph], pattern, out),
        }
    }

    /// The quads matching `pattern`; an unbound graph reads the default graph and every named
    /// graph.
    pub(crate) fn matching(&self, pattern: &IdPattern) -> RdfResult<Vec<IdQuad>> {
        let mut out = Vec::new();
        match pattern.graph {
            Some(graph) => self.matching_in(graph, pattern, &mut out)?,
            None => {
                self.matching_in(DEFAULT_GRAPH, pattern, &mut out)?;
                for graph in self.named_graphs()? {
                    self.matching_in(graph, pattern, &mut out)?;
                }
            }
        }
        Ok(out)
    }

    /// The numbers of `quad`'s terms, or `None` when one was never stored (so the quad is not
    /// there).
    pub(crate) fn ids_of_quad(&self, quad: &Quad) -> RdfResult<Option<IdQuad>> {
        let graph = match &quad.graph {
            None => DEFAULT_GRAPH,
            Some(term) => match self.id_of(term)? {
                Some(id) => id,
                None => return Ok(None),
            },
        };
        let (Some(subject), Some(predicate), Some(object)) =
            (self.id_of(&quad.subject)?, self.id_of(&quad.predicate)?, self.id_of(&quad.object)?)
        else {
            return Ok(None);
        };
        Ok(Some([graph, subject, predicate, object]))
    }

    /// The quad numbered `ids` as terms.
    pub(crate) fn quad_of(&self, ids: IdQuad) -> RdfResult<Quad> {
        Ok(Quad {
            graph: if ids[0] == DEFAULT_GRAPH { None } else { Some(self.term_of(ids[0])?) },
            subject: self.term_of(ids[1])?,
            predicate: self.term_of(ids[2])?,
            object: self.term_of(ids[3])?,
        })
    }
}

/// Changes quads within a transaction.
pub(crate) struct RdfWrite<'k> {
    pub(crate) keys: &'k mut dyn KeysWrite,
}

impl RdfWrite<'_> {
    pub(crate) fn read(&self) -> RdfRead<'_> {
        RdfRead { keys: &*self.keys }
    }

    /// A number no term or blank node scope has had.
    pub(crate) fn next_number(&mut self) -> RdfResult<u64> {
        let current = self
            .keys
            .get(COUNTER)
            .map_err(storage)?
            .and_then(|bytes| bytes.try_into().ok())
            .map_or(0, u64::from_be_bytes);
        let next = current
            .checked_add(1)
            .ok_or_else(|| RdfError::storage("the term numbers are used up".to_owned()))?;
        self.keys.put(COUNTER, &next.to_be_bytes()).map_err(storage)?;
        Ok(next)
    }

    /// The number of `term`, given one when it has none yet.
    pub(crate) fn intern(&mut self, term: &Term) -> RdfResult<u64> {
        if let Some(id) = self.read().id_of(term)? {
            return Ok(id);
        }
        let id = self.next_number()?;
        self.keys.put(&term_key(term), &id.to_be_bytes()).map_err(storage)?;
        self.keys.put(&id_key(id), &term.to_bytes()).map_err(storage)?;
        Ok(id)
    }

    fn put_ids(&mut self, [graph, subject, predicate, object]: IdQuad) -> RdfResult<bool> {
        let primary = order_key(BY_SUBJECT, [graph, subject, predicate, object]);
        if self.keys.get(&primary).map_err(storage)?.is_some() {
            return Ok(false);
        }
        self.keys.put(&primary, &[]).map_err(storage)?;
        self.keys.put(&order_key(BY_PREDICATE, [graph, predicate, object, subject]), &[]).map_err(storage)?;
        self.keys.put(&order_key(BY_OBJECT, [graph, object, subject, predicate]), &[]).map_err(storage)?;
        if graph != DEFAULT_GRAPH {
            self.keys.put(&graph_key(graph), &[]).map_err(storage)?;
        }
        Ok(true)
    }

    /// Stores `quad`; returns whether it was new.
    pub(crate) fn insert(&mut self, quad: &Quad) -> RdfResult<bool> {
        if !matches!(quad.subject, Term::Iri(_) | Term::Blank(_)) || !matches!(quad.predicate, Term::Iri(_)) {
            return Err(RdfError::invalid("a subject is an IRI or a blank node, and a predicate an IRI"));
        }
        if matches!(quad.graph, Some(Term::Literal(_))) {
            return Err(RdfError::invalid("a graph name is an IRI or a blank node"));
        }
        let graph = match &quad.graph {
            None => DEFAULT_GRAPH,
            Some(term) => self.intern(term)?,
        };
        let ids = [graph, self.intern(&quad.subject)?, self.intern(&quad.predicate)?, self.intern(&quad.object)?];
        self.put_ids(ids)
    }

    /// Removes the quad numbered `ids`; returns whether it was there.
    pub(crate) fn remove_ids(&mut self, [graph, subject, predicate, object]: IdQuad) -> RdfResult<bool> {
        let removed = self
            .keys
            .delete(&order_key(BY_SUBJECT, [graph, subject, predicate, object]))
            .map_err(storage)?;
        if removed {
            self.keys.delete(&order_key(BY_PREDICATE, [graph, predicate, object, subject])).map_err(storage)?;
            self.keys.delete(&order_key(BY_OBJECT, [graph, object, subject, predicate])).map_err(storage)?;
            if graph != DEFAULT_GRAPH
                && self.read().matching(&IdPattern { graph: Some(graph), ..IdPattern::default() })?.is_empty()
            {
                self.keys.delete(&graph_key(graph)).map_err(storage)?;
            }
        }
        Ok(removed)
    }

    /// Removes `quad`; returns whether it was there.
    pub(crate) fn remove(&mut self, quad: &Quad) -> RdfResult<bool> {
        match self.read().ids_of_quad(quad)? {
            Some(ids) => self.remove_ids(ids),
            None => Ok(false),
        }
    }

    /// Removes every quad of `graph`; returns how many.
    pub(crate) fn clear_graph(&mut self, graph: u64) -> RdfResult<usize> {
        let quads = self.read().matching(&IdPattern { graph: Some(graph), ..IdPattern::default() })?;
        for ids in &quads {
            self.remove_ids(*ids)?;
        }
        Ok(quads.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_keyspace::{Keyspace, MemoryKeyspace};

    fn quad(subject: &str, predicate: &str, object: &str, graph: Option<&str>) -> Quad {
        Quad {
            subject: Term::iri(subject),
            predicate: Term::iri(predicate),
            object: Term::iri(object),
            graph: graph.map(Term::iri),
        }
    }

    #[test]
    fn quads_are_found_by_any_bound_positions() {
        let keyspace = MemoryKeyspace::default();
        keyspace
            .write(0, &mut |keys| {
                let mut store = RdfWrite { keys };
                for added in [quad("s1", "p", "o1", None), quad("s1", "p", "o2", None), quad("s2", "q", "o1", Some("g"))] {
                    store.insert(&added).map_err(|error| error.message)?;
                }
                assert!(!store.insert(&quad("s1", "p", "o1", None)).map_err(|error| error.message)?);
                let read = store.read();
                let id = |name: &str| read.id_of(&Term::iri(name)).ok().flatten();
                let by_object = read
                    .matching(&IdPattern { object: id("o1"), ..IdPattern::default() })
                    .map_err(|error| error.message)?;
                assert_eq!(by_object.len(), 2);
                let in_default = read
                    .matching(&IdPattern { graph: Some(DEFAULT_GRAPH), predicate: id("p"), ..IdPattern::default() })
                    .map_err(|error| error.message)?;
                assert_eq!(in_default.len(), 2);
                assert_eq!(read.named_graphs().map_err(|error| error.message)?, vec![id("g").unwrap_or_default()]);
                assert!(store.remove(&quad("s2", "q", "o1", Some("g"))).map_err(|error| error.message)?);
                assert!(store.read().named_graphs().map_err(|error| error.message)?.is_empty());
                Ok(())
            })
            .unwrap();
    }
}
