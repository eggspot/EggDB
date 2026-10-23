use crate::cpus::{CpuSet, Cpus};
use crate::error::{Error, ErrorKind, Result};
use crate::rng::SplitMix64;
use crate::topology::CpuTopology;
use std::collections::{BTreeSet, HashMap};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::ThreadId;

/// Processors for deterministic tests (D9): a set of CPUs the test chooses, pins that are
/// recorded instead of applied, and failures injected on purpose.
///
/// Pinning follows [`OsCpus`](crate::OsCpus)' rules: an empty set or one outside
/// [`Cpus::available`] fails with [`ErrorKind::InvalidInput`], and pinning can be refused with
/// [`ErrorKind::Unsupported`], as on Apple's systems. Every pin request is numbered from zero,
/// and any of them can be made to fail with [`ErrorKind::InjectedFault`], either by number or
/// at random from a seed; an injected failure comes before any other check. The topology is
/// unknown until a test sets one with [`SimCpus::set_topology`], so engine code meets the
/// `None` answers it must handle. Clones share the same state.
#[derive(Debug, Clone)]
pub struct SimCpus {
    shared: Arc<Mutex<State>>,
}

#[derive(Debug)]
struct State {
    available: CpuSet,
    parallelism: Option<NonZeroUsize>,
    topology: CpuTopology,
    refuse_pinning: bool,
    pins: HashMap<ThreadId, CpuSet>,
    pin_requests: u64,
    failing_pins: BTreeSet<u64>,
    random_failures: Option<RandomFailures>,
}

#[derive(Debug)]
struct RandomFailures {
    rng: SplitMix64,
    per_million: u32,
}

impl SimCpus {
    /// How many CPUs a new simulation has, numbered from zero.
    pub const DEFAULT_CPUS: usize = 4;

    /// Creates a simulation with [`SimCpus::DEFAULT_CPUS`] CPUs, all available, and as many
    /// threads able to run at once.
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(State {
                available: CpuSet::first(Self::DEFAULT_CPUS).unwrap_or_default(),
                parallelism: None,
                topology: CpuTopology::unknown(),
                refuse_pinning: false,
                pins: HashMap::new(),
                pin_requests: 0,
                failing_pins: BTreeSet::new(),
                random_failures: None,
            })),
        }
    }

    /// Sets the CPUs the process may run on, as an affinity mask or a cgroup cpuset would.
    /// Fails with [`ErrorKind::InvalidInput`] for an empty set, one smaller than the
    /// parallelism set with [`SimCpus::set_parallelism`], or one with a CPU the topology set
    /// with [`SimCpus::set_topology`] does not place.
    pub fn set_available(&self, cpus: CpuSet) -> Result<()> {
        let mut state = lock(&self.shared);
        if cpus.is_empty()
            || state
                .parallelism
                .is_some_and(|count| count.get() > cpus.len())
            || !places_all(&state.topology, &cpus)
        {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        state.available = cpus;
        Ok(())
    }

    /// Sets how many threads can run at once, as a container's CPU quota would, or `None` for
    /// one per available CPU. Fails with [`ErrorKind::InvalidInput`] above the number of
    /// available CPUs.
    pub fn set_parallelism(&self, parallelism: Option<NonZeroUsize>) -> Result<()> {
        let mut state = lock(&self.shared);
        if parallelism.is_some_and(|count| count.get() > state.available.len()) {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        state.parallelism = parallelism;
        Ok(())
    }

    /// Sets the machine's topology, for example one made with
    /// [`CpuTopology::symmetric`]. Fails with [`ErrorKind::InvalidInput`] when the topology
    /// places CPUs but not every available one; a topology known only by its counts is always
    /// accepted.
    pub fn set_topology(&self, topology: CpuTopology) -> Result<()> {
        let mut state = lock(&self.shared);
        if !places_all(&topology, &state.available) {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        state.topology = topology;
        Ok(())
    }

    /// Makes pinning fail with [`ErrorKind::Unsupported`], as on a platform that cannot pin
    /// threads, or lets it succeed again.
    pub fn refuse_pinning(&self, refuse: bool) {
        lock(&self.shared).refuse_pinning = refuse;
    }

    /// Makes the pin request numbered `index` fail.
    pub fn fail_pin(&self, index: u64) {
        lock(&self.shared).failing_pins.insert(index);
    }

    /// Makes each later pin request fail with probability `failures_per_million` in a million,
    /// drawn from a generator seeded with `seed`. A rate of zero stops random failures.
    pub fn fail_randomly(&self, seed: u64, failures_per_million: u32) {
        lock(&self.shared).random_failures = (failures_per_million > 0).then(|| RandomFailures {
            rng: SplitMix64::new(seed),
            per_million: failures_per_million,
        });
    }

    /// How many pin requests have been made so far, including failed ones.
    pub fn pin_request_count(&self) -> u64 {
        lock(&self.shared).pin_requests
    }

    /// The CPUs `thread` was last pinned to, or `None` if it was never pinned.
    pub fn pinned(&self, thread: ThreadId) -> Option<CpuSet> {
        lock(&self.shared).pins.get(&thread).cloned()
    }
}

impl Default for SimCpus {
    fn default() -> Self {
        Self::new()
    }
}

impl Cpus for SimCpus {
    fn available(&self) -> CpuSet {
        lock(&self.shared).available.clone()
    }

    fn parallelism(&self) -> NonZeroUsize {
        let state = lock(&self.shared);
        state.parallelism.unwrap_or_else(|| {
            NonZeroUsize::new(state.available.len()).unwrap_or(NonZeroUsize::MIN)
        })
    }

    fn topology(&self) -> CpuTopology {
        lock(&self.shared).topology.clone()
    }

    fn pin_current_thread(&self, cpus: &CpuSet) -> Result<()> {
        let mut state = lock(&self.shared);
        if state.next_pin_fails() {
            return Err(Error::new(ErrorKind::InjectedFault));
        }
        if cpus.is_empty() || !cpus.is_subset(&state.available) {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        if state.refuse_pinning {
            return Err(Error::new(ErrorKind::Unsupported));
        }
        state.pins.insert(std::thread::current().id(), cpus.clone());
        Ok(())
    }
}

impl State {
    fn next_pin_fails(&mut self) -> bool {
        let index = self.pin_requests;
        self.pin_requests += 1;
        let chosen = self.failing_pins.remove(&index);
        let random = self
            .random_failures
            .as_mut()
            .is_some_and(|failures| failures.rng.one_in_million(failures.per_million));
        chosen || random
    }
}

fn places_all(topology: &CpuTopology, cpus: &CpuSet) -> bool {
    topology.locations().is_empty() || cpus.is_subset(&topology.cpus())
}

fn lock(shared: &Mutex<State>) -> MutexGuard<'_, State> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::SimCpus;
    use crate::{CpuSet, CpuTopology, Cpus, ErrorKind, SiblingNumbering};
    use std::num::NonZeroUsize;

    fn set(list: &str) -> CpuSet {
        CpuSet::parse_list(list).unwrap()
    }

    #[test]
    fn a_new_simulation_has_four_cpus() {
        let cpus = SimCpus::new();
        assert_eq!(cpus.available(), set("0-3"));
        assert_eq!(cpus.parallelism().get(), 4);
    }

    #[test]
    fn pins_are_recorded_per_thread() {
        let cpus = SimCpus::new();
        cpus.pin_current_thread(&set("1")).unwrap();
        let worker = cpus.clone();
        let worker_id = std::thread::spawn(move || {
            worker.pin_current_thread(&set("2-3")).unwrap();
            std::thread::current().id()
        })
        .join()
        .unwrap();
        assert_eq!(cpus.pinned(std::thread::current().id()), Some(set("1")));
        assert_eq!(cpus.pinned(worker_id), Some(set("2-3")));
        assert_eq!(cpus.pin_request_count(), 2);
    }

    #[test]
    fn the_available_set_and_parallelism_follow_the_test() {
        let cpus = SimCpus::new();
        cpus.set_available(set("8-15,32")).unwrap();
        assert_eq!(cpus.parallelism().get(), 9);
        cpus.set_parallelism(NonZeroUsize::new(2)).unwrap();
        assert_eq!(cpus.parallelism().get(), 2);
        let error = cpus.set_parallelism(NonZeroUsize::new(10)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        let error = cpus.set_available(set("0")).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        let error = cpus.pin_current_thread(&set("0")).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        cpus.pin_current_thread(&set("32")).unwrap();
        let error = cpus.set_available(CpuSet::new()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn refused_pins_change_nothing() {
        let cpus = SimCpus::new();
        cpus.refuse_pinning(true);
        let error = cpus.pin_current_thread(&set("0")).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert_eq!(cpus.pinned(std::thread::current().id()), None);
        cpus.refuse_pinning(false);
        assert!(cpus.pin_current_thread(&set("0")).is_ok());
    }

    #[test]
    fn a_chosen_pin_fails() {
        let cpus = SimCpus::new();
        cpus.fail_pin(1);
        assert!(cpus.pin_current_thread(&set("0")).is_ok());
        let error = cpus.pin_current_thread(&set("1")).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InjectedFault);
        assert_eq!(cpus.pinned(std::thread::current().id()), Some(set("0")));
    }

    #[test]
    fn random_failures_are_reproducible_from_the_seed() {
        let outcomes = |seed| {
            let cpus = SimCpus::new();
            cpus.fail_randomly(seed, 300_000);
            (0..64)
                .map(|_| cpus.pin_current_thread(&set("0")).is_ok())
                .collect::<Vec<_>>()
        };
        assert_eq!(outcomes(3), outcomes(3));
        assert!(outcomes(3).contains(&false));
        assert!(outcomes(3).contains(&true));
    }

    #[test]
    fn the_topology_is_unknown_until_a_test_sets_one() {
        let cpus = SimCpus::new();
        assert_eq!(cpus.topology(), CpuTopology::unknown());
        let two = NonZeroUsize::new(2).unwrap();
        let machine = CpuTopology::symmetric(two, two, two, SiblingNumbering::Spread).unwrap();
        cpus.set_topology(machine.clone()).unwrap();
        assert_eq!(cpus.topology(), machine);
        assert_eq!(
            machine.one_cpu_per_core(&cpus.available()),
            Some(set("0-3"))
        );
    }

    #[test]
    fn the_topology_must_place_every_available_cpu() {
        let cpus = SimCpus::new();
        cpus.set_available(set("0-5")).unwrap();
        let one = NonZeroUsize::new(1).unwrap();
        let two = NonZeroUsize::new(2).unwrap();
        let small = CpuTopology::symmetric(one, two, two, SiblingNumbering::Adjacent).unwrap();
        let error = cpus.set_topology(small.clone()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        cpus.set_available(set("1-2")).unwrap();
        cpus.set_topology(small).unwrap();
        let error = cpus.set_available(set("3-4")).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        let counts = CpuTopology::from_counts(Some(one), Some(two), None, None).unwrap();
        cpus.set_topology(counts).unwrap();
        cpus.set_available(set("3-4")).unwrap();
    }
}
