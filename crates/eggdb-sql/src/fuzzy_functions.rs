use crate::arrays::{ArrayElement, ArrayValue};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{SqlType, Value};
use std::collections::BTreeSet;

const MAX_LEVENSHTEIN_CHARACTERS: usize = 255;
const MAX_METAPHONE_LENGTH: i64 = 255;
const MAX_TRIGRAM_CHARACTERS: usize = 1 << 16;

const FUNCTIONS: [&str; 10] = [
    "levenshtein",
    "levenshtein_less_equal",
    "soundex",
    "difference",
    "metaphone",
    "similarity",
    "word_similarity",
    "strict_word_similarity",
    "show_trgm",
    "similarity_dist",
];

pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    match name {
        "levenshtein" | "levenshtein_less_equal" | "difference" => Some(SqlType::Integer),
        "soundex" | "metaphone" => Some(SqlType::Text),
        "similarity" | "word_similarity" | "strict_word_similarity" | "similarity_dist" => {
            Some(SqlType::Real)
        }
        "show_trgm" => Some(SqlType::Array(ArrayElement::Text)),
        _ => None,
    }
}

fn wrong_arguments(name: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("function {name} does not take these arguments"),
    )
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Text(text) | Value::Bpchar(text) => Some(text.clone()),
        other => other.to_text(),
    }
}

fn integer(value: &Value) -> Option<i64> {
    text(value).and_then(|text| text.trim().parse().ok())
}

pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !FUNCTIONS.contains(&name) {
        return None;
    }
    if values.iter().any(Value::is_null) {
        return Some(Ok(Value::Null));
    }
    Some(call(name, values))
}

fn call(name: &str, values: &[Value]) -> SqlResult<Value> {
    let texts: Vec<String> = values.iter().filter_map(text).collect();
    match (name, texts.as_slice()) {
        ("levenshtein", [left, right]) => {
            levenshtein_value(left, right, (1, 1, 1), None)
        }
        ("levenshtein", [left, right, insert, delete, substitute]) => {
            let costs = costs(&values[2..5], name)?;
            let _ = (insert, delete, substitute);
            levenshtein_value(left, right, costs, None)
        }
        ("levenshtein_less_equal", [left, right, most]) => {
            let most = integer(&values[2]).ok_or_else(|| wrong_arguments(name))?;
            levenshtein_value(left, right, (1, 1, 1), Some(most))
        }
        ("levenshtein_less_equal", [left, right, _, _, _, _]) => {
            let costs = costs(&values[2..5], name)?;
            let most = integer(&values[5]).ok_or_else(|| wrong_arguments(name))?;
            levenshtein_value(left, right, costs, Some(most))
        }
        ("soundex", [word]) => Ok(Value::Text(soundex(word))),
        ("difference", [left, right]) => {
            let (left, right) = (soundex(left), soundex(right));
            let same = left
                .chars()
                .zip(right.chars())
                .filter(|(a, b)| a == b)
                .count();
            Ok(Value::Integer(same as i32))
        }
        ("metaphone", [word, _]) => {
            let length = integer(&values[1]).ok_or_else(|| wrong_arguments(name))?;
            if length <= 0 {
                return Err(SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "output cannot be empty string",
                ));
            }
            if length > MAX_METAPHONE_LENGTH {
                return Err(SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("output exceeds the maximum length of {MAX_METAPHONE_LENGTH} characters"),
                ));
            }
            Ok(Value::Text(metaphone(word, length as usize)))
        }
        ("similarity", [left, right]) => {
            Ok(Value::Real(similarity(&trigram_set(left)?, &trigram_set(right)?) as f32))
        }
        ("similarity_dist", [left, right]) => Ok(Value::Real(
            (1.0 - similarity(&trigram_set(left)?, &trigram_set(right)?)) as f32,
        )),
        ("word_similarity", [left, right]) => Ok(Value::Real(word_similarity(left, right, false)? as f32)),
        ("strict_word_similarity", [left, right]) => {
            Ok(Value::Real(word_similarity(left, right, true)? as f32))
        }
        ("show_trgm", [written]) => {
            let items: Vec<Value> = trigram_set(written)?.into_iter().map(Value::Text).collect();
            Ok(Value::Array(Box::new(ArrayValue::new(ArrayElement::Text, items))))
        }
        _ => Err(wrong_arguments(name)),
    }
}

fn costs(values: &[Value], name: &str) -> SqlResult<(i64, i64, i64)> {
    let read = |value: &Value| integer(value).ok_or_else(|| wrong_arguments(name));
    Ok((read(&values[0])?, read(&values[1])?, read(&values[2])?))
}

fn levenshtein_value(
    left: &str,
    right: &str,
    (insert, delete, substitute): (i64, i64, i64),
    most: Option<i64>,
) -> SqlResult<Value> {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    if left.len() > MAX_LEVENSHTEIN_CHARACTERS || right.len() > MAX_LEVENSHTEIN_CHARACTERS {
        return Err(SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!(
                "levenshtein argument exceeds maximum length of {MAX_LEVENSHTEIN_CHARACTERS} characters"
            ),
        ));
    }
    let mut previous: Vec<i64> = (0..=right.len() as i64).map(|at| at * insert).collect();
    let mut current = vec![0i64; right.len() + 1];
    for (row, left_character) in left.iter().enumerate() {
        current[0] = (row as i64 + 1) * delete;
        for (column, right_character) in right.iter().enumerate() {
            let replaced = previous[column]
                + if left_character == right_character {
                    0
                } else {
                    substitute
                };
            current[column + 1] = replaced
                .min(previous[column + 1] + delete)
                .min(current[column] + insert);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    let distance = previous[right.len()];
    let distance = match most {
        Some(most) if distance > most => most.saturating_add(1),
        _ => distance,
    };
    Ok(Value::Integer(
        i32::try_from(distance).unwrap_or(if distance < 0 { i32::MIN } else { i32::MAX }),
    ))
}

fn soundex_code(letter: char) -> Option<char> {
    match letter {
        'B' | 'F' | 'P' | 'V' => Some('1'),
        'C' | 'G' | 'J' | 'K' | 'Q' | 'S' | 'X' | 'Z' => Some('2'),
        'D' | 'T' => Some('3'),
        'L' => Some('4'),
        'M' | 'N' => Some('5'),
        'R' => Some('6'),
        _ => None,
    }
}

pub(crate) fn soundex(word: &str) -> String {
    let letters: Vec<char> = word
        .chars()
        .filter(char::is_ascii_alphabetic)
        .map(|letter| letter.to_ascii_uppercase())
        .collect();
    let Some(&first) = letters.first() else {
        return String::new();
    };
    let mut out = String::from(first);
    let mut last = soundex_code(first);
    for &letter in &letters[1..] {
        if out.len() == 4 {
            break;
        }
        let code = soundex_code(letter);
        match code {
            Some(digit) if code != last => out.push(digit),
            _ => {}
        }
        if !matches!(letter, 'H' | 'W') {
            last = code;
        }
    }
    while out.len() < 4 {
        out.push('0');
    }
    out
}

fn vowel(letter: char) -> bool {
    matches!(letter, 'A' | 'E' | 'I' | 'O' | 'U')
}

fn front_vowel(letter: Option<char>) -> bool {
    matches!(letter, Some('E' | 'I' | 'Y'))
}

pub(crate) fn metaphone(word: &str, longest: usize) -> String {
    let letters: Vec<char> = word
        .chars()
        .filter(char::is_ascii_alphabetic)
        .map(|letter| letter.to_ascii_uppercase())
        .collect();
    let at = |index: usize| letters.get(index).copied();
    let mut out = String::new();
    let mut index = 0;
    match (at(0), at(1)) {
        (Some('A'), Some('E')) => {
            out.push('E');
            index = 2;
        }
        (Some('G' | 'K' | 'P'), Some('N')) => {
            out.push('N');
            index = 2;
        }
        (Some('W'), Some('R')) => {
            out.push('R');
            index = 2;
        }
        (Some('W'), Some('H')) => {
            out.push('W');
            index = 2;
        }
        (Some('X'), _) => {
            out.push('S');
            index = 1;
        }
        (Some(first), _) if vowel(first) => {
            out.push(first);
            index = 1;
        }
        _ => {}
    }
    while index < letters.len() && out.chars().count() < longest {
        let letter = letters[index];
        let previous = index.checked_sub(1).and_then(at);
        let next = at(index + 1);
        let after = at(index + 2);
        if previous == Some(letter) && letter != 'C' {
            index += 1;
            continue;
        }
        let mut skip = 0;
        match letter {
            'A' | 'E' | 'I' | 'O' | 'U' => {}
            'B' => {
                if !(previous == Some('M') && after.is_none()) {
                    out.push('B');
                }
            }
            'C' => {
                if next == Some('I') && after == Some('A') {
                    out.push('X');
                } else if next == Some('H') {
                    if previous == Some('S') {
                        out.push('K');
                    } else {
                        out.push('X');
                    }
                    skip = 1;
                } else if front_vowel(next) {
                    if previous != Some('S') {
                        out.push('S');
                    }
                } else {
                    out.push('K');
                }
            }
            'D' => {
                if next == Some('G') && front_vowel(after) {
                    out.push('J');
                    skip = 2;
                } else {
                    out.push('T');
                }
            }
            'G' => {
                let silent_gh = next == Some('H') && after.is_some_and(|letter| !vowel(letter));
                let silent_gn = next == Some('N')
                    && (after.is_none() || (after == Some('E') && at(index + 3) == Some('D')));
                if silent_gh || silent_gn {
                } else if front_vowel(next) && previous != Some('G') {
                    out.push('J');
                } else {
                    out.push('K');
                }
            }
            'H' => {
                let after_softener = matches!(previous, Some('C' | 'S' | 'P' | 'T' | 'G'));
                let between = previous.is_some_and(vowel) && !next.is_some_and(vowel);
                if !after_softener && !between {
                    out.push('H');
                }
            }
            'K' => {
                if previous != Some('C') {
                    out.push('K');
                }
            }
            'P' => {
                if next == Some('H') {
                    out.push('F');
                    skip = 1;
                } else {
                    out.push('P');
                }
            }
            'Q' => out.push('K'),
            'S' => {
                if next == Some('H') {
                    out.push('X');
                    skip = 1;
                } else if next == Some('I') && matches!(after, Some('O' | 'A')) {
                    out.push('X');
                } else {
                    out.push('S');
                }
            }
            'T' => {
                if next == Some('I') && matches!(after, Some('O' | 'A')) {
                    out.push('X');
                } else if next == Some('H') {
                    out.push('0');
                    skip = 1;
                } else if !(next == Some('C') && after == Some('H')) {
                    out.push('T');
                }
            }
            'V' => out.push('F'),
            'W' | 'Y' => {
                if next.is_some_and(vowel) {
                    out.push(letter);
                }
            }
            'X' => {
                out.push('K');
                if out.chars().count() < longest {
                    out.push('S');
                }
            }
            'Z' => out.push('S'),
            other => out.push(other),
        }
        index += 1 + skip;
    }
    out.chars().take(longest).collect()
}

fn trigram_words(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn word_trigrams(word: &str) -> Vec<String> {
    let padded: Vec<char> = format!("  {word} ").chars().collect();
    padded
        .windows(3)
        .map(|window| window.iter().collect())
        .collect()
}

fn checked_length(text: &str) -> SqlResult<()> {
    if text.chars().count() > MAX_TRIGRAM_CHARACTERS {
        return Err(SqlError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("trigram functions take at most {MAX_TRIGRAM_CHARACTERS} characters"),
        ));
    }
    Ok(())
}

fn trigram_set(text: &str) -> SqlResult<BTreeSet<String>> {
    checked_length(text)?;
    Ok(trigram_words(text)
        .iter()
        .flat_map(|word| word_trigrams(word))
        .collect())
}

fn similarity(left: &BTreeSet<String>, right: &BTreeSet<String>) -> f64 {
    let union = left.union(right).count();
    if union == 0 {
        return 0.0;
    }
    left.intersection(right).count() as f64 / union as f64
}

fn word_similarity(needle: &str, haystack: &str, strict: bool) -> SqlResult<f64> {
    let wanted = trigram_set(needle)?;
    checked_length(haystack)?;
    if wanted.is_empty() {
        return Ok(0.0);
    }
    let words: Vec<Vec<String>> = trigram_words(haystack)
        .iter()
        .map(|word| word_trigrams(word))
        .collect();
    let mut best = 0.0f64;
    if strict {
        for start in 0..words.len() {
            let mut extent: BTreeSet<String> = BTreeSet::new();
            for word in &words[start..] {
                extent.extend(word.iter().cloned());
                best = best.max(similarity(&wanted, &extent));
            }
        }
        return Ok(best);
    }
    let ordered: Vec<&String> = words.iter().flatten().collect();
    let starts: Vec<usize> = (0..ordered.len())
        .filter(|&at| wanted.contains(ordered[at]))
        .collect();
    for &start in &starts {
        let mut extent: BTreeSet<String> = BTreeSet::new();
        for trigram in &ordered[start..] {
            extent.insert((*trigram).clone());
            if wanted.contains(*trigram) {
                best = best.max(similarity(&wanted, &extent));
            }
        }
    }
    Ok(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_distances_and_sound_codes_match_fuzzystrmatch() {
        assert_eq!(
            levenshtein_value("GUMBO", "GAMBOL", (1, 1, 1), None).unwrap(),
            Value::Integer(2)
        );
        assert_eq!(
            levenshtein_value("GUMBO", "GAMBOL", (2, 1, 1), None).unwrap(),
            Value::Integer(3)
        );
        assert_eq!(
            levenshtein_value("extensive", "exhaustive", (1, 1, 1), Some(2)).unwrap(),
            Value::Integer(3)
        );
        assert_eq!(soundex("Anne"), "A500");
        assert_eq!(soundex("Robert"), "R163");
        assert_eq!(soundex("Tymczak"), "T522");
        assert_eq!(metaphone("GUMBO", 4), "KM");
    }

    #[test]
    fn trigram_similarities_match_pg_trgm() {
        let cat = trigram_set("cat").unwrap();
        assert_eq!(
            cat.iter().cloned().collect::<Vec<String>>(),
            vec!["  c", " ca", "at ", "cat"]
        );
        assert!((word_similarity("word", "two words", false).unwrap() - 0.8).abs() < 1e-9);
        assert!(
            (word_similarity("word", "two words", true).unwrap() - 4.0 / 7.0).abs() < 1e-9
        );
        let similar = similarity(&trigram_set("word").unwrap(), &trigram_set("words").unwrap());
        assert!((similar - 4.0 / 7.0).abs() < 1e-9);
    }
}
