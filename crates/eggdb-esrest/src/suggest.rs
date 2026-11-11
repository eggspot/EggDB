use crate::json_out::Out;
use eggdb_sql::json::{Json, Node};

const DEFAULT_SIZE: usize = 5;
const MAX_SIZE: usize = 10_000;
const DEFAULT_WEIGHT: f64 = 1.0;

fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Node> {
    match &node.value {
        Json::Object(members) => members
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value),
        _ => None,
    }
}

fn text(node: &Node) -> Option<&str> {
    match &node.value {
        Json::String(text) => Some(text),
        _ => None,
    }
}

fn number(node: &Node) -> Option<f64> {
    match &node.value {
        Json::Number(text) | Json::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// One searched document as the suggester sees it: its index, id, parsed source and the
/// source's text as stored.
pub(crate) struct Candidate<'d> {
    pub(crate) index: &'d str,
    pub(crate) id: &'d str,
    pub(crate) source: &'d Node,
    pub(crate) source_text: &'d str,
}

/// The inputs a `completion` field holds, with their weights: a string, an array of strings,
/// an `{"input": ..., "weight": n}` object, or an array of those.
fn inputs(node: &Node, out: &mut Vec<(String, f64)>) {
    match &node.value {
        Json::String(text) => out.push((text.clone(), DEFAULT_WEIGHT)),
        Json::Array(items) => {
            for item in items {
                if !matches!(item.value, Json::Array(_)) {
                    inputs(item, out);
                }
            }
        }
        Json::Object(_) => {
            let weight = member(node, "weight").and_then(number).unwrap_or(DEFAULT_WEIGHT);
            match member(node, "input").map(|input| &input.value) {
                Some(Json::String(text)) => out.push((text.clone(), weight)),
                Some(Json::Array(items)) => out.extend(
                    items
                        .iter()
                        .filter_map(text)
                        .map(|text| (text.to_owned(), weight)),
                ),
                _ => {}
            }
        }
        _ => {}
    }
}

fn field_node<'n>(source: &'n Node, path: &str) -> Option<&'n Node> {
    let mut current = source;
    for part in path.split('.') {
        current = member(current, part)?;
    }
    Some(current)
}

/// Writes the `suggest` section of a search response for its `completion` suggestions: per
/// suggestion, the documents whose field has an input starting with the prefix (letter case
/// ignored, as the default `simple` analyzer does), best weight first, one option per
/// document. Other suggesters answer with no options.
pub(crate) fn write(out: &mut Out, suggest: &Node, candidates: &[Candidate<'_>]) {
    let global = member(suggest, "text").and_then(text).unwrap_or_default();
    out.begin_object();
    if let Json::Object(members) = &suggest.value {
        for (name, spec) in members {
            if name == "text" {
                continue;
            }
            let prefix = member(spec, "prefix")
                .or_else(|| member(spec, "text"))
                .and_then(text)
                .unwrap_or(global);
            out.key(name).begin_array().begin_object();
            out.key("text")
                .string(prefix)
                .key("offset")
                .integer(0)
                .key("length")
                .integer(prefix.chars().count() as u64)
                .key("options")
                .begin_array();
            if let Some(completion) = member(spec, "completion") {
                options(out, completion, prefix, candidates);
            }
            out.end_array().end_object().end_array();
        }
    }
    out.end_object();
}

fn options(out: &mut Out, completion: &Node, prefix: &str, candidates: &[Candidate<'_>]) {
    let Some(field) = member(completion, "field").and_then(text) else {
        return;
    };
    let size = member(completion, "size")
        .and_then(number)
        .map_or(DEFAULT_SIZE, |size| (size.max(0.0) as usize).min(MAX_SIZE));
    let skip_duplicates = member(completion, "skip_duplicates")
        .is_some_and(|node| node.value == Json::Boolean(true));
    let wanted = prefix.to_lowercase();
    let mut found: Vec<(f64, String, usize)> = Vec::new();
    for (at, candidate) in candidates.iter().enumerate() {
        let mut held = Vec::new();
        if let Some(node) = field_node(candidate.source, field) {
            inputs(node, &mut held);
        }
        let best = held
            .into_iter()
            .filter(|(input, _)| input.to_lowercase().starts_with(&wanted))
            .max_by(|left, right| left.1.total_cmp(&right.1).then(right.0.cmp(&left.0)));
        if let Some((input, weight)) = best {
            found.push((weight, input, at));
        }
    }
    found.sort_by(|left, right| {
        right
            .0
            .total_cmp(&left.0)
            .then_with(|| left.1.cmp(&right.1))
            .then(left.2.cmp(&right.2))
    });
    if skip_duplicates {
        let mut seen = std::collections::HashSet::new();
        found.retain(|(_, input, _)| seen.insert(input.clone()));
    }
    for (weight, input, at) in found.into_iter().take(size) {
        let candidate = &candidates[at];
        out.begin_object()
            .key("text")
            .string(&input)
            .key("_index")
            .string(candidate.index)
            .key("_id")
            .string(candidate.id)
            .key("_score")
            .number(weight)
            .key("_source")
            .raw(candidate.source_text)
            .end_object();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_sql::json::parse_document;

    #[test]
    fn completion_options_start_with_the_prefix_best_weight_first() {
        let sources = [
            r#"{"suggest": {"input": ["Nirvana", "Nevermind"], "weight": 34}}"#,
            r#"{"suggest": "Nine Inch Nails"}"#,
            r#"{"suggest": ["Metallica"]}"#,
        ];
        let parsed: Vec<Node> = sources
            .iter()
            .map(|text| parse_document(text).expect("source parses"))
            .collect();
        let ids = ["1", "2", "3"];
        let candidates: Vec<Candidate<'_>> = parsed
            .iter()
            .zip(sources)
            .zip(ids)
            .map(|((source, source_text), id)| Candidate {
                index: "music",
                id,
                source,
                source_text,
            })
            .collect();
        let suggest = parse_document(
            r#"{"song": {"prefix": "ni", "completion": {"field": "suggest"}}}"#,
        )
        .expect("suggest parses");
        let mut out = Out::default();
        write(&mut out, &suggest, &candidates);
        let text = out.finish();
        let nirvana = text.find("\"text\":\"Nirvana\"").expect("Nirvana is offered");
        let nails = text.find("\"text\":\"Nine Inch Nails\"").expect("NIN is offered");
        assert!(nirvana < nails, "{text}");
        assert!(!text.contains("Metallica\",\"_index"), "{text}");
        assert!(text.contains("\"length\":2"), "{text}");
    }
}
