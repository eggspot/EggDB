const STOP_WORDS: &[&str] = &[
    "ad", "al", "allo", "ai", "agli", "all", "agl", "alla", "alle", "con", "col", "coi", "da",
    "dal", "dallo", "dai", "dagli", "dall", "dagl", "dalla", "dalle", "di", "del", "dello", "dei",
    "degli", "dell", "degl", "della", "delle", "in", "nel", "nello", "nei", "negli", "nell",
    "negl", "nella", "nelle", "su", "sul", "sullo", "sui", "sugli", "sull", "sugl", "sulla",
    "sulle", "per", "tra", "contro", "io", "tu", "lui", "lei", "noi", "voi", "loro", "mio", "mia",
    "miei", "mie", "tuo", "tua", "tuoi", "tue", "suo", "sua", "suoi", "sue", "nostro", "nostra",
    "nostri", "nostre", "vostro", "vostra", "vostri", "vostre", "mi", "ti", "ci", "vi", "lo", "la",
    "li", "le", "gli", "ne", "il", "un", "uno", "una", "ma", "ed", "se", "perché", "anche", "come",
    "dov", "dove", "che", "chi", "cui", "non", "più", "quale", "quanto", "quanti", "quanta",
    "quante", "quello", "quelli", "quella", "quelle", "questo", "questi", "questa", "queste", "si",
    "tutto", "tutti", "a", "c", "e", "i", "l", "o", "ho", "hai", "ha", "abbiamo", "avete", "hanno",
    "abbia", "avevo", "aveva", "avevano", "ebbe", "sono", "sei", "è", "siamo", "siete", "sia",
    "ero", "era", "erano", "fu", "furono", "essere", "stato", "stata", "stati", "state", "fare",
    "faccio", "fa", "fanno", "sto", "sta", "stanno",
];
const PRONOUNS: &[&str] = &[
    "ci", "gli", "la", "le", "li", "lo", "mi", "ne", "si", "ti", "vi", "sene", "gliela", "gliele",
    "glieli", "glielo", "gliene", "mela", "mele", "meli", "melo", "mene", "tela", "tele", "teli",
    "telo", "tene", "cela", "cele", "celi", "celo", "cene", "vela", "vele", "veli", "velo", "vene",
];
const VERB_SUFFIXES: &[&str] = &[
    "ammo", "ando", "ano", "are", "arono", "asse", "assero", "assi", "assimo", "ata", "ate", "ati",
    "ato", "ava", "avamo", "avano", "avate", "avi", "avo", "emmo", "enda", "ende", "endi", "endo",
    "erà", "erai", "eranno", "ere", "erebbe", "erebbero", "erei", "eremmo", "eremo", "ereste",
    "eresti", "erete", "erò", "erono", "essero", "ete", "eva", "evamo", "evano", "evate", "evi",
    "evo", "Yamo", "iamo", "immo", "irà", "irai", "iranno", "ire", "irebbe", "irebbero", "irei",
    "iremmo", "iremo", "ireste", "iresti", "irete", "irò", "irono", "isca", "iscano", "isce",
    "isci", "isco", "iscono", "issero", "ita", "ite", "iti", "ito", "iva", "ivamo", "ivano",
    "ivate", "ivi", "ivo", "ar", "ir",
];
const STANDARD_SUFFIXES: &[&str] = &[
    "anza", "anze", "ico", "ici", "ica", "ice", "iche", "ichi", "ismo", "ismi", "abile", "abili",
    "ibile", "ibili", "ista", "iste", "isti", "istà", "istè", "istì", "oso", "osi", "osa", "ose",
    "mente", "atrice", "atrici", "ante", "anti", "azione", "azioni", "atore", "atori", "logia",
    "logie", "uzione", "uzioni", "usione", "usioni", "enza", "enze", "amento", "amenti", "imento",
    "imenti", "amente", "ità", "ivo", "ivi", "iva", "ive",
];

/// Whether `word` (lower case) is on Snowball's Italian stop word list.
pub fn is_stop_word(word: &str) -> bool {
    STOP_WORDS.contains(&word)
}

fn is_vowel(letter: char) -> bool {
    matches!(
        letter,
        'a' | 'e' | 'i' | 'o' | 'u' | 'à' | 'è' | 'ì' | 'ò' | 'ù'
    )
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

fn replace_tail(letters: &mut Vec<char>, start: usize, with: &str) {
    letters.truncate(start);
    letters.extend(with.chars());
}

fn prepared(word: &str) -> Vec<char> {
    let mut letters: Vec<char> = word
        .chars()
        .map(|letter| match letter {
            'á' => 'à',
            'é' => 'è',
            'í' => 'ì',
            'ó' => 'ò',
            'ú' => 'ù',
            other => other,
        })
        .collect();
    for at in 1..letters.len() {
        if letters[at] == 'u' && letters[at - 1] == 'q' {
            letters[at] = 'U';
        }
    }
    for at in 1..letters.len().saturating_sub(1) {
        if is_vowel(letters[at - 1]) && is_vowel(letters[at + 1]) {
            match letters[at] {
                'u' => letters[at] = 'U',
                'i' => letters[at] = 'I',
                _ => {}
            }
        }
    }
    letters
}

fn attached_pronoun(letters: &mut Vec<char>, rv: usize) {
    let Some((_, start)) = longest(letters, PRONOUNS) else {
        return;
    };
    if start < rv {
        return;
    }
    let before = &letters[..start];
    if let Some((_, at)) = longest(before, &["ando", "endo"])
        && at >= rv
    {
        letters.truncate(start);
    } else if let Some((_, at)) = longest(before, &["ar", "er", "ir"])
        && at >= rv
    {
        replace_tail(letters, start, "e");
    }
}

fn standard_suffix(letters: &mut Vec<char>, rv: usize, r1: usize, r2: usize) -> bool {
    let Some((suffix, start)) = longest(letters, STANDARD_SUFFIXES) else {
        return false;
    };
    let drop_if = |letters: &mut Vec<char>, ending: &str| {
        if let Some(at) = ends_with(letters, ending)
            && at >= r2
        {
            letters.truncate(at);
            return true;
        }
        false
    };
    match suffix {
        "azione" | "azioni" | "atore" | "atori" => {
            if start < r2 {
                return false;
            }
            letters.truncate(start);
            drop_if(letters, "ic");
        }
        "logia" | "logie" => {
            if start < r2 {
                return false;
            }
            replace_tail(letters, start, "log");
        }
        "uzione" | "uzioni" | "usione" | "usioni" => {
            if start < r2 {
                return false;
            }
            replace_tail(letters, start, "u");
        }
        "enza" | "enze" => {
            if start < r2 {
                return false;
            }
            replace_tail(letters, start, "ente");
        }
        "amento" | "amenti" | "imento" | "imenti" => {
            if start < rv {
                return false;
            }
            letters.truncate(start);
        }
        "amente" => {
            if start < r1 {
                return false;
            }
            letters.truncate(start);
            if drop_if(letters, "iv") {
                drop_if(letters, "at");
            } else if !drop_if(letters, "os") && !drop_if(letters, "ic") {
                drop_if(letters, "abil");
            }
        }
        "ità" => {
            if start < r2 {
                return false;
            }
            letters.truncate(start);
            if !drop_if(letters, "abil") && !drop_if(letters, "ic") {
                drop_if(letters, "iv");
            }
        }
        "ivo" | "ivi" | "iva" | "ive" => {
            if start < r2 {
                return false;
            }
            letters.truncate(start);
            if drop_if(letters, "at") {
                drop_if(letters, "ic");
            }
        }
        _ => {
            if start < r2 {
                return false;
            }
            letters.truncate(start);
        }
    }
    true
}

fn verb_suffix(letters: &mut Vec<char>, rv: usize) {
    if let Some((_, start)) = longest(letters, VERB_SUFFIXES)
        && start >= rv
    {
        letters.truncate(start);
    }
}

fn final_vowel(letters: &mut Vec<char>, rv: usize) {
    if let Some(&last) = letters.last()
        && matches!(last, 'a' | 'e' | 'i' | 'o' | 'à' | 'è' | 'ì' | 'ò')
        && letters.len() - 1 >= rv
    {
        letters.pop();
        if letters.last() == Some(&'i') && letters.len() - 1 >= rv {
            letters.pop();
        }
    }
    if let Some((_, start)) = longest(letters, &["ch", "gh"])
        && start >= rv
    {
        letters.pop();
    }
}

/// The stem of `word` (lower case) by Snowball's Italian stemmer: attached pronouns, the
/// standard and verb suffixes and a final vowel removed over RV, R1 and R2.
pub fn stem(word: &str) -> String {
    let mut letters = prepared(word);
    let rv = region_rv(&letters);
    let r1 = region_after(&letters, 0);
    let r2 = region_after(&letters, r1).max(r1);
    attached_pronoun(&mut letters, rv);
    if !standard_suffix(&mut letters, rv, r1, r2) {
        verb_suffix(&mut letters, rv);
    }
    final_vowel(&mut letters, rv);
    letters
        .into_iter()
        .map(|letter| match letter {
            'U' => 'u',
            'I' => 'i',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_drop_suffixes_and_final_vowels() {
        for (word, expected) in [
            ("parlando", "parl"),
            ("ragazzi", "ragazz"),
            ("velocemente", "veloc"),
            ("amiche", "amic"),
        ] {
            assert_eq!(stem(word), expected, "{word}");
        }
        assert!(is_stop_word("della"));
        assert!(!is_stop_word("casa"));
    }
}
