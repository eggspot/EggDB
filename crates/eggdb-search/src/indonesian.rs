const FIRST_ORDER_DI_MENG: u8 = 1;
const SECOND_ORDER_PE: u8 = 2;
const FIRST_ORDER_KE_PENG: u8 = 3;
const SECOND_ORDER_BER: u8 = 4;

fn is_vowel(letter: char) -> bool {
    matches!(letter, 'a' | 'e' | 'i' | 'o' | 'u')
}

struct Word {
    letters: Vec<char>,
    measure: usize,
    prefix: u8,
}

impl Word {
    fn ends_with(&self, suffix: &str) -> bool {
        let suffix: Vec<char> = suffix.chars().collect();
        self.letters.ends_with(&suffix)
    }

    fn starts_with(&self, prefix: &str) -> bool {
        let prefix: Vec<char> = prefix.chars().collect();
        self.letters.starts_with(&prefix)
    }

    fn drop_end(&mut self, count: usize) {
        let keep = self.letters.len().saturating_sub(count);
        self.letters.truncate(keep);
        self.measure = self.measure.saturating_sub(1);
    }

    fn replace_start(&mut self, count: usize, with: &str) {
        let mut letters: Vec<char> = with.chars().collect();
        letters.extend_from_slice(&self.letters[count.min(self.letters.len())..]);
        self.letters = letters;
        self.measure = self.measure.saturating_sub(1);
    }

    fn remove_particle(&mut self) {
        if let Some(particle) = ["kah", "lah", "pun"].iter().find(|particle| self.ends_with(particle)) {
            self.drop_end(particle.len());
        }
    }

    fn remove_possessive_pronoun(&mut self) {
        if let Some(pronoun) = ["nya", "ku", "mu"].iter().find(|pronoun| self.ends_with(pronoun)) {
            self.drop_end(pronoun.chars().count());
        }
    }

    fn remove_suffix(&mut self) {
        if self.ends_with("kan") && self.prefix != FIRST_ORDER_KE_PENG && self.prefix != SECOND_ORDER_PE {
            self.drop_end(3);
        } else if self.ends_with("an") && self.prefix != FIRST_ORDER_DI_MENG {
            self.drop_end(2);
        } else if self.ends_with("i")
            && self.prefix <= SECOND_ORDER_PE
            && self.letters.len() >= 2
            && self.letters[self.letters.len() - 2] != 's'
        {
            self.drop_end(1);
        }
    }

    fn vowel_at(&self, at: usize) -> bool {
        self.letters.get(at).is_some_and(|&letter| is_vowel(letter))
    }

    fn remove_first_order_prefix(&mut self) -> bool {
        let (count, prefix, replacement) = if self.starts_with("meny") && self.vowel_at(4) {
            (4, FIRST_ORDER_DI_MENG, "s")
        } else if self.starts_with("peny") && self.vowel_at(4) {
            (4, FIRST_ORDER_KE_PENG, "s")
        } else if self.starts_with("meng") {
            (4, FIRST_ORDER_DI_MENG, "")
        } else if self.starts_with("peng") {
            (4, FIRST_ORDER_KE_PENG, "")
        } else if self.starts_with("mem") {
            (3, FIRST_ORDER_DI_MENG, if self.vowel_at(3) { "p" } else { "" })
        } else if self.starts_with("pem") {
            (3, FIRST_ORDER_KE_PENG, if self.vowel_at(3) { "p" } else { "" })
        } else if self.starts_with("men") {
            (3, FIRST_ORDER_DI_MENG, "")
        } else if self.starts_with("pen") {
            (3, FIRST_ORDER_KE_PENG, "")
        } else if self.starts_with("ter") {
            (3, FIRST_ORDER_DI_MENG, "")
        } else if self.starts_with("di") || self.starts_with("me") {
            (2, FIRST_ORDER_DI_MENG, "")
        } else if self.starts_with("ke") {
            (2, FIRST_ORDER_KE_PENG, "")
        } else {
            return false;
        };
        self.prefix = prefix;
        self.replace_start(count, replacement);
        true
    }

    fn remove_second_order_prefix(&mut self) -> bool {
        if self.starts_with("pelajar") {
            self.replace_start(7, "ajar");
        } else if self.starts_with("belajar") {
            self.prefix = SECOND_ORDER_BER;
            self.replace_start(7, "ajar");
        } else if self.starts_with("per") {
            self.prefix = SECOND_ORDER_PE;
            self.replace_start(3, "");
        } else if self.starts_with("ber") {
            self.prefix = SECOND_ORDER_BER;
            self.replace_start(3, "");
        } else if self.starts_with("pe") {
            self.prefix = SECOND_ORDER_PE;
            self.replace_start(2, "");
        } else if self.starts_with("be")
            && self.letters.get(2).is_some_and(|&letter| !is_vowel(letter))
            && self.letters.get(3..5) == Some(&['e', 'r'][..])
        {
            self.prefix = SECOND_ORDER_BER;
            self.replace_start(2, "");
        } else {
            return false;
        }
        true
    }
}

/// The stem of `word` (lower case) by Snowball's Indonesian stemmer (after Tala): particles
/// (`-kah`, `-lah`, `-pun`), possessive pronouns (`-ku`, `-mu`, `-nya`), first-order prefixes
/// (`di-`, `ke-`, `me(N)-`, `pe(N)-`, `ter-`) with their sound changes, the suffixes `-kan`, `-an`
/// and `-i` as the prefix allows, and second-order prefixes (`ber-`, `per-`, `pe-`, `be-`), each only
/// while the word keeps more than two vowels.
pub fn stem(word: &str) -> String {
    let letters: Vec<char> = word.chars().collect();
    let measure = letters.iter().filter(|&&letter| is_vowel(letter)).count();
    let mut word = Word {
        letters,
        measure,
        prefix: 0,
    };
    if word.measure <= 2 {
        return word.letters.into_iter().collect();
    }
    word.remove_particle();
    if word.measure > 2 {
        word.remove_possessive_pronoun();
    }
    if word.measure > 2 {
        if word.remove_first_order_prefix() {
            if word.measure > 2 {
                word.remove_suffix();
                if word.measure > 2 {
                    word.remove_second_order_prefix();
                }
            }
        } else {
            word.remove_second_order_prefix();
            if word.measure > 2 {
                word.remove_suffix();
            }
        }
    }
    word.letters.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_drop_affixes_while_more_than_two_vowels_stay() {
        for (word, expected) in [
            ("bukunya", "buku"),
            ("membaca", "baca"),
            ("menyapu", "sapu"),
            ("permainan", "main"),
            ("makan", "makan"),
        ] {
            assert_eq!(stem(word), expected, "{word}");
        }
    }
}
