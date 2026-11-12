const MARKED_I: char = 'I';
const MARKED_U: char = 'U';

const STEP_ZERO: &[(&str, &str)] = &[
    ("ul", ""),
    ("ului", ""),
    ("aua", "a"),
    ("ea", "e"),
    ("ele", "e"),
    ("elor", "e"),
    ("ii", "i"),
    ("iua", "i"),
    ("iei", "i"),
    ("iile", "i"),
    ("iilor", "i"),
    ("ilor", "i"),
    ("ile", "i"),
    ("atei", "at"),
    ("aţie", "aţi"),
    ("aţia", "aţi"),
];

const COMBINED: &[(&str, &str)] = &[
    ("abilitate", "abil"),
    ("abilitati", "abil"),
    ("abilităi", "abil"),
    ("abilităţi", "abil"),
    ("ibilitate", "ibil"),
    ("ivitate", "iv"),
    ("ivitati", "iv"),
    ("ivităi", "iv"),
    ("ivităţi", "iv"),
    ("icitate", "ic"),
    ("icitati", "ic"),
    ("icităi", "ic"),
    ("icităţi", "ic"),
    ("icator", "ic"),
    ("icatori", "ic"),
    ("iciv", "ic"),
    ("iciva", "ic"),
    ("icive", "ic"),
    ("icivi", "ic"),
    ("icivă", "ic"),
    ("ical", "ic"),
    ("icala", "ic"),
    ("icale", "ic"),
    ("icali", "ic"),
    ("icală", "ic"),
    ("ativ", "at"),
    ("ativa", "at"),
    ("ative", "at"),
    ("ativi", "at"),
    ("ativă", "at"),
    ("aţiune", "at"),
    ("atoare", "at"),
    ("ator", "at"),
    ("atori", "at"),
    ("ătoare", "at"),
    ("ător", "at"),
    ("ători", "at"),
    ("itiv", "it"),
    ("itiva", "it"),
    ("itive", "it"),
    ("itivi", "it"),
    ("itivă", "it"),
    ("iţiune", "it"),
    ("itoare", "it"),
    ("itor", "it"),
    ("itori", "it"),
];

const STANDARD_DELETED: &[&str] = &[
    "at", "ata", "ată", "ati", "ate", "ut", "uta", "ută", "uti", "ute", "it", "ita", "ită", "iti", "ite",
    "ic", "ica", "ice", "ici", "ică", "abil", "abila", "abile", "abili", "abilă", "ibil", "ibila", "ibile",
    "ibili", "ibilă", "oasa", "oasă", "oase", "os", "osi", "oşi", "ant", "anta", "ante", "anti", "antă",
    "ator", "atori", "itate", "itati", "ităi", "ităţi", "iv", "iva", "ive", "ivi", "ivă",
];
const STANDARD_TO_IST: &[&str] = &["ism", "isme", "ist", "ista", "iste", "isti", "istă", "işti"];
const STANDARD_AFTER_T: &[&str] = &["iune", "iuni"];

const VERB_AFTER_CONSONANT: &[&str] = &[
    "are", "ere", "ire", "âre", "ind", "ând", "indu", "ându", "eze", "ească", "ez", "ezi", "ează", "esc",
    "eşti", "eşte", "ăsc", "ăşti", "ăşte", "am", "ai", "au", "eam", "eai", "ea", "eaţi", "eau", "iam",
    "iai", "ia", "iaţi", "iau", "ui", "aşi", "arăm", "arăţi", "ară", "uşi", "urăm", "urăţi", "ură", "işi",
    "irăm", "irăţi", "iră", "âi", "âşi", "ârăm", "ârăţi", "âră", "asem", "aseşi", "ase", "aserăm",
    "aserăţi", "aseră", "isem", "iseşi", "ise", "iserăm", "iserăţi", "iseră", "âsem", "âseşi", "âse",
    "âserăm", "âserăţi", "âseră", "usem", "useşi", "use", "userăm", "userăţi", "useră",
];
const VERB_DELETED: &[&str] = &[
    "ăm", "aţi", "em", "eţi", "im", "iţi", "âm", "âţi", "seşi", "serăm", "serăţi", "seră", "sei", "se",
    "sesem", "seseşi", "sese", "seserăm", "seserăţi", "seseră",
];
const RESIDUAL: &[&str] = &["a", "e", "i", "ie", "ă"];

fn is_vowel(letter: char) -> bool {
    matches!(letter, 'a' | 'ă' | 'â' | 'e' | 'i' | 'î' | 'o' | 'u')
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

fn region_rv(letters: &[char]) -> usize {
    if letters.len() < 2 {
        return letters.len();
    }
    let after = |from: usize, wanted_vowel: bool| {
        (from..letters.len())
            .find(|&at| is_vowel(letters[at]) == wanted_vowel)
            .map_or(letters.len(), |at| at + 1)
    };
    if !is_vowel(letters[1]) {
        after(2, true)
    } else if is_vowel(letters[0]) {
        after(2, false)
    } else {
        3.min(letters.len())
    }
}

fn ends_with(letters: &[char], suffix: &str) -> Option<usize> {
    let suffix: Vec<char> = suffix.chars().collect();
    letters.ends_with(&suffix).then(|| letters.len() - suffix.len())
}

fn longest<'s>(letters: &[char], suffixes: impl Iterator<Item = &'s str>) -> Option<(&'s str, usize)> {
    suffixes
        .filter_map(|suffix| ends_with(letters, suffix).map(|start| (suffix, start)))
        .max_by_key(|(suffix, _)| suffix.chars().count())
}

fn replace_tail(letters: &mut Vec<char>, start: usize, with: &str) {
    letters.truncate(start);
    letters.extend(with.chars());
}

fn prepared(word: &str) -> Vec<char> {
    let mut letters: Vec<char> = word
        .chars()
        .map(|letter| match letter {
            'ș' => 'ş',
            'ț' => 'ţ',
            other => other,
        })
        .collect();
    for at in 1..letters.len().saturating_sub(1) {
        if is_vowel(letters[at - 1]) && is_vowel(letters[at + 1]) {
            letters[at] = match letters[at] {
                'i' => MARKED_I,
                'u' => MARKED_U,
                other => other,
            };
        }
    }
    letters
}

fn step_zero(letters: &mut Vec<char>, r1: usize) {
    let Some((suffix, start)) = longest(letters, STEP_ZERO.iter().map(|(suffix, _)| *suffix)) else {
        return;
    };
    if start < r1 {
        return;
    }
    if suffix == "ile" && ends_with(&letters[..start], "ab").is_some() {
        return;
    }
    let with = STEP_ZERO.iter().find(|(known, _)| *known == suffix).map_or("", |(_, with)| *with);
    replace_tail(letters, start, with);
}

fn combined_suffixes(letters: &mut Vec<char>, r1: usize) -> bool {
    let mut changed = false;
    while let Some((suffix, start)) = longest(letters, COMBINED.iter().map(|(suffix, _)| *suffix)) {
        if start < r1 {
            break;
        }
        let with = COMBINED.iter().find(|(known, _)| *known == suffix).map_or("", |(_, with)| *with);
        replace_tail(letters, start, with);
        changed = true;
    }
    changed
}

fn standard_suffix(letters: &mut Vec<char>, r2: usize) -> bool {
    let all = STANDARD_DELETED.iter().chain(STANDARD_TO_IST).chain(STANDARD_AFTER_T).copied();
    let Some((suffix, start)) = longest(letters, all) else {
        return false;
    };
    if start < r2 {
        return false;
    }
    if STANDARD_TO_IST.contains(&suffix) {
        replace_tail(letters, start, "ist");
    } else if STANDARD_AFTER_T.contains(&suffix) {
        if start == 0 || letters[start - 1] != 'ţ' {
            return false;
        }
        letters.truncate(start - 1);
        letters.push('t');
    } else {
        letters.truncate(start);
    }
    true
}

fn verb_suffix(letters: &mut Vec<char>, rv: usize) {
    let all = VERB_AFTER_CONSONANT.iter().chain(VERB_DELETED).copied();
    let Some((suffix, start)) = longest(letters, all) else {
        return;
    };
    if start < rv {
        return;
    }
    if VERB_AFTER_CONSONANT.contains(&suffix) {
        let before = start.checked_sub(1).map(|at| letters[at]);
        if !before.is_some_and(|letter| !is_vowel(letter) || letter == 'u') {
            return;
        }
    }
    letters.truncate(start);
}

/// The stem of `word` (lower case) by Snowball's Romanian stemmer: `ș` and `ț` read as `ş` and
/// `ţ`, `i` and `u` between vowels kept apart, then the plural and article endings, the
/// combined, standard and verb suffixes and a final vowel removed over RV, R1 and R2.
pub fn stem(word: &str) -> String {
    let mut letters = prepared(word);
    let rv = region_rv(&letters);
    let r1 = region_after(&letters, 0);
    let r2 = region_after(&letters, r1).max(r1);
    step_zero(&mut letters, r1);
    let combined = combined_suffixes(&mut letters, r1);
    let standard = standard_suffix(&mut letters, r2);
    if !combined && !standard {
        verb_suffix(&mut letters, rv);
    }
    if let Some((_, start)) = longest(&letters, RESIDUAL.iter().copied())
        && start >= rv
    {
        letters.truncate(start);
    }
    letters
        .into_iter()
        .map(|letter| match letter {
            MARKED_I => 'i',
            MARKED_U => 'u',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_drop_articles_plurals_and_verb_endings() {
        for (word, expected) in [("cântare", "cânt"), ("oamenilor", "oamen"), ("frumoasă", "frumoas")] {
            assert_eq!(stem(word), expected, "{word}");
        }
    }
}
