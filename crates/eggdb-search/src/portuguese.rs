const STOP_WORDS: &[&str] = &[
    "de", "a", "o", "que", "e", "do", "da", "em", "um", "para", "com", "não", "uma", "os", "no",
    "se", "na", "por", "mais", "as", "dos", "como", "mas", "ao", "ele", "das", "à", "seu", "sua",
    "ou", "quando", "muito", "nos", "já", "eu", "também", "só", "pelo", "pela", "até", "isso",
    "ela", "entre", "depois", "sem", "mesmo", "aos", "seus", "quem", "nas", "me", "esse", "eles",
    "você", "essa", "num", "nem", "suas", "meu", "às", "minha", "numa", "pelos", "elas", "qual",
    "nós", "lhe", "deles", "essas", "esses", "pelas", "este", "dele", "tu", "te", "vocês", "vos",
    "lhes", "meus", "minhas", "teu", "tua", "teus", "tuas", "nosso", "nossa", "nossos", "nossas",
    "dela", "delas", "esta", "estes", "estas", "aquele", "aquela", "aqueles", "aquelas", "isto",
    "aquilo", "estou", "está", "estamos", "estão", "estive", "esteve", "estivemos", "estiveram",
    "estava", "estávamos", "estavam", "hei", "há", "havemos", "hão", "houve", "sou", "somos",
    "são", "era", "éramos", "eram", "fui", "foi", "fomos", "foram", "seja", "sejam", "tenho",
    "tem", "temos", "têm", "tinha", "tínhamos", "tinham", "tive", "teve", "tivemos", "tiveram",
];
const STANDARD_SUFFIXES: &[&str] = &[
    "eza", "ezas", "ico", "ica", "icos", "icas", "ismo", "ismos", "ável", "ível", "ista", "istas",
    "oso", "osa", "osos", "osas", "amento", "amentos", "imento", "imentos", "adora", "ador",
    "aça~o", "adoras", "adores", "aço~es", "ante", "antes", "ância", "logia", "logias", "ução",
    "uções", "ência", "ências", "amente", "mente", "idade", "idades", "iva", "ivo", "ivas", "ivos",
    "ira", "iras",
];
const VERB_SUFFIXES: &[&str] = &[
    "ada", "ida", "ia", "aria", "eria", "iria", "ará", "ara", "erá", "era", "irá", "ava", "asse",
    "esse", "isse", "aste", "este", "iste", "ei", "arei", "erei", "irei", "am", "iam", "ariam",
    "eriam", "iriam", "aram", "eram", "iram", "avam", "em", "arem", "erem", "irem", "assem",
    "essem", "issem", "ado", "ido", "ando", "endo", "indo", "ara~o", "era~o", "ira~o", "ar", "er",
    "ir", "as", "adas", "idas", "ias", "arias", "erias", "irias", "arás", "aras", "erás", "eras",
    "irás", "avas", "es", "ardes", "erdes", "irdes", "ares", "eres", "ires", "asses", "esses",
    "isses", "astes", "estes", "istes", "is", "ais", "eis", "íeis", "aríeis", "eríeis", "iríeis",
    "áreis", "areis", "éreis", "ereis", "íreis", "ireis", "ásseis", "ésseis", "ísseis", "áveis",
    "ados", "idos", "ámos", "amos", "íamos", "aríamos", "eríamos", "iríamos", "áramos", "éramos",
    "íramos", "ávamos", "emos", "aremos", "eremos", "iremos", "ássemos", "êssemos", "íssemos",
    "imos", "armos", "ermos", "irmos", "eu", "iu", "ou", "ira", "iras",
];

/// Whether `word` (lower case) is on Snowball's Portuguese stop word list.
pub fn is_stop_word(word: &str) -> bool {
    STOP_WORDS.contains(&word)
}

fn is_vowel(letter: char) -> bool {
    matches!(
        letter,
        'a' | 'e' | 'i' | 'o' | 'u' | 'á' | 'é' | 'í' | 'ó' | 'ú' | 'â' | 'ê' | 'ô'
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
        "logia" | "logias" => {
            if start < r2 {
                return false;
            }
            replace_tail(letters, start, "log");
        }
        "ução" | "uções" => {
            if start < r2 {
                return false;
            }
            replace_tail(letters, start, "u");
        }
        "ência" | "ências" => {
            if start < r2 {
                return false;
            }
            replace_tail(letters, start, "ente");
        }
        "amente" => {
            if start < r1 {
                return false;
            }
            letters.truncate(start);
            if drop_if(letters, "iv") {
                drop_if(letters, "at");
            } else if !drop_if(letters, "os") && !drop_if(letters, "ic") {
                drop_if(letters, "ad");
            }
        }
        "mente" => {
            if start < r2 {
                return false;
            }
            letters.truncate(start);
            if !drop_if(letters, "ante") && !drop_if(letters, "avel") {
                drop_if(letters, "ível");
            }
        }
        "idade" | "idades" => {
            if start < r2 {
                return false;
            }
            letters.truncate(start);
            if !drop_if(letters, "abil") && !drop_if(letters, "ic") {
                drop_if(letters, "iv");
            }
        }
        "iva" | "ivo" | "ivas" | "ivos" => {
            if start < r2 {
                return false;
            }
            letters.truncate(start);
            drop_if(letters, "at");
        }
        "ira" | "iras" => {
            if start < rv || start == 0 || letters[start - 1] != 'e' {
                return false;
            }
            replace_tail(letters, start, "ir");
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

fn verb_suffix(letters: &mut Vec<char>, rv: usize) -> bool {
    if let Some((_, start)) = longest(letters, VERB_SUFFIXES)
        && start >= rv
    {
        letters.truncate(start);
        return true;
    }
    false
}

/// The stem of `word` (lower case) by Snowball's Portuguese stemmer: nasal vowels kept apart,
/// the standard, verb and residual suffixes removed over RV, R1 and R2.
pub fn stem(word: &str) -> String {
    let mut letters: Vec<char> = word.replace('ã', "a~").replace('õ', "o~").chars().collect();
    let rv = region_rv(&letters);
    let r1 = region_after(&letters, 0);
    let r2 = region_after(&letters, r1).max(r1);
    let changed = standard_suffix(&mut letters, rv, r1, r2) || verb_suffix(&mut letters, rv);
    if changed {
        if let Some(start) = ends_with(&letters, "ci")
            && start + 1 >= rv
        {
            letters.pop();
        }
    } else if let Some((_, start)) = longest(&letters, &["os", "a", "i", "o", "á", "í", "ó"])
        && start >= rv
    {
        letters.truncate(start);
    }
    if let Some((_, start)) = longest(&letters, &["e", "é", "ê"])
        && start >= rv
    {
        letters.truncate(start);
        if (ends_with(&letters, "gu").is_some() || ends_with(&letters, "ci").is_some())
            && letters.len() - 1 >= rv
        {
            letters.pop();
        }
    } else if letters.last() == Some(&'ç') {
        letters.pop();
        letters.push('c');
    }
    letters
        .into_iter()
        .collect::<String>()
        .replace("a~", "ã")
        .replace("o~", "õ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_drop_verb_and_noun_endings() {
        for (word, expected) in [
            ("falando", "fal"),
            ("meninas", "menin"),
            ("rapidamente", "rapid"),
            ("nações", "naçõ"),
        ] {
            assert_eq!(stem(word), expected, "{word}");
        }
        assert!(is_stop_word("que"));
        assert!(!is_stop_word("casa"));
    }
}
