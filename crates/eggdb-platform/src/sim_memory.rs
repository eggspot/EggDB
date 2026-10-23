use crate::error::{Error, ErrorKind, Result};
use crate::memory::{Memory, MemoryRegion, Pages, round_up};
use crate::rng::SplitMix64;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// Memory for deterministic tests (D9): heap allocations that behave like the operating
/// system's, under a byte budget, with failures injected on purpose.
///
/// Every allocation is numbered from zero, and any of them can be made to fail with
/// [`ErrorKind::InjectedFault`], either by number or at random from a seed. An allocation that
/// would take the live bytes over the budget fails with [`ErrorKind::OutOfMemory`]. Huge pages
/// can be switched off to test the engine's path without them. Clones share the same state.
#[derive(Debug, Clone)]
pub struct SimMemory {
    shared: Arc<Mutex<State>>,
    live_bytes: Arc<AtomicUsize>,
}

#[derive(Debug)]
struct State {
    page_size: usize,
    huge_page_size: Option<usize>,
    budget: usize,
    allocations: u64,
    failing_allocations: BTreeSet<u64>,
    random_failures: Option<RandomFailures>,
}

#[derive(Debug)]
struct RandomFailures {
    rng: SplitMix64,
    per_million: u32,
}

impl SimMemory {
    /// The normal page size of a new simulation.
    pub const DEFAULT_PAGE_SIZE: usize = 4096;

    /// The huge page size of a new simulation.
    pub const DEFAULT_HUGE_PAGE_SIZE: usize = 2 << 20;

    /// The budget of a new simulation, which also caps any later [`SimMemory::set_budget`], so
    /// fuzz targets cannot exhaust the test machine's memory.
    pub const MAX_BUDGET: usize = 1 << 30;

    /// Creates a simulation with 4 KiB pages, 2 MiB huge pages and a budget of
    /// [`SimMemory::MAX_BUDGET`].
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(State {
                page_size: Self::DEFAULT_PAGE_SIZE,
                huge_page_size: Some(Self::DEFAULT_HUGE_PAGE_SIZE),
                budget: Self::MAX_BUDGET,
                allocations: 0,
                failing_allocations: BTreeSet::new(),
                random_failures: None,
            })),
            live_bytes: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// How many bytes the regions allocated from this simulation and not yet dropped hold.
    pub fn live_bytes(&self) -> usize {
        self.live_bytes.load(Ordering::Relaxed)
    }

    /// How many allocations have been attempted so far, including failed ones.
    pub fn allocation_count(&self) -> u64 {
        lock(&self.shared).allocations
    }

    /// Makes allocations fail with [`ErrorKind::OutOfMemory`] once the live bytes would exceed
    /// `budget`, capped at [`SimMemory::MAX_BUDGET`]. Regions already allocated are kept.
    pub fn set_budget(&self, budget: usize) {
        lock(&self.shared).budget = budget.min(Self::MAX_BUDGET);
    }

    /// Sets the size of a normal page. Fails with [`ErrorKind::InvalidInput`] unless it is a
    /// power of two of at least 512 bytes.
    pub fn set_page_size(&self, page_size: usize) -> Result<()> {
        if !page_size.is_power_of_two() || page_size < 512 {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        lock(&self.shared).page_size = page_size;
        Ok(())
    }

    /// Sets the size of a huge page, or `None` for a platform without huge pages, on which
    /// [`Pages::Huge`] fails with [`ErrorKind::Unsupported`]. Fails with
    /// [`ErrorKind::InvalidInput`] unless the size is a power of two no smaller than the normal
    /// page size.
    pub fn set_huge_page_size(&self, huge_page_size: Option<usize>) -> Result<()> {
        let mut state = lock(&self.shared);
        if let Some(size) = huge_page_size
            && (!size.is_power_of_two() || size < state.page_size)
        {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        state.huge_page_size = huge_page_size;
        Ok(())
    }

    /// Makes the allocation numbered `index` fail.
    pub fn fail_allocation(&self, index: u64) {
        lock(&self.shared).failing_allocations.insert(index);
    }

    /// Makes each later allocation fail with probability `failures_per_million` in a million,
    /// drawn from a generator seeded with `seed`. A rate of zero stops random failures.
    pub fn fail_randomly(&self, seed: u64, failures_per_million: u32) {
        lock(&self.shared).random_failures = (failures_per_million > 0).then(|| RandomFailures {
            rng: SplitMix64::new(seed),
            per_million: failures_per_million,
        });
    }
}

impl Default for SimMemory {
    fn default() -> Self {
        Self::new()
    }
}

impl Memory for SimMemory {
    fn page_size(&self) -> usize {
        lock(&self.shared).page_size
    }

    fn huge_page_size(&self) -> Option<usize> {
        lock(&self.shared).huge_page_size
    }

    fn allocate(&self, len: usize, pages: Pages) -> Result<MemoryRegion> {
        let mut state = lock(&self.shared);
        if state.next_allocation_fails() {
            return Err(Error::new(ErrorKind::InjectedFault));
        }
        let granularity = match pages {
            Pages::Normal => state.page_size,
            Pages::Huge => state
                .huge_page_size
                .ok_or_else(|| Error::new(ErrorKind::Unsupported))?,
        };
        let len = round_up(len, granularity).ok_or_else(|| Error::new(ErrorKind::InvalidInput))?;
        let live = self.live_bytes();
        if live
            .checked_add(len)
            .is_none_or(|total| total > state.budget)
        {
            return Err(Error::new(ErrorKind::OutOfMemory));
        }
        MemoryRegion::heap(len, granularity, pages, Some(Arc::clone(&self.live_bytes)))
    }
}

impl State {
    fn next_allocation_fails(&mut self) -> bool {
        let index = self.allocations;
        self.allocations += 1;
        let chosen = self.failing_allocations.remove(&index);
        let random = self
            .random_failures
            .as_mut()
            .is_some_and(|failures| failures.rng.one_in_million(failures.per_million));
        chosen || random
    }
}

fn lock(shared: &Mutex<State>) -> MutexGuard<'_, State> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::SimMemory;
    use crate::{ErrorKind, Memory, Pages};

    #[test]
    fn allocations_round_up_to_whole_pages() {
        let memory = SimMemory::new();
        let region = memory.allocate(1, Pages::Normal).unwrap();
        assert_eq!(region.len(), SimMemory::DEFAULT_PAGE_SIZE);
        let huge = memory.allocate(1, Pages::Huge).unwrap();
        assert_eq!(huge.len(), SimMemory::DEFAULT_HUGE_PAGE_SIZE);
        assert!(
            huge.as_ptr()
                .addr()
                .is_multiple_of(SimMemory::DEFAULT_HUGE_PAGE_SIZE)
        );
    }

    #[test]
    fn live_bytes_follow_regions_until_they_are_dropped() {
        let memory = SimMemory::new();
        let first = memory.allocate(4096, Pages::Normal).unwrap();
        let second = memory.allocate(8192, Pages::Normal).unwrap();
        assert_eq!(memory.live_bytes(), 12288);
        drop(first);
        assert_eq!(memory.live_bytes(), 8192);
        drop(second);
        assert_eq!(memory.live_bytes(), 0);
    }

    #[test]
    fn the_budget_turns_allocations_into_out_of_memory() {
        let memory = SimMemory::new();
        memory.set_budget(8192);
        let kept = memory.allocate(8192, Pages::Normal).unwrap();
        let error = memory.allocate(1, Pages::Normal).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::OutOfMemory);
        drop(kept);
        assert!(memory.allocate(8192, Pages::Normal).is_ok());
    }

    #[test]
    fn the_budget_cannot_exceed_the_cap() {
        let memory = SimMemory::new();
        memory.set_budget(usize::MAX);
        let error = memory
            .allocate(SimMemory::MAX_BUDGET + 1, Pages::Normal)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::OutOfMemory);
    }

    #[test]
    fn huge_pages_can_be_switched_off() {
        let memory = SimMemory::new();
        memory.set_huge_page_size(None).unwrap();
        assert_eq!(memory.huge_page_size(), None);
        let error = memory.allocate(1, Pages::Huge).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(memory.allocate(1, Pages::Normal).is_ok());
    }

    #[test]
    fn page_sizes_must_be_powers_of_two() {
        let memory = SimMemory::new();
        for size in [0, 100, 3000, 256] {
            let error = memory.set_page_size(size).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
        }
        memory.set_page_size(16384).unwrap();
        assert_eq!(memory.allocate(1, Pages::Normal).unwrap().len(), 16384);
        let error = memory.set_huge_page_size(Some(8192)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn a_chosen_allocation_fails() {
        let memory = SimMemory::new();
        memory.fail_allocation(1);
        assert!(memory.allocate(1, Pages::Normal).is_ok());
        let error = memory.allocate(1, Pages::Normal).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InjectedFault);
        assert!(memory.allocate(1, Pages::Normal).is_ok());
        assert_eq!(memory.allocation_count(), 3);
        assert_eq!(memory.live_bytes(), 0);
    }

    #[test]
    fn random_failures_are_reproducible_from_the_seed() {
        let outcomes = |seed| {
            let memory = SimMemory::new();
            memory.fail_randomly(seed, 300_000);
            (0..64)
                .map(|_| memory.allocate(1, Pages::Normal).is_ok())
                .collect::<Vec<_>>()
        };
        assert_eq!(outcomes(9), outcomes(9));
        assert!(outcomes(9).contains(&false));
        assert!(outcomes(9).contains(&true));
    }

    #[test]
    fn zero_and_unroundable_lengths_are_invalid() {
        let memory = SimMemory::new();
        for len in [0, usize::MAX] {
            let error = memory.allocate(len, Pages::Normal).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
        }
    }
}
