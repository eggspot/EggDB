use crate::format::NO_PAGE;
use std::cell::Cell;

const FIBONACCI_MULTIPLIER: u64 = 0x9E37_79B9_7F4A_7C15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    page: u64,
    frame: u32,
}

const EMPTY: Slot = Slot {
    page: NO_PAGE,
    frame: 0,
};

#[derive(Debug)]
pub(crate) struct PageTable {
    slots: Box<[Cell<Slot>]>,
    shift: u32,
    len: Cell<usize>,
}

impl PageTable {
    pub(crate) fn for_frames(frames: usize) -> Option<Self> {
        let capacity = frames.checked_mul(2)?.checked_next_power_of_two()?.max(2);
        Some(Self {
            slots: (0..capacity).map(|_| Cell::new(EMPTY)).collect(),
            shift: u64::BITS - capacity.trailing_zeros(),
            len: Cell::new(0),
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.len.get()
    }

    pub(crate) fn get(&self, page: u64) -> Option<u32> {
        self.position(page)
            .map(|position| self.slots[position].get().frame)
    }

    pub(crate) fn insert(&self, page: u64, frame: u32) {
        let mut position = self.home(page);
        for _ in 0..self.slots.len() {
            if self.slots[position].get().page == NO_PAGE {
                self.slots[position].set(Slot { page, frame });
                self.len.set(self.len.get() + 1);
                return;
            }
            position = self.next(position);
        }
    }

    pub(crate) fn remove(&self, page: u64) -> Option<u32> {
        let mut hole = self.position(page)?;
        let frame = self.slots[hole].get().frame;
        let mut position = hole;
        loop {
            position = self.next(position);
            let slot = self.slots[position].get();
            if slot.page == NO_PAGE || position == hole {
                break;
            }
            if !cyclically_within(hole, self.home(slot.page), position) {
                self.slots[hole].set(slot);
                hole = position;
            }
        }
        self.slots[hole].set(EMPTY);
        self.len.set(self.len.get() - 1);
        Some(frame)
    }

    fn position(&self, page: u64) -> Option<usize> {
        let mut position = self.home(page);
        for _ in 0..self.slots.len() {
            let slot = self.slots[position].get();
            if slot.page == NO_PAGE {
                return None;
            }
            if slot.page == page {
                return Some(position);
            }
            position = self.next(position);
        }
        None
    }

    fn home(&self, page: u64) -> usize {
        (page.wrapping_mul(FIBONACCI_MULTIPLIER) >> self.shift) as usize
    }

    fn next(&self, position: usize) -> usize {
        (position + 1) & (self.slots.len() - 1)
    }
}

fn cyclically_within(hole: usize, home: usize, position: usize) -> bool {
    if hole <= position {
        hole < home && home <= position
    } else {
        hole < home || home <= position
    }
}

#[cfg(test)]
mod tests {
    use super::PageTable;
    use crate::test_rng::TestRng;
    use std::collections::BTreeMap;

    #[test]
    fn matches_a_map_under_random_inserts_and_removes() {
        let frames = 64;
        let table = PageTable::for_frames(frames).unwrap();
        let mut model = BTreeMap::new();
        let mut rng = TestRng::new(0x7AB1E);
        for step in 0..200_000u32 {
            let page = 2 + rng.below(300) as u64;
            if model.contains_key(&page) {
                assert_eq!(table.remove(page), model.remove(&page), "step {step}");
            } else if model.len() < frames {
                table.insert(page, step);
                model.insert(page, step);
            }
            let probe = 2 + rng.below(300) as u64;
            assert_eq!(table.get(probe), model.get(&probe).copied(), "step {step}");
            assert_eq!(table.len(), model.len());
        }
    }

    #[test]
    fn page_zero_is_never_found() {
        let table = PageTable::for_frames(1).unwrap();
        assert_eq!(table.get(0), None);
        table.insert(5, 0);
        assert_eq!(table.get(0), None);
        assert_eq!(table.remove(0), None);
    }

    #[test]
    fn a_size_that_overflows_is_refused() {
        assert!(PageTable::for_frames(usize::MAX).is_none());
    }
}
