//! PostgreSQL's `thesaurus` dictionary template: phrases of sample words, each replaced as a
//! whole by its substitute words, both normalized by a subdictionary.

use crate::analyzer::{Dictionary, Recognition};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    sample: Vec<Option<String>>,
    substitute: Vec<String>,
}

/// The rules of a thesaurus with the subdictionary that normalizes their words and the text
/// they are matched against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thesaurus {
    rules: Vec<Rule>,
    subdictionary: Box<Dictionary>,
}

fn rule_error(kind: &str, word: &str, problem: &str, rule: usize) -> String {
    format!("thesaurus {kind} word \"{word}\" {problem} (rule {rule})")
}

impl Thesaurus {
    /// Reads rules written as PostgreSQL's `.ths` files hold them, `sample words : substitute
    /// words` on each line (or separated by commas), with `#` starting a comment, `?` in a
    /// sample standing for any stop word, and `*` before a substitute word keeping it as
    /// written. Sample and substitute words are normalized by `subdictionary`.
    ///
    /// # Errors
    /// A line without `:`, an empty sample, a sample word that is a stop word or that the
    /// subdictionary does not know, and a substitute word it does not know, each naming its
    /// rule as PostgreSQL's messages do.
    pub fn parse(text: &str, subdictionary: Dictionary) -> Result<Self, String> {
        let mut rules = Vec::new();
        let lines = text
            .split(['\n', ','])
            .map(|line| line.split('#').next().unwrap_or_default().trim())
            .filter(|line| !line.is_empty());
        for (index, line) in lines.enumerate() {
            let number = index + 1;
            let (sample, substitute) = line
                .split_once(':')
                .ok_or_else(|| format!("unexpected delimiter in thesaurus rule {number}"))?;
            let sample = sample
                .split_whitespace()
                .map(|word| Self::sample_word(&subdictionary, word, number))
                .collect::<Result<Vec<_>, String>>()?;
            if sample.is_empty() {
                return Err(format!("thesaurus rule {number} has an empty sample"));
            }
            let substitute = substitute
                .split_whitespace()
                .filter_map(|word| Self::substitute_word(&subdictionary, word, number).transpose())
                .collect::<Result<Vec<_>, String>>()?;
            rules.push(Rule { sample, substitute });
        }
        Ok(Self {
            rules,
            subdictionary: Box::new(subdictionary),
        })
    }

    fn sample_word(subdictionary: &Dictionary, word: &str, rule: usize) -> Result<Option<String>, String> {
        if word == "?" {
            return Ok(None);
        }
        match subdictionary.recognize(word) {
            Recognition::Lexeme(lexeme) => Ok(Some(lexeme)),
            Recognition::Stop => Err(rule_error("sample", word, "is a stop word; use \"?\" to stand for one", rule)),
            Recognition::Unknown => Err(rule_error("sample", word, "isn't recognized by subdictionary", rule)),
        }
    }

    fn substitute_word(subdictionary: &Dictionary, word: &str, rule: usize) -> Result<Option<String>, String> {
        if let Some(kept) = word.strip_prefix('*') {
            return Ok((!kept.is_empty()).then(|| kept.to_lowercase()));
        }
        match subdictionary.recognize(word) {
            Recognition::Lexeme(lexeme) => Ok(Some(lexeme)),
            Recognition::Stop => Ok(None),
            Recognition::Unknown => Err(rule_error("substitute", word, "isn't recognized by subdictionary", rule)),
        }
    }

    /// The longest rule whose sample matches the words that start `words`: the number of
    /// words it covers and its substitute lexemes. An earlier rule wins a tie.
    pub fn longest_match(&self, words: &[&str]) -> Option<(usize, Vec<String>)> {
        let mut normalized: Vec<Recognition> = Vec::new();
        let mut best: Option<(usize, &Vec<String>)> = None;
        for rule in &self.rules {
            let length = rule.sample.len();
            if length > words.len() || best.is_some_and(|(covered, _)| covered >= length) {
                continue;
            }
            while normalized.len() < length {
                normalized.push(self.subdictionary.recognize(words[normalized.len()]));
            }
            let matches = rule.sample.iter().zip(&normalized).all(|(wanted, found)| match (wanted, found) {
                (Some(lexeme), Recognition::Lexeme(seen)) => lexeme == seen,
                (None, Recognition::Stop) => true,
                _ => false,
            });
            if matches {
                best = Some((length, &rule.substitute));
            }
        }
        best.map(|(covered, substitute)| (covered, substitute.clone()))
    }

    /// What a single word becomes: the first substitute lexeme of a one-word rule it matches.
    pub fn recognize(&self, word: &str) -> Recognition {
        match self.longest_match(&[word]) {
            Some((_, substitute)) => substitute
                .into_iter()
                .next()
                .map_or(Recognition::Stop, Recognition::Lexeme),
            None => Recognition::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::{Analyzer, StopWords, word_list};
    use crate::vector::Config;
    use crate::{TsQuery, TsVector};

    fn stemmer() -> Dictionary {
        Dictionary::Snowball {
            language: Config::English,
            stop_words: StopWords::Listed(word_list("the, a, of")),
        }
    }

    fn astronomy() -> Thesaurus {
        Thesaurus::parse(
            "supernovae stars : sn\ncrab nebulae : crab # a comment\none ? two : *Swsw\nsupernovae : sn star",
            stemmer(),
        )
        .unwrap()
    }

    #[test]
    fn the_longest_sample_wins_and_stop_words_match_question_marks() {
        let thesaurus = astronomy();
        assert_eq!(thesaurus.longest_match(&["supernovae", "stars", "x"]), Some((2, vec!["sn".to_owned()])));
        assert_eq!(
            thesaurus.longest_match(&["supernovae", "rocks"]),
            Some((1, vec!["sn".to_owned(), "star".to_owned()]))
        );
        assert_eq!(thesaurus.longest_match(&["one", "the", "two"]), Some((3, vec!["swsw".to_owned()])));
        assert_eq!(thesaurus.longest_match(&["one", "big", "two"]), None);
        assert_eq!(thesaurus.recognize("supernovae"), Recognition::Lexeme("sn".to_owned()));
        assert_eq!(thesaurus.recognize("stars"), Recognition::Unknown);
    }

    #[test]
    fn rules_with_stop_words_or_without_a_colon_are_refused() {
        assert!(Thesaurus::parse("the stars : sn", stemmer()).unwrap_err().contains("rule 1"));
        assert!(Thesaurus::parse("a b c", stemmer()).unwrap_err().contains("delimiter"));
        assert!(Thesaurus::parse(": sn", stemmer()).is_err());
    }

    #[test]
    fn an_analyzer_replaces_phrases_in_vectors_and_queries() {
        let mut analyzer = Analyzer::new(false);
        analyzer.map(
            "asciiword",
            vec![
                Dictionary::Thesaurus {
                    thesaurus: Box::new(astronomy()),
                },
                stemmer(),
            ],
        );
        let vector = TsVector::from_text(&analyzer, "Bright supernovae stars of crab nebulae");
        assert_eq!(vector.to_string(), "'bright':1 'crab':5 'sn':2");
        let query = TsQuery::plain(&analyzer, "supernovae stars");
        assert_eq!(query.to_string(), "'sn'");
    }
}
