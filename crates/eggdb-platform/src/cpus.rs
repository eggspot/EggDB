use crate::error::{Error, ErrorKind, Result};
use std::fmt;
use std::num::NonZeroUsize;

/// The operating system's number for one logical CPU. On Windows it is the processor group
/// times 64 plus the processor's number within its group. Numbers may have gaps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CpuId(u32);

impl CpuId {
    /// The CPU numbered `index`. Fails with [`ErrorKind::InvalidInput`] at
    /// [`CpuSet::MAX_CPUS`] or above.
    pub fn new(index: u32) -> Result<Self> {
        if index as usize >= CpuSet::MAX_CPUS {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        Ok(Self(index))
    }

    /// The CPU's number.
    pub fn index(self) -> u32 {
        self.0
    }
}

/// A set of logical CPUs, written in the list format Linux uses (`0-3,8,10-11`).
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct CpuSet {
    words: Vec<u64>,
}

impl CpuSet {
    /// One more than the largest CPU number a set holds; more than any operating system
    /// supports today.
    pub const MAX_CPUS: usize = 1 << 16;

    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// The CPUs numbered `0` up to but not including `count`. Fails with
    /// [`ErrorKind::InvalidInput`] when `count` is above [`CpuSet::MAX_CPUS`].
    pub fn first(count: usize) -> Result<Self> {
        if count > Self::MAX_CPUS {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        let mut set = Self::new();
        set.insert_range(0, count);
        Ok(set)
    }

    /// Parses the list format: comma-separated CPU numbers and inclusive ranges such as
    /// `0-3,8-11`, with surrounding whitespace ignored and an empty string for the empty set.
    /// Fails with [`ErrorKind::InvalidInput`] on anything else, on a range that runs backwards,
    /// and on numbers at [`CpuSet::MAX_CPUS`] or above.
    pub fn parse_list(list: &str) -> Result<Self> {
        let mut set = Self::new();
        let list = list.trim();
        if list.is_empty() {
            return Ok(set);
        }
        for item in list.split(',') {
            let (start, end) = match item.split_once('-') {
                Some((start, end)) => (parse_index(start)?, parse_index(end)?),
                None => {
                    let index = parse_index(item)?;
                    (index, index)
                }
            };
            if start > end {
                return Err(Error::new(ErrorKind::InvalidInput));
            }
            set.insert_range(start, end + 1);
        }
        Ok(set)
    }

    /// Adds `cpu` to the set.
    pub fn insert(&mut self, cpu: CpuId) {
        let (word, bit) = position(cpu.index() as usize);
        if self.words.len() <= word {
            self.words.resize(word + 1, 0);
        }
        self.words[word] |= bit;
    }

    /// Removes `cpu` from the set.
    pub fn remove(&mut self, cpu: CpuId) {
        let (word, bit) = position(cpu.index() as usize);
        if let Some(bits) = self.words.get_mut(word) {
            *bits &= !bit;
        }
        self.trim();
    }

    /// Whether `cpu` is in the set.
    pub fn contains(&self, cpu: CpuId) -> bool {
        let (word, bit) = position(cpu.index() as usize);
        self.words.get(word).is_some_and(|bits| bits & bit != 0)
    }

    /// How many CPUs the set holds.
    pub fn len(&self) -> usize {
        self.words
            .iter()
            .map(|bits| bits.count_ones() as usize)
            .sum()
    }

    /// Whether the set holds no CPU.
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// Whether every CPU in this set is also in `other`.
    pub fn is_subset(&self, other: &CpuSet) -> bool {
        self.words.iter().enumerate().all(|(index, bits)| {
            let others = other.words.get(index).copied().unwrap_or(0);
            bits & !others == 0
        })
    }

    /// The CPUs in the set, in increasing order.
    pub fn iter(&self) -> impl Iterator<Item = CpuId> + '_ {
        self.words.iter().enumerate().flat_map(|(word, &bits)| {
            (0..u64::BITS)
                .filter(move |bit| bits & (1 << bit) != 0)
                .map(move |bit| CpuId(word as u32 * u64::BITS + bit))
        })
    }

    fn insert_range(&mut self, start: usize, end: usize) {
        if start >= end {
            return;
        }
        let (first_word, last_word) = (start / 64, (end - 1) / 64);
        if self.words.len() <= last_word {
            self.words.resize(last_word + 1, 0);
        }
        for word in first_word..=last_word {
            let low = if word == first_word { start % 64 } else { 0 };
            let high = if word == last_word {
                (end - 1) % 64
            } else {
                63
            };
            self.words[word] |= (u64::MAX >> (63 - high)) & (u64::MAX << low);
        }
    }

    fn trim(&mut self) {
        while self.words.last() == Some(&0) {
            self.words.pop();
        }
    }
}

impl FromIterator<CpuId> for CpuSet {
    fn from_iter<I: IntoIterator<Item = CpuId>>(cpus: I) -> Self {
        let mut set = Self::new();
        for cpu in cpus {
            set.insert(cpu);
        }
        set
    }
}

impl fmt::Display for CpuSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut cpus = self.iter().map(CpuId::index).peekable();
        let mut first = true;
        while let Some(start) = cpus.next() {
            let mut end = start;
            while cpus.next_if_eq(&(end + 1)).is_some() {
                end += 1;
            }
            if !first {
                f.write_str(",")?;
            }
            first = false;
            if start == end {
                write!(f, "{start}")?;
            } else {
                write!(f, "{start}-{end}")?;
            }
        }
        Ok(())
    }
}

impl fmt::Debug for CpuSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CpuSet({self})")
    }
}

fn position(index: usize) -> (usize, u64) {
    (index / 64, 1 << (index % 64))
}

fn parse_index(text: &str) -> Result<usize> {
    let invalid = || Error::new(ErrorKind::InvalidInput);
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let index = text.parse::<usize>().map_err(|_| invalid())?;
    if index >= CpuSet::MAX_CPUS {
        return Err(invalid());
    }
    Ok(index)
}

/// The processors a process runs on.
pub trait Cpus: Send + Sync {
    /// The CPUs this process may run on, as they were when the value was created: the
    /// affinity mask and cgroup cpuset on Linux, every active processor elsewhere.
    fn available(&self) -> CpuSet;

    /// How many threads can run at once, which can be fewer than [`Cpus::available`] when a
    /// container limits the process's CPU time. The engine sizes its thread-per-core shards
    /// from this.
    fn parallelism(&self) -> NonZeroUsize;

    /// Restricts the calling thread to `cpus`. Pinning is given or refused, never faked: it
    /// fails with [`ErrorKind::Unsupported`] where the platform can only hint (Apple's systems)
    /// or has no way to pin, and with [`ErrorKind::InvalidInput`] when `cpus` is empty, is not
    /// a subset of [`Cpus::available`], or spans several Windows processor groups.
    fn pin_current_thread(&self, cpus: &CpuSet) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::{CpuId, CpuSet};
    use crate::ErrorKind;
    use crate::rng::SplitMix64;

    fn cpu(index: u32) -> CpuId {
        CpuId::new(index).unwrap()
    }

    #[test]
    fn lists_parse_into_sets_and_print_back() {
        let set = CpuSet::parse_list("0-3,8,10-11\n").unwrap();
        assert_eq!(set.len(), 7);
        assert!(set.contains(cpu(2)) && set.contains(cpu(8)) && !set.contains(cpu(9)));
        assert_eq!(set.to_string(), "0-3,8,10-11");
        assert_eq!(CpuSet::parse_list("5,1,2,3").unwrap().to_string(), "1-3,5");
        assert_eq!(CpuSet::parse_list("4-4").unwrap().to_string(), "4");
    }

    #[test]
    fn empty_lists_are_empty_sets() {
        for list in ["", "\n", "  "] {
            let set = CpuSet::parse_list(list).unwrap();
            assert!(set.is_empty());
            assert_eq!(set.to_string(), "");
        }
    }

    #[test]
    fn malformed_lists_are_invalid_input() {
        for list in [
            ",",
            "1,",
            ",1",
            "1-",
            "-1",
            "3-1",
            "a",
            "1 ,2",
            "+1",
            "1-2-3",
            "65536",
            "0-65536",
            "99999999999999999999999",
        ] {
            let error = CpuSet::parse_list(list).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "{list:?}");
        }
    }

    #[test]
    fn the_largest_cpu_fits_and_the_next_does_not() {
        let last = u32::try_from(CpuSet::MAX_CPUS - 1).unwrap();
        let set = CpuSet::parse_list(&last.to_string()).unwrap();
        assert!(set.contains(cpu(last)));
        assert_eq!(
            CpuId::new(last + 1).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
        assert_eq!(
            CpuSet::first(CpuSet::MAX_CPUS + 1).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
    }

    #[test]
    fn sets_insert_remove_and_compare() {
        let mut set: CpuSet = [cpu(1), cpu(70)].into_iter().collect();
        assert_eq!(set.iter().collect::<Vec<_>>(), [cpu(1), cpu(70)]);
        let all = CpuSet::first(128).unwrap();
        assert!(set.is_subset(&all));
        assert!(!all.is_subset(&set));
        set.remove(cpu(70));
        assert_eq!(set, CpuSet::parse_list("1").unwrap());
        set.remove(cpu(1));
        assert!(set.is_empty());
        assert_eq!(set, CpuSet::new());
        assert!(CpuSet::new().is_subset(&set));
    }

    #[test]
    fn ranges_fill_whole_and_partial_words() {
        for (start, end) in [
            (0, 0),
            (0, 1),
            (3, 64),
            (63, 65),
            (0, 200),
            (64, 128),
            (100, 300),
        ] {
            let list = if end > start {
                format!("{start}-{}", end - 1)
            } else {
                String::new()
            };
            let set = CpuSet::parse_list(&list).unwrap();
            assert_eq!(set.len(), end - start, "{list}");
            let expected: Vec<_> = (start..end).map(|index| cpu(index as u32)).collect();
            assert_eq!(set.iter().collect::<Vec<_>>(), expected, "{list}");
        }
    }

    #[test]
    fn hostile_lists_parse_quickly() {
        let list = vec!["0-65535"; 10_000].join(",");
        assert_eq!(CpuSet::parse_list(&list).unwrap().len(), CpuSet::MAX_CPUS);
    }

    #[test]
    fn random_lists_never_panic_and_valid_ones_round_trip() {
        const ALPHABET: &[u8] = b"0123456789--,,, \n7";
        let mut rng = SplitMix64::new(61);
        for _ in 0..20_000 {
            let len = (rng.next_u64() % 24) as usize;
            let text: String = (0..len)
                .map(|_| char::from(ALPHABET[(rng.next_u64() % ALPHABET.len() as u64) as usize]))
                .collect();
            if let Ok(set) = CpuSet::parse_list(&text) {
                let printed = set.to_string();
                assert_eq!(CpuSet::parse_list(&printed).unwrap(), set, "{text:?}");
            }
        }
    }
}
