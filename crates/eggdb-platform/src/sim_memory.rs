use crate::error::{Error, ErrorKind, Result};
use crate::memory::{Accounting, Memory, MemoryRegion, PageMode, check_len};
use crate::rng::SplitMix64;
use std::collections::BTreeSet;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// Memory for deterministic tests (D9): heap allocations that follow the operating system's
/// rules, under a byte budget, with failures injected on purpose.
///
/// Page sizes are settable, so code that assumes 4 KiB pages fails on every machine. Lengths
/// that are not whole pages fail with [`ErrorKind::InvalidInput`], as [`OsMemory`](crate::OsMemory)
/// does. An allocation that would take the live bytes over the budget, or the live huge-page
/// bytes over the huge page pool, fails with [`ErrorKind::OutOfMemory`]. Huge pages and
/// transparent huge pages can each be switched off to test the engine's path without them.
///
/// Every allocation is numbered from zero, and any of them can be made to fail with
/// [`ErrorKind::InjectedFault`], either by number or at random from a seed. An injected failure
/// comes before any other check, so it hides an invalid length too. Clones share the same state.
#[derive(Debug, Clone)]
pub struct SimMemory {
    shared: Arc<Mutex<State>>,
    accounting: Arc<Accounting>,
}

#[derive(Debug)]
struct State {
    page_size: usize,
    huge_page_size: Option<usize>,
    refuse_transparent_huge_pages: bool,
    budget: usize,
    huge_page_pool: usize,
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

    /// The budget and huge page pool of a new simulation, which also cap any later
    /// [`SimMemory::set_budget`] and [`SimMemory::set_huge_page_pool`], so fuzz targets cannot
    /// exhaust the test machine's memory.
    pub const MAX_BUDGET: usize = 1 << 30;

    /// Creates a simulation with 4 KiB pages, 2 MiB huge and transparent huge pages, and a
    /// budget and huge page pool of [`SimMemory::MAX_BUDGET`] each.
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(State {
                page_size: Self::DEFAULT_PAGE_SIZE,
                huge_page_size: Some(Self::DEFAULT_HUGE_PAGE_SIZE),
                refuse_transparent_huge_pages: false,
                budget: Self::MAX_BUDGET,
                huge_page_pool: Self::MAX_BUDGET,
                allocations: 0,
                failing_allocations: BTreeSet::new(),
                random_failures: None,
            })),
            accounting: Arc::new(Accounting::default()),
        }
    }

    /// How many bytes the regions allocated from this simulation and not yet dropped hold.
    pub fn live_bytes(&self) -> usize {
        self.accounting.live_bytes.load(Ordering::Relaxed)
    }

    /// The most bytes that were live at once.
    pub fn peak_bytes(&self) -> usize {
        self.accounting.peak_bytes.load(Ordering::Relaxed)
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

    /// Makes [`PageMode::Huge`] allocations fail with [`ErrorKind::OutOfMemory`] once the live
    /// huge-page bytes would exceed `pool`, capped at [`SimMemory::MAX_BUDGET`], as when the
    /// administrator reserved too few huge pages. A pool of zero is Linux's default.
    pub fn set_huge_page_pool(&self, pool: usize) {
        lock(&self.shared).huge_page_pool = pool.min(Self::MAX_BUDGET);
    }

    /// Sets the size of a normal page. Fails with [`ErrorKind::InvalidInput`] unless it is a
    /// power of two of at least 512 bytes and smaller than the huge page size.
    pub fn set_page_size(&self, page_size: usize) -> Result<()> {
        let mut state = lock(&self.shared);
        if !page_size.is_power_of_two()
            || page_size < 512
            || state.huge_page_size.is_some_and(|huge| huge <= page_size)
        {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        state.page_size = page_size;
        Ok(())
    }

    /// Sets the size of a huge page, or `None` for a platform without huge pages, on which both
    /// huge modes fail with [`ErrorKind::Unsupported`]. Fails with [`ErrorKind::InvalidInput`]
    /// unless the size is a power of two larger than the normal page size.
    pub fn set_huge_page_size(&self, huge_page_size: Option<usize>) -> Result<()> {
        let mut state = lock(&self.shared);
        if let Some(size) = huge_page_size
            && (!size.is_power_of_two() || size <= state.page_size)
        {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        state.huge_page_size = huge_page_size;
        Ok(())
    }

    /// Makes [`PageMode::TransparentHuge`] fail with [`ErrorKind::Unsupported`], as a kernel
    /// built without transparent huge pages does, or lets it succeed again.
    pub fn refuse_transparent_huge_pages(&self, refuse: bool) {
        lock(&self.shared).refuse_transparent_huge_pages = refuse;
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

    fn allocate(&self, len: usize, mode: PageMode) -> Result<MemoryRegion> {
        let mut state = lock(&self.shared);
        if state.next_allocation_fails() {
            return Err(Error::new(ErrorKind::InjectedFault));
        }
        let granularity = state.granularity(mode)?;
        check_len(len, granularity)?;
        let live = self.accounting.live_bytes.load(Ordering::Relaxed);
        if exceeds(live, len, state.budget) {
            return Err(Error::new(ErrorKind::OutOfMemory));
        }
        let live_huge = self.accounting.live_huge_bytes.load(Ordering::Relaxed);
        if mode == PageMode::Huge && exceeds(live_huge, len, state.huge_page_pool) {
            return Err(Error::new(ErrorKind::OutOfMemory));
        }
        MemoryRegion::heap(len, granularity, mode, Some(Arc::clone(&self.accounting)))
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

    fn granularity(&self, mode: PageMode) -> Result<usize> {
        let unsupported = || Error::new(ErrorKind::Unsupported);
        match mode {
            PageMode::Normal => Ok(self.page_size),
            PageMode::Huge => self.huge_page_size.ok_or_else(unsupported),
            PageMode::TransparentHuge if !self.refuse_transparent_huge_pages => {
                self.huge_page_size.ok_or_else(unsupported)
            }
            PageMode::TransparentHuge => Err(unsupported()),
        }
    }
}

fn exceeds(live: usize, len: usize, limit: usize) -> bool {
    live.checked_add(len).is_none_or(|total| total > limit)
}

fn lock(shared: &Mutex<State>) -> MutexGuard<'_, State> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::SimMemory;
    use crate::{ErrorKind, Memory, PageMode};

    const PAGE: usize = SimMemory::DEFAULT_PAGE_SIZE;
    const HUGE: usize = SimMemory::DEFAULT_HUGE_PAGE_SIZE;

    #[test]
    fn huge_regions_start_on_a_huge_page_boundary() {
        let memory = SimMemory::new();
        for mode in [PageMode::Huge, PageMode::TransparentHuge] {
            let region = memory.allocate(HUGE, mode).unwrap();
            assert!(region.as_ptr().addr().is_multiple_of(HUGE));
            assert_eq!(region.mode(), mode);
        }
    }

    #[test]
    fn live_and_peak_bytes_follow_the_regions() {
        let memory = SimMemory::new();
        let first = memory.allocate(PAGE, PageMode::Normal).unwrap();
        let second = memory.allocate(2 * PAGE, PageMode::Normal).unwrap();
        assert_eq!(memory.live_bytes(), 3 * PAGE);
        drop(first);
        assert_eq!(memory.live_bytes(), 2 * PAGE);
        drop(second);
        assert_eq!(memory.live_bytes(), 0);
        assert_eq!(memory.peak_bytes(), 3 * PAGE);
    }

    #[test]
    fn the_budget_turns_allocations_into_out_of_memory() {
        let memory = SimMemory::new();
        memory.set_budget(2 * PAGE);
        let kept = memory.allocate(2 * PAGE, PageMode::Normal).unwrap();
        let error = memory.allocate(PAGE, PageMode::Normal).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::OutOfMemory);
        drop(kept);
        assert!(memory.allocate(2 * PAGE, PageMode::Normal).is_ok());
    }

    #[test]
    fn the_budget_cannot_exceed_the_cap() {
        let memory = SimMemory::new();
        memory.set_budget(usize::MAX);
        let error = memory
            .allocate(SimMemory::MAX_BUDGET + PAGE, PageMode::Normal)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::OutOfMemory);
    }

    #[test]
    fn an_exhausted_huge_page_pool_is_out_of_memory_for_huge_pages_only() {
        let memory = SimMemory::new();
        memory.set_huge_page_pool(HUGE);
        let kept = memory.allocate(HUGE, PageMode::Huge).unwrap();
        let error = memory.allocate(HUGE, PageMode::Huge).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::OutOfMemory);
        assert!(memory.allocate(HUGE, PageMode::TransparentHuge).is_ok());
        assert!(memory.allocate(HUGE, PageMode::Normal).is_ok());
        drop(kept);
        assert!(memory.allocate(HUGE, PageMode::Huge).is_ok());
    }

    #[test]
    fn huge_pages_can_be_switched_off() {
        let memory = SimMemory::new();
        memory.refuse_transparent_huge_pages(true);
        let error = memory
            .allocate(HUGE, PageMode::TransparentHuge)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(memory.allocate(HUGE, PageMode::Huge).is_ok());
        memory.set_huge_page_size(None).unwrap();
        assert_eq!(memory.huge_page_size(), None);
        let error = memory.allocate(HUGE, PageMode::Huge).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(memory.allocate(PAGE, PageMode::Normal).is_ok());
    }

    #[test]
    fn page_sizes_must_be_powers_of_two_in_order() {
        let memory = SimMemory::new();
        for size in [0, 100, 3000, 256, HUGE, 2 * HUGE] {
            let error = memory.set_page_size(size).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "{size}");
        }
        memory.set_page_size(16384).unwrap();
        assert_eq!(
            memory.allocate(16384, PageMode::Normal).unwrap().len(),
            16384
        );
        let error = memory.allocate(PAGE, PageMode::Normal).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        for size in [8192, 16384, 3 << 20] {
            let error = memory.set_huge_page_size(Some(size)).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "{size}");
        }
    }

    #[test]
    fn a_chosen_allocation_fails_and_takes_nothing() {
        let memory = SimMemory::new();
        memory.fail_allocation(1);
        assert!(memory.allocate(PAGE, PageMode::Normal).is_ok());
        let error = memory.allocate(PAGE, PageMode::Normal).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InjectedFault);
        assert!(memory.allocate(PAGE, PageMode::Normal).is_ok());
        assert_eq!(memory.allocation_count(), 3);
        assert_eq!(memory.live_bytes(), 0);
    }

    #[test]
    fn random_failures_are_reproducible_from_the_seed() {
        let outcomes = |seed| {
            let memory = SimMemory::new();
            memory.fail_randomly(seed, 300_000);
            (0..64)
                .map(|_| memory.allocate(PAGE, PageMode::Normal).is_ok())
                .collect::<Vec<_>>()
        };
        assert_eq!(outcomes(9), outcomes(9));
        assert!(outcomes(9).contains(&false));
        assert!(outcomes(9).contains(&true));
    }

    #[test]
    fn clones_share_the_accounting() {
        let memory = SimMemory::new();
        let clone = memory.clone();
        let region = clone.allocate(PAGE, PageMode::Normal).unwrap();
        assert_eq!(memory.live_bytes(), PAGE);
        assert_eq!(memory.allocation_count(), 1);
        drop(region);
        assert_eq!(memory.live_bytes(), 0);
    }
}
