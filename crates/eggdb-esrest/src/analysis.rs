use eggdb_sql::json::{Json, Node};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

const MOST_SYNONYM_RULES: usize = 10_000;
const MOST_STOP_WORDS: usize = 10_000;
const ENGLISH_STOP_WORDS: [&str; 33] = [
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into", "is", "it", "no", "not", "of",
    "on", "or", "such", "that", "the", "their", "then", "there", "these", "they", "this", "to", "was", "will", "with",
];

thread_local! {
    static FIELD_ANALYZERS: RefCell<HashMap<String, Analyzer>> = RefCell::new(HashMap::new());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tokenizer {
    Standard,
    Whitespace,
    Keyword,
}

#[derive(Debug, Clone, PartialEq)]
enum Filter {
    Lowercase,
    AsciiFolding,
    Stop(HashSet<String>),
    Synonyms(HashMap<String, String>),
    Stemmer(eggdb_search::Config),
}

/// A text analyzer from an index's `settings.analysis`: a tokenizer and its token filters in
/// order, or one of Elasticsearch's built-in analyzers.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Analyzer {
    tokenizer: Tokenizer,
    filters: Vec<Filter>,
}

fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Node> {
    match &node.value {
        Json::Object(members) => members.iter().find(|(key, _)| key == name).map(|(_, value)| value),
        _ => None,
    }
}

fn text(node: &Node) -> Option<&str> {
    match &node.value {
        Json::String(value) => Some(value),
        _ => None,
    }
}

fn strings(node: Option<&Node>) -> Vec<String> {
    match node.map(|node| &node.value) {
        Some(Json::Array(items)) => items.iter().filter_map(text).map(str::to_owned).collect(),
        Some(Json::String(value)) => vec![value.clone()],
        _ => Vec::new(),
    }
}

fn folded(letter: char) -> char {
    match letter {
        'à'..='å' => 'a',
        'ç' => 'c',
        'è'..='ë' => 'e',
        'ì'..='ï' => 'i',
        'ñ' => 'n',
        'ò'..='ö' | 'ø' => 'o',
        'ù'..='ü' => 'u',
        'ý' | 'ÿ' => 'y',
        'đ' => 'd',
        'ă' | 'ạ' | 'ả' | 'ấ' | 'ầ' | 'ẩ' | 'ẫ' | 'ậ' | 'ắ' | 'ằ' | 'ẳ' | 'ẵ' | 'ặ' => 'a',
        'ẹ' | 'ẻ' | 'ẽ' | 'ế' | 'ề' | 'ể' | 'ễ' | 'ệ' => 'e',
        'ỉ' | 'ị' | 'ĩ' => 'i',
        'ơ' | 'ọ' | 'ỏ' | 'ố' | 'ồ' | 'ổ' | 'ỗ' | 'ộ' | 'ớ' | 'ờ' | 'ở' | 'ỡ' | 'ợ' => 'o',
        'ư' | 'ụ' | 'ủ' | 'ứ' | 'ừ' | 'ử' | 'ữ' | 'ự' | 'ũ' => 'u',
        'ỳ' | 'ỵ' | 'ỷ' | 'ỹ' => 'y',
        other => other,
    }
}

fn stop_words(node: Option<&Node>) -> HashSet<String> {
    let mut words = HashSet::new();
    for word in strings(node).into_iter().take(MOST_STOP_WORDS) {
        if word == "_english_" {
            words.extend(ENGLISH_STOP_WORDS.iter().map(|word| (*word).to_owned()));
        } else if word != "_none_" {
            words.insert(word.to_lowercase());
        }
    }
    words
}

fn synonyms(node: Option<&Node>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for rule in strings(node).into_iter().take(MOST_SYNONYM_RULES) {
        let (sources, target) = match rule.split_once("=>") {
            Some((sources, targets)) => (sources.to_owned(), targets.split(',').next().unwrap_or_default().trim().to_lowercase()),
            None => (rule.clone(), rule.split(',').next().unwrap_or_default().trim().to_lowercase()),
        };
        if target.is_empty() || target.contains(' ') {
            continue;
        }
        for source in sources.split(',').map(|word| word.trim().to_lowercase()) {
            if !source.is_empty() && !source.contains(' ') {
                map.insert(source, target.clone());
            }
        }
    }
    map
}

fn stemmer_config(name: &str) -> Option<eggdb_search::Config> {
    let language = name.trim_start_matches("light_").trim_start_matches("minimal_").trim_start_matches("possessive_");
    match language {
        "porter" | "porter2" => eggdb_search::Config::named("english"),
        other => eggdb_search::Config::named(other),
    }
}

fn custom_filter(name: &str, definitions: Option<&Node>) -> Option<Filter> {
    if let Some(definition) = definitions.and_then(|definitions| member(definitions, name)) {
        return match member(definition, "type").and_then(text).unwrap_or_default() {
            "lowercase" => Some(Filter::Lowercase),
            "asciifolding" => Some(Filter::AsciiFolding),
            "stop" => Some(Filter::Stop(stop_words(member(definition, "stopwords")))),
            "synonym" | "synonym_graph" => Some(Filter::Synonyms(synonyms(member(definition, "synonyms")))),
            "stemmer" | "snowball" => member(definition, "language")
                .or_else(|| member(definition, "name"))
                .and_then(text)
                .and_then(|language| stemmer_config(&language.to_ascii_lowercase()))
                .map(Filter::Stemmer),
            _ => None,
        };
    }
    match name {
        "lowercase" => Some(Filter::Lowercase),
        "asciifolding" => Some(Filter::AsciiFolding),
        "stop" => Some(Filter::Stop(ENGLISH_STOP_WORDS.iter().map(|word| (*word).to_owned()).collect())),
        "porter_stem" | "stemmer" | "kstem" => eggdb_search::Config::named("english").map(Filter::Stemmer),
        _ => None,
    }
}

impl Analyzer {
    fn standard() -> Self {
        Self { tokenizer: Tokenizer::Standard, filters: vec![Filter::Lowercase] }
    }

    fn built_in(name: &str) -> Option<Self> {
        Some(match name {
            "standard" | "simple" => Self::standard(),
            "whitespace" => Self { tokenizer: Tokenizer::Whitespace, filters: Vec::new() },
            "keyword" => Self { tokenizer: Tokenizer::Keyword, filters: Vec::new() },
            "stop" => Self {
                tokenizer: Tokenizer::Standard,
                filters: vec![Filter::Lowercase, Filter::Stop(ENGLISH_STOP_WORDS.iter().map(|word| (*word).to_owned()).collect())],
            },
            language => {
                let config = eggdb_search::Config::named(language)?;
                Self { tokenizer: Tokenizer::Standard, filters: vec![Filter::Lowercase, Filter::Stemmer(config)] }
            }
        })
    }

    fn filtered(&self, tokens: Vec<String>) -> Vec<String> {
        let mut tokens = tokens;
        for filter in &self.filters {
            tokens = match filter {
                Filter::Lowercase => tokens.into_iter().map(|token| token.to_lowercase()).collect(),
                Filter::AsciiFolding => tokens.into_iter().map(|token| token.chars().map(folded).collect()).collect(),
                Filter::Stop(words) => tokens.into_iter().filter(|token| !words.contains(token)).collect(),
                Filter::Synonyms(map) => tokens.into_iter().map(|token| map.get(&token).cloned().unwrap_or(token)).collect(),
                Filter::Stemmer(config) => tokens.into_iter().map(|token| config.lexeme(&token).unwrap_or(token)).collect(),
            };
        }
        tokens
    }

    /// The tokens of `text`.
    pub(crate) fn tokens(&self, text: &str) -> Vec<String> {
        let raw: Vec<String> = match self.tokenizer {
            Tokenizer::Standard => eggdb_search::words(text).into_iter().map(str::to_owned).collect(),
            Tokenizer::Whitespace => text.split_whitespace().map(str::to_owned).collect(),
            Tokenizer::Keyword => vec![text.to_owned()],
        };
        self.filtered(raw)
    }

    /// Query terms that the standard analyzer already split and lowered, run through this
    /// analyzer's filters.
    pub(crate) fn refine(&self, terms: Vec<String>) -> Vec<String> {
        match self.tokenizer {
            Tokenizer::Keyword => vec![terms.join(" ")],
            _ => self.filtered(terms),
        }
    }
}

/// The analyzers an index's `settings.analysis` defines, by name, beside the built-in ones.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Analysis {
    analyzers: HashMap<String, Analyzer>,
}

impl Analysis {
    /// Reads `settings.analysis` (or `settings.index.analysis`).
    pub(crate) fn add_settings(&mut self, settings: &Node) {
        let Some(analysis) = member(settings, "analysis").or_else(|| member(settings, "index").and_then(|index| member(index, "analysis"))) else {
            return;
        };
        let filters = member(analysis, "filter");
        let Some(Json::Object(defined)) = member(analysis, "analyzer").map(|node| &node.value) else {
            return;
        };
        for (name, definition) in defined {
            let kind = member(definition, "type").and_then(text).unwrap_or("custom");
            let analyzer = if kind == "custom" {
                let tokenizer = match member(definition, "tokenizer").and_then(text).unwrap_or("standard") {
                    "whitespace" => Tokenizer::Whitespace,
                    "keyword" => Tokenizer::Keyword,
                    _ => Tokenizer::Standard,
                };
                let filters = strings(member(definition, "filter")).iter().filter_map(|filter| custom_filter(filter, filters)).collect();
                Analyzer { tokenizer, filters }
            } else if kind == "stop" {
                Analyzer {
                    tokenizer: Tokenizer::Standard,
                    filters: vec![Filter::Lowercase, Filter::Stop(stop_words(member(definition, "stopwords")))],
                }
            } else {
                let Some(mut analyzer) = Analyzer::built_in(kind) else {
                    continue;
                };
                if let Some(words) = member(definition, "stopwords") {
                    analyzer.filters.insert(1.min(analyzer.filters.len()), Filter::Stop(stop_words(Some(words))));
                }
                analyzer
            };
            self.analyzers.insert(name.clone(), analyzer);
        }
    }

    /// The analyzer named `name`: one the settings define, else a built-in one.
    pub(crate) fn analyzer(&self, name: &str) -> Option<Analyzer> {
        self.analyzers.get(name).cloned().or_else(|| Analyzer::built_in(name))
    }
}

/// While held, documents' text in the fields it names is analyzed by their analyzers on this
/// thread; dropping it restores what was there before.
pub(crate) struct FieldAnalyzers {
    previous: HashMap<String, Analyzer>,
}

impl Drop for FieldAnalyzers {
    fn drop(&mut self) {
        let previous = std::mem::take(&mut self.previous);
        FIELD_ANALYZERS.with(|held| *held.borrow_mut() = previous);
    }
}

/// Uses `analyzers` (field name to analyzer) for documents' text in matching and scoring on
/// this thread until the returned guard drops.
pub(crate) fn enter(analyzers: HashMap<String, Analyzer>) -> FieldAnalyzers {
    let previous = FIELD_ANALYZERS.with(|held| std::mem::replace(&mut *held.borrow_mut(), analyzers));
    FieldAnalyzers { previous }
}

/// The tokens of a document's `text` in `field`: its analyzer's, or the standard analyzer's.
pub(crate) fn analyze_field(field: &str, text: &str) -> Vec<String> {
    FIELD_ANALYZERS
        .with(|held| held.borrow().get(field).map(|analyzer| analyzer.tokens(text)))
        .unwrap_or_else(|| crate::query::analyze(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_sql::json::parse_document;

    #[test]
    fn custom_analyzers_apply_stop_words_synonyms_and_stemming() {
        let settings = parse_document(
            r#"{"analysis":{"filter":{"my_syn":{"type":"synonym","synonyms":["quick, fast","car => automobile"]}},
            "analyzer":{"mine":{"type":"custom","tokenizer":"standard","filter":["lowercase","stop","my_syn","porter_stem"]}}}}"#,
        )
        .unwrap();
        let mut analysis = Analysis::default();
        analysis.add_settings(&settings);
        let analyzer = analysis.analyzer("mine").unwrap();
        assert_eq!(analyzer.tokens("The Fast cars"), vec!["quick".to_owned(), "car".to_owned()]);
        assert_eq!(analysis.analyzer("keyword").unwrap().tokens("New York"), vec!["New York".to_owned()]);
    }
}
