const STOP_WORDS: &[&str] = &[
    "de", "en", "van", "ik", "te", "dat", "die", "in", "een", "hij", "het", "niet", "zijn", "is",
    "was", "op", "aan", "met", "als", "voor", "had", "er", "maar", "om", "hem", "dan", "zou",
    "of", "wat", "mijn", "men", "dit", "zo", "door", "over", "ze", "zich", "bij", "ook", "tot",
    "je", "mij", "uit", "der", "daar", "haar", "naar", "heb", "hoe", "heeft", "hebben", "deze",
    "u", "want", "nog", "zal", "me", "zij", "nu", "ge", "geen", "omdat", "iets", "worden", "toch",
    "al", "waren", "veel", "meer", "doen", "toen", "moet", "ben", "zonder", "kan", "hun", "dus",
    "alles", "onder", "ja", "eens", "hier", "wie", "werd", "altijd", "doch", "wordt", "wezen",
    "kunnen", "ons", "zelf", "tegen", "na", "reeds", "wil", "kon", "niets", "uw", "iemand",
    "geweest", "andere",
];
const MARKED_Y: char = 'Y';
const MARKED_I: char = 'I';

/// Whether `word` (lower case) is on Snowball's Dutch stop word list.
pub fn is_stop_word(word: &str) -> bool {
    STOP_WORDS.contains(&word)
}

fn is_vowel(letter: char) -> bool {
    matches!(letter, 'a' | 'e' | 'i' | 'o' | 'u' | 'y' | 'è')
}

fn region_after(letters: &[char], from: usize) -> usize {
    let mut at = from;
    while at < letters.len() && !is_vowel(letters[at]) {
        at += 1;
    }
    while at < letters.len() && is_vowel(letters[at]) {
        at += 1;
    }
    (at + 1).min(letters.len())
}

fn ends_with(letters: &[char], suffix: &str) -> Option<usize> {
    let suffix: Vec<char> = suffix.chars().collect();
    letters
        .ends_with(&suffix)
        .then(|| letters.len() - suffix.len())
}

fn longest(letters: &[char], suffixes: &[&'static str]) -> Option<(&'static str, usize)> {
    suffixes
        .iter()
        .filter_map(|suffix| ends_with(letters, suffix).map(|start| (*suffix, start)))
        .max_by_key(|(suffix, _)| suffix.chars().count())
}

fn prepared(word: &str) -> Vec<char> {
    let mut letters: Vec<char> = word
        .chars()
        .map(|letter| match letter {
            'ä' | 'á' => 'a',
            'ë' | 'é' => 'e',
            'ï' | 'í' => 'i',
            'ö' | 'ó' => 'o',
            'ü' | 'ú' => 'u',
            other => other,
        })
        .collect();
    if letters.first() == Some(&'y') {
        letters[0] = MARKED_Y;
    }
    for at in 1..letters.len() {
        if letters[at] == 'y' && is_vowel(letters[at - 1]) {
            letters[at] = MARKED_Y;
        } else if letters[at] == 'i'
            && is_vowel(letters[at - 1])
            && letters.get(at + 1).is_some_and(|&next| is_vowel(next))
        {
            letters[at] = MARKED_I;
        }
    }
    letters
}

fn undouble(letters: &mut Vec<char>) {
    if longest(letters, &["kk", "dd", "tt"]).is_some() {
        letters.pop();
    }
}

fn en_ending(letters: &mut Vec<char>, start: usize, r1: usize) -> bool {
    if start < r1 || start == 0 || is_vowel(letters[start - 1]) {
        return false;
    }
    if ends_with(&letters[..start], "gem").is_some() {
        return false;
    }
    letters.truncate(start);
    undouble(letters);
    true
}

fn step_one(letters: &mut Vec<char>, r1: usize) {
    let Some((suffix, start)) = longest(letters, &["heden", "en", "ene", "s", "se"]) else {
        return;
    };
    match suffix {
        "heden" => {
            if start >= r1 {
                letters.truncate(start);
                letters.extend("heid".chars());
            }
        }
        "en" | "ene" => {
            en_ending(letters, start, r1);
        }
        _ => {
            if start >= r1
                && start > 0
                && !is_vowel(letters[start - 1])
                && letters[start - 1] != 'j'
            {
                letters.truncate(start);
            }
        }
    }
}

fn step_two(letters: &mut Vec<char>, r1: usize) -> bool {
    let length = letters.len();
    if length >= 2
        && letters[length - 1] == 'e'
        && length - 1 >= r1
        && !is_vowel(letters[length - 2])
    {
        letters.pop();
        undouble(letters);
        return true;
    }
    false
}

fn step_three(letters: &mut Vec<char>, r1: usize, r2: usize, e_removed: bool) {
    if let Some(start) = ends_with(letters, "heid")
        && start >= r2
        && (start == 0 || letters[start - 1] != 'c')
    {
        letters.truncate(start);
        if let Some(en) = ends_with(letters, "en") {
            en_ending(letters, en, r1);
        }
    }
    let Some((suffix, start)) = longest(letters, &["end", "ing", "ig", "lijk", "baar", "bar"]) else {
        return;
    };
    if start < r2 {
        return;
    }
    let after_e = start > 0 && letters[start - 1] == 'e';
    match suffix {
        "end" | "ing" => {
            letters.truncate(start);
            if let Some(ig) = ends_with(letters, "ig")
                && ig >= r2
                && !(ig > 0 && letters[ig - 1] == 'e')
            {
                letters.truncate(ig);
            } else {
                undouble(letters);
            }
        }
        "ig" => {
            if !after_e {
                letters.truncate(start);
            }
        }
        "lijk" => {
            letters.truncate(start);
            step_two(letters, r1);
        }
        "baar" => letters.truncate(start),
        _ => {
            if e_removed {
                letters.truncate(start);
            }
        }
    }
}

fn undouble_vowel(letters: &mut Vec<char>) {
    let length = letters.len();
    if length < 4 {
        return;
    }
    let (consonant, first, second, last) = (
        letters[length - 4],
        letters[length - 3],
        letters[length - 2],
        letters[length - 1],
    );
    if !is_vowel(consonant)
        && first == second
        && matches!(first, 'a' | 'e' | 'o' | 'u')
        && !is_vowel(last)
        && last != MARKED_I
    {
        letters.remove(length - 2);
    }
}

/// The stem of `word` (lower case) by Snowball's Dutch stemmer: accents and umlauts dropped,
/// `-heden`, `-en`, `-s`, `-e`, `-heid` and the derivational suffixes removed over R1 and R2,
/// and a doubled vowel before a final consonant halved.
pub fn stem(word: &str) -> String {
    let mut letters = prepared(word);
    let r1 = region_after(&letters, 0).max(3).min(letters.len());
    let r2 = region_after(&letters, r1).max(r1);
    step_one(&mut letters, r1);
    let e_removed = step_two(&mut letters, r1);
    step_three(&mut letters, r1, r2, e_removed);
    undouble_vowel(&mut letters);
    letters
        .into_iter()
        .map(|letter| match letter {
            MARKED_Y => 'y',
            MARKED_I => 'i',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_drop_plurals_and_derivations() {
        for (word, expected) in [
            ("lichamelijk", "licham"),
            ("maanden", "maand"),
            ("bomen", "bom"),
            ("mogelijkheden", "mogelijk"),
        ] {
            assert_eq!(stem(word), expected, "{word}");
        }
        assert!(is_stop_word("van"));
        assert!(!is_stop_word("huis"));
    }
}
