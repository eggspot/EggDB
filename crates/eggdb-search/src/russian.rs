const STOP_WORDS: &[&str] = &[
    "и", "в", "во", "не", "что", "он", "на", "я", "с", "со", "как", "а", "то", "все", "она", "так",
    "его", "но", "да", "ты", "к", "у", "же", "вы", "за", "бы", "по", "только", "ее", "мне", "было",
    "вот", "от", "меня", "еще", "нет", "о", "из", "ему", "теперь", "когда", "даже", "ну", "вдруг",
    "ли", "если", "уже", "или", "ни", "быть", "был", "него", "до", "вас", "нибудь", "опять", "уж",
    "вам", "ведь", "там", "потом", "себя", "ничего", "ей", "может", "они", "тут", "где", "есть",
    "надо", "ней", "для", "мы", "тебя", "их", "чем", "была", "сам", "чтоб", "без", "будто", "чего",
    "раз", "тоже", "себе", "под", "будет", "ж", "тогда", "кто", "этот", "того", "потому", "этого",
    "какой", "совсем", "ним", "здесь", "этом", "один", "почти", "мой", "тем", "чтобы", "нее",
    "сейчас", "были", "куда", "зачем", "всех", "никогда", "можно", "при", "наконец", "два", "об",
    "другой", "хоть", "после", "над", "больше", "тот", "через", "эти", "нас", "про", "всего",
    "них", "какая", "много", "разве", "три", "эту", "моя", "впрочем", "хорошо", "свою", "этой",
    "перед", "иногда", "лучше", "чуть", "том", "нельзя", "такой", "им", "более", "всегда",
    "конечно", "всю", "между",
];
const PERFECTIVE_GERUND_AFTER_A: &[&str] = &["в", "вши", "вшись"];
const PERFECTIVE_GERUND: &[&str] = &["ив", "ивши", "ившись", "ыв", "ывши", "ывшись"];
const REFLEXIVE: &[&str] = &["ся", "сь"];
const ADJECTIVE: &[&str] = &[
    "ее", "ие", "ые", "ое", "ими", "ыми", "ей", "ий", "ый", "ой", "ем", "им", "ым", "ом", "его",
    "ого", "ему", "ому", "их", "ых", "ую", "юю", "ая", "яя", "ою", "ею",
];
const PARTICIPLE_AFTER_A: &[&str] = &["ем", "нн", "вш", "ющ", "щ"];
const PARTICIPLE: &[&str] = &["ивш", "ывш", "ующ"];
const VERB_AFTER_A: &[&str] = &[
    "ла", "на", "ете", "йте", "ли", "й", "л", "ем", "н", "ло", "но", "ет", "ют", "ны", "ть", "ешь",
    "нно",
];
const VERB: &[&str] = &[
    "ила", "ыла", "ена", "ейте", "уйте", "ите", "или", "ыли", "ей", "уй", "ил", "ыл", "им", "ым",
    "ен", "ило", "ыло", "ено", "ят", "ует", "уют", "ит", "ыт", "ены", "ить", "ыть", "ишь", "ую",
    "ю",
];
const NOUN: &[&str] = &[
    "а", "ев", "ов", "ие", "ье", "е", "иями", "ями", "ами", "еи", "ии", "и", "ией", "ей", "ой",
    "ий", "й", "иям", "ям", "ием", "ем", "ам", "ом", "о", "у", "ах", "иях", "ях", "ы", "ь", "ию",
    "ью", "ю", "ия", "ья", "я",
];

/// Whether `word` (lower case) is on Snowball's Russian stop word list.
pub fn is_stop_word(word: &str) -> bool {
    STOP_WORDS.contains(&word)
}

fn is_vowel(letter: char) -> bool {
    matches!(letter, 'а' | 'е' | 'и' | 'о' | 'у' | 'ы' | 'э' | 'ю' | 'я')
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

fn longest(letters: &[char], suffixes: &[&str], from: usize) -> Option<usize> {
    suffixes
        .iter()
        .filter_map(|suffix| ends_with(letters, suffix))
        .filter(|start| *start >= from)
        .min()
}

fn after_a(letters: &[char], start: usize, rv: usize) -> bool {
    start > rv && matches!(letters[start - 1], 'а' | 'я')
}

fn grouped(letters: &[char], after_a_group: &[&str], plain_group: &[&str], rv: usize) -> Option<usize> {
    let first = longest(letters, after_a_group, rv).filter(|&start| after_a(letters, start, rv));
    let second = longest(letters, plain_group, rv);
    match (first, second) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (found, None) | (None, found) => found,
    }
}

fn first_step(letters: &mut Vec<char>, rv: usize) {
    if let Some(start) = grouped(letters, PERFECTIVE_GERUND_AFTER_A, PERFECTIVE_GERUND, rv) {
        letters.truncate(start);
        return;
    }
    if let Some(start) = longest(letters, REFLEXIVE, rv) {
        letters.truncate(start);
    }
    if let Some(start) = longest(letters, ADJECTIVE, rv) {
        letters.truncate(start);
        if let Some(participle) = grouped(letters, PARTICIPLE_AFTER_A, PARTICIPLE, rv) {
            letters.truncate(participle);
        }
        return;
    }
    if let Some(start) = grouped(letters, VERB_AFTER_A, VERB, rv) {
        letters.truncate(start);
        return;
    }
    if let Some(start) = longest(letters, NOUN, rv) {
        letters.truncate(start);
    }
}

/// The stem of `word` (lower case) by Snowball's Russian stemmer: `ё` read as `е`, the
/// perfective gerund, reflexive, adjectival, verb and noun endings removed in RV, `и`,
/// derivational and superlative endings removed, and `нн` and a final `ь` reduced.
pub fn stem(word: &str) -> String {
    let mut letters: Vec<char> = word
        .chars()
        .map(|letter| if letter == 'ё' { 'е' } else { letter })
        .collect();
    let rv = letters
        .iter()
        .position(|&letter| is_vowel(letter))
        .map_or(letters.len(), |at| at + 1);
    let r1 = region_after(&letters, 0);
    let r2 = region_after(&letters, r1).max(r1);
    first_step(&mut letters, rv);
    if letters.last() == Some(&'и') && letters.len() > rv {
        letters.pop();
    }
    if let Some(start) = longest(&letters, &["ост", "ость"], r2.max(rv)) {
        letters.truncate(start);
    }
    if let Some(start) = longest(&letters, &["ейш", "ейше"], rv) {
        letters.truncate(start);
    }
    if ends_with(&letters, "нн").is_some_and(|start| start >= rv) {
        letters.pop();
    } else if letters.last() == Some(&'ь') && letters.len() > rv {
        letters.pop();
    }
    letters.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_drop_case_and_verb_endings() {
        for (word, expected) in [
            ("красивая", "красив"),
            ("бегать", "бега"),
            ("книги", "книг"),
            ("новейший", "нов"),
        ] {
            assert_eq!(stem(word), expected, "{word}");
        }
        assert!(is_stop_word("и"));
        assert!(!is_stop_word("книга"));
    }
}
