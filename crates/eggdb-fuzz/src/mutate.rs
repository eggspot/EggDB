const MOST_EDITS: usize = 8;
const INTERESTING: [u8; 8] = [0, 1, 0x7f, 0x80, 0xfe, 0xff, b'0', b'\n'];

/// A small xorshift generator: fast, seeded, and good enough to choose mutations.
#[derive(Debug, Clone)]
pub(crate) struct Random(u64);

impl Random {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub(crate) fn next(&mut self) -> u64 {
        let mut state = self.0;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        self.0 = state;
        state
    }

    pub(crate) fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        (self.next() % bound as u64) as usize
    }

    pub(crate) fn bytes(&mut self, count: usize) -> Vec<u8> {
        (0..count).map(|_| self.next() as u8).collect()
    }
}

/// `seed` changed by a few random edits: bit flips, byte overwrites with interesting values,
/// insertions, deletions, truncation, duplication of a slice, or a splice with another input.
pub(crate) fn mutated(random: &mut Random, seed: &[u8], corpus: &[Vec<u8>]) -> Vec<u8> {
    let mut input = seed.to_vec();
    for _ in 0..=random.below(MOST_EDITS) {
        let length = input.len();
        match random.below(7) {
            0 if length > 0 => {
                let at = random.below(length);
                input[at] ^= 1 << random.below(8);
            }
            1 if length > 0 => {
                let at = random.below(length);
                input[at] = INTERESTING[random.below(INTERESTING.len())];
            }
            2 => {
                let at = random.below(length + 1);
                let count = 1 + random.below(8);
                let inserted = random.bytes(count);
                input.splice(at..at, inserted);
            }
            3 if length > 0 => {
                let at = random.below(length);
                let end = (at + 1 + random.below(8)).min(length);
                input.drain(at..end);
            }
            4 if length > 0 => input.truncate(random.below(length)),
            5 if length > 0 => {
                let at = random.below(length);
                let end = (at + 1 + random.below(32)).min(length);
                let copy = input[at..end].to_vec();
                let to = random.below(input.len() + 1);
                input.splice(to..to, copy);
            }
            _ => {
                let other = &corpus[random.below(corpus.len())];
                if !other.is_empty() {
                    let cut = random.below(input.len() + 1);
                    let from = random.below(other.len());
                    input.truncate(cut);
                    input.extend_from_slice(&other[from..]);
                }
            }
        }
    }
    input
}
