use crate::mappings::FieldTypes;
use crate::query::{Query, Stats, parse, score};
use eggdb_sql::json::{Json, Node};
use std::collections::HashMap;

const MAX_K: usize = 10_000;
const MAX_DIMENSIONS: usize = 4096;

/// One `knn` section of a `_search` body: the field, the query vector, how many nearest
/// documents it keeps, the filter they must pass first, a minimum similarity and a boost.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct KnnSearch {
    field: String,
    vector: Vec<f64>,
    k: usize,
    filter: Vec<Query>,
    similarity_floor: Option<f64>,
    boost: f64,
}

fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Node> {
    match &node.value {
        Json::Object(members) => members
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value),
        _ => None,
    }
}

fn number(node: &Node) -> Option<f64> {
    match &node.value {
        Json::Number(text) => text.parse().ok(),
        _ => None,
    }
}

fn vector_of(node: &Node) -> Option<Vec<f64>> {
    match &node.value {
        Json::Array(items) if !items.is_empty() && items.len() <= MAX_DIMENSIONS => {
            items.iter().map(number).collect()
        }
        _ => None,
    }
}

/// The `knn` member of a search body, one object or an array of them; `default_k` is the
/// search's `size`.
pub(crate) fn parse_knn(node: &Node, default_k: usize) -> Result<Vec<KnnSearch>, String> {
    let sections: Vec<&Node> = match &node.value {
        Json::Array(items) => items.iter().collect(),
        Json::Object(_) => vec![node],
        _ => return Err("[knn] must be an object or an array".to_owned()),
    };
    sections
        .into_iter()
        .map(|section| {
            let field = match member(section, "field").map(|node| &node.value) {
                Some(Json::String(field)) => field.clone(),
                _ => return Err("[knn] needs a field".to_owned()),
            };
            let vector = member(section, "query_vector")
                .and_then(vector_of)
                .ok_or_else(|| "[knn] needs a query_vector of numbers".to_owned())?;
            let k = member(section, "k")
                .and_then(number)
                .map_or(default_k, |k| k as usize);
            if k == 0 || k > MAX_K {
                return Err(format!("[k] must be between 1 and {MAX_K}"));
            }
            let filter = match member(section, "filter") {
                None => Vec::new(),
                Some(node) => match &node.value {
                    Json::Array(items) => items
                        .iter()
                        .map(|item| parse(item, 1))
                        .collect::<Result<_, _>>()?,
                    _ => vec![parse(node, 1)?],
                },
            };
            Ok(KnnSearch {
                field,
                vector,
                k,
                filter,
                similarity_floor: member(section, "similarity").and_then(number),
                boost: member(section, "boost").and_then(number).unwrap_or(1.0),
            })
        })
        .collect()
}

fn stored_vector(source: &Node, path: &str) -> Option<Vec<f64>> {
    let mut current = source;
    for part in path.split('.') {
        current = member(current, part)?;
    }
    vector_of(current)
}

/// Elasticsearch's `_score` for a vector at `similarity`: cosine and dot product mapped into
/// 0..1, the L2 distance as `1 / (1 + d²)`, and `max_inner_product` kept positive.
fn similarity_score(similarity: &str, query: &[f64], stored: &[f64]) -> Option<f64> {
    if query.len() != stored.len() {
        return None;
    }
    let dot: f64 = query.iter().zip(stored).map(|(a, b)| a * b).sum();
    Some(match similarity {
        "dot_product" => (1.0 + dot) / 2.0,
        "l2_norm" => {
            let squared: f64 = query.iter().zip(stored).map(|(a, b)| (a - b).powi(2)).sum();
            1.0 / (1.0 + squared)
        }
        "max_inner_product" if dot < 0.0 => 1.0 / (1.0 - dot),
        "max_inner_product" => dot + 1.0,
        _ => {
            let norms = query.iter().map(|a| a * a).sum::<f64>().sqrt()
                * stored.iter().map(|b| b * b).sum::<f64>().sqrt();
            if norms == 0.0 {
                return None;
            }
            (1.0 + dot / norms) / 2.0
        }
    })
}

/// The documents each `knn` section keeps (its `k` nearest that pass its filter and floor),
/// by position in `documents`, with their scores summed over the sections.
pub(crate) fn nearest(
    searches: &[KnnSearch],
    documents: &[(String, Node)],
    types: &FieldTypes,
    stats: &Stats,
) -> HashMap<usize, f64> {
    let mut found: HashMap<usize, f64> = HashMap::new();
    for search in searches {
        let similarity = types.vector_similarity(&search.field);
        let mut scored: Vec<(f64, usize)> = documents
            .iter()
            .enumerate()
            .filter(|(_, (id, source))| {
                search
                    .filter
                    .iter()
                    .all(|filter| score(filter, id, source, stats).is_some())
            })
            .filter_map(|(at, (_, source))| {
                let stored = stored_vector(source, &search.field)?;
                let value = similarity_score(similarity, &search.vector, &stored)?;
                search
                    .similarity_floor
                    .is_none_or(|floor| value >= floor)
                    .then_some((value, at))
            })
            .collect();
        scored.sort_by(|left, right| right.0.total_cmp(&left.0).then(left.1.cmp(&right.1)));
        for (value, at) in scored.into_iter().take(search.k) {
            *found.entry(at).or_default() += value * search.boost;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similarities_follow_elasticsearch_scores() {
        let close = |left: Option<f64>, right: f64| {
            left.is_some_and(|left| (left - right).abs() < 1e-9)
        };
        assert!(close(similarity_score("cosine", &[1.0, 0.0], &[2.0, 0.0]), 1.0));
        assert!(close(similarity_score("cosine", &[1.0, 0.0], &[0.0, 3.0]), 0.5));
        assert!(close(similarity_score("dot_product", &[0.6, 0.8], &[0.6, 0.8]), 1.0));
        assert!(close(similarity_score("l2_norm", &[0.0, 0.0], &[3.0, 4.0]), 1.0 / 26.0));
        assert!(close(similarity_score("max_inner_product", &[1.0], &[-1.0]), 0.5));
        assert_eq!(similarity_score("cosine", &[1.0], &[1.0, 2.0]), None);
        assert_eq!(similarity_score("cosine", &[0.0, 0.0], &[1.0, 2.0]), None);
    }
}
