//! Runs SPARQL 1.1 Update operations (SPARQL 1.1 Update, section 3.1 and 3.2) in one write
//! transaction (D889): every operation of a request sees the changes of the ones before it,
//! and a failing operation undoes the whole request.

use crate::sparql_ast::{Dataset, GraphTarget, Node, QuadPattern, TransferKind, UpdateOperation};
use crate::sparql_eval::{Active, Evaluator, Solution};
use crate::store::{DEFAULT_GRAPH, IdPattern, RdfWrite};
use crate::term::{Quad, Term};
use crate::{RdfError, RdfResult};
use std::collections::HashMap;

/// What an update request changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UpdateSummary {
    /// Quads added.
    pub inserted: usize,
    /// Quads removed.
    pub deleted: usize,
}

fn ground(node: &Node, solution: &Solution, blanks: &mut HashMap<String, Term>, fresh: &mut dyn FnMut() -> Term) -> Option<Term> {
    match node {
        Node::Variable(name) if name.starts_with("_:") => {
            Some(blanks.entry(name.clone()).or_insert_with(&mut *fresh).clone())
        }
        Node::Variable(name) => solution.get(name).cloned(),
        Node::Term(Term::Blank(label)) => Some(blanks.entry(label.clone()).or_insert_with(&mut *fresh).clone()),
        Node::Term(term) => Some(term.clone()),
    }
}

fn quad_of(
    pattern: &QuadPattern,
    solution: &Solution,
    default_graph: Option<&Term>,
    blanks: &mut HashMap<String, Term>,
    fresh: &mut dyn FnMut() -> Term,
) -> Option<Quad> {
    let subject = ground(&pattern.subject, solution, blanks, fresh)?;
    let predicate = ground(&pattern.predicate, solution, blanks, fresh)?;
    let object = ground(&pattern.object, solution, blanks, fresh)?;
    let graph = match &pattern.graph {
        None => default_graph.cloned(),
        Some(node) => Some(ground(node, solution, blanks, fresh)?),
    };
    let valid = matches!(subject, Term::Iri(_) | Term::Blank(_))
        && matches!(predicate, Term::Iri(_))
        && !matches!(graph, Some(Term::Literal(_)));
    valid.then_some(Quad {
        subject,
        predicate,
        object,
        graph,
    })
}

fn graph_id(write: &RdfWrite<'_>, target: &GraphTarget) -> RdfResult<Option<Vec<u64>>> {
    Ok(match target {
        GraphTarget::Default => Some(vec![DEFAULT_GRAPH]),
        GraphTarget::Named(term) => write.read().id_of(term)?.map(|id| vec![id]),
        GraphTarget::AllNamed => Some(write.read().named_graphs()?),
        GraphTarget::All => {
            let mut graphs = vec![DEFAULT_GRAPH];
            graphs.extend(write.read().named_graphs()?);
            Some(graphs)
        }
    })
}

fn graph_quads(write: &RdfWrite<'_>, graph: u64) -> RdfResult<Vec<[u64; 4]>> {
    write.read().matching(&IdPattern {
        graph: Some(graph),
        ..IdPattern::default()
    })
}

fn has_quads(write: &RdfWrite<'_>, graph: u64) -> RdfResult<bool> {
    Ok(!graph_quads(write, graph)?.is_empty())
}

fn transfer(write: &mut RdfWrite<'_>, kind: TransferKind, from: &GraphTarget, to: &GraphTarget, silent: bool, summary: &mut UpdateSummary) -> RdfResult<()> {
    if from == to {
        return Ok(());
    }
    let source = match graph_id(write, from)?.and_then(|graphs| graphs.into_iter().next()) {
        Some(graph) if graph == DEFAULT_GRAPH || has_quads(write, graph)? => graph,
        _ if silent => return Ok(()),
        _ => return Err(RdfError::invalid("the source graph does not exist")),
    };
    let target = match to {
        GraphTarget::Default => DEFAULT_GRAPH,
        GraphTarget::Named(term) => write.intern(term)?,
        _ => return Err(RdfError::invalid("ADD, MOVE and COPY name one graph on each side")),
    };
    if kind != TransferKind::Add {
        summary.deleted += write.clear_graph(target)?;
    }
    for [_, subject, predicate, object] in graph_quads(write, source)? {
        let quad = write.read().quad_of([target, subject, predicate, object])?;
        if write.insert(&quad)? {
            summary.inserted += 1;
        }
    }
    if kind == TransferKind::Move {
        summary.deleted += write.clear_graph(source)?;
    }
    Ok(())
}

fn data(quads: &[QuadPattern]) -> RdfResult<Vec<Quad>> {
    let ground_only = || RdfError::invalid("INSERT DATA and DELETE DATA take ground triples only");
    let term = |node: &Node| match node {
        Node::Term(term) => Ok(term.clone()),
        Node::Variable(_) => Err(ground_only()),
    };
    let mut found = Vec::new();
    for pattern in quads {
        let quad = Quad {
            subject: term(&pattern.subject)?,
            predicate: term(&pattern.predicate)?,
            object: term(&pattern.object)?,
            graph: pattern.graph.as_ref().map(term).transpose()?,
        };
        let valid = matches!(quad.subject, Term::Iri(_) | Term::Blank(_))
            && matches!(quad.predicate, Term::Iri(_))
            && !matches!(quad.graph, Some(Term::Literal(_)));
        if !valid {
            return Err(ground_only());
        }
        found.push(quad);
    }
    Ok(found)
}

fn holds_placeholder(quad: &Quad) -> bool {
    [Some(&quad.subject), Some(&quad.object), quad.graph.as_ref()]
        .into_iter()
        .flatten()
        .any(|term| matches!(term, Term::Blank(label) if label.is_empty()))
}

/// Runs `operations` in `write`.
pub(crate) fn run(write: &mut RdfWrite<'_>, operations: &[UpdateOperation]) -> RdfResult<UpdateSummary> {
    let mut summary = UpdateSummary::default();
    for operation in operations {
        match operation {
            UpdateOperation::InsertData(quads) => {
                let quads = crate::scoped_blanks(write, data(quads)?)?;
                for quad in &quads {
                    if write.insert(quad)? {
                        summary.inserted += 1;
                    }
                }
            }
            UpdateOperation::DeleteData(quads) => {
                for quad in data(quads)? {
                    if matches!(quad.subject, Term::Blank(_)) || matches!(quad.object, Term::Blank(_)) {
                        return Err(RdfError::invalid("DELETE DATA takes no blank nodes"));
                    }
                    if write.remove(&quad)? {
                        summary.deleted += 1;
                    }
                }
            }
            UpdateOperation::Modify {
                with,
                delete,
                insert,
                using,
                pattern,
            } => {
                let with_id = match with {
                    Some(term) => write.read().id_of(term)?,
                    None => None,
                };
                let dataset: Option<&Dataset> = using.as_ref();
                let solutions = {
                    let read = write.read();
                    let mut evaluator = Evaluator::new(&read, dataset)?;
                    if dataset.is_none() && with.is_some() {
                        evaluator = evaluator.with_default_graph(Some(with_id.unwrap_or(u64::MAX)));
                    }
                    evaluator.evaluate(pattern, Active::Default)?
                };
                let mut removed = Vec::new();
                let mut added = Vec::new();
                let scope = write.next_number()?;
                let mut counter = 0u64;
                for solution in &solutions {
                    let mut blanks = HashMap::new();
                    let mut no_blanks = || Term::Blank(String::new());
                    for template in delete {
                        if let Some(quad) = quad_of(template, solution, with.as_ref(), &mut blanks, &mut no_blanks)
                            && !holds_placeholder(&quad)
                        {
                            removed.push(quad);
                        }
                    }
                    let mut blanks = HashMap::new();
                    let mut fresh = || {
                        counter += 1;
                        Term::Blank(format!("u{scope}_{counter}"))
                    };
                    for template in insert {
                        if let Some(quad) = quad_of(template, solution, with.as_ref(), &mut blanks, &mut fresh) {
                            added.push(quad);
                        }
                    }
                }
                for quad in &removed {
                    if write.remove(quad)? {
                        summary.deleted += 1;
                    }
                }
                for quad in &added {
                    if write.insert(quad)? {
                        summary.inserted += 1;
                    }
                }
            }
            UpdateOperation::Clear(target, silent) => match graph_id(write, target)? {
                Some(graphs) => {
                    if let GraphTarget::Named(_) = target
                        && !*silent
                    {
                        let mut exists = false;
                        for graph in &graphs {
                            exists |= has_quads(write, *graph)?;
                        }
                        if !exists {
                            return Err(RdfError::invalid("the graph to clear does not exist"));
                        }
                    }
                    for graph in graphs {
                        summary.deleted += write.clear_graph(graph)?;
                    }
                }
                None if *silent => {}
                None => return Err(RdfError::invalid("the graph to clear does not exist")),
            },
            UpdateOperation::Create(term, silent) => {
                if let Some(graph) = write.read().id_of(term)?
                    && has_quads(write, graph)?
                    && !*silent
                {
                    return Err(RdfError::invalid("the graph to create already exists"));
                }
            }
            UpdateOperation::Transfer { kind, from, to, silent } => transfer(write, *kind, from, to, *silent, &mut summary)?,
        }
    }
    Ok(summary)
}
