use crate::vector::words;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Script {
    Spaced,
    Cjk,
    Thai,
}

fn is_cjk(character: char) -> bool {
    matches!(
        u32::from(character),
        0x1100..=0x11FF
            | 0x3040..=0x30FF
            | 0x3130..=0x318F
            | 0x31F0..=0x31FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA960..=0xA97F
            | 0xAC00..=0xD7AF
            | 0xF900..=0xFAFF
            | 0xFF66..=0xFF9F
            | 0x20000..=0x2FA1F
    )
}

fn is_thai(character: char) -> bool {
    matches!(u32::from(character), 0x0E01..=0x0E5B)
}

fn is_thai_mark(character: char) -> bool {
    matches!(u32::from(character), 0x0E31 | 0x0E34..=0x0E3A | 0x0E47..=0x0E4E)
}

fn is_prolonged_sound_mark(character: char) -> bool {
    matches!(character, '\u{30FC}' | '\u{FF70}')
}

fn script_of(character: char) -> Option<Script> {
    if is_cjk(character) || is_prolonged_sound_mark(character) {
        Some(Script::Cjk)
    } else if is_thai(character) {
        Some(Script::Thai)
    } else if character.is_alphanumeric() {
        Some(Script::Spaced)
    } else {
        None
    }
}

fn runs(text: &str) -> Vec<(Script, usize, usize)> {
    let mut runs: Vec<(Script, usize, usize)> = Vec::new();
    for (at, character) in text.char_indices() {
        let Some(script) = script_of(character) else {
            continue;
        };
        let end = at + character.len_utf8();
        match runs.last_mut() {
            Some((last, _, run_end)) if *last == script && *run_end == at => *run_end = end,
            _ => runs.push((script, at, end)),
        }
    }
    runs
}

fn units(run: &str, script: Script) -> Vec<(usize, usize)> {
    let mut units: Vec<(usize, usize)> = Vec::new();
    for (at, character) in run.char_indices() {
        let end = at + character.len_utf8();
        let joins_previous = match script {
            Script::Thai => is_thai_mark(character),
            _ => is_prolonged_sound_mark(character),
        };
        match units.last_mut() {
            Some(unit) if joins_previous => unit.1 = end,
            _ => units.push((at, end)),
        }
    }
    units
}

fn bigrams(run: &str, script: Script) -> Vec<&str> {
    let units = units(run, script);
    if units.len() < 2 {
        return vec![run];
    }
    units
        .windows(2)
        .filter_map(|pair| match pair {
            [first, second] => run.get(first.0..second.1),
            _ => None,
        })
        .collect()
}

/// The words of `text` for languages written without spaces between words: runs of Chinese,
/// Japanese and Korean characters, and runs of Thai letters (each with the vowel and tone marks
/// written over and under it), become their overlapping two-character pieces, a run of one
/// character stays whole, and other text splits into words as [`words`] splits it.
pub fn segmented_words(text: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    for (script, start, end) in runs(text) {
        let Some(run) = text.get(start..end) else {
            continue;
        };
        match script {
            Script::Spaced => pieces.extend(words(run)),
            Script::Cjk | Script::Thai => pieces.extend(bigrams(run, script)),
        }
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_without_spaces_become_overlapping_pairs() {
        assert_eq!(segmented_words("東京都 in Japan"), ["東京", "京都", "in", "Japan"]);
        assert_eq!(segmented_words("日"), ["日"]);
        assert_eq!(segmented_words("コーヒー"), ["コーヒー"]);
        assert_eq!(segmented_words("ภาษา กินข้าว"), ["ภา", "าษ", "ษา", "กิน", "นข้", "ข้า", "าว"]);
    }
}
