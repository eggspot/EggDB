use crate::english;
use std::collections::BTreeMap;
use std::fmt;

const MAX_POSITION: u32 = 16_383;

/// How text becomes lexemes, named as PostgreSQL's text search configurations are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Config {
    /// Lower case words, English stop words dropped, English stems.
    English,
    /// Lower case words, nothing dropped or stemmed.
    Simple,
}

impl Config {
    /// The configuration named `name` (`english`, `simple`, optionally `pg_catalog.`-qualified).
    pub fn named(name: &str) -> Option<Self> {
        match name.trim().trim_start_matches("pg_catalog.").to_ascii_lowercase().as_str() {
            "english" => Some(Self::English),
            "simple" => Some(Self::Simple),
            _ => None,
        }
    }

    /// The lexeme `word` becomes, or `None` for a stop word.
    pub fn lexeme(self, word: &str) -> Option<String> {
        let lower = word.to_lowercase();
        match self {
            Self::Simple => Some(lower),
            Self::English if english::is_stop_word(&lower) => None,
            Self::English => Some(english::stem(&lower)),
        }
    }
}

/// The words of `text`: runs of letters and digits.
pub fn words(text: &str) -> Vec<&str> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect()
}

/// A text-search vector: each lexeme with the positions (from 1) of the words it came from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TsVector {
    lexemes: BTreeMap<String, Vec<u32>>,
}

impl TsVector {
    /// The vector of `text` under `config`, as `to_tsvector` makes it.
    pub fn from_text(config: Config, text: &str) -> Self {
        let mut lexemes: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        for (index, word) in words(text).into_iter().enumerate() {
            let position = (index as u32 + 1).min(MAX_POSITION);
            if let Some(lexeme) = config.lexeme(word) {
                let positions = lexemes.entry(lexeme).or_default();
                if positions.last() != Some(&position) {
                    positions.push(position);
                }
            }
        }
        Self { lexemes }
    }

    /// The vector written in PostgreSQL's text form (`'cat':3 'fat':2,5`), or `None` when the
    /// text is not one.
    pub fn parse(text: &str) -> Option<Self> {
        let mut lexemes = BTreeMap::new();
        let mut rest = text.trim();
        while !rest.is_empty() {
            let (lexeme, after) = quoted(rest)?;
            let mut positions = Vec::new();
            let mut after = after;
            if let Some(list) = after.strip_prefix(':') {
                let end = list.find(' ').unwrap_or(list.len());
                for part in list[..end].split(',') {
                    let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
                    positions.push(digits.parse().ok()?);
                }
                after = &list[end..];
            }
            lexemes.insert(lexeme, positions);
            rest = after.trim_start();
        }
        Some(Self { lexemes })
    }

    /// The lexemes and their positions.
    pub fn lexemes(&self) -> &BTreeMap<String, Vec<u32>> {
        &self.lexemes
    }

    /// The positions of `lexeme`, or of every lexeme starting with it when `prefix`.
    pub fn positions(&self, lexeme: &str, prefix: bool) -> Vec<u32> {
        if !prefix {
            return self.lexemes.get(lexeme).cloned().unwrap_or_default();
        }
        let mut all: Vec<u32> = self
            .lexemes
            .range(lexeme.to_owned()..)
            .take_while(|(key, _)| key.starts_with(lexeme))
            .flat_map(|(_, positions)| positions.iter().copied())
            .collect();
        all.sort_unstable();
        all
    }

    /// Whether `lexeme` is in the vector.
    pub fn contains(&self, lexeme: &str, prefix: bool) -> bool {
        if prefix {
            self.lexemes
                .range(lexeme.to_owned()..)
                .next()
                .is_some_and(|(key, _)| key.starts_with(lexeme))
        } else {
            self.lexemes.contains_key(lexeme)
        }
    }

    /// How many words the text had, as its largest position.
    pub fn length(&self) -> usize {
        self.lexemes.values().map(Vec::len).sum()
    }
}

pub(crate) fn quoted(text: &str) -> Option<(String, &str)> {
    let rest = text.strip_prefix('\'')?;
    let mut out = String::new();
    let mut characters = rest.char_indices();
    while let Some((at, character)) = characters.next() {
        match character {
            '\'' if rest[at + 1..].starts_with('\'') => {
                out.push('\'');
                characters.next();
            }
            '\'' => return Some((out, &rest[at + 1..])),
            '\\' => {
                if let Some((_, escaped)) = characters.next() {
                    out.push(escaped);
                }
            }
            other => out.push(other),
        }
    }
    None
}

pub(crate) fn quote(lexeme: &str) -> String {
    format!("'{}'", lexeme.replace('\\', "\\\\").replace('\'', "''"))
}

impl fmt::Display for TsVector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for (lexeme, positions) in &self.lexemes {
            if !first {
                f.write_str(" ")?;
            }
            first = false;
            f.write_str(&quote(lexeme))?;
            if !positions.is_empty() {
                let listed: Vec<String> = positions.iter().map(u32::to_string).collect();
                write!(f, ":{}", listed.join(","))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vectors_keep_positions_drop_stop_words_and_read_back() {
        let vector = TsVector::from_text(Config::English, "The fat cats ate fat rats");
        assert_eq!(vector.to_string(), "'ate':4 'cat':3 'fat':2,5 'rat':6");
        assert_eq!(TsVector::parse(&vector.to_string()), Some(vector.clone()));
        assert_eq!(TsVector::from_text(Config::Simple, "The Cat").to_string(), "'cat':2 'the':1");
        assert_eq!(TsVector::parse("'it''s' 'x':1A"), TsVector::parse("'it''s' 'x':1"));
        assert!(TsVector::parse("'open").is_none());
        assert!(vector.contains("ca", true) && !vector.contains("ca", false));
    }
}
