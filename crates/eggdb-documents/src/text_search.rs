use crate::error::{DocumentError, DocumentResult};
use eggdb_bson::{Bson, Document};

const MAX_DEPTH: usize = 100;

/// A `$text` search: its words (stemmed and lower-cased unless it is case-sensitive or its
/// language is `none`), its quoted phrases, the words it excludes with `-`, and the fields its
/// collection's text index covers (all string fields when not known).
#[derive(Debug, Clone, PartialEq)]
pub struct TextSearch {
    pub(crate) words: Vec<String>,
    pub(crate) phrases: Vec<String>,
    pub(crate) negated: Vec<String>,
    pub(crate) case_sensitive: bool,
    pub(crate) stemmed: bool,
    pub(crate) fields: Option<Vec<String>>,
}

fn normalized(word: &str, case_sensitive: bool, stemmed: bool) -> Option<String> {
    let folded = if case_sensitive { word.to_owned() } else { word.to_lowercase() };
    if stemmed && eggdb_search::is_stop_word(&folded.to_lowercase()) {
        return None;
    }
    Some(if stemmed { eggdb_search::stem(&folded) } else { folded })
}

impl TextSearch {
    /// Reads a `$text` operator's document: `$search`, `$language`, `$caseSensitive` and
    /// `$diacriticSensitive`.
    pub(crate) fn parse(value: &Bson) -> DocumentResult<Self> {
        let Bson::Document(options) = value else {
            return Err(DocumentError::bad_value("$text needs an object"));
        };
        let Some(Bson::String(search)) = options.get("$search") else {
            return Err(DocumentError::bad_value("$text needs a $search string"));
        };
        let case_sensitive = options.get_bool("$caseSensitive") == Some(true);
        let stemmed = !matches!(options.get_str("$language"), Some("none"));
        for (name, _) in options.iter() {
            if !matches!(name.as_str(), "$search" | "$language" | "$caseSensitive" | "$diacriticSensitive") {
                return Err(DocumentError::bad_value(format!("unknown $text option {name}")));
            }
        }
        let mut search_words = Vec::new();
        let mut phrases = Vec::new();
        let mut negated = Vec::new();
        let mut rest = search.as_str();
        while let Some(open) = rest.find('"') {
            let (before, after) = rest.split_at(open);
            search_words.push(before.to_owned());
            let Some(close) = after[1..].find('"') else {
                search_words.push(after[1..].to_owned());
                rest = "";
                break;
            };
            let phrase = &after[1..1 + close];
            if before.ends_with('-') {
                negated.extend(eggdb_search::words(phrase).into_iter().filter_map(|word| normalized(word, case_sensitive, stemmed)));
            } else if !phrase.trim().is_empty() {
                phrases.push(if case_sensitive { phrase.to_owned() } else { phrase.to_lowercase() });
            }
            rest = &after[close + 2..];
        }
        search_words.push(rest.to_owned());
        let mut words = Vec::new();
        for chunk in &search_words {
            for token in chunk.split_whitespace() {
                let (excluded, token) = match token.strip_prefix('-') {
                    Some(rest) => (true, rest),
                    None => (false, token),
                };
                for word in eggdb_search::words(token) {
                    if let Some(word) = normalized(word, case_sensitive, stemmed) {
                        if excluded {
                            negated.push(word);
                        } else {
                            words.push(word);
                        }
                    }
                }
            }
        }
        if words.is_empty() && phrases.is_empty() {
            return Err(DocumentError::bad_value("$text needs at least one word or phrase to search for"));
        }
        Ok(Self {
            words,
            phrases,
            negated,
            case_sensitive,
            stemmed,
            fields: None,
        })
    }

    /// The fields a text index's key document covers: those whose value is `"text"`, or every
    /// string field for `$**`.
    pub(crate) fn index_fields(key: &Document) -> Option<Vec<String>> {
        let fields: Vec<String> = key
            .iter()
            .filter(|(_, kind)| matches!(kind, Bson::String(kind) if kind == "text"))
            .map(|(name, _)| name.clone())
            .collect();
        (!fields.is_empty()).then_some(fields)
    }

    fn texts(&self, document: &Document) -> Vec<String> {
        let mut out = Vec::new();
        match &self.fields {
            Some(fields) if !fields.iter().any(|field| field == "$**") => {
                for field in fields {
                    for reached in crate::path::reached(document, field) {
                        if let Some(value) = reached.get() {
                            collect(value, &mut out, 0);
                        }
                    }
                }
            }
            _ => collect(&Bson::Document(document.clone()), &mut out, 0),
        }
        out
    }

    /// Whether `document` matches: every phrase appears, no excluded word does, and (without
    /// phrases) at least one word does.
    pub(crate) fn matches(&self, document: &Document) -> bool {
        let texts = self.texts(document);
        let folded: Vec<String> = texts
            .iter()
            .map(|text| if self.case_sensitive { text.clone() } else { text.to_lowercase() })
            .collect();
        let words: Vec<String> = texts
            .iter()
            .flat_map(|text| eggdb_search::words(text).into_iter().filter_map(|word| normalized(word, self.case_sensitive, self.stemmed)).collect::<Vec<_>>())
            .collect();
        if self.negated.iter().any(|word| words.contains(word)) {
            return false;
        }
        if !self.phrases.iter().all(|phrase| folded.iter().any(|text| text.contains(phrase.as_str()))) {
            return false;
        }
        !self.phrases.is_empty() || self.words.iter().any(|word| words.contains(word))
    }
}

fn collect(value: &Bson, out: &mut Vec<String>, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    match value {
        Bson::String(text) => out.push(text.clone()),
        Bson::Document(document) => {
            for (_, inner) in document.iter() {
                collect(inner, out, depth + 1);
            }
        }
        Bson::Array(items) => {
            for inner in items {
                collect(inner, out, depth + 1);
            }
        }
        _ => {}
    }
}
