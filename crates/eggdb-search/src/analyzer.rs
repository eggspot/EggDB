use crate::segment::segmented_words;
use crate::vector::{Config, Lexer, words};
use std::collections::{BTreeMap, BTreeSet};

/// The token types of PostgreSQL's default text search parser, with its descriptions, as
/// `ts_token_type('default')` lists them.
pub const TOKEN_TYPES: [(&str, &str); 23] = [
    ("asciiword", "Word, all ASCII"),
    ("word", "Word, all letters"),
    ("numword", "Word, letters and digits"),
    ("asciihword", "Hyphenated word, all ASCII"),
    ("hword", "Hyphenated word, all letters"),
    ("numhword", "Hyphenated word, letters and digits"),
    ("hword_asciipart", "Hyphenated word part, all ASCII"),
    ("hword_part", "Hyphenated word part, all letters"),
    ("hword_numpart", "Hyphenated word part, letters and digits"),
    ("email", "Email address"),
    ("protocol", "Protocol head"),
    ("url", "URL"),
    ("host", "Host"),
    ("url_path", "URL path"),
    ("file", "File or path name"),
    ("sfloat", "Scientific notation"),
    ("float", "Decimal notation"),
    ("int", "Signed integer"),
    ("uint", "Unsigned integer"),
    ("version", "Version number"),
    ("tag", "XML tag"),
    ("entity", "XML entity"),
    ("blank", "Space symbols"),
];

const WORD_TYPES: [&str; 9] = [
    "asciiword",
    "word",
    "asciihword",
    "hword",
    "hword_asciipart",
    "hword_part",
    "numword",
    "numhword",
    "hword_numpart",
];

/// Whether `name` is one of the default parser's token types.
pub fn is_token_type(name: &str) -> bool {
    TOKEN_TYPES.iter().any(|(token_type, _)| *token_type == name)
}

/// The token type the default parser gives `word`, one of EggDB's words: `asciiword`,
/// `word`, `numword` or `uint`.
pub fn token_type_of(word: &str) -> &'static str {
    let digits = word.chars().filter(char::is_ascii_digit).count();
    let letters = word.chars().count() - digits;
    match (letters, digits) {
        (0, _) => "uint",
        (_, 0) if word.is_ascii() => "asciiword",
        (_, 0) => "word",
        _ => "numword",
    }
}

/// The stop words a dictionary drops.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum StopWords {
    /// None.
    #[default]
    None,
    /// The list of a built-in configuration, as PostgreSQL's `<language>.stop` file holds it.
    Of(Config),
    /// The words listed (lower case).
    Listed(BTreeSet<String>),
}

impl StopWords {
    fn contains(&self, lower: &str) -> bool {
        match self {
            Self::None => false,
            Self::Of(config) => config.is_stop_word(lower),
            Self::Listed(words) => words.contains(lower),
        }
    }
}

/// What a dictionary makes of a word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recognition {
    /// The word becomes this lexeme.
    Lexeme(String),
    /// The word is a stop word and is dropped.
    Stop,
    /// The dictionary does not know the word; the next one in the mapping is asked.
    Unknown,
}

/// A text search dictionary, after one of PostgreSQL's templates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dictionary {
    /// The `simple` template: the word in lower case unless it is a stop word; with `accept`
    /// off, words that are not stop words are left to the next dictionary.
    Simple {
        /// The stop words dropped.
        stop_words: StopWords,
        /// Whether a word that is not a stop word is taken as it is.
        accept: bool,
    },
    /// The `snowball` template: the stem of every word that is not a stop word.
    Snowball {
        /// The language stemmed.
        language: Config,
        /// The stop words dropped.
        stop_words: StopWords,
    },
    /// The `synonym` template: a listed word becomes its synonym; other words are left to the
    /// next dictionary.
    Synonym {
        /// Each word (lower case unless `case_sensitive`) with its replacement.
        synonyms: BTreeMap<String, String>,
        /// Whether words are matched as written.
        case_sensitive: bool,
    },
}

impl Dictionary {
    /// What this dictionary makes of `word`.
    pub fn recognize(&self, word: &str) -> Recognition {
        let lower = word.to_lowercase();
        match self {
            Self::Simple { stop_words, accept } => {
                if stop_words.contains(&lower) {
                    Recognition::Stop
                } else if *accept {
                    Recognition::Lexeme(lower)
                } else {
                    Recognition::Unknown
                }
            }
            Self::Snowball { language, stop_words } => {
                if stop_words.contains(&lower) {
                    Recognition::Stop
                } else {
                    Recognition::Lexeme(language.stem(&lower))
                }
            }
            Self::Synonym { synonyms, case_sensitive } => {
                let key = if *case_sensitive { word } else { lower.as_str() };
                match synonyms.get(key) {
                    Some(synonym) if *case_sensitive => Recognition::Lexeme(synonym.clone()),
                    Some(synonym) => Recognition::Lexeme(synonym.to_lowercase()),
                    None => Recognition::Unknown,
                }
            }
        }
    }
}

/// The words of a stop word list written as PostgreSQL's `.stop` files are, one per line, or
/// separated by spaces or commas; lower cased.
pub fn word_list(text: &str) -> BTreeSet<String> {
    text.split(|character: char| character.is_whitespace() || character == ',')
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The pairs of a synonym list written as PostgreSQL's `.syn` files are, a word and its
/// synonym on each line (or separated by commas); a line of one word is skipped.
pub fn synonym_list(text: &str, case_sensitive: bool) -> BTreeMap<String, String> {
    text.split(['\n', ','])
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let word = parts.next()?;
            let synonym = parts.next()?.trim_end_matches('*');
            let word = if case_sensitive { word.to_owned() } else { word.to_lowercase() };
            Some((word, synonym.to_owned()))
        })
        .collect()
}

/// A text search configuration built from dictionaries, as `CREATE TEXT SEARCH
/// CONFIGURATION` and `ALTER ... MAPPING` make one: each token type with the dictionaries
/// asked in turn. A word whose token type maps to no dictionary, or that no dictionary knows,
/// is dropped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Analyzer {
    mappings: BTreeMap<String, Vec<Dictionary>>,
    segments: bool,
}

impl Analyzer {
    /// An analyzer with no mappings; `segments` cuts text without spaces into pieces as the
    /// `cjk` and `thai` configurations do.
    pub fn new(segments: bool) -> Self {
        Self {
            mappings: BTreeMap::new(),
            segments,
        }
    }

    /// Maps `token_type` to `dictionaries`, replacing its earlier mapping.
    pub fn map(&mut self, token_type: &str, dictionaries: Vec<Dictionary>) {
        self.mappings.insert(token_type.to_owned(), dictionaries);
    }

    /// The token types a built-in configuration maps to its own stemmer (the word types),
    /// the others mapping to the `simple` dictionary, as PostgreSQL's built-in configurations
    /// do.
    pub fn word_types() -> &'static [&'static str] {
        &WORD_TYPES
    }
}

impl Lexer for &Analyzer {
    fn words<'t>(self, text: &'t str) -> Vec<&'t str> {
        if self.segments {
            segmented_words(text)
        } else {
            words(text)
        }
    }

    fn lexeme(self, word: &str) -> Option<String> {
        let dictionaries = self.mappings.get(token_type_of(word))?;
        for dictionary in dictionaries {
            match dictionary.recognize(word) {
                Recognition::Lexeme(lexeme) => return Some(lexeme),
                Recognition::Stop => return None,
                Recognition::Unknown => {}
            }
        }
        None
    }

    fn segments(self) -> bool {
        self.segments
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TsVector;

    #[test]
    fn mappings_ask_dictionaries_in_turn() {
        let synonyms = Dictionary::Synonym {
            synonyms: synonym_list("postgres pgsql\npostgresql pgsql", false),
            case_sensitive: false,
        };
        let stem = Dictionary::Snowball {
            language: Config::English,
            stop_words: StopWords::Listed(word_list("the, a")),
        };
        let mut analyzer = Analyzer::new(false);
        analyzer.map("asciiword", vec![synonyms, stem]);
        let vector = TsVector::from_text(&analyzer, "The Postgres databases 42");
        assert_eq!(vector.to_string(), "'databas':3 'pgsql':2");
        assert_eq!(token_type_of("42"), "uint");
        assert_eq!(token_type_of("über"), "word");
    }
}
