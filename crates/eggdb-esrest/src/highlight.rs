use crate::json_out::Out;
use crate::query::{Query, analyze, text_of, values_at};
use eggdb_sql::json::{Json, Node};

const DEFAULT_PRE_TAG: &str = "<em>";
const DEFAULT_POST_TAG: &str = "</em>";

#[derive(Debug, Default, Clone)]
struct FieldTerms {
    words: Vec<String>,
    prefixes: Vec<String>,
}

/// A search's `highlight`: the fields asked for, the tags around matched words, and the words
/// and prefixes the query looks for in each field.
#[derive(Debug, Clone)]
pub(crate) struct Highlight {
    patterns: Vec<String>,
    pre_tag: String,
    post_tag: String,
    terms: Vec<(String, FieldTerms)>,
}

fn add(terms: &mut Vec<(String, FieldTerms)>, field: &str, words: &[String], prefix: Option<&str>) {
    let at = match terms.iter().position(|(known, _)| known == field) {
        Some(at) => at,
        None => {
            terms.push((field.to_owned(), FieldTerms::default()));
            terms.len() - 1
        }
    };
    let held = &mut terms[at].1;
    for word in words {
        if !held.words.contains(word) {
            held.words.push(word.clone());
        }
    }
    if let Some(prefix) = prefix {
        held.prefixes.push(prefix.to_lowercase());
    }
}

fn collect(query: &Query, terms: &mut Vec<(String, FieldTerms)>) {
    match query {
        Query::Match { fields, terms: words, .. } => {
            for field in fields {
                add(terms, field, words, None);
            }
        }
        Query::MatchPhrase { field, terms: words } => add(terms, field, words, None),
        Query::Term { field, value: Json::String(text) } => add(terms, field, &analyze(text), None),
        Query::Prefix { field, prefix } => add(terms, field, &[], Some(prefix)),
        Query::Bool { must, filter, should, .. } => {
            for inner in must.iter().chain(filter).chain(should) {
                collect(inner, terms);
            }
        }
        _ => {}
    }
}

fn glob(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((head, tail)) => name.starts_with(head) && name[head.len()..].ends_with(tail) && name.len() >= head.len() + tail.len(),
    }
}

fn first_tag(node: Option<&Node>, default: &str) -> String {
    match node.map(|node| &node.value) {
        Some(Json::Array(items)) => items.first().and_then(text_of).unwrap_or_else(|| default.to_owned()),
        Some(Json::String(text)) => text.clone(),
        _ => default.to_owned(),
    }
}

fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Node> {
    match &node.value {
        Json::Object(members) => members.iter().find(|(key, _)| key == name).map(|(_, value)| value),
        _ => None,
    }
}

/// Reads a search body's `highlight` for `query` (with its wildcard fields already expanded).
pub(crate) fn highlight_of(node: &Node, query: &Query) -> Result<Highlight, String> {
    let patterns = match member(node, "fields").map(|fields| &fields.value) {
        Some(Json::Object(members)) => members.iter().map(|(name, _)| name.clone()).collect(),
        Some(Json::Array(items)) => items
            .iter()
            .filter_map(|item| match &item.value {
                Json::Object(members) => members.first().map(|(name, _)| name.clone()),
                Json::String(name) => Some(name.clone()),
                _ => None,
            })
            .collect(),
        _ => return Err("[highlight] needs [fields]".to_owned()),
    };
    let mut terms = Vec::new();
    collect(query, &mut terms);
    Ok(Highlight {
        patterns,
        pre_tag: first_tag(member(node, "pre_tags"), DEFAULT_PRE_TAG),
        post_tag: first_tag(member(node, "post_tags"), DEFAULT_POST_TAG),
        terms,
    })
}

impl Highlight {
    fn marked(&self, text: &str, terms: &FieldTerms) -> Option<String> {
        let mut out = String::with_capacity(text.len() + 16);
        let mut marked_any = false;
        let mut word_start: Option<usize> = None;
        let mut flush = |out: &mut String, start: usize, end: usize| {
            let word = &text[start..end];
            let folded = word.to_lowercase();
            if terms.words.contains(&folded) || terms.prefixes.iter().any(|prefix| folded.starts_with(prefix.as_str())) {
                marked_any = true;
                out.push_str(&self.pre_tag);
                out.push_str(word);
                out.push_str(&self.post_tag);
            } else {
                out.push_str(word);
            }
        };
        for (at, character) in text.char_indices() {
            if character.is_alphanumeric() {
                word_start.get_or_insert(at);
                continue;
            }
            if let Some(start) = word_start.take() {
                flush(&mut out, start, at);
            }
            out.push(character);
        }
        if let Some(start) = word_start {
            flush(&mut out, start, text.len());
        }
        marked_any.then_some(out)
    }

    /// The `highlight` object of one hit: each asked field that holds a matched word, with
    /// its values in which the matched words are wrapped in the tags; `None` when none does.
    pub(crate) fn of(&self, source: &Node) -> Option<String> {
        let mut out = Out::default();
        out.begin_object();
        let mut any = false;
        for (field, terms) in &self.terms {
            if !self.patterns.iter().any(|pattern| glob(pattern, field)) {
                continue;
            }
            let fragments: Vec<String> = values_at(source, field)
                .into_iter()
                .filter_map(text_of)
                .filter_map(|text| self.marked(&text, terms))
                .collect();
            if fragments.is_empty() {
                continue;
            }
            any = true;
            out.key(field).begin_array();
            for fragment in &fragments {
                out.string(fragment);
            }
            out.end_array();
        }
        out.end_object();
        any.then(|| out.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_sql::json::parse_document;

    #[test]
    fn matched_words_are_wrapped_in_the_tags() {
        let query = Query::Match {
            fields: vec!["title".to_owned()],
            terms: vec!["rust".to_owned()],
            all: false,
        };
        let spec = parse_document("{\"fields\":{\"title\":{}},\"pre_tags\":[\"<b>\"],\"post_tags\":[\"</b>\"]}").unwrap();
        let highlight = highlight_of(&spec, &query).unwrap();
        let source = parse_document("{\"title\":\"Rust, and more rust\",\"body\":\"rust\"}").unwrap();
        assert_eq!(highlight.of(&source).unwrap(), "{\"title\":[\"<b>Rust</b>, and more <b>rust</b>\"]}");
    }
}
