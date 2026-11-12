const STOP_WORDS: &[&str] = &[
    "acaba", "ama", "aslında", "az", "bazı", "belki", "biri", "birkaç", "birşey", "biz", "bu",
    "çok", "çünkü", "da", "daha", "de", "defa", "diye", "eğer", "en", "gibi", "hem", "hep",
    "hepsi", "her", "hiç", "için", "ile", "ise", "kez", "ki", "kim", "mı", "mu", "mü", "nasıl",
    "ne", "neden", "nerde", "nerede", "nereye", "niçin", "niye", "o", "sanki", "şey", "siz", "şu",
    "tüm", "ve", "veya", "ya", "yani",
];

const POSSESSIVES: &[&str] = &["mız", "miz", "muz", "müz", "nız", "niz", "nuz", "nüz", "m", "n"];
const HIGH_VOWELS: &[&str] = &["ı", "i", "u", "ü"];
const LOW_VOWELS: &[&str] = &["a", "e"];
const PAST_ENDINGS: &[&str] = &[
    "tım", "tim", "tum", "tüm", "dım", "dim", "dum", "düm", "tın", "tin", "tun", "tün", "dın", "din",
    "dun", "dün", "tık", "tik", "tuk", "tük", "dık", "dik", "duk", "dük", "tı", "ti", "tu", "tü", "dı",
    "di", "du", "dü",
];

/// Whether `word` (lower case) is on PostgreSQL's Turkish stop word list.
pub fn is_stop_word(word: &str) -> bool {
    STOP_WORDS.contains(&word)
}

fn is_vowel(letter: char) -> bool {
    matches!(letter, 'a' | 'e' | 'ı' | 'i' | 'o' | 'ö' | 'u' | 'ü')
}

fn is_high_vowel(letter: char) -> bool {
    matches!(letter, 'ı' | 'i' | 'u' | 'ü')
}

fn harmonic_vowels(last: char) -> &'static [char] {
    match last {
        'a' => &['a', 'ı', 'o', 'u'],
        'e' => &['e', 'i', 'ö', 'ü'],
        'ı' => &['a', 'ı'],
        'i' => &['e', 'i'],
        'o' | 'u' => &['o', 'u'],
        'ö' | 'ü' => &['ö', 'ü'],
        _ => &[],
    }
}

#[derive(Clone, Copy)]
enum Joining {
    Direct,
    OptionalN,
    OptionalS,
    OptionalY,
    OptionalHighVowel,
}

impl Joining {
    fn is_optional_letter(self, letter: char) -> bool {
        match self {
            Self::Direct => false,
            Self::OptionalN => letter == 'n',
            Self::OptionalS => letter == 's',
            Self::OptionalY => letter == 'y',
            Self::OptionalHighVowel => is_high_vowel(letter),
        }
    }

    fn may_follow(self, letter: char) -> bool {
        match self {
            Self::OptionalHighVowel => !is_vowel(letter),
            _ => is_vowel(letter),
        }
    }
}

type Step = fn(&mut Word) -> bool;

struct Word {
    letters: Vec<char>,
    cursor: usize,
    bra: usize,
    ket: usize,
    continue_with_nouns: bool,
}

impl Word {
    fn new(word: &str) -> Self {
        let letters: Vec<char> = word.chars().filter(|&letter| letter != '\u{307}').collect();
        let cursor = letters.len();
        Self { letters, cursor, bra: cursor, ket: cursor, continue_with_nouns: true }
    }

    fn back(&self, distance: usize) -> Option<char> {
        self.cursor
            .checked_sub(distance)
            .and_then(|at| self.letters.get(at))
            .copied()
    }

    fn reset(&mut self) {
        self.cursor = self.letters.len();
        self.bra = self.cursor;
        self.ket = self.cursor;
    }

    fn attempt(&mut self, step: impl FnOnce(&mut Self) -> bool) -> bool {
        let from_end = self.letters.len() - self.cursor.min(self.letters.len());
        let matched = step(self);
        if !matched {
            self.cursor = self.letters.len().saturating_sub(from_end);
        }
        matched
    }

    fn either(&mut self, steps: &[Step]) -> bool {
        steps.iter().any(|step| self.attempt(|word| step(word)))
    }

    fn optionally(&mut self, step: impl FnOnce(&mut Self) -> bool) {
        self.attempt(step);
    }

    fn open(&mut self) {
        self.ket = self.cursor;
    }

    fn close_and_delete(&mut self) {
        self.bra = self.cursor;
        let (bra, ket) = (self.bra.min(self.ket), self.ket.min(self.letters.len()));
        if bra < ket {
            self.letters.drain(bra..ket);
            if self.cursor >= ket {
                self.cursor -= ket - bra;
            } else if self.cursor > bra {
                self.cursor = bra;
            }
        }
        self.ket = self.bra;
    }

    fn ends_with_at_cursor(&self, suffix: &str) -> Option<usize> {
        let suffix: Vec<char> = suffix.chars().collect();
        let start = self.cursor.checked_sub(suffix.len())?;
        (self.letters.get(start..self.cursor)? == suffix.as_slice()).then_some(start)
    }

    fn among(&mut self, suffixes: &[&str]) -> bool {
        let longest = suffixes
            .iter()
            .filter_map(|suffix| self.ends_with_at_cursor(suffix))
            .min();
        match longest {
            Some(start) => {
                self.cursor = start;
                true
            }
            None => false,
        }
    }

    fn vowel_harmony(&self) -> bool {
        let Some(last) = (0..self.cursor)
            .rev()
            .find(|&at| self.letters.get(at).copied().is_some_and(is_vowel))
        else {
            return false;
        };
        let group = self.letters.get(last).copied().map_or(&[][..], harmonic_vowels);
        self.letters
            .get(..last)
            .is_some_and(|earlier| earlier.iter().any(|letter| group.contains(letter)))
    }

    fn joins(&mut self, joining: Joining) -> bool {
        if matches!(joining, Joining::Direct) {
            return true;
        }
        let Some(previous) = self.back(1) else {
            return false;
        };
        if !self.back(2).is_some_and(|letter| joining.may_follow(letter)) {
            return false;
        }
        if joining.is_optional_letter(previous) {
            self.cursor -= 1;
        }
        true
    }

    fn mark(&mut self, harmony: bool, suffixes: &[&str], joining: Joining) -> bool {
        self.attempt(|word| (!harmony || word.vowel_harmony()) && word.among(suffixes) && word.joins(joining))
    }

    fn possessives(&mut self) -> bool {
        self.mark(false, POSSESSIVES, Joining::OptionalHighVowel)
    }

    fn su(&mut self) -> bool {
        self.mark(true, HIGH_VOWELS, Joining::OptionalS)
    }

    fn lari(&mut self) -> bool {
        self.mark(false, &["leri", "ları"], Joining::Direct)
    }

    fn yu(&mut self) -> bool {
        self.mark(true, HIGH_VOWELS, Joining::OptionalY)
    }

    fn nu(&mut self) -> bool {
        self.mark(true, &["nı", "ni", "nu", "nü"], Joining::Direct)
    }

    fn nun(&mut self) -> bool {
        self.mark(true, &["ın", "in", "un", "ün"], Joining::OptionalN)
    }

    fn ya(&mut self) -> bool {
        self.mark(true, LOW_VOWELS, Joining::OptionalY)
    }

    fn na(&mut self) -> bool {
        self.mark(true, &["na", "ne"], Joining::Direct)
    }

    fn da(&mut self) -> bool {
        self.mark(true, &["da", "de", "ta", "te"], Joining::Direct)
    }

    fn nda(&mut self) -> bool {
        self.mark(true, &["nda", "nde"], Joining::Direct)
    }

    fn dan(&mut self) -> bool {
        self.mark(true, &["dan", "den", "tan", "ten"], Joining::Direct)
    }

    fn ndan(&mut self) -> bool {
        self.mark(true, &["ndan", "nden"], Joining::Direct)
    }

    fn yla(&mut self) -> bool {
        self.mark(true, &["la", "le"], Joining::OptionalY)
    }

    fn ki(&mut self) -> bool {
        self.mark(false, &["ki"], Joining::Direct)
    }

    fn nca(&mut self) -> bool {
        self.mark(true, &["ca", "ce"], Joining::OptionalN)
    }

    fn yum(&mut self) -> bool {
        self.mark(true, &["ım", "im", "um", "üm"], Joining::OptionalY)
    }

    fn sun(&mut self) -> bool {
        self.mark(true, &["sın", "sin", "sun", "sün"], Joining::Direct)
    }

    fn yuz(&mut self) -> bool {
        self.mark(true, &["ız", "iz", "uz", "üz"], Joining::OptionalY)
    }

    fn sunuz(&mut self) -> bool {
        self.mark(false, &["sınız", "siniz", "sunuz", "sünüz"], Joining::Direct)
    }

    fn lar(&mut self) -> bool {
        self.mark(true, &["ler", "lar"], Joining::Direct)
    }

    fn nuz(&mut self) -> bool {
        self.mark(true, &["nız", "niz", "nuz", "nüz"], Joining::Direct)
    }

    fn dur(&mut self) -> bool {
        self.mark(true, &["tır", "tir", "tur", "tür", "dır", "dir", "dur", "dür"], Joining::Direct)
    }

    fn casina(&mut self) -> bool {
        self.mark(false, &["casına", "cesine"], Joining::Direct)
    }

    fn ydu(&mut self) -> bool {
        self.mark(true, PAST_ENDINGS, Joining::OptionalY)
    }

    fn ysa(&mut self) -> bool {
        self.mark(false, &["sam", "san", "sak", "sem", "sen", "sek", "sa", "se"], Joining::OptionalY)
    }

    fn ymus(&mut self) -> bool {
        self.mark(true, &["mış", "miş", "muş", "müş"], Joining::OptionalY)
    }

    fn yken(&mut self) -> bool {
        self.mark(false, &["ken"], Joining::OptionalY)
    }

    fn person(&mut self) -> bool {
        self.either(&[Self::sunuz, Self::lar, Self::yum, Self::sun, Self::yuz]);
        true
    }

    fn remove(&mut self, marks: &[Step]) -> bool {
        self.open();
        self.either(marks) && {
            self.close_and_delete();
            true
        }
    }

    fn deleted(&mut self, mark: Step) -> bool {
        mark(self) && {
            self.close_and_delete();
            true
        }
    }

    fn plural_then_chain(&mut self) {
        self.optionally(|word| word.remove(&[Self::lar]) && word.chain_before_ki());
    }

    fn possessive_then_plural(&mut self) -> bool {
        self.remove(&[Self::possessives, Self::su]) && {
            self.plural_then_chain();
            true
        }
    }

    fn chain_before_ki(&mut self) -> bool {
        self.attempt(|word| {
            word.open();
            word.ki()
                && word.either(&[
                    |word| {
                        word.deleted(Self::da) && {
                            word.optionally(|word| {
                                word.open();
                                word.either(&[
                                    |word| {
                                        word.deleted(Self::lar) && {
                                            word.optionally(Self::chain_before_ki);
                                            true
                                        }
                                    },
                                    |word| {
                                        word.deleted(Self::possessives) && {
                                            word.plural_then_chain();
                                            true
                                        }
                                    },
                                ])
                            });
                            true
                        }
                    },
                    |word| {
                        word.deleted(Self::nun) && {
                            word.optionally(|word| {
                                word.either(&[
                                    |word| word.remove(&[Self::lari]),
                                    Self::possessive_then_plural,
                                    Self::chain_before_ki,
                                ])
                            });
                            true
                        }
                    },
                    |word| {
                        word.nda()
                            && word.either(&[
                                |word| word.deleted(Self::lari),
                                |word| {
                                    word.deleted(Self::su) && {
                                        word.plural_then_chain();
                                        true
                                    }
                                },
                                Self::chain_before_ki,
                            ])
                    },
                ])
        })
    }

    fn nominal_verb_suffixes(&mut self) -> bool {
        self.open();
        self.continue_with_nouns = true;
        let matched = self.either(&[
            |word| word.either(&[Self::ymus, Self::ydu, Self::ysa, Self::yken]),
            |word| word.casina() && word.person() && word.ymus(),
            |word| {
                word.deleted(Self::lar) && {
                    word.optionally(|word| {
                        word.open();
                        word.either(&[Self::dur, Self::ydu, Self::ysa, Self::ymus])
                    });
                    word.continue_with_nouns = false;
                    true
                }
            },
            |word| word.nuz() && word.either(&[Self::ydu, Self::ysa]),
            |word| {
                word.either(&[Self::sunuz, Self::yuz, Self::sun, Self::yum]) && {
                    word.close_and_delete();
                    word.optionally(|word| {
                        word.open();
                        word.ymus()
                    });
                    true
                }
            },
            |word| {
                word.deleted(Self::dur) && {
                    word.optionally(|word| {
                        word.open();
                        word.person() && word.ymus()
                    });
                    true
                }
            },
        ]);
        if matched {
            self.close_and_delete();
        }
        matched
    }

    fn noun_suffixes(&mut self) -> bool {
        self.either(&[
            |word| {
                word.remove(&[Self::lar]) && {
                    word.optionally(Self::chain_before_ki);
                    true
                }
            },
            |word| {
                word.remove(&[Self::nca]) && {
                    word.optionally(|word| {
                        word.either(&[
                            |word| word.remove(&[Self::lari]),
                            Self::possessive_then_plural,
                            |word| word.remove(&[Self::lar]) && word.chain_before_ki(),
                        ])
                    });
                    true
                }
            },
            |word| {
                word.open();
                word.either(&[Self::nda, Self::na])
                    && word.either(&[
                        |word| word.deleted(Self::lari),
                        |word| {
                            word.deleted(Self::su) && {
                                word.plural_then_chain();
                                true
                            }
                        },
                        Self::chain_before_ki,
                    ])
            },
            |word| {
                word.open();
                word.either(&[Self::ndan, Self::nu])
                    && word.either(&[
                        |word| {
                            word.deleted(Self::su) && {
                                word.plural_then_chain();
                                true
                            }
                        },
                        Self::lari,
                    ])
            },
            |word| {
                word.remove(&[Self::dan]) && {
                    word.optionally(|word| {
                        word.open();
                        word.either(&[
                            |word| {
                                word.deleted(Self::possessives) && {
                                    word.plural_then_chain();
                                    true
                                }
                            },
                            |word| {
                                word.deleted(Self::lar) && {
                                    word.optionally(Self::chain_before_ki);
                                    true
                                }
                            },
                            Self::chain_before_ki,
                        ])
                    });
                    true
                }
            },
            |word| {
                word.remove(&[Self::nun, Self::yla]) && {
                    word.optionally(|word| {
                        word.either(&[
                            |word| word.remove(&[Self::lar]) && word.chain_before_ki(),
                            Self::possessive_then_plural,
                            Self::chain_before_ki,
                        ])
                    });
                    true
                }
            },
            |word| word.remove(&[Self::lari]),
            Self::chain_before_ki,
            |word| {
                word.remove(&[Self::da, Self::yu, Self::ya]) && {
                    word.optionally(|word| {
                        word.either(&[
                            |word| {
                                word.remove(&[Self::possessives]) && {
                                    word.optionally(|word| word.remove(&[Self::lar]));
                                    true
                                }
                            },
                            |word| word.remove(&[Self::lar]),
                        ]) && {
                            word.optionally(Self::chain_before_ki);
                            true
                        }
                    });
                    true
                }
            },
            Self::possessive_then_plural,
        ])
    }

    fn append_high_vowel_after_d_or_g(&mut self) {
        if !matches!(self.letters.last(), Some('d' | 'g')) {
            return;
        }
        let appended = match self.letters.iter().rev().copied().find(|&letter| is_vowel(letter)) {
            Some('a' | 'ı') => 'ı',
            Some('e' | 'i') => 'i',
            Some('o' | 'u') => 'u',
            Some('ö' | 'ü') => 'ü',
            _ => return,
        };
        self.letters.push(appended);
    }

    fn devoice_last_consonant(&mut self) {
        if let Some(last) = self.letters.last_mut() {
            *last = match *last {
                'b' => 'p',
                'c' => 'ç',
                'd' => 't',
                'ğ' => 'k',
                other => other,
            };
        }
    }
}

fn is_reserved(letters: &[char]) -> bool {
    matches!(letters, ['a', 'd'] | ['s', 'o', 'y', 'a', 'd'])
}

/// The stem of `word` (lower case) by Snowball's Turkish stemmer: a word of one syllable is
/// kept; otherwise the nominal verb suffixes (person, tense and copula endings) are removed,
/// then, unless a verb plural ended the word, the noun suffixes (case, possessive, plural and
/// the relative `ki` chain), each checked against vowel harmony and its joining consonant;
/// the stem's final `b`, `c`, `d` and `ğ` then read as `p`, `ç`, `t` and `k`.
pub fn stem(word: &str) -> String {
    let mut word = Word::new(word);
    if word.letters.iter().filter(|&&letter| is_vowel(letter)).count() < 2 {
        return word.letters.into_iter().collect();
    }
    word.nominal_verb_suffixes();
    word.reset();
    if word.continue_with_nouns {
        word.noun_suffixes();
        word.reset();
    }
    if !is_reserved(&word.letters) {
        word.append_high_vowel_after_d_or_g();
        word.devoice_last_consonant();
    }
    word.letters.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_drop_plural_case_and_verb_suffixes() {
        for (word, expected) in [("kitaplar", "kitap"), ("evlerde", "ev"), ("kitap", "kitap")] {
            assert_eq!(stem(word), expected, "{word}");
        }
        assert!(is_stop_word("ve"));
        assert!(!is_stop_word("kitap"));
    }
}
