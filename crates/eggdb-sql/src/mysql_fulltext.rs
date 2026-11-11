use crate::error::{SqlError, SqlResult, SqlState};
use crate::parser::mysql_match::BOOLEAN_MODE;
use crate::types::Value;

const MIN_TOKEN_CHARACTERS: usize = 3;
const STOPWORDS: [&str; 36] = [
    "a", "about", "an", "are", "as", "at", "be", "by", "com", "de", "en", "for", "from", "how", "i",
    "in", "is", "it", "la", "of", "on", "or", "that", "the", "this", "to", "was", "what", "when",
    "where", "who", "will", "with", "und", "the", "www",
];
const STRONGER: f64 = 1.5;
const WEAKER: f64 = 0.5;
const NEGATED: f64 = -0.5;
const MATCHED_FLOOR: f64 = 1e-6;

fn words(text: &str) -> Vec<String> {
    text.split(|character: char| !(character.is_alphanumeric() || character == '_' || character == '\''))
        .map(|word| word.trim_matches('\'').to_lowercase())
        .filter(|word| !word.is_empty())
        .collect()
}

fn indexed(word: &str) -> bool {
    word.chars().count() >= MIN_TOKEN_CHARACTERS && !STOPWORDS.contains(&word)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Operator {
    Optional,
    Required,
    Excluded,
    Weighted(f64),
}

#[derive(Debug, Clone, PartialEq)]
struct Term {
    operator: Operator,
    words: Vec<String>,
    prefix: bool,
}

fn boolean_terms(query: &str) -> Vec<Term> {
    let mut terms = Vec::new();
    let mut characters = query.chars().peekable();
    while let Some(&character) = characters.peek() {
        if character.is_whitespace() || matches!(character, '(' | ')') {
            characters.next();
            continue;
        }
        let mut operator = Operator::Optional;
        while let Some(&sign) = characters.peek() {
            operator = match sign {
                '+' => Operator::Required,
                '-' => Operator::Excluded,
                '>' => Operator::Weighted(STRONGER),
                '<' => Operator::Weighted(WEAKER),
                '~' => Operator::Weighted(NEGATED),
                _ => break,
            };
            characters.next();
        }
        if characters.peek() == Some(&'"') {
            characters.next();
            let phrase: String = characters.by_ref().take_while(|character| *character != '"').collect();
            terms.push(Term {
                operator,
                words: words(&phrase),
                prefix: false,
            });
            continue;
        }
        let mut written = String::new();
        while let Some(&next) = characters.peek() {
            if next.is_whitespace() || matches!(next, '(' | ')' | '"') {
                break;
            }
            written.push(next);
            characters.next();
        }
        let prefix = written.ends_with('*');
        let found = words(written.trim_end_matches('*'));
        if !found.is_empty() {
            terms.push(Term {
                operator,
                words: found,
                prefix,
            });
        }
    }
    terms
        .into_iter()
        .filter(|term| term.prefix || term.words.len() > 1 || term.words.iter().all(|word| indexed(word)))
        .collect()
}

fn occurrences(document: &[String], term: &Term) -> usize {
    let matches_word = |found: &String, wanted: &String, last: bool| {
        if last && term.prefix {
            found.starts_with(wanted.as_str())
        } else {
            found == wanted
        }
    };
    if term.words.is_empty() || document.len() < term.words.len() {
        return 0;
    }
    document
        .windows(term.words.len())
        .filter(|window| {
            window
                .iter()
                .zip(&term.words)
                .enumerate()
                .all(|(index, (found, wanted))| matches_word(found, wanted, index + 1 == term.words.len()))
        })
        .count()
}

fn natural_score(document: &[String], query: &str) -> f64 {
    let mut seen: Vec<String> = Vec::new();
    let mut score = 0.0;
    for word in words(query).into_iter().filter(|word| indexed(word)) {
        if seen.contains(&word) {
            continue;
        }
        let count = document.iter().filter(|found| **found == word).count();
        if count > 0 {
            score += 1.0 + (count as f64).ln();
        }
        seen.push(word);
    }
    score
}

fn boolean_score(document: &[String], query: &str) -> f64 {
    let terms = boolean_terms(query);
    let mut score = 0.0;
    let mut matched_any = false;
    for term in &terms {
        let count = occurrences(document, term);
        match term.operator {
            Operator::Excluded if count > 0 => return 0.0,
            Operator::Required if count == 0 => return 0.0,
            Operator::Excluded => {}
            Operator::Required | Operator::Optional if count > 0 => {
                matched_any = true;
                score += 1.0 + (count as f64).ln();
            }
            Operator::Weighted(weight) if count > 0 => {
                matched_any = true;
                score += weight * (1.0 + (count as f64).ln());
            }
            _ => {}
        }
    }
    if matched_any { score.max(MATCHED_FLOOR) } else { 0.0 }
}

/// MySQL's `MATCH (columns) AGAINST (query [mode])`: the relevance of the columns' text to the
/// query, 0 when it does not match. Words are InnoDB's tokens of at least three characters, not
/// in its default stopword list; natural language mode adds `1 + ln(count)` for each query word
/// the text holds, and boolean mode reads `+`, `-`, `>`, `<`, `~`, `word*` and `"phrases"`.
pub(crate) fn match_against(values: &[Value]) -> SqlResult<Value> {
    let [mode, query, columns @ ..] = values else {
        return Err(SqlError::new(SqlState::SYNTAX_ERROR, "MATCH needs columns and AGAINST needs a search string"));
    };
    let Some(query) = query.to_text() else {
        return Ok(Value::Double(0.0));
    };
    let mut document = Vec::new();
    for column in columns {
        if let Some(text) = column.to_text() {
            document.extend(words(&text));
        }
    }
    let boolean = mode.to_text().as_deref() == Some(BOOLEAN_MODE);
    Ok(Value::Double(if boolean {
        boolean_score(&document, &query)
    } else {
        natural_score(&document, &query)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(mode: &str, query: &str, text: &str) -> f64 {
        match match_against(&[Value::Text(mode.into()), Value::Text(query.into()), Value::Text(text.into())]).unwrap() {
            Value::Double(score) => score,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn natural_and_boolean_modes_match_like_innodb() {
        assert!(score("natural", "database tutorial", "MySQL Tutorial for beginners") > 0.0);
        assert_eq!(score("natural", "the of", "the book of the year"), 0.0);
        assert!(score("boolean", "+mysql -oracle", "MySQL vs PostgreSQL") > 0.0);
        assert_eq!(score("boolean", "+mysql -oracle", "MySQL vs Oracle"), 0.0);
        assert!(score("boolean", "data*", "Databases explained") > 0.0);
        assert!(score("boolean", "\"full text\"", "a full text search") > 0.0);
        assert_eq!(score("boolean", "\"text full\"", "a full text search"), 0.0);
    }
}
