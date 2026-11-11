use crate::query::TsQuery;
use crate::vector::Config;

/// The options of `ts_headline`, with PostgreSQL's defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlineOptions {
    /// Written before each matching word.
    pub start_sel: String,
    /// Written after each matching word.
    pub stop_sel: String,
    /// The most words in the excerpt (or in each fragment).
    pub max_words: usize,
    /// The fewest words in the excerpt.
    pub min_words: usize,
    /// Words this long or shorter are not left at the ends of the excerpt.
    pub short_word: usize,
    /// Whether the whole document is shown, highlighted.
    pub highlight_all: bool,
    /// How many fragments around matches to show; 0 shows one excerpt.
    pub max_fragments: usize,
    /// What separates fragments.
    pub fragment_delimiter: String,
}

impl Default for HeadlineOptions {
    fn default() -> Self {
        Self {
            start_sel: "<b>".to_owned(),
            stop_sel: "</b>".to_owned(),
            max_words: 35,
            min_words: 15,
            short_word: 3,
            highlight_all: false,
            max_fragments: 0,
            fragment_delimiter: " ... ".to_owned(),
        }
    }
}

fn unquoted(value: &str) -> String {
    let value = value.trim();
    value
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .map_or_else(|| value.to_owned(), |inner| inner.replace("\"\"", "\""))
}

fn split_options(text: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for character in text.chars() {
        match character {
            '"' => {
                quoted = !quoted;
                current.push(character);
            }
            ',' if !quoted => items.push(std::mem::take(&mut current)),
            other => current.push(other),
        }
    }
    items.push(current);
    items
}

impl HeadlineOptions {
    /// Options written as `ts_headline` takes them: `MaxWords=10, StartSel=<em>`, names in any
    /// case, values optionally double-quoted.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut options = Self::default();
        for item in split_options(text) {
            if item.trim().is_empty() {
                continue;
            }
            let (name, value) = item
                .split_once('=')
                .ok_or_else(|| format!("invalid ts_headline option \"{}\"", item.trim()))?;
            let value = unquoted(value);
            let number = || {
                value
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| format!("invalid value for option \"{}\"", name.trim()))
            };
            match name.trim().to_ascii_lowercase().as_str() {
                "startsel" => options.start_sel = value.clone(),
                "stopsel" => options.stop_sel = value.clone(),
                "fragmentdelimiter" => options.fragment_delimiter = value.clone(),
                "maxwords" => options.max_words = usize::try_from(number()?).unwrap_or(0),
                "minwords" => {
                    let minimum = number()?;
                    if minimum <= 0 {
                        return Err("MinWords should be positive".to_owned());
                    }
                    options.min_words = usize::try_from(minimum).unwrap_or(0);
                }
                "shortword" => {
                    let short = number()?;
                    if short < 0 {
                        return Err("ShortWord should be >= 0".to_owned());
                    }
                    options.short_word = usize::try_from(short).unwrap_or(0);
                }
                "maxfragments" => {
                    let fragments = number()?;
                    if fragments < 0 {
                        return Err("MaxFragments should be >= 0".to_owned());
                    }
                    options.max_fragments = usize::try_from(fragments).unwrap_or(0);
                }
                "highlightall" => {
                    options.highlight_all = matches!(
                        value.trim().to_ascii_lowercase().as_str(),
                        "1" | "on" | "true" | "t" | "y" | "yes"
                    );
                }
                other => return Err(format!("unrecognized headline parameter: \"{other}\"")),
            }
        }
        if !options.highlight_all && options.min_words >= options.max_words {
            return Err("MinWords should be less than MaxWords".to_owned());
        }
        Ok(options)
    }
}

struct Token<'t> {
    text: &'t str,
    word: Option<usize>,
}

fn tokens(document: &str) -> (Vec<Token<'_>>, Vec<usize>) {
    let mut tokens = Vec::new();
    let mut word_tokens = Vec::new();
    let mut start = 0;
    let mut in_word = None;
    for (at, character) in document.char_indices() {
        let wordish = character.is_alphanumeric();
        match in_word {
            Some(true) if !wordish => {
                word_tokens.push(tokens.len());
                tokens.push(Token {
                    text: &document[start..at],
                    word: Some(word_tokens.len() - 1),
                });
                start = at;
            }
            Some(false) if wordish => {
                tokens.push(Token {
                    text: &document[start..at],
                    word: None,
                });
                start = at;
            }
            _ => {}
        }
        in_word = Some(wordish);
    }
    if start < document.len() {
        if in_word == Some(true) {
            word_tokens.push(tokens.len());
            tokens.push(Token {
                text: &document[start..],
                word: Some(word_tokens.len() - 1),
            });
        } else {
            tokens.push(Token {
                text: &document[start..],
                word: None,
            });
        }
    }
    (tokens, word_tokens)
}

fn matched(config: Config, word: &str, terms: &[(&str, bool)]) -> bool {
    let Some(lexeme) = config.lexeme(word) else {
        return false;
    };
    terms.iter().any(|(term, prefix)| {
        if *prefix {
            lexeme.starts_with(term)
        } else {
            lexeme == *term
        }
    })
}

fn render(
    tokens: &[Token<'_>],
    from_token: usize,
    to_token: usize,
    hits: &[bool],
    options: &HeadlineOptions,
    out: &mut String,
) {
    for token in tokens.get(from_token..to_token).unwrap_or_default() {
        match token.word {
            Some(word) if hits.get(word).copied().unwrap_or(false) => {
                out.push_str(&options.start_sel);
                out.push_str(token.text);
                out.push_str(&options.stop_sel);
            }
            _ => out.push_str(token.text),
        }
    }
}

fn trimmed_window(
    words: &[&str],
    hits: &[bool],
    mut start: usize,
    mut end: usize,
    options: &HeadlineOptions,
) -> (usize, usize) {
    let short = |at: usize| {
        !hits[at] && words[at].chars().count() <= options.short_word
    };
    while end > start + options.min_words.max(1) && short(end - 1) {
        end -= 1;
    }
    while start + options.min_words.max(1) < end && short(start) {
        start += 1;
    }
    (start, end)
}

/// The excerpt of `document` that `ts_headline` shows for `query` under `config`, with
/// matching words between `start_sel` and `stop_sel`. The excerpt starts at the first match
/// and holds up to `max_words` words, moved back to fill the window near the end and trimmed of
/// short words at both ends; without a match it is the first `min_words` words.
pub fn headline(config: Config, document: &str, query: &TsQuery, options: &HeadlineOptions) -> String {
    let (tokens, word_tokens) = tokens(document);
    let words: Vec<&str> = word_tokens.iter().map(|&at| tokens[at].text).collect();
    let terms = query.terms();
    let hits: Vec<bool> = words.iter().map(|word| matched(config, word, &terms)).collect();
    let mut out = String::new();
    if options.highlight_all || words.is_empty() {
        render(&tokens, 0, tokens.len(), &hits, options, &mut out);
        return out;
    }
    let token_range = |start: usize, end: usize| {
        (word_tokens[start], word_tokens[end - 1] + 1)
    };
    let max_words = options.max_words.max(1);
    let matches: Vec<usize> = (0..words.len()).filter(|&at| hits[at]).collect();
    if options.max_fragments > 0 && !matches.is_empty() {
        let mut fragments: Vec<(usize, usize)> = Vec::new();
        for &at in &matches {
            if fragments.len() >= options.max_fragments {
                break;
            }
            if fragments.iter().any(|&(start, end)| (start..end).contains(&at)) {
                continue;
            }
            let start = at.saturating_sub(max_words / 2);
            let end = (start + max_words).min(words.len());
            fragments.push(trimmed_window(&words, &hits, start, end, options));
        }
        for (index, (start, end)) in fragments.into_iter().enumerate() {
            if index > 0 {
                out.push_str(&options.fragment_delimiter);
            }
            let (from, to) = token_range(start, end);
            render(&tokens, from, to, &hits, options, &mut out);
        }
        return out;
    }
    let (start, end) = match matches.first() {
        Some(&first) => {
            let end = (first + max_words).min(words.len());
            let start = end.saturating_sub(max_words).min(first);
            trimmed_window(&words, &hits, start, end, options)
        }
        None => (0, options.min_words.max(1).min(words.len())),
    };
    let (from, to) = token_range(start, end);
    render(&tokens, from, to, &hits, options, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_are_marked_and_options_are_read() {
        let query = TsQuery::parse(Config::English, "rat").unwrap();
        let options = HeadlineOptions::default();
        assert_eq!(
            headline(Config::English, "The fat rats ate", &query, &options),
            "The fat <b>rats</b> ate"
        );
        let options = HeadlineOptions::parse("StartSel=[, StopSel=], HighlightAll=true").unwrap();
        assert_eq!(
            headline(Config::English, "a rat, a cat.", &query, &options),
            "a [rat], a cat."
        );
        assert!(HeadlineOptions::parse("MinWords=40").is_err());
        assert!(HeadlineOptions::parse("Colour=red").is_err());
    }
}
