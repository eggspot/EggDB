//! Evaluates SPARQL 1.1 queries over the quad store (D888), following the algebra's
//! semantics (SPARQL 1.1 Query Language, section 18.5): basic graph patterns joined triple by
//! triple, left joins, unions, minus, filters, extends, inline data, named graphs, property
//! paths, grouping and aggregates, ordering, projection, `DISTINCT` and slicing, and the four
//! query forms.

use crate::sparql_ast::{
    AggregateKind, Arithmetic, Comparison, Expr, GraphPattern, Node, Path, Projection, Query, QueryForm,
    TriplePattern, Verb,
};
use crate::sparql_values::{self as values, Evaluated, TypeError};
use crate::store::{DEFAULT_GRAPH, IdPattern, RdfRead};
use crate::term::{Literal, Quad, Term, XSD_DATE_TIME, XSD_INTEGER};
use crate::{RdfError, RdfResult};
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};

const MAX_SOLUTIONS: usize = 1_000_000;
const MAX_PATH_NODES: usize = 1_000_000;
const ORDER_KEY: &str = "\u{0}order";

/// A solution: variables bound to terms.
pub(crate) type Solution = BTreeMap<String, Term>;

/// What a query returned.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryOutcome {
    /// A `SELECT` query's variables and rows; `None` is an unbound variable.
    Solutions {
        /// The projected variables, in order.
        variables: Vec<String>,
        /// One value per variable in each row.
        rows: Vec<Vec<Option<Term>>>,
    },
    /// An `ASK` query's answer.
    Boolean(bool),
    /// A `CONSTRUCT` or `DESCRIBE` query's triples.
    Graph(Vec<Quad>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Active {
    Default,
    Named(u64),
}

fn too_many() -> RdfError {
    RdfError::invalid(format!("the query produces more than {MAX_SOLUTIONS} intermediate solutions"))
}

fn compatible(left: &Solution, right: &Solution) -> bool {
    left.iter().all(|(name, value)| right.get(name).is_none_or(|other| other == value))
}

fn merged(left: &Solution, right: &Solution) -> Solution {
    let mut both = left.clone();
    for (name, value) in right {
        both.insert(name.clone(), value.clone());
    }
    both
}

fn bind(solution: &mut Solution, node: &Node, term: Term) -> bool {
    match node {
        Node::Term(expected) => *expected == term,
        Node::Variable(name) => match solution.get(name) {
            Some(bound) => *bound == term,
            None => {
                solution.insert(name.clone(), term);
                true
            }
        },
    }
}

fn collect_variables(pattern: &GraphPattern, out: &mut Vec<String>) {
    let mut add = |name: &str| {
        if !name.starts_with("_:") && !out.iter().any(|known| known == name) {
            out.push(name.to_owned());
        }
    };
    match pattern {
        GraphPattern::Triples(triples) => {
            for triple in triples {
                for node in [&triple.subject, &triple.object] {
                    if let Node::Variable(name) = node {
                        add(name);
                    }
                }
                if let Verb::Variable(name) = &triple.verb {
                    add(name);
                }
            }
        }
        GraphPattern::Join(left, right)
        | GraphPattern::Optional(left, right, _)
        | GraphPattern::Union(left, right) => {
            collect_variables(left, out);
            collect_variables(right, out);
        }
        GraphPattern::Minus(left, _) | GraphPattern::Filter(_, left) => collect_variables(left, out),
        GraphPattern::Bind(inner, name, _) => {
            collect_variables(inner, out);
            add(name);
        }
        GraphPattern::Values(names, _) => names.iter().for_each(|name| add(name)),
        GraphPattern::Service { inner, .. } => collect_variables(inner, out),
        GraphPattern::Graph(node, inner) => {
            if let Node::Variable(name) = node {
                add(name);
            }
            collect_variables(inner, out);
        }
        GraphPattern::Select(query) => {
            for name in projected_names(query) {
                add(&name);
            }
        }
    }
}

fn projected_names(query: &Query) -> Vec<String> {
    match &query.form {
        QueryForm::Select {
            projection: Some(columns),
            ..
        } => columns.iter().map(|column| column.name.clone()).collect(),
        _ => {
            let mut names = Vec::new();
            collect_variables(&query.pattern, &mut names);
            names
        }
    }
}

fn has_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Aggregate { .. } => true,
        Expr::Or(left, right) | Expr::And(left, right) | Expr::Compare(_, left, right) | Expr::Arithmetic(_, left, right) => {
            has_aggregate(left) || has_aggregate(right)
        }
        Expr::Not(inner) | Expr::Negate(inner) => has_aggregate(inner),
        Expr::In(inner, list, _) => has_aggregate(inner) || list.iter().any(has_aggregate),
        Expr::Call(_, arguments) => arguments.iter().any(has_aggregate),
        Expr::Variable(_) | Expr::Constant(_) | Expr::Exists(..) => false,
    }
}

pub(crate) struct Evaluator<'r, 'k> {
    read: &'r RdfRead<'k>,
    default_graphs: Vec<u64>,
    named_graphs: Vec<u64>,
    terms: RefCell<HashMap<u64, Term>>,
    ids: RefCell<HashMap<Term, Option<u64>>>,
    blanks: Cell<u64>,
    random: Cell<u64>,
    now: Term,
}

impl<'r, 'k> Evaluator<'r, 'k> {
    pub(crate) fn new(read: &'r RdfRead<'k>, dataset: Option<&crate::sparql_ast::Dataset>) -> RdfResult<Self> {
        let clock = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let mut evaluator = Self {
            read,
            default_graphs: vec![DEFAULT_GRAPH],
            named_graphs: Vec::new(),
            terms: RefCell::new(HashMap::new()),
            ids: RefCell::new(HashMap::new()),
            blanks: Cell::new(0),
            random: Cell::new(clock.as_nanos() as u64 | 1),
            now: Term::Literal(Literal::typed(&utc_stamp(clock.as_secs()), XSD_DATE_TIME)),
        };
        match dataset {
            None => evaluator.named_graphs = read.named_graphs()?,
            Some(dataset) => {
                evaluator.default_graphs.clear();
                for iri in &dataset.default {
                    if let Some(id) = evaluator.id(&Term::Iri(iri.clone()))? {
                        evaluator.default_graphs.push(id);
                    }
                }
                for iri in &dataset.named {
                    if let Some(id) = evaluator.id(&Term::Iri(iri.clone()))? {
                        evaluator.named_graphs.push(id);
                    }
                }
            }
        }
        Ok(evaluator)
    }

    /// Evaluates against a default graph made of `graph` (for `WITH` in updates).
    pub(crate) fn with_default_graph(mut self, graph: Option<u64>) -> Self {
        if let Some(graph) = graph {
            self.default_graphs = vec![graph];
        }
        self
    }

    fn term(&self, id: u64) -> RdfResult<Term> {
        if let Some(term) = self.terms.borrow().get(&id) {
            return Ok(term.clone());
        }
        let term = self.read.term_of(id)?;
        self.terms.borrow_mut().insert(id, term.clone());
        Ok(term)
    }

    pub(crate) fn id(&self, term: &Term) -> RdfResult<Option<u64>> {
        if let Some(id) = self.ids.borrow().get(term) {
            return Ok(*id);
        }
        let id = self.read.id_of(term)?;
        self.ids.borrow_mut().insert(term.clone(), id);
        Ok(id)
    }

    fn graphs(&self, active: Active) -> Vec<u64> {
        match active {
            Active::Default => self.default_graphs.clone(),
            Active::Named(graph) => vec![graph],
        }
    }

    fn quads(&self, active: Active, subject: Option<u64>, predicate: Option<u64>, object: Option<u64>) -> RdfResult<Vec<[u64; 4]>> {
        let mut found = Vec::new();
        for graph in self.graphs(active) {
            found.extend(self.read.matching(&IdPattern {
                graph: Some(graph),
                subject,
                predicate,
                object,
            })?);
            if found.len() > MAX_SOLUTIONS {
                return Err(too_many());
            }
        }
        Ok(found)
    }

    fn fresh_blank(&self) -> Term {
        let next = self.blanks.get() + 1;
        self.blanks.set(next);
        Term::Blank(format!("q{next}"))
    }

    fn next_random(&self) -> u64 {
        let mut state = self.random.get();
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        self.random.set(state);
        state
    }

    fn uuid(&self) -> String {
        let (high, low) = (self.next_random(), self.next_random());
        let bytes: Vec<u8> = high.to_be_bytes().into_iter().chain(low.to_be_bytes()).collect();
        let hex: String = bytes
            .iter()
            .enumerate()
            .map(|(at, byte)| match at {
                6 => format!("{:02x}", (byte & 0x0f) | 0x40),
                8 => format!("{:02x}", (byte & 0x3f) | 0x80),
                _ => format!("{byte:02x}"),
            })
            .collect();
        format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
    }

    pub(crate) fn evaluate(&self, pattern: &GraphPattern, active: Active) -> RdfResult<Vec<Solution>> {
        Ok(match pattern {
            GraphPattern::Triples(triples) => self.basic(triples, active)?,
            GraphPattern::Join(left, right) => {
                let left = self.evaluate(left, active)?;
                let right = self.evaluate(right, active)?;
                let mut joined = Vec::new();
                for first in &left {
                    for second in &right {
                        if compatible(first, second) {
                            joined.push(merged(first, second));
                            if joined.len() > MAX_SOLUTIONS {
                                return Err(too_many());
                            }
                        }
                    }
                }
                joined
            }
            GraphPattern::Optional(left, right, condition) => {
                let left = self.evaluate(left, active)?;
                let right = self.evaluate(right, active)?;
                let mut joined = Vec::new();
                for first in &left {
                    let mut matched = false;
                    for second in &right {
                        if !compatible(first, second) {
                            continue;
                        }
                        let both = merged(first, second);
                        let keep = match condition {
                            None => true,
                            Some(condition) => self
                                .value(condition, &both, active, None)
                                .and_then(|term| values::effective_boolean(&term))
                                .unwrap_or(false),
                        };
                        if keep {
                            matched = true;
                            joined.push(both);
                        }
                    }
                    if !matched {
                        joined.push(first.clone());
                    }
                    if joined.len() > MAX_SOLUTIONS {
                        return Err(too_many());
                    }
                }
                joined
            }
            GraphPattern::Union(left, right) => {
                let mut both = self.evaluate(left, active)?;
                both.extend(self.evaluate(right, active)?);
                both
            }
            GraphPattern::Minus(left, right) => {
                let left = self.evaluate(left, active)?;
                let right = self.evaluate(right, active)?;
                left.into_iter()
                    .filter(|first| {
                        !right.iter().any(|second| {
                            compatible(first, second) && first.keys().any(|name| second.contains_key(name))
                        })
                    })
                    .collect()
            }
            GraphPattern::Filter(condition, inner) => self
                .evaluate(inner, active)?
                .into_iter()
                .filter(|solution| {
                    self.value(condition, solution, active, None)
                        .and_then(|term| values::effective_boolean(&term))
                        .unwrap_or(false)
                })
                .collect(),
            GraphPattern::Bind(inner, name, expr) => {
                let mut extended = self.evaluate(inner, active)?;
                for solution in &mut extended {
                    if solution.contains_key(name) {
                        return Err(RdfError::invalid(format!("BIND assigns ?{name}, which is already in scope")));
                    }
                    if let Ok(value) = self.value(expr, solution, active, None) {
                        solution.insert(name.clone(), value);
                    }
                }
                extended
            }
            GraphPattern::Service { endpoint, query, silent, .. } => match crate::remote_solutions(endpoint, query) {
                Ok(solutions) => solutions,
                Err(_) if *silent => vec![Solution::new()],
                Err(error) => return Err(error),
            },
            GraphPattern::Values(names, rows) => rows
                .iter()
                .map(|row| {
                    names
                        .iter()
                        .zip(row)
                        .filter_map(|(name, value)| value.clone().map(|value| (name.clone(), value)))
                        .collect()
                })
                .collect(),
            GraphPattern::Graph(node, inner) => {
                let mut found = Vec::new();
                match node {
                    Node::Term(term) => {
                        if let Some(graph) = self.id(term)?
                            && self.named_graphs.contains(&graph)
                        {
                            found = self.evaluate(inner, Active::Named(graph))?;
                        }
                    }
                    Node::Variable(name) => {
                        for graph in self.named_graphs.clone() {
                            let graph_term = self.term(graph)?;
                            for mut solution in self.evaluate(inner, Active::Named(graph))? {
                                if bind(&mut solution, &Node::Variable(name.clone()), graph_term.clone()) {
                                    found.push(solution);
                                }
                            }
                            if found.len() > MAX_SOLUTIONS {
                                return Err(too_many());
                            }
                        }
                    }
                }
                found
            }
            GraphPattern::Select(query) => {
                let (names, rows) = self.select_rows(query, active)?;
                rows.into_iter()
                    .map(|row| {
                        names
                            .iter()
                            .zip(row)
                            .filter_map(|(name, value)| value.map(|value| (name.clone(), value)))
                            .collect()
                    })
                    .collect()
            }
        })
    }

    fn basic(&self, triples: &[TriplePattern], active: Active) -> RdfResult<Vec<Solution>> {
        let mut solutions = vec![Solution::new()];
        for triple in triples {
            let mut next = Vec::new();
            for solution in &solutions {
                self.match_triple(triple, solution, active, &mut next)?;
                if next.len() > MAX_SOLUTIONS {
                    return Err(too_many());
                }
            }
            solutions = next;
            if solutions.is_empty() {
                break;
            }
        }
        Ok(solutions)
    }

    fn bound(node: &Node, solution: &Solution) -> Option<Term> {
        match node {
            Node::Term(term) => Some(term.clone()),
            Node::Variable(name) => solution.get(name).cloned(),
        }
    }

    fn match_triple(&self, triple: &TriplePattern, solution: &Solution, active: Active, out: &mut Vec<Solution>) -> RdfResult<()> {
        let subject = Self::bound(&triple.subject, solution);
        let object = Self::bound(&triple.object, solution);
        let predicate = match &triple.verb {
            Verb::Variable(name) => solution.get(name).cloned(),
            Verb::Path(Path::Iri(iri)) => Some(Term::Iri(iri.clone())),
            Verb::Path(path) => {
                for (from, to) in self.path(path, subject.as_ref(), object.as_ref(), active)? {
                    let mut extended = solution.clone();
                    if bind(&mut extended, &triple.subject, from) && bind(&mut extended, &triple.object, to) {
                        out.push(extended);
                    }
                }
                return Ok(());
            }
        };
        let id_of = |term: &Option<Term>| -> RdfResult<Option<Option<u64>>> {
            match term {
                None => Ok(Some(None)),
                Some(term) => Ok(self.id(term)?.map(Some)),
            }
        };
        let (Some(subject_id), Some(predicate_id), Some(object_id)) = (id_of(&subject)?, id_of(&predicate)?, id_of(&object)?) else {
            return Ok(());
        };
        for [_, s, p, o] in self.quads(active, subject_id, predicate_id, object_id)? {
            let mut extended = solution.clone();
            let predicate_node = match &triple.verb {
                Verb::Variable(name) => Node::Variable(name.clone()),
                Verb::Path(_) => Node::Term(self.term(p)?),
            };
            if bind(&mut extended, &triple.subject, self.term(s)?)
                && bind(&mut extended, &predicate_node, self.term(p)?)
                && bind(&mut extended, &triple.object, self.term(o)?)
            {
                out.push(extended);
            }
        }
        Ok(())
    }

    fn all_nodes(&self, active: Active) -> RdfResult<Vec<Term>> {
        let mut seen = HashSet::new();
        let mut nodes = Vec::new();
        for [_, s, _, o] in self.quads(active, None, None, None)? {
            for id in [s, o] {
                if seen.insert(id) {
                    nodes.push(self.term(id)?);
                }
            }
        }
        Ok(nodes)
    }

    fn step(&self, iri: &str, from: Option<&Term>, to: Option<&Term>, active: Active) -> RdfResult<Vec<(Term, Term)>> {
        let Some(predicate) = self.id(&Term::Iri(iri.to_owned()))? else {
            return Ok(Vec::new());
        };
        let id_of = |term: Option<&Term>| -> RdfResult<Option<Option<u64>>> {
            match term {
                None => Ok(Some(None)),
                Some(term) => Ok(self.id(term)?.map(Some)),
            }
        };
        let (Some(from_id), Some(to_id)) = (id_of(from)?, id_of(to)?) else {
            return Ok(Vec::new());
        };
        let mut pairs = Vec::new();
        for [_, s, _, o] in self.quads(active, from_id, Some(predicate), to_id)? {
            pairs.push((self.term(s)?, self.term(o)?));
        }
        Ok(pairs)
    }

    fn reachable(&self, inner: &Path, start: &Term, forward: bool, include_start: bool, active: Active) -> RdfResult<Vec<Term>> {
        let mut seen: HashSet<Term> = HashSet::new();
        let mut reached = Vec::new();
        if include_start {
            seen.insert(start.clone());
            reached.push(start.clone());
        }
        let mut frontier = vec![start.clone()];
        while let Some(node) = frontier.pop() {
            let pairs = if forward {
                self.path(inner, Some(&node), None, active)?
            } else {
                self.path(inner, None, Some(&node), active)?
            };
            for (from, to) in pairs {
                let next = if forward { to } else { from };
                if seen.insert(next.clone()) {
                    reached.push(next.clone());
                    frontier.push(next);
                    if seen.len() > MAX_PATH_NODES {
                        return Err(too_many());
                    }
                }
            }
        }
        Ok(reached)
    }

    fn path(&self, path: &Path, from: Option<&Term>, to: Option<&Term>, active: Active) -> RdfResult<Vec<(Term, Term)>> {
        Ok(match path {
            Path::Iri(iri) => self.step(iri, from, to, active)?,
            Path::Inverse(inner) => self
                .path(inner, to, from, active)?
                .into_iter()
                .map(|(a, b)| (b, a))
                .collect(),
            Path::Alternative(choices) => {
                let mut pairs = Vec::new();
                for choice in choices {
                    pairs.extend(self.path(choice, from, to, active)?);
                }
                pairs
            }
            Path::Sequence(parts) => {
                let Some((first, rest)) = parts.split_first() else {
                    return Ok(Vec::new());
                };
                let mut pairs = self.path(first, from, if rest.is_empty() { to } else { None }, active)?;
                for (index, part) in rest.iter().enumerate() {
                    let last = index + 1 == rest.len();
                    let mut next = Vec::new();
                    for (start, middle) in pairs {
                        for (_, end) in self.path(part, Some(&middle), if last { to } else { None }, active)? {
                            next.push((start.clone(), end));
                            if next.len() > MAX_SOLUTIONS {
                                return Err(too_many());
                            }
                        }
                    }
                    pairs = next;
                }
                pairs
            }
            Path::ZeroOrOne(inner) | Path::ZeroOrMore(inner) | Path::OneOrMore(inner) => {
                let include_start = !matches!(path, Path::OneOrMore(_));
                let only_one = matches!(path, Path::ZeroOrOne(_));
                let mut pairs: Vec<(Term, Term)> = Vec::new();
                let mut seen: HashSet<(Term, Term)> = HashSet::new();
                let mut push = |pair: (Term, Term), pairs: &mut Vec<(Term, Term)>| {
                    if seen.insert(pair.clone()) {
                        pairs.push(pair);
                    }
                };
                let starts: Vec<(Term, bool)> = match (from, to) {
                    (Some(from), _) => vec![(from.clone(), true)],
                    (None, Some(to)) => vec![(to.clone(), false)],
                    (None, None) => self.all_nodes(active)?.into_iter().map(|node| (node, true)).collect(),
                };
                for (start, forward) in starts {
                    let reached = if only_one {
                        let mut reached = if include_start { vec![start.clone()] } else { Vec::new() };
                        let pairs = if forward {
                            self.path(inner, Some(&start), None, active)?
                        } else {
                            self.path(inner, None, Some(&start), active)?
                        };
                        reached.extend(pairs.into_iter().map(|(a, b)| if forward { b } else { a }));
                        reached
                    } else {
                        self.reachable(inner, &start, forward, include_start, active)?
                    };
                    for end in reached {
                        let pair = if forward { (start.clone(), end) } else { (end, start.clone()) };
                        let fits = if forward { to.is_none_or(|to| *to == pair.1) } else { true };
                        if fits {
                            push(pair, &mut pairs);
                        }
                    }
                }
                pairs
            }
            Path::Negated(items) => {
                let forward: Vec<&str> = items.iter().filter(|(_, inverse)| !inverse).map(|(iri, _)| iri.as_str()).collect();
                let inverse: Vec<&str> = items.iter().filter(|(_, inverse)| *inverse).map(|(iri, _)| iri.as_str()).collect();
                let mut pairs = Vec::new();
                let id_of = |term: Option<&Term>| -> RdfResult<Option<Option<u64>>> {
                    match term {
                        None => Ok(Some(None)),
                        Some(term) => Ok(self.id(term)?.map(Some)),
                    }
                };
                if !forward.is_empty() || inverse.is_empty() {
                    if let (Some(from_id), Some(to_id)) = (id_of(from)?, id_of(to)?) {
                        for [_, s, p, o] in self.quads(active, from_id, None, to_id)? {
                            if let Term::Iri(predicate) = self.term(p)?
                                && !forward.contains(&predicate.as_str())
                            {
                                pairs.push((self.term(s)?, self.term(o)?));
                            }
                        }
                    }
                }
                if !inverse.is_empty()
                    && let (Some(from_id), Some(to_id)) = (id_of(to)?, id_of(from)?)
                {
                    for [_, s, p, o] in self.quads(active, from_id, None, to_id)? {
                        if let Term::Iri(predicate) = self.term(p)?
                            && !inverse.contains(&predicate.as_str())
                        {
                            pairs.push((self.term(o)?, self.term(s)?));
                        }
                    }
                }
                pairs
            }
        })
    }

    fn aggregate(&self, kind: AggregateKind, distinct: bool, argument: Option<&Expr>, separator: Option<&str>, group: &[Solution], active: Active) -> Evaluated {
        let Some(argument) = argument else {
            let count = if distinct {
                let mut unique: Vec<&Solution> = Vec::new();
                for solution in group {
                    if !unique.contains(&solution) {
                        unique.push(solution);
                    }
                }
                unique.len()
            } else {
                group.len()
            };
            return Ok(values::Number::Integer(count as i128).term());
        };
        let mut found: Vec<Term> = Vec::new();
        for solution in group {
            if let Ok(value) = self.value(argument, solution, active, None) {
                if distinct && found.contains(&value) {
                    continue;
                }
                found.push(value);
            }
        }
        match kind {
            AggregateKind::Count => Ok(values::Number::Integer(found.len() as i128).term()),
            AggregateKind::Sum | AggregateKind::Avg => {
                let mut total = values::Number::Integer(0).term();
                for value in &found {
                    total = values::arithmetic(Arithmetic::Add, &total, value)?;
                }
                if kind == AggregateKind::Sum || found.is_empty() {
                    return Ok(total);
                }
                values::arithmetic(Arithmetic::Divide, &total, &values::Number::Integer(found.len() as i128).term())
            }
            AggregateKind::Min | AggregateKind::Max => {
                let picked = found.into_iter().reduce(|best, next| {
                    let order = values::order(Some(&next), Some(&best));
                    let better = if kind == AggregateKind::Min { order == Ordering::Less } else { order == Ordering::Greater };
                    if better { next } else { best }
                });
                picked.ok_or(TypeError)
            }
            AggregateKind::Sample => found.into_iter().next().ok_or(TypeError),
            AggregateKind::GroupConcat => {
                let mut texts = Vec::new();
                for value in &found {
                    texts.push(values::string_value(value)?);
                }
                Ok(values::plain(&texts.join(separator.unwrap_or(" "))))
            }
        }
    }

    pub(crate) fn value(&self, expr: &Expr, solution: &Solution, active: Active, group: Option<&[Solution]>) -> Evaluated {
        let value = |inner: &Expr| self.value(inner, solution, active, group);
        match expr {
            Expr::Variable(name) => solution.get(name).cloned().ok_or(TypeError),
            Expr::Constant(term) => Ok(term.clone()),
            Expr::Or(left, right) => {
                let left = value(left).and_then(|term| values::effective_boolean(&term));
                let right = value(right).and_then(|term| values::effective_boolean(&term));
                match (left, right) {
                    (Ok(true), _) | (_, Ok(true)) => Ok(values::boolean(true)),
                    (Ok(false), Ok(false)) => Ok(values::boolean(false)),
                    _ => Err(TypeError),
                }
            }
            Expr::And(left, right) => {
                let left = value(left).and_then(|term| values::effective_boolean(&term));
                let right = value(right).and_then(|term| values::effective_boolean(&term));
                match (left, right) {
                    (Ok(false), _) | (_, Ok(false)) => Ok(values::boolean(false)),
                    (Ok(true), Ok(true)) => Ok(values::boolean(true)),
                    _ => Err(TypeError),
                }
            }
            Expr::Not(inner) => Ok(values::boolean(!values::effective_boolean(&value(inner)?)?)),
            Expr::Compare(comparison, left, right) => {
                let (left, right) = (value(left)?, value(right)?);
                let result = match comparison {
                    Comparison::Equal => values::equals(&left, &right)?,
                    Comparison::NotEqual => !values::equals(&left, &right)?,
                    other => {
                        let order = values::compare(&left, &right).ok_or(TypeError)?;
                        match other {
                            Comparison::Less => order == Ordering::Less,
                            Comparison::LessOrEqual => order != Ordering::Greater,
                            Comparison::Greater => order == Ordering::Greater,
                            _ => order != Ordering::Less,
                        }
                    }
                };
                Ok(values::boolean(result))
            }
            Expr::Arithmetic(operator, left, right) => values::arithmetic(*operator, &value(left)?, &value(right)?),
            Expr::Negate(inner) => values::arithmetic(
                Arithmetic::Subtract,
                &Term::Literal(Literal::typed("0", XSD_INTEGER)),
                &value(inner)?,
            ),
            Expr::In(inner, list, negated) => {
                let wanted = value(inner)?;
                let mut failed = false;
                for item in list {
                    match value(item).and_then(|term| values::equals(&wanted, &term)) {
                        Ok(true) => return Ok(values::boolean(!negated)),
                        Ok(false) => {}
                        Err(_) => failed = true,
                    }
                }
                if failed { Err(TypeError) } else { Ok(values::boolean(*negated)) }
            }
            Expr::Exists(pattern, negated) => {
                let found = self
                    .evaluate(pattern, active)
                    .map_err(|_| TypeError)?
                    .iter()
                    .any(|candidate| compatible(solution, candidate));
                Ok(values::boolean(found != *negated))
            }
            Expr::Aggregate {
                kind,
                distinct,
                argument,
                separator,
            } => {
                let group = group.ok_or(TypeError)?;
                self.aggregate(*kind, *distinct, argument.as_deref(), separator.as_deref(), group, active)
            }
            Expr::Call(name, arguments) => match name.as_str() {
                "bound" => Ok(values::boolean(matches!(arguments.first(), Some(Expr::Variable(variable)) if solution.contains_key(variable)))),
                "if" => {
                    let [condition, then, otherwise] = arguments.as_slice() else {
                        return Err(TypeError);
                    };
                    if values::effective_boolean(&value(condition)?)? { value(then) } else { value(otherwise) }
                }
                "coalesce" => arguments.iter().find_map(|argument| value(argument).ok()).ok_or(TypeError),
                "now" => Ok(self.now.clone()),
                "rand" => Ok(values::Number::Double((self.next_random() >> 11) as f64 / (1u64 << 53) as f64).term()),
                "uuid" => Ok(Term::Iri(format!("urn:uuid:{}", self.uuid()))),
                "struuid" => Ok(values::plain(&self.uuid())),
                "bnode" => Ok(self.fresh_blank()),
                "md5" | "sha1" | "sha256" | "sha384" | "sha512" => Err(TypeError),
                _ => {
                    let mut evaluated = Vec::with_capacity(arguments.len());
                    for argument in arguments {
                        evaluated.push(value(argument)?);
                    }
                    values::call(name, &evaluated)
                }
            },
        }
    }

    /// A `SELECT` query's projected variables and rows, evaluated in `active`.
    pub(crate) fn select_rows(&self, query: &Query, active: Active) -> RdfResult<(Vec<String>, Vec<Vec<Option<Term>>>)> {
        let solutions = self.evaluate(&query.pattern, active)?;
        let QueryForm::Select { distinct, projection } = &query.form else {
            return Err(RdfError::invalid("a subquery is a SELECT"));
        };
        let grouped = !query.group_by.is_empty()
            || query.having.iter().any(has_aggregate)
            || query.order_by.iter().any(|key| has_aggregate(&key.expr))
            || projection.as_ref().is_some_and(|columns| columns.iter().any(|column| has_aggregate(&column.expr)));
        let mut rows: Vec<Solution> = if grouped {
            self.grouped(query, projection.as_deref(), solutions, active)?
        } else {
            let mut extended = solutions;
            for solution in &mut extended {
                for column in projection.iter().flatten() {
                    if matches!(&column.expr, Expr::Variable(name) if *name == column.name) {
                        continue;
                    }
                    if let Ok(value) = self.value(&column.expr, solution, active, None) {
                        solution.insert(column.name.clone(), value);
                    }
                }
                for (index, key) in query.order_by.iter().enumerate() {
                    if let Ok(value) = self.value(&key.expr, solution, active, None) {
                        solution.insert(format!("{ORDER_KEY}{index}"), value);
                    }
                }
            }
            extended
        };
        if !query.order_by.is_empty() {
            rows.sort_by(|left, right| {
                for (index, key) in query.order_by.iter().enumerate() {
                    let name = format!("{ORDER_KEY}{index}");
                    let order = values::order(left.get(&name), right.get(&name));
                    let order = if key.descending { order.reverse() } else { order };
                    if order != Ordering::Equal {
                        return order;
                    }
                }
                Ordering::Equal
            });
        }
        let names = projected_names(query);
        let mut projected: Vec<Vec<Option<Term>>> = rows
            .iter()
            .map(|row| names.iter().map(|name| row.get(name).cloned()).collect())
            .collect();
        if *distinct {
            let mut seen: HashSet<Vec<Option<Term>>> = HashSet::new();
            projected.retain(|row| seen.insert(row.clone()));
        }
        let start = usize::try_from(query.offset).unwrap_or(usize::MAX);
        let projected: Vec<Vec<Option<Term>>> = projected
            .into_iter()
            .skip(start)
            .take(query.limit.map_or(usize::MAX, |limit| usize::try_from(limit).unwrap_or(usize::MAX)))
            .collect();
        Ok((names, projected))
    }

    fn grouped(&self, query: &Query, projection: Option<&[Projection]>, solutions: Vec<Solution>, active: Active) -> RdfResult<Vec<Solution>> {
        let mut groups: Vec<(Vec<Option<Term>>, Vec<Solution>)> = Vec::new();
        for solution in solutions {
            let key: Vec<Option<Term>> = query
                .group_by
                .iter()
                .map(|condition| self.value(&condition.expr, &solution, active, None).ok())
                .collect();
            match groups.iter_mut().find(|(known, _)| *known == key) {
                Some((_, members)) => members.push(solution),
                None => groups.push((key, vec![solution])),
            }
        }
        if groups.is_empty() && query.group_by.is_empty() {
            groups.push((Vec::new(), Vec::new()));
        }
        let mut rows = Vec::new();
        for (key, members) in groups {
            let mut base = Solution::new();
            for (condition, value) in query.group_by.iter().zip(&key) {
                if let Some(value) = value {
                    base.insert(condition.name.clone(), value.clone());
                }
            }
            let kept = query.having.iter().all(|condition| {
                self.value(condition, &base, active, Some(&members))
                    .and_then(|term| values::effective_boolean(&term))
                    .unwrap_or(false)
            });
            if !kept {
                continue;
            }
            for column in projection.into_iter().flatten() {
                if base.contains_key(&column.name) {
                    continue;
                }
                if let Ok(value) = self.value(&column.expr, &base, active, Some(&members)) {
                    base.insert(column.name.clone(), value);
                }
            }
            for (index, order_key) in query.order_by.iter().enumerate() {
                if let Ok(value) = self.value(&order_key.expr, &base, active, Some(&members)) {
                    base.insert(format!("{ORDER_KEY}{index}"), value);
                }
            }
            rows.push(base);
        }
        Ok(rows)
    }

    fn instantiate(&self, template: &[TriplePattern], solution: &Solution, blanks: &mut HashMap<String, Term>, out: &mut Vec<Quad>) {
        let mut place = |node: &Node, blanks: &mut HashMap<String, Term>| -> Option<Term> {
            match node {
                Node::Variable(name) if name.starts_with("_:") => {
                    Some(blanks.entry(name.clone()).or_insert_with(|| self.fresh_blank()).clone())
                }
                Node::Variable(name) => solution.get(name).cloned(),
                Node::Term(Term::Blank(label)) => {
                    Some(blanks.entry(label.clone()).or_insert_with(|| self.fresh_blank()).clone())
                }
                Node::Term(term) => Some(term.clone()),
            }
        };
        for triple in template {
            let predicate = match &triple.verb {
                Verb::Variable(name) => solution.get(name).cloned(),
                Verb::Path(Path::Iri(iri)) => Some(Term::Iri(iri.clone())),
                Verb::Path(_) => None,
            };
            let (Some(subject), Some(predicate), Some(object)) = (place(&triple.subject, blanks), predicate, place(&triple.object, blanks)) else {
                continue;
            };
            if matches!(subject, Term::Literal(_)) || !matches!(predicate, Term::Iri(_)) {
                continue;
            }
            let quad = Quad {
                subject,
                predicate,
                object,
                graph: None,
            };
            if !out.contains(&quad) {
                out.push(quad);
            }
        }
    }

    fn describe(&self, resource: &Term, out: &mut Vec<Quad>) -> RdfResult<()> {
        let Some(id) = self.id(resource)? else {
            return Ok(());
        };
        for [_, s, p, o] in self.quads(Active::Default, Some(id), None, None)? {
            let quad = Quad {
                subject: self.term(s)?,
                predicate: self.term(p)?,
                object: self.term(o)?,
                graph: None,
            };
            if !out.contains(&quad) {
                out.push(quad);
            }
        }
        Ok(())
    }

    /// Runs a query.
    pub(crate) fn query(&self, query: &Query) -> RdfResult<QueryOutcome> {
        match &query.form {
            QueryForm::Select { .. } => {
                let (variables, rows) = self.select_rows(query, Active::Default)?;
                Ok(QueryOutcome::Solutions { variables, rows })
            }
            QueryForm::Ask => Ok(QueryOutcome::Boolean(!self.evaluate(&query.pattern, Active::Default)?.is_empty())),
            QueryForm::Construct(template) => {
                let mut solutions = self.evaluate(&query.pattern, Active::Default)?;
                self.order_and_slice(query, &mut solutions);
                let mut triples = Vec::new();
                for solution in &solutions {
                    let mut blanks = HashMap::new();
                    self.instantiate(template, solution, &mut blanks, &mut triples);
                }
                Ok(QueryOutcome::Graph(triples))
            }
            QueryForm::Describe(described) => {
                let mut solutions = self.evaluate(&query.pattern, Active::Default)?;
                self.order_and_slice(query, &mut solutions);
                let mut triples = Vec::new();
                let mut resources: Vec<Term> = Vec::new();
                if described.is_empty() {
                    for solution in &solutions {
                        resources.extend(solution.iter().filter(|(name, _)| !name.starts_with("_:")).map(|(_, term)| term.clone()));
                    }
                }
                for node in described {
                    match node {
                        Node::Term(term) => resources.push(term.clone()),
                        Node::Variable(name) => resources.extend(solutions.iter().filter_map(|solution| solution.get(name).cloned())),
                    }
                }
                for resource in &resources {
                    self.describe(resource, &mut triples)?;
                }
                Ok(QueryOutcome::Graph(triples))
            }
        }
    }

    fn order_and_slice(&self, query: &Query, solutions: &mut Vec<Solution>) {
        if !query.order_by.is_empty() {
            let keyed: Vec<Vec<Option<Term>>> = solutions
                .iter()
                .map(|solution| {
                    query
                        .order_by
                        .iter()
                        .map(|key| self.value(&key.expr, solution, Active::Default, None).ok())
                        .collect()
                })
                .collect();
            let mut indexed: Vec<(Vec<Option<Term>>, Solution)> = keyed.into_iter().zip(solutions.drain(..)).collect();
            indexed.sort_by(|(left, _), (right, _)| {
                for ((a, b), key) in left.iter().zip(right).zip(&query.order_by) {
                    let order = values::order(a.as_ref(), b.as_ref());
                    let order = if key.descending { order.reverse() } else { order };
                    if order != Ordering::Equal {
                        return order;
                    }
                }
                Ordering::Equal
            });
            solutions.extend(indexed.into_iter().map(|(_, solution)| solution));
        }
        let start = usize::try_from(query.offset).unwrap_or(usize::MAX).min(solutions.len());
        solutions.drain(..start);
        if let Some(limit) = query.limit {
            solutions.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        }
    }
}

fn utc_stamp(seconds: u64) -> String {
    let days = seconds / 86_400;
    let rest = seconds % 86_400;
    let (hours, minutes, second) = (rest / 3600, rest % 3600 / 60, rest % 60);
    let shifted = days as i64 + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_stamps_follow_the_civil_calendar() {
        assert_eq!(utc_stamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_stamp(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc_stamp(1_762_948_800), "2025-11-12T12:00:00Z");
    }
}
