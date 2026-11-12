use crate::scandinavian::Language;
use crate::segment::segmented_words;
use crate::{dutch, english, finnish, french, german, hungarian, indonesian, italian, portuguese, romanian, russian, spanish, turkish};
use std::collections::BTreeMap;
use std::fmt;

const MAX_POSITION: u32 = 16_383;
const WEIGHT_SHIFT: u32 = 14;

/// A position's weight, from `setweight`: `A` highest, `D` the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Weight {
    /// The default weight, not written in the text form.
    D,
    /// Weight `C`.
    C,
    /// Weight `B`.
    B,
    /// Weight `A`.
    A,
}

impl Weight {
    /// The weight a letter (`A` to `D`, any case) names.
    pub fn named(letter: char) -> Option<Self> {
        match letter.to_ascii_uppercase() {
            'A' => Some(Self::A),
            'B' => Some(Self::B),
            'C' => Some(Self::C),
            'D' => Some(Self::D),
            _ => None,
        }
    }

    fn code(self) -> u32 {
        match self {
            Self::D => 0,
            Self::C => 1,
            Self::B => 2,
            Self::A => 3,
        }
    }

    fn of_code(code: u32) -> Self {
        match code & 3 {
            3 => Self::A,
            2 => Self::B,
            1 => Self::C,
            _ => Self::D,
        }
    }

    /// The index of this weight in a `ts_rank` weights array (`{D, C, B, A}`).
    pub fn rank_index(self) -> usize {
        self.code() as usize
    }

    fn letter(self) -> Option<char> {
        match self {
            Self::A => Some('A'),
            Self::B => Some('B'),
            Self::C => Some('C'),
            Self::D => None,
        }
    }
}

fn position_of(stored: u32) -> u32 {
    stored & MAX_POSITION
}

fn weight_of(stored: u32) -> Weight {
    Weight::of_code(stored >> WEIGHT_SHIFT)
}

fn stored(position: u32, weight: Weight) -> u32 {
    position.min(MAX_POSITION) | (weight.code() << WEIGHT_SHIFT)
}

/// How text becomes lexemes, named as PostgreSQL's text search configurations are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Config {
    /// Lower case words, English stop words dropped, English stems.
    English,
    /// Lower case words, nothing dropped or stemmed.
    Simple,
    /// Lower case words, German stop words dropped, German stems.
    German,
    /// Lower case words, Spanish stop words dropped, Spanish stems.
    Spanish,
    /// Lower case words, Swedish stop words dropped, Swedish stems.
    Swedish,
    /// Lower case words, Norwegian stop words dropped, Norwegian stems.
    Norwegian,
    /// Lower case words, Danish stop words dropped, Danish stems.
    Danish,
    /// Lower case words, Italian stop words dropped, Italian stems.
    Italian,
    /// Lower case words, Russian stop words dropped, Russian stems.
    Russian,
    /// Lower case words, Dutch stop words dropped, Dutch stems.
    Dutch,
    /// Lower case words, Portuguese stop words dropped, Portuguese stems.
    Portuguese,
    /// Lower case words, French stop words dropped, French stems.
    French,
    /// Lower case words, Finnish stop words dropped, Finnish stems.
    Finnish,
    /// Lower case words, Hungarian stop words dropped, Hungarian stems.
    Hungarian,
    /// Lower case words and Indonesian stems; Snowball has no Indonesian stop word list.
    Indonesian,
    /// Lower case words and Romanian stems; PostgreSQL ships no Romanian stop word list.
    Romanian,
    /// Lower case words, Turkish stop words dropped, Turkish stems.
    Turkish,
    /// Lower case words, with runs of Chinese, Japanese and Korean characters cut into
    /// overlapping two-character pieces (Elasticsearch's `cjk` analyzer).
    Cjk,
    /// Lower case words, with runs of Thai letters cut into overlapping two-letter pieces.
    Thai,
}

impl Config {
    /// The configuration named `name` (`english`, `german`, `spanish`, `swedish`, `norwegian`,
    /// `danish`, `italian`, `russian`, `dutch`, `portuguese`, `french`, `finnish`, `hungarian`, `indonesian`, `romanian`, `turkish`, `cjk`, `thai`, `simple`, optionally
    /// `pg_catalog.`-qualified).
    pub fn named(name: &str) -> Option<Self> {
        match name
            .trim()
            .trim_start_matches("pg_catalog.")
            .to_ascii_lowercase()
            .as_str()
        {
            "english" => Some(Self::English),
            "simple" => Some(Self::Simple),
            "german" => Some(Self::German),
            "spanish" => Some(Self::Spanish),
            "swedish" => Some(Self::Swedish),
            "norwegian" => Some(Self::Norwegian),
            "danish" => Some(Self::Danish),
            "italian" => Some(Self::Italian),
            "russian" => Some(Self::Russian),
            "dutch" => Some(Self::Dutch),
            "portuguese" => Some(Self::Portuguese),
            "french" => Some(Self::French),
            "finnish" => Some(Self::Finnish),
            "hungarian" => Some(Self::Hungarian),
            "indonesian" => Some(Self::Indonesian),
            "romanian" => Some(Self::Romanian),
            "turkish" => Some(Self::Turkish),
            "cjk" => Some(Self::Cjk),
            "thai" => Some(Self::Thai),
            _ => None,
        }
    }

    /// Whether this configuration cuts text written without spaces into pieces.
    pub fn segments(self) -> bool {
        matches!(self, Self::Cjk | Self::Thai)
    }

    /// The words of `text` this configuration makes lexemes of: [`segmented_words`] for `cjk`
    /// and `thai`, [`words`] for the others.
    pub fn words(self, text: &str) -> Vec<&str> {
        if self.segments() {
            segmented_words(text)
        } else {
            words(text)
        }
    }

    /// The lexeme `word` becomes, or `None` for a stop word.
    pub fn lexeme(self, word: &str) -> Option<String> {
        let lower = word.to_lowercase();
        if self.is_stop_word(&lower) {
            None
        } else {
            Some(self.stem(&lower))
        }
    }

    /// Whether `word` (lower case) is on this configuration's stop word list, as PostgreSQL's
    /// `<language>.stop` file lists them; `simple`, `indonesian`, `romanian`, `cjk` and `thai`
    /// have none.
    pub fn is_stop_word(self, word: &str) -> bool {
        match self {
            Self::Simple | Self::Cjk | Self::Thai | Self::Indonesian | Self::Romanian => false,
            Self::English => english::is_stop_word(word),
            Self::German => german::is_stop_word(word),
            Self::Spanish => spanish::is_stop_word(word),
            Self::Italian => italian::is_stop_word(word),
            Self::Russian => russian::is_stop_word(word),
            Self::Dutch => dutch::is_stop_word(word),
            Self::Portuguese => portuguese::is_stop_word(word),
            Self::French => french::is_stop_word(word),
            Self::Finnish => finnish::is_stop_word(word),
            Self::Hungarian => hungarian::is_stop_word(word),
            Self::Turkish => turkish::is_stop_word(word),
            Self::Swedish => Language::Swedish.is_stop_word(word),
            Self::Norwegian => Language::Norwegian.is_stop_word(word),
            Self::Danish => Language::Danish.is_stop_word(word),
        }
    }

    /// The stem of `word` (lower case) by this configuration's Snowball stemmer, the word itself
    /// for `simple`, `cjk` and `thai`.
    pub fn stem(self, word: &str) -> String {
        match self {
            Self::Simple | Self::Cjk | Self::Thai => word.to_owned(),
            Self::English => english::stem(word),
            Self::German => german::stem(word),
            Self::Spanish => spanish::stem(word),
            Self::Italian => italian::stem(word),
            Self::Russian => russian::stem(word),
            Self::Dutch => dutch::stem(word),
            Self::Portuguese => portuguese::stem(word),
            Self::French => french::stem(word),
            Self::Finnish => finnish::stem(word),
            Self::Hungarian => hungarian::stem(word),
            Self::Indonesian => indonesian::stem(word),
            Self::Romanian => romanian::stem(word),
            Self::Turkish => turkish::stem(word),
            Self::Swedish => Language::Swedish.stem(word),
            Self::Norwegian => Language::Norwegian.stem(word),
            Self::Danish => Language::Danish.stem(word),
        }
    }

    /// The configuration's name, as `Config::named` reads it.
    pub fn name(self) -> &'static str {
        match self {
            Self::English => "english",
            Self::Simple => "simple",
            Self::German => "german",
            Self::Spanish => "spanish",
            Self::Swedish => "swedish",
            Self::Norwegian => "norwegian",
            Self::Danish => "danish",
            Self::Italian => "italian",
            Self::Russian => "russian",
            Self::Dutch => "dutch",
            Self::Portuguese => "portuguese",
            Self::French => "french",
            Self::Finnish => "finnish",
            Self::Hungarian => "hungarian",
            Self::Indonesian => "indonesian",
            Self::Romanian => "romanian",
            Self::Turkish => "turkish",
            Self::Cjk => "cjk",
            Self::Thai => "thai",
        }
    }
}

/// What turns text into lexemes: a built-in [`Config`] or a configuration built from
/// dictionaries ([`crate::Analyzer`]).
pub trait Lexer: Copy {
    /// The words of `text`, in order.
    fn words<'t>(self, text: &'t str) -> Vec<&'t str>;
    /// The lexeme `word` becomes, or `None` when it is dropped.
    fn lexeme(self, word: &str) -> Option<String>;
    /// Whether text written without spaces is cut into pieces.
    fn segments(self) -> bool;
}

impl Lexer for Config {
    fn words<'t>(self, text: &'t str) -> Vec<&'t str> {
        Config::words(self, text)
    }

    fn lexeme(self, word: &str) -> Option<String> {
        Config::lexeme(self, word)
    }

    fn segments(self) -> bool {
        Config::segments(self)
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
    pub fn from_text(config: impl Lexer, text: &str) -> Self {
        let mut lexemes: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        for (index, word) in config.words(text).into_iter().enumerate() {
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
                    let weight = match &part[digits.len()..] {
                        "" => Weight::D,
                        letter if letter.chars().count() == 1 => {
                            Weight::named(letter.chars().next()?)?
                        }
                        _ => return None,
                    };
                    let position: u32 = digits.parse().ok()?;
                    if position == 0 {
                        return None;
                    }
                    positions.push(stored(position, weight));
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
        let mut all: Vec<u32> = self
            .weighted_positions(lexeme, prefix)
            .into_iter()
            .map(|(position, _)| position)
            .collect();
        all.dedup();
        all
    }

    /// The positions of `lexeme` (or of every lexeme starting with it when `prefix`) with
    /// their weights, in position order.
    pub fn weighted_positions(&self, lexeme: &str, prefix: bool) -> Vec<(u32, Weight)> {
        let stored_positions: Vec<u32> = if prefix {
            self.lexemes
                .range(lexeme.to_owned()..)
                .take_while(|(key, _)| key.starts_with(lexeme))
                .flat_map(|(_, positions)| positions.iter().copied())
                .collect()
        } else {
            self.lexemes.get(lexeme).cloned().unwrap_or_default()
        };
        let mut all: Vec<(u32, Weight)> = stored_positions
            .into_iter()
            .map(|stored| (position_of(stored), weight_of(stored)))
            .collect();
        all.sort_unstable();
        all
    }

    /// The same vector with every position's weight set to `weight`, or only those of the
    /// lexemes in `only` when it is given, as `setweight` does.
    pub fn with_weight(&self, weight: Weight, only: Option<&[String]>) -> Self {
        let mut lexemes = self.lexemes.clone();
        for (lexeme, positions) in &mut lexemes {
            if only.is_some_and(|listed| !listed.iter().any(|named| named == lexeme)) {
                continue;
            }
            for position in positions.iter_mut() {
                *position = stored(position_of(*position), weight);
            }
        }
        Self { lexemes }
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
                let listed: Vec<String> = positions
                    .iter()
                    .map(|stored| {
                        let mut text = position_of(*stored).to_string();
                        if let Some(letter) = weight_of(*stored).letter() {
                            text.push(letter);
                        }
                        text
                    })
                    .collect();
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
        assert_eq!(
            TsVector::from_text(Config::Simple, "The Cat").to_string(),
            "'cat':2 'the':1"
        );
        assert_eq!(
            TsVector::parse("'it''s' 'x':1A"),
            TsVector::parse("'it''s' 'x':1")
        );
        assert!(TsVector::parse("'open").is_none());
        assert!(vector.contains("ca", true) && !vector.contains("ca", false));
    }
}
