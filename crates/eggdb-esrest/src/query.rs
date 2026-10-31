use eggdb_sql::json::{Json, Node};
use std::collections::{BTreeMap, HashMap, HashSet};

const MAX_DEPTH: usize = 32;

/// A query of the query DSL.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Query {
    MatchAll,
    MatchNone,
    Match { fields: Vec<String>, terms: Vec<String>, all: bool },
    MatchPhrase { field: String, terms: Vec<String> },
    Term { field: String, value: Json },
    Terms { field: String, values: Vec<Json> },
    Range { field: String, bounds: Vec<(String, Json)> },
    Exists { field: String },
    Prefix { field: String, prefix: String },
    Ids { values: Vec<String> },
    Bool { must: Vec<Query>, filter: Vec<Query>, should: Vec<Query>, must_not: Vec<Query>, minimum_should_match: usize },
}

fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Node> {
    match &node.value {
        Json::Object(members) => members.iter().find(|(key, _)| key == name).map(|(_, value)| value),
        _ => None,
    }
}

fn text_of(node: &Node) -> Option<String> {
    match &node.value {
        Json::String(text) => Some(text.clone()),
        Json::Number(text) => Some(text.clone()),
        Json::Boolean(value) => Some(value.to_string()),
        _ => None,
    }
}

pub(crate) fn analyze(text: &str) -> Vec<String> {
    eggdb_search::words(text).into_iter().map(str::to_lowercase).collect()
}

fn single<'n>(node: &'n Node, what: &str) -> Result<(&'n str, &'n Node), String> {
    match &node.value {
        Json::Object(members) if members.len() == 1 => Ok((members[0].0.as_str(), &members[0].1)),
        _ => Err(format!("[{what}] query malformed, no start_object after query name")),
    }
}

fn list(node: Option<&Node>, depth: usize) -> Result<Vec<Query>, String> {
    match node.map(|node| &node.value) {
        None => Ok(Vec::new()),
        Some(Json::Array(items)) => items.iter().map(|item| parse(item, depth + 1)).collect(),
        Some(_) => Ok(vec![parse(node.ok_or_else(|| "missing clause".to_owned())?, depth + 1)?]),
    }
}

/// Reads one query of the query DSL.
pub(crate) fn parse(node: &Node, depth: usize) -> Result<Query, String> {
    if depth > MAX_DEPTH {
        return Err("the query nests too deeply".to_owned());
    }
    let (kind, body) = single(node, "query")?;
    Ok(match kind {
        "match_all" => Query::MatchAll,
        "match_none" => Query::MatchNone,
        "match" | "match_phrase" => {
            let (field, spec) = single(body, kind)?;
            let (query, all) = match &spec.value {
                Json::Object(_) => (
                    member(spec, "query").and_then(text_of).ok_or_else(|| format!("[{kind}] query needs a query"))?,
                    member(spec, "operator").and_then(text_of).is_some_and(|operator| operator.eq_ignore_ascii_case("and")),
                ),
                _ => (text_of(spec).ok_or_else(|| format!("[{kind}] needs text"))?, false),
            };
            if kind == "match_phrase" {
                Query::MatchPhrase { field: field.to_owned(), terms: analyze(&query) }
            } else {
                Query::Match { fields: vec![field.to_owned()], terms: analyze(&query), all }
            }
        }
        "multi_match" => {
            let query = member(body, "query").and_then(text_of).ok_or_else(|| "[multi_match] needs a query".to_owned())?;
            let fields = match member(body, "fields").map(|node| &node.value) {
                Some(Json::Array(items)) => items
                    .iter()
                    .filter_map(text_of)
                    .map(|field| field.split('^').next().unwrap_or_default().to_owned())
                    .collect(),
                _ => Vec::new(),
            };
            let all = member(body, "operator").and_then(text_of).is_some_and(|operator| operator.eq_ignore_ascii_case("and"));
            Query::Match { fields, terms: analyze(&query), all }
        }
        "term" => {
            let (field, spec) = single(body, kind)?;
            let value = match &spec.value {
                Json::Object(_) => member(spec, "value").map(|node| node.value.clone()).ok_or_else(|| "[term] needs a value".to_owned())?,
                other => other.clone(),
            };
            Query::Term { field: field.to_owned(), value }
        }
        "terms" => {
            let (field, spec) = single(body, kind)?;
            let Json::Array(items) = &spec.value else {
                return Err("[terms] needs an array of values".to_owned());
            };
            Query::Terms { field: field.to_owned(), values: items.iter().map(|item| item.value.clone()).collect() }
        }
        "range" => {
            let (field, spec) = single(body, kind)?;
            let Json::Object(members) = &spec.value else {
                return Err("[range] needs bounds".to_owned());
            };
            let bounds = members
                .iter()
                .filter(|(name, _)| matches!(name.as_str(), "gt" | "gte" | "lt" | "lte"))
                .map(|(name, value)| (name.clone(), value.value.clone()))
                .collect();
            Query::Range { field: field.to_owned(), bounds }
        }
        "exists" => Query::Exists {
            field: member(body, "field").and_then(text_of).ok_or_else(|| "[exists] needs a field".to_owned())?,
        },
        "prefix" => {
            let (field, spec) = single(body, kind)?;
            let prefix = match &spec.value {
                Json::Object(_) => member(spec, "value").and_then(text_of),
                _ => text_of(spec),
            }
            .ok_or_else(|| "[prefix] needs a value".to_owned())?;
            Query::Prefix { field: field.to_owned(), prefix }
        }
        "ids" => {
            let values = match member(body, "values").map(|node| &node.value) {
                Some(Json::Array(items)) => items.iter().filter_map(text_of).collect(),
                _ => Vec::new(),
            };
            Query::Ids { values }
        }
        "bool" => {
            let should = list(member(body, "should"), depth)?;
            let must = list(member(body, "must"), depth)?;
            let filter = list(member(body, "filter"), depth)?;
            let minimum = member(body, "minimum_should_match")
                .and_then(text_of)
                .and_then(|text| text.parse().ok())
                .unwrap_or(usize::from(must.is_empty() && filter.is_empty() && !should.is_empty()));
            Query::Bool { must, filter, should, must_not: list(member(body, "must_not"), depth)?, minimum_should_match: minimum }
        }
        other => return Err(format!("unknown query [{other}]")),
    })
}

/// The values at a dotted path of a document, arrays flattened.
pub(crate) fn values_at<'n>(source: &'n Node, path: &str) -> Vec<&'n Node> {
    let mut current = vec![source];
    for part in path.split('.') {
        let mut next = Vec::new();
        for node in current {
            if let Some(child) = member(node, part) {
                match &child.value {
                    Json::Array(items) => next.extend(items.iter()),
                    _ => next.push(child),
                }
            }
        }
        current = next;
    }
    current
}

/// What BM25 needs about one field across the searched documents.
#[derive(Debug, Default)]
pub(crate) struct FieldStats {
    documents: u64,
    total_length: u64,
    holding: HashMap<String, u64>,
}

/// The statistics of every field a query matches text in.
#[derive(Debug, Default)]
pub(crate) struct Stats {
    fields: HashMap<String, FieldStats>,
}

fn text_fields(query: &Query, out: &mut HashSet<String>) {
    match query {
        Query::Match { fields, .. } => out.extend(fields.iter().cloned()),
        Query::MatchPhrase { field, .. } => {
            out.insert(field.clone());
        }
        Query::Bool { must, filter, should, must_not, .. } => {
            for inner in must.iter().chain(filter).chain(should).chain(must_not) {
                text_fields(inner, out);
            }
        }
        _ => {}
    }
}

impl Stats {
    pub(crate) fn of(query: &Query, documents: &[(String, Node)]) -> Self {
        let mut wanted = HashSet::new();
        text_fields(query, &mut wanted);
        let mut stats = Self::default();
        for field in wanted {
            let mut field_stats = FieldStats::default();
            for (_, source) in documents {
                let tokens: Vec<String> = values_at(source, &field).into_iter().filter_map(text_of).flat_map(|text| analyze(&text)).collect();
                field_stats.documents += 1;
                field_stats.total_length += tokens.len() as u64;
                for token in tokens.into_iter().collect::<HashSet<_>>() {
                    *field_stats.holding.entry(token).or_default() += 1;
                }
            }
            stats.fields.insert(field, field_stats);
        }
        stats
    }
}

fn compare(value: &Json, bound: &Json) -> Option<std::cmp::Ordering> {
    match (value, bound) {
        (Json::Number(left), Json::Number(right)) => left.parse::<f64>().ok()?.partial_cmp(&right.parse::<f64>().ok()?),
        (Json::Number(left), Json::String(right)) | (Json::String(right), Json::Number(left)) if right.parse::<f64>().is_ok() => {
            let ordering = left.parse::<f64>().ok()?.partial_cmp(&right.parse::<f64>().ok()?)?;
            Some(if matches!(value, Json::Number(_)) { ordering } else { ordering.reverse() })
        }
        (Json::String(left), Json::String(right)) => Some(left.cmp(right)),
        _ => None,
    }
}

fn same(value: &Json, wanted: &Json) -> bool {
    match (value, wanted) {
        (Json::Number(left), Json::Number(right)) => left.parse::<f64>().ok() == right.parse::<f64>().ok(),
        (Json::String(left), Json::Number(right)) | (Json::Number(right), Json::String(left)) => left == right,
        (left, right) => left == right,
    }
}

/// The document's score for `query`, or `None` when it does not match.
pub(crate) fn score(query: &Query, id: &str, source: &Node, stats: &Stats) -> Option<f64> {
    match query {
        Query::MatchAll => Some(1.0),
        Query::MatchNone => None,
        Query::Match { fields, terms, all } => {
            let mut best: Option<f64> = None;
            for field in fields {
                let tokens: Vec<String> = values_at(source, field).into_iter().filter_map(text_of).flat_map(|text| analyze(&text)).collect();
                let matched: Vec<&String> = terms.iter().filter(|term| tokens.contains(term)).collect();
                if matched.is_empty() || (*all && matched.len() < terms.len()) {
                    continue;
                }
                let field_stats = stats.fields.get(field);
                let documents = field_stats.map_or(1, |stats| stats.documents.max(1));
                let average = field_stats.map_or(1.0, |stats| stats.total_length as f64 / stats.documents.max(1) as f64);
                let frequencies: Vec<(u64, u64)> = matched
                    .iter()
                    .map(|term| {
                        let frequency = tokens.iter().filter(|token| token == term).count() as u64;
                        let holding = field_stats.and_then(|stats| stats.holding.get(*term)).copied().unwrap_or(1);
                        (frequency, holding)
                    })
                    .collect();
                let field_score = eggdb_search::bm25(&frequencies, tokens.len() as u64, documents, average);
                best = Some(best.map_or(field_score, |held| held.max(field_score)));
            }
            best
        }
        Query::MatchPhrase { field, terms } => {
            let found = values_at(source, field).into_iter().filter_map(text_of).any(|text| {
                let tokens = analyze(&text);
                !terms.is_empty() && tokens.windows(terms.len()).any(|window| window == terms.as_slice())
            });
            found.then_some(1.0)
        }
        Query::Term { field, value } => values_at(source, field).iter().any(|node| same(&node.value, value)).then_some(1.0),
        Query::Terms { field, values } => values_at(source, field)
            .iter()
            .any(|node| values.iter().any(|wanted| same(&node.value, wanted)))
            .then_some(1.0),
        Query::Range { field, bounds } => values_at(source, field)
            .iter()
            .any(|node| {
                bounds.iter().all(|(operator, bound)| match compare(&node.value, bound) {
                    Some(ordering) => match operator.as_str() {
                        "gt" => ordering.is_gt(),
                        "gte" => ordering.is_ge(),
                        "lt" => ordering.is_lt(),
                        _ => ordering.is_le(),
                    },
                    None => false,
                })
            })
            .then_some(1.0),
        Query::Exists { field } => values_at(source, field).iter().any(|node| node.value != Json::Null).then_some(1.0),
        Query::Prefix { field, prefix } => values_at(source, field)
            .into_iter()
            .filter_map(text_of)
            .any(|text| text.starts_with(prefix.as_str()))
            .then_some(1.0),
        Query::Ids { values } => values.iter().any(|value| value == id).then_some(1.0),
        Query::Bool { must, filter, should, must_not, minimum_should_match } => {
            let mut total = 0.0;
            for inner in must {
                total += score(inner, id, source, stats)?;
            }
            for inner in filter {
                score(inner, id, source, stats)?;
            }
            if must_not.iter().any(|inner| score(inner, id, source, stats).is_some()) {
                return None;
            }
            let mut matched_should = 0;
            for inner in should {
                if let Some(value) = score(inner, id, source, stats) {
                    matched_should += 1;
                    total += value;
                }
            }
            if matched_should < *minimum_should_match {
                return None;
            }
            if must.is_empty() && should.is_empty() {
                total = 0.0;
            }
            Some(total)
        }
    }
}

/// A bucket key's text and the document count.
pub(crate) fn term_buckets(documents: &[&Node], field: &str, size: usize) -> Vec<(String, bool, u64)> {
    let mut counts: BTreeMap<(String, bool), u64> = BTreeMap::new();
    for source in documents {
        let mut seen = HashSet::new();
        for node in values_at(source, field) {
            let key = match &node.value {
                Json::String(text) => (text.clone(), false),
                Json::Number(text) => (text.clone(), true),
                Json::Boolean(value) => (value.to_string(), false),
                _ => continue,
            };
            if seen.insert(key.clone()) {
                *counts.entry(key).or_default() += 1;
            }
        }
    }
    let mut buckets: Vec<(String, bool, u64)> = counts.into_iter().map(|((key, number), count)| (key, number, count)).collect();
    buckets.sort_by(|left, right| right.2.cmp(&left.2).then_with(|| left.0.cmp(&right.0)));
    buckets.truncate(size);
    buckets
}

/// The numbers at `field` of every document.
pub(crate) fn numbers(documents: &[&Node], field: &str) -> Vec<f64> {
    documents
        .iter()
        .flat_map(|source| values_at(source, field))
        .filter_map(|node| match &node.value {
            Json::Number(text) => text.parse().ok(),
            _ => None,
        })
        .collect()
}
