//! Linguistic collations (D1345): PostgreSQL's ICU-style names (`"und-x-icu"`, `"en-US-x-icu"`,
//! `"de-DE"`, `"unicode"`, ...) for the languages whose CLDR standard collation is the root one,
//! ordered by the Unicode Collation Algorithm. `x COLLATE "de-x-icu"` makes a text value that
//! compares by it; `-u-ks-level1` and `-u-ks-level2` make the ones that ignore accents and case,
//! or case, nondeterministic as PostgreSQL's are.

use crate::types::{DefinedValue, Value};
use eggdb_text::Strength;
use std::cmp::Ordering;

const ROOT_LANGUAGES: [&str; 11] = ["und", "root", "unicode", "en", "de", "fr", "it", "pt", "nl", "ru", "id"];

/// A linguistic collation: how many levels it compares, and whether only identical texts are
/// equal under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Collation {
    /// How many levels it compares.
    pub strength: Strength,
    /// Whether texts equal at its strength are still told apart by their code points.
    pub deterministic: bool,
}

/// A text value with a linguistic collation, as `COLLATE` makes it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CollatedText {
    /// The text.
    pub text: String,
    /// Its collation.
    pub collation: Collation,
}

/// The collation named `name`, or `None` when it is no linguistic collation EggDB has.
pub(crate) fn named(name: &str) -> Option<Collation> {
    let lowered = name.to_ascii_lowercase().replace('_', "-");
    let (base, strength) = if let Some(base) = lowered.strip_suffix("-u-ks-level1") {
        (base, Some(Strength::Primary))
    } else if let Some(base) = lowered.strip_suffix("-u-ks-level2") {
        (base, Some(Strength::Secondary))
    } else {
        (lowered.as_str(), None)
    };
    let base = base
        .strip_suffix("-x-icu")
        .or_else(|| base.strip_suffix(".utf8"))
        .or_else(|| base.strip_suffix(".utf-8"))
        .unwrap_or(base);
    let mut parts = base.split('-');
    let language = parts.next()?;
    let region = parts.next();
    if parts.next().is_some() || !ROOT_LANGUAGES.contains(&language) || (language == "fr" && region == Some("ca")) {
        return None;
    }
    if region.is_some_and(|region| region.len() != 2 || !region.bytes().all(|byte| byte.is_ascii_alphabetic())) {
        return None;
    }
    Some(Collation {
        strength: strength.unwrap_or(Strength::Tertiary),
        deterministic: strength.is_none(),
    })
}

pub(crate) fn value(text: String, collation: Collation) -> Value {
    Value::Defined(Box::new(DefinedValue::Collated(CollatedText { text, collation })))
}

pub(crate) fn collated_of(value: &Value) -> Option<&CollatedText> {
    match value {
        Value::Defined(defined) => match defined.as_ref() {
            DefinedValue::Collated(collated) => Some(collated),
            _ => None,
        },
        _ => None,
    }
}

/// Orders two texts under `collation`.
pub(crate) fn compare(left: &str, right: &str, collation: Collation) -> Ordering {
    if collation.deterministic {
        eggdb_text::compare_collated(left, right, collation.strength)
    } else {
        eggdb_text::sort_key(left, collation.strength).cmp(&eggdb_text::sort_key(right, collation.strength))
    }
}

/// Orders `left` and `right` when one of them is collated text and the other text: by the
/// collated side's collation.
pub(crate) fn compare_with_text(left: &Value, right: &Value) -> Option<Ordering> {
    let collation = collated_of(left).or_else(|| collated_of(right))?.collation;
    let text = |value: &Value| collated_of(value).map(|collated| collated.text.clone()).or_else(|| value.character_text().map(str::to_owned));
    Some(compare(&text(left)?, &text(right)?, collation))
}

/// The bytes that equal values share: the text itself for a deterministic collation, the sort
/// key for one that treats different texts as equal.
pub(crate) fn equality_key(out: &mut Vec<u8>, collated: &CollatedText) {
    if collated.collation.deterministic {
        out.push(2);
        out.extend_from_slice(&(collated.text.len() as u64).to_le_bytes());
        out.extend_from_slice(collated.text.as_bytes());
    } else {
        let key = eggdb_text::sort_key(&collated.text, collated.collation.strength);
        out.push(31);
        out.extend_from_slice(&(key.len() as u64).to_le_bytes());
        out.extend_from_slice(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_postgresql_and_refuse_tailored_languages() {
        assert!(named("und-x-icu").is_some());
        assert!(named("en-US-x-icu").is_some());
        assert!(named("de_DE.utf8").is_some());
        assert!(named("unicode").is_some());
        assert_eq!(named("und-u-ks-level2").map(|found| found.deterministic), Some(false));
        assert!(named("sv-SE-x-icu").is_none());
        assert!(named("fr-CA-x-icu").is_none());
        assert!(named("C").is_none());
    }

    fn answer(session: &mut crate::Session<'_>, sql: &str) -> String {
        let mut outputs = Vec::new();
        session
            .execute(sql, &mut |result| outputs.push(crate::tests::render(&result)))
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        outputs.pop().unwrap_or_default()
    }

    #[test]
    fn collate_orders_and_groups_by_the_collation() {
        let store = crate::memory_store::MemoryStore::default();
        let mut session = crate::Session::new(&store);
        answer(
            &mut session,
            "CREATE TABLE words (w text); INSERT INTO words VALUES ('banana'), ('Apple'), ('apple'), ('Äpfel'), ('cherry'), ('APPLE')",
        );
        assert_eq!(
            answer(&mut session, "SELECT w FROM words ORDER BY w COLLATE \"en-US-x-icu\""),
            "w\nÄpfel\napple\nApple\nAPPLE\nbanana\ncherry\nSELECT 6"
        );
        assert_eq!(
            answer(&mut session, "SELECT w FROM words ORDER BY w"),
            "w\nAPPLE\nApple\napple\nbanana\ncherry\nÄpfel\nSELECT 6"
        );
        assert_eq!(
            answer(
                &mut session,
                "SELECT count(DISTINCT w COLLATE \"und-u-ks-level2\") AS kinds FROM words"
            ),
            "kinds\n4\nSELECT 1"
        );
        assert_eq!(
            answer(&mut session, "SELECT 'apple' COLLATE \"de-x-icu\" < 'Banana' AS earlier"),
            "earlier\nt\nSELECT 1"
        );
    }
}
