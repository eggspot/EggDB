use crate::cpus::{CpuId, CpuSet};
use crate::error::{Error, ErrorKind, Result};
use std::collections::BTreeMap;
use std::num::NonZeroUsize;

macro_rules! topology_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u32);

        impl $name {
            /// The identifier numbered `index`.
            pub const fn new(index: u32) -> Self {
                Self(index)
            }

            /// The identifier's number.
            pub const fn index(self) -> u32 {
                self.0
            }
        }
    };
}

topology_id!(
    /// A physical core, numbered by the platform layer from zero in the order of each core's
    /// lowest CPU, so the numbers are unique across the machine and the same on every start
    /// (operating systems number cores only within a package, or not at all).
    CoreId
);
topology_id!(
    /// A processor package (socket), numbered like [`CoreId`].
    PackageId
);
topology_id!(
    /// A group of CPUs sharing one last-level cache, numbered like [`CoreId`].
    CacheId
);
topology_id!(
    /// A NUMA node, with the operating system's own number, which memory placement takes.
    NumaNodeId
);

/// Where one logical CPU sits in the machine. Anything the operating system does not report is
/// `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CpuLocation {
    cpu: CpuId,
    core: Option<CoreId>,
    package: Option<PackageId>,
    numa_node: Option<NumaNodeId>,
    last_level_cache: Option<CacheId>,
}

impl CpuLocation {
    /// A CPU whose place is not known yet.
    pub const fn new(cpu: CpuId) -> Self {
        Self {
            cpu,
            core: None,
            package: None,
            numa_node: None,
            last_level_cache: None,
        }
    }

    /// The same location on `core`.
    pub const fn with_core(self, core: CoreId) -> Self {
        Self {
            core: Some(core),
            ..self
        }
    }

    /// The same location in `package`.
    pub const fn with_package(self, package: PackageId) -> Self {
        Self {
            package: Some(package),
            ..self
        }
    }

    /// The same location on `numa_node`.
    pub const fn with_numa_node(self, numa_node: NumaNodeId) -> Self {
        Self {
            numa_node: Some(numa_node),
            ..self
        }
    }

    /// The same location sharing the last-level cache `cache`.
    pub const fn with_last_level_cache(self, cache: CacheId) -> Self {
        Self {
            last_level_cache: Some(cache),
            ..self
        }
    }

    /// The CPU.
    pub const fn cpu(&self) -> CpuId {
        self.cpu
    }

    /// The physical core the CPU is a hardware thread of.
    pub const fn core(&self) -> Option<CoreId> {
        self.core
    }

    /// The package the CPU is in.
    pub const fn package(&self) -> Option<PackageId> {
        self.package
    }

    /// The NUMA node the CPU belongs to.
    pub const fn numa_node(&self) -> Option<NumaNodeId> {
        self.numa_node
    }

    /// The group of CPUs whose last-level cache the CPU shares.
    pub const fn last_level_cache(&self) -> Option<CacheId> {
        self.last_level_cache
    }
}

/// The shape of the machine's processors: which CPUs share a core, a package, a NUMA node and a
/// last-level cache, and how long a cache line is. It describes every online CPU, not only the
/// ones the process may run on; intersect it with [`Cpus::available`](crate::Cpus::available).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CpuTopology {
    locations: Vec<CpuLocation>,
    physical_cores: Option<NonZeroUsize>,
    logical_cpus: Option<NonZeroUsize>,
    packages: Option<NonZeroUsize>,
    cache_line_size: Option<NonZeroUsize>,
}

/// How [`CpuTopology::symmetric`] numbers the hardware threads of a core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SiblingNumbering {
    /// The threads of a core have consecutive numbers (0 and 1 share a core), as Windows and
    /// most arm64 systems number them.
    Adjacent,
    /// Each core's first threads come first, then their siblings (0 and the core count share a
    /// core), as Linux numbers them on most x86 machines.
    Spread,
}

impl CpuTopology {
    /// A topology about which nothing is known.
    pub fn unknown() -> Self {
        Self::default()
    }

    /// A topology with a location for each CPU. The counts of cores, packages and CPUs come from
    /// the locations, and are `None` when any CPU lacks that fact. Fails with
    /// [`ErrorKind::InvalidInput`] when a CPU appears twice, when the CPUs of one core disagree
    /// about their package, NUMA node or last-level cache, or when `cache_line_size` is not a
    /// power of two.
    pub fn from_locations(
        mut locations: Vec<CpuLocation>,
        cache_line_size: Option<NonZeroUsize>,
    ) -> Result<Self> {
        let invalid = || Error::new(ErrorKind::InvalidInput);
        check_cache_line(cache_line_size)?;
        locations.sort_by_key(CpuLocation::cpu);
        if locations.windows(2).any(|pair| pair[0].cpu == pair[1].cpu) {
            return Err(invalid());
        }
        let mut cores: BTreeMap<CoreId, &CpuLocation> = BTreeMap::new();
        for location in &locations {
            let Some(core) = location.core else { continue };
            let first = *cores.entry(core).or_insert(location);
            let agrees = first.package == location.package
                && first.numa_node == location.numa_node
                && first.last_level_cache == location.last_level_cache;
            if !agrees {
                return Err(invalid());
            }
        }
        let count = |known: fn(&CpuLocation) -> Option<u32>| {
            let ids: Option<Vec<u32>> = locations.iter().map(known).collect();
            ids.and_then(|mut ids| {
                ids.sort_unstable();
                ids.dedup();
                NonZeroUsize::new(ids.len())
            })
        };
        Ok(Self {
            physical_cores: count(|location| location.core.map(CoreId::index)),
            packages: count(|location| location.package.map(PackageId::index)),
            logical_cpus: NonZeroUsize::new(locations.len()),
            locations,
            cache_line_size,
        })
    }

    /// A topology known only by its counts, as on Apple's systems, which report no CPU's place.
    /// Fails with [`ErrorKind::InvalidInput`] when there are more cores than CPUs or more
    /// packages than cores, or when `cache_line_size` is not a power of two.
    pub fn from_counts(
        physical_cores: Option<NonZeroUsize>,
        logical_cpus: Option<NonZeroUsize>,
        packages: Option<NonZeroUsize>,
        cache_line_size: Option<NonZeroUsize>,
    ) -> Result<Self> {
        check_cache_line(cache_line_size)?;
        let at_most = |fewer: Option<NonZeroUsize>, more: Option<NonZeroUsize>| {
            fewer.zip(more).is_none_or(|(fewer, more)| fewer <= more)
        };
        if !at_most(physical_cores, logical_cpus) || !at_most(packages, physical_cores) {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        Ok(Self {
            locations: Vec::new(),
            physical_cores,
            logical_cpus,
            packages,
            cache_line_size,
        })
    }

    /// A machine of `packages` packages, each with `cores_per_package` cores of
    /// `threads_per_core` hardware threads, one NUMA node and one last-level cache per package,
    /// and 64-byte cache lines; for tests of code that places work on it. Fails with
    /// [`ErrorKind::InvalidInput`] when it would hold more than [`CpuSet::MAX_CPUS`] CPUs.
    pub fn symmetric(
        packages: NonZeroUsize,
        cores_per_package: NonZeroUsize,
        threads_per_core: NonZeroUsize,
        numbering: SiblingNumbering,
    ) -> Result<Self> {
        let invalid = || Error::new(ErrorKind::InvalidInput);
        let cores = packages
            .checked_mul(cores_per_package)
            .ok_or_else(invalid)?;
        let cpus = cores.checked_mul(threads_per_core).ok_or_else(invalid)?;
        if cpus.get() > CpuSet::MAX_CPUS {
            return Err(invalid());
        }
        let id = |index: usize| u32::try_from(index).map_err(|_| invalid());
        let mut locations = Vec::with_capacity(cpus.get());
        for core in 0..cores.get() {
            let package = core / cores_per_package;
            for thread in 0..threads_per_core.get() {
                let cpu = match numbering {
                    SiblingNumbering::Adjacent => core * threads_per_core.get() + thread,
                    SiblingNumbering::Spread => thread * cores.get() + core,
                };
                locations.push(
                    CpuLocation::new(CpuId::new(id(cpu)?)?)
                        .with_core(CoreId(id(core)?))
                        .with_package(PackageId(id(package)?))
                        .with_numa_node(NumaNodeId(id(package)?))
                        .with_last_level_cache(CacheId(id(package)?)),
                );
            }
        }
        Self::from_locations(locations, NonZeroUsize::new(64))
    }

    /// The location of every CPU the operating system reported, in increasing CPU order; empty
    /// where it reports none.
    pub fn locations(&self) -> &[CpuLocation] {
        &self.locations
    }

    /// The location of `cpu`, if it was reported.
    pub fn location(&self, cpu: CpuId) -> Option<&CpuLocation> {
        self.locations
            .binary_search_by_key(&cpu, CpuLocation::cpu)
            .ok()
            .map(|index| &self.locations[index])
    }

    /// The CPUs with a location.
    pub fn cpus(&self) -> CpuSet {
        self.locations.iter().map(CpuLocation::cpu).collect()
    }

    /// How many physical cores the machine has.
    pub fn physical_cores(&self) -> Option<NonZeroUsize> {
        self.physical_cores
    }

    /// How many logical CPUs (hardware threads) the machine has online.
    pub fn logical_cpus(&self) -> Option<NonZeroUsize> {
        self.logical_cpus
    }

    /// How many processor packages the machine has.
    pub fn packages(&self) -> Option<NonZeroUsize> {
        self.packages
    }

    /// The cache line size the operating system reports, in bytes. Data layouts use the
    /// constant [`CACHE_LINE_PADDING`](crate::CACHE_LINE_PADDING) instead, since a layout cannot
    /// depend on a value read at run time.
    pub fn cache_line_size(&self) -> Option<NonZeroUsize> {
        self.cache_line_size
    }

    /// The NUMA nodes that have CPUs, in increasing order.
    pub fn numa_nodes(&self) -> Vec<NumaNodeId> {
        let mut nodes: Vec<NumaNodeId> = self
            .locations
            .iter()
            .filter_map(CpuLocation::numa_node)
            .collect();
        nodes.sort_unstable();
        nodes.dedup();
        nodes
    }

    /// The CPUs of `core`.
    pub fn core_cpus(&self, core: CoreId) -> CpuSet {
        self.cpus_where(|location| location.core == Some(core))
    }

    /// The CPUs of `node`.
    pub fn numa_node_cpus(&self, node: NumaNodeId) -> CpuSet {
        self.cpus_where(|location| location.numa_node == Some(node))
    }

    /// The lowest CPU of each core that has a CPU in `among`, taken from `among`: one shard per
    /// physical core instead of one per hardware thread. `None` when a CPU in `among` has no
    /// known core.
    pub fn one_cpu_per_core(&self, among: &CpuSet) -> Option<CpuSet> {
        let mut chosen: BTreeMap<CoreId, CpuId> = BTreeMap::new();
        for cpu in among.iter() {
            let core = self.location(cpu)?.core?;
            chosen.entry(core).or_insert(cpu);
        }
        Some(chosen.into_values().collect())
    }

    fn cpus_where(&self, keep: impl Fn(&CpuLocation) -> bool) -> CpuSet {
        self.locations
            .iter()
            .filter(|location| keep(location))
            .map(CpuLocation::cpu)
            .collect()
    }
}

fn check_cache_line(size: Option<NonZeroUsize>) -> Result<()> {
    if size.is_some_and(|size| !size.is_power_of_two()) {
        return Err(Error::new(ErrorKind::InvalidInput));
    }
    Ok(())
}

#[cfg(any(test, target_os = "linux", target_os = "android", windows))]
pub(crate) fn number_groups(mut groups: Vec<CpuSet>) -> Option<BTreeMap<CpuId, u32>> {
    groups.retain(|group| !group.is_empty());
    groups.sort_by_key(|group| group.iter().next());
    groups.dedup();
    let mut numbers = BTreeMap::new();
    for (number, group) in groups.iter().enumerate() {
        let number = u32::try_from(number).ok()?;
        for cpu in group.iter() {
            if numbers.insert(cpu, number).is_some() {
                return None;
            }
        }
    }
    Some(numbers)
}

#[cfg(test)]
pub(crate) fn assert_numbered_densely(topology: &CpuTopology) {
    let distinct = |id: fn(&CpuLocation) -> Option<u32>| -> Vec<u32> {
        let mut ids: Vec<u32> = topology.locations().iter().filter_map(id).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    for ids in [
        distinct(|location| location.core().map(CoreId::index)),
        distinct(|location| location.package().map(PackageId::index)),
        distinct(|location| location.last_level_cache().map(CacheId::index)),
    ] {
        assert!(ids.iter().copied().eq(0..ids.len() as u32), "{ids:?}");
    }
    let cores = distinct(|location| location.core().map(CoreId::index)).len();
    let packages = distinct(|location| location.package().map(PackageId::index)).len();
    if let Some(counted) = topology.physical_cores() {
        assert_eq!(counted.get(), cores);
    }
    if let Some(counted) = topology.packages() {
        assert_eq!(counted.get(), packages);
    }
}

#[cfg(any(test, target_os = "linux", target_os = "android", windows))]
#[derive(Debug, Default)]
pub(crate) struct TopologyReport {
    pub(crate) cpus: CpuSet,
    pub(crate) cores: Option<BTreeMap<CpuId, u32>>,
    pub(crate) packages: Option<BTreeMap<CpuId, u32>>,
    pub(crate) numa_nodes: Option<BTreeMap<CpuId, NumaNodeId>>,
    pub(crate) last_level_caches: Option<BTreeMap<CpuId, u32>>,
    pub(crate) cache_line_size: Option<NonZeroUsize>,
}

#[cfg(any(test, target_os = "linux", target_os = "android", windows))]
impl TopologyReport {
    pub(crate) fn into_topology(self) -> CpuTopology {
        let cache_line_size = self.cache_line_size.filter(|size| size.is_power_of_two());
        CpuTopology::from_locations(self.locations(true), cache_line_size)
            .or_else(|_| CpuTopology::from_locations(self.locations(false), cache_line_size))
            .unwrap_or_default()
    }

    fn locations(&self, with_groups: bool) -> Vec<CpuLocation> {
        let number = |numbers: &Option<BTreeMap<CpuId, u32>>, cpu: CpuId| {
            numbers
                .as_ref()
                .and_then(|numbers| numbers.get(&cpu))
                .copied()
        };
        self.cpus
            .iter()
            .map(|cpu| {
                let mut location = CpuLocation::new(cpu);
                if let Some(core) = number(&self.cores, cpu) {
                    location = location.with_core(CoreId(core));
                }
                if !with_groups {
                    return location;
                }
                if let Some(package) = number(&self.packages, cpu) {
                    location = location.with_package(PackageId(package));
                }
                if let Some(node) = self.numa_nodes.as_ref().and_then(|nodes| nodes.get(&cpu)) {
                    location = location.with_numa_node(*node);
                }
                if let Some(cache) = number(&self.last_level_caches, cpu) {
                    location = location.with_last_level_cache(CacheId(cache));
                }
                location
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CacheId, CoreId, CpuLocation, CpuTopology, NumaNodeId, PackageId, SiblingNumbering,
        number_groups,
    };
    use crate::{CpuId, CpuSet, ErrorKind};
    use std::num::NonZeroUsize;

    fn cpu(index: u32) -> CpuId {
        CpuId::new(index).unwrap()
    }

    fn set(list: &str) -> CpuSet {
        CpuSet::parse_list(list).unwrap()
    }

    fn count(value: usize) -> Option<NonZeroUsize> {
        NonZeroUsize::new(value)
    }

    fn machine(numbering: SiblingNumbering) -> CpuTopology {
        CpuTopology::symmetric(
            NonZeroUsize::new(2).unwrap(),
            NonZeroUsize::new(4).unwrap(),
            NonZeroUsize::new(2).unwrap(),
            numbering,
        )
        .unwrap()
    }

    #[test]
    fn symmetric_machines_number_siblings_both_ways() {
        let adjacent = machine(SiblingNumbering::Adjacent);
        assert_eq!(adjacent.core_cpus(CoreId::new(1)), set("2-3"));
        let spread = machine(SiblingNumbering::Spread);
        assert_eq!(spread.core_cpus(CoreId::new(1)), set("1,9"));
        for topology in [&adjacent, &spread] {
            assert_eq!(topology.logical_cpus(), count(16));
            assert_eq!(topology.physical_cores(), count(8));
            assert_eq!(topology.packages(), count(2));
            assert_eq!(topology.cache_line_size(), count(64));
            assert_eq!(
                topology.numa_nodes(),
                [NumaNodeId::new(0), NumaNodeId::new(1)]
            );
            assert_eq!(topology.cpus(), set("0-15"));
        }
        assert_eq!(adjacent.numa_node_cpus(NumaNodeId::new(1)), set("8-15"));
        assert_eq!(spread.numa_node_cpus(NumaNodeId::new(1)), set("4-7,12-15"));
    }

    #[test]
    fn one_cpu_per_core_skips_hardware_siblings() {
        let spread = machine(SiblingNumbering::Spread);
        assert_eq!(spread.one_cpu_per_core(&set("0-15")), Some(set("0-7")));
        assert_eq!(spread.one_cpu_per_core(&set("8-9,1")), Some(set("1,8")));
        let adjacent = machine(SiblingNumbering::Adjacent);
        assert_eq!(
            adjacent.one_cpu_per_core(&set("0-15")),
            Some(set("0,2,4,6,8,10,12,14"))
        );
        assert_eq!(adjacent.one_cpu_per_core(&set("16")), None);
        assert_eq!(CpuTopology::unknown().one_cpu_per_core(&set("0")), None);
        assert_eq!(
            adjacent.one_cpu_per_core(&CpuSet::new()),
            Some(CpuSet::new())
        );
    }

    #[test]
    fn locations_are_sorted_and_found_by_cpu() {
        let topology = CpuTopology::from_locations(
            vec![
                CpuLocation::new(cpu(5)).with_core(CoreId::new(1)),
                CpuLocation::new(cpu(2)).with_core(CoreId::new(0)),
            ],
            None,
        )
        .unwrap();
        let cpus: Vec<_> = topology.locations().iter().map(CpuLocation::cpu).collect();
        assert_eq!(cpus, [cpu(2), cpu(5)]);
        assert_eq!(
            topology.location(cpu(5)).unwrap().core(),
            Some(CoreId::new(1))
        );
        assert!(topology.location(cpu(3)).is_none());
        assert_eq!(topology.physical_cores(), count(2));
        assert_eq!(topology.packages(), None);
        assert_eq!(topology.cache_line_size(), None);
    }

    #[test]
    fn counts_are_unknown_when_any_cpu_is_unplaced() {
        let topology = CpuTopology::from_locations(
            vec![
                CpuLocation::new(cpu(0)).with_core(CoreId::new(0)),
                CpuLocation::new(cpu(1)),
            ],
            None,
        )
        .unwrap();
        assert_eq!(topology.physical_cores(), None);
        assert_eq!(topology.logical_cpus(), count(2));
    }

    #[test]
    fn contradictory_locations_are_invalid_input() {
        let core = |index: u32, package: u32| {
            CpuLocation::new(cpu(index))
                .with_core(CoreId::new(0))
                .with_package(PackageId::new(package))
        };
        let cache = |index: u32, cache: u32| {
            CpuLocation::new(cpu(index))
                .with_core(CoreId::new(0))
                .with_last_level_cache(CacheId::new(cache))
        };
        let node = |index: u32, node: u32| {
            CpuLocation::new(cpu(index))
                .with_core(CoreId::new(0))
                .with_numa_node(NumaNodeId::new(node))
        };
        for locations in [
            vec![core(0, 0), core(0, 0)],
            vec![core(0, 0), core(1, 1)],
            vec![cache(0, 0), cache(1, 1)],
            vec![node(0, 0), node(1, 1)],
        ] {
            let error = CpuTopology::from_locations(locations, None).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
        }
        let error = CpuTopology::from_locations(Vec::new(), count(96)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn counts_alone_must_nest() {
        let topology = CpuTopology::from_counts(count(8), count(16), count(1), count(128)).unwrap();
        assert!(topology.locations().is_empty());
        assert_eq!(topology.physical_cores(), count(8));
        assert_eq!(topology.cpus(), CpuSet::new());
        for (cores, cpus, packages, line) in [
            (count(8), count(4), None, None),
            (count(2), None, count(3), None),
            (None, None, None, count(48)),
        ] {
            let error = CpuTopology::from_counts(cores, cpus, packages, line).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
        }
        assert!(CpuTopology::from_counts(None, None, None, None).is_ok());
    }

    #[test]
    fn symmetric_machines_beyond_the_largest_cpu_are_invalid_input() {
        let big = NonZeroUsize::new(CpuSet::MAX_CPUS).unwrap();
        let two = NonZeroUsize::new(2).unwrap();
        let error = CpuTopology::symmetric(big, two, two, SiblingNumbering::Adjacent).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        let huge = NonZeroUsize::new(usize::MAX).unwrap();
        let error = CpuTopology::symmetric(huge, huge, huge, SiblingNumbering::Spread).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn groups_are_numbered_by_their_lowest_cpu_and_must_not_overlap() {
        let numbers = number_groups(vec![set("4-5"), CpuSet::new(), set("0,9")]).unwrap();
        assert_eq!(numbers[&cpu(0)], 0);
        assert_eq!(numbers[&cpu(9)], 0);
        assert_eq!(numbers[&cpu(4)], 1);
        assert_eq!(number_groups(vec![set("0-1"), set("1-2")]), None);
    }
}
