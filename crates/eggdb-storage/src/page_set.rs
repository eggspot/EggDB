use crate::error::StorageError;
use eggdb_platform::{Error, ErrorKind};

const BITS_PER_WORD: u64 = 64;

pub(crate) struct PageSet {
    words: Vec<u64>,
    page_count: u64,
}

impl PageSet {
    pub(crate) fn for_pages(page_count: u64) -> Result<Self, StorageError> {
        let out_of_memory = || StorageError::Io(Error::new(ErrorKind::OutOfMemory));
        let word_count =
            usize::try_from(page_count.div_ceil(BITS_PER_WORD)).map_err(|_| out_of_memory())?;
        let mut words = Vec::new();
        words
            .try_reserve_exact(word_count)
            .map_err(|_| out_of_memory())?;
        words.resize(word_count, 0);
        Ok(Self { words, page_count })
    }

    pub(crate) fn insert(&mut self, page: u64) -> Result<bool, StorageError> {
        if page >= self.page_count {
            return Err(StorageError::PageOutOfRange {
                page,
                page_count: self.page_count,
            });
        }
        let (word, bit) = Self::position(page);
        let was_present = self.words[word] & bit != 0;
        self.words[word] |= bit;
        Ok(!was_present)
    }

    pub(crate) fn contains(&self, page: u64) -> bool {
        if page >= self.page_count {
            return false;
        }
        let (word, bit) = Self::position(page);
        self.words[word] & bit != 0
    }

    fn position(page: u64) -> (usize, u64) {
        ((page / BITS_PER_WORD) as usize, 1 << (page % BITS_PER_WORD))
    }
}

#[cfg(test)]
mod tests {
    use super::PageSet;
    use crate::error::StorageError;

    #[test]
    fn a_page_is_inserted_once_and_pages_past_the_end_are_refused() {
        let mut set = PageSet::for_pages(130).unwrap();
        assert!(!set.contains(64));
        assert!(set.insert(64).unwrap());
        assert!(!set.insert(64).unwrap());
        assert!(set.contains(64));
        assert!(set.insert(129).unwrap());
        assert!(!set.contains(63) && !set.contains(65) && !set.contains(500));
        assert!(matches!(
            set.insert(130),
            Err(StorageError::PageOutOfRange {
                page: 130,
                page_count: 130
            })
        ));
    }
}
