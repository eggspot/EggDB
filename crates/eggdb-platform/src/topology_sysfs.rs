use crate::cpus::{CpuId, CpuSet};
use crate::topology::{CpuTopology, NumaNodeId, TopologyReport, number_groups};
use std::collections::{BTreeMap, HashSet};
use std::num::NonZeroUsize;

const MAX_CACHE_INDEXES: usize = 32;

pub(crate) fn read(file: &dyn Fn(&str) -> Option<String>) -> CpuTopology {
    let Some(online) = list(file, "cpu/online").filter(|online| !online.is_empty()) else {
        return CpuTopology::unknown();
    };
    let caches: BTreeMap<CpuId, Vec<DataCache>> = online
        .iter()
        .map(|cpu| (cpu, data_caches(file, cpu)))
        .collect();
    let cores = grouped(&online, |cpu| {
        list(file, &topology_file(cpu, "core_cpus_list"))
            .or_else(|| list(file, &topology_file(cpu, "thread_siblings_list")))
    });
    let packages = grouped(&online, |cpu| {
        list(file, &topology_file(cpu, "package_cpus_list"))
            .or_else(|| list(file, &topology_file(cpu, "core_siblings_list")))
    });
    let last_level_caches = grouped(&online, |cpu| {
        caches
            .get(&cpu)?
            .iter()
            .max_by_key(|cache| cache.level)?
            .shared
            .clone()
    });
    let numa_nodes = numa_nodes(file, &online);
    let cache_line_size = caches
        .values()
        .flatten()
        .filter_map(|cache| cache.line_size)
        .max();
    TopologyReport {
        cpus: online,
        cores,
        packages,
        numa_nodes,
        last_level_caches,
        cache_line_size,
    }
    .into_topology()
}

#[derive(Debug)]
struct DataCache {
    level: u32,
    shared: Option<CpuSet>,
    line_size: Option<NonZeroUsize>,
}

fn data_caches(file: &dyn Fn(&str) -> Option<String>, cpu: CpuId) -> Vec<DataCache> {
    let mut caches = Vec::new();
    for index in 0..MAX_CACHE_INDEXES {
        let directory = format!("cpu/cpu{}/cache/index{index}", cpu.index());
        let Some(level) = file(&format!("{directory}/level")) else {
            break;
        };
        let kind = file(&format!("{directory}/type")).unwrap_or_default();
        let Ok(level) = level.trim().parse() else {
            continue;
        };
        if !matches!(kind.trim(), "Data" | "Unified") {
            continue;
        }
        caches.push(DataCache {
            level,
            shared: list(file, &format!("{directory}/shared_cpu_list")),
            line_size: file(&format!("{directory}/coherency_line_size"))
                .and_then(|size| size.trim().parse().ok()),
        });
    }
    caches
}

fn numa_nodes(
    file: &dyn Fn(&str) -> Option<String>,
    online: &CpuSet,
) -> Option<BTreeMap<CpuId, NumaNodeId>> {
    let node_numbers = list(file, "node/online")?;
    let mut nodes = BTreeMap::new();
    for number in node_numbers.iter() {
        let cpus = list(file, &format!("node/node{}/cpulist", number.index()))?;
        for cpu in cpus.iter() {
            if nodes.insert(cpu, NumaNodeId::new(number.index())).is_some() {
                return None;
            }
        }
    }
    online
        .iter()
        .all(|cpu| nodes.contains_key(&cpu))
        .then_some(nodes)
}

fn grouped(
    online: &CpuSet,
    group_of: impl Fn(CpuId) -> Option<CpuSet>,
) -> Option<BTreeMap<CpuId, u32>> {
    let mut groups: HashSet<CpuSet> = HashSet::new();
    for cpu in online.iter() {
        let group = group_of(cpu)?;
        if !group.contains(cpu) {
            return None;
        }
        groups.insert(group);
    }
    let within_online = groups
        .into_iter()
        .map(|group| group.iter().filter(|cpu| online.contains(*cpu)).collect())
        .collect();
    number_groups(within_online)
}

fn topology_file(cpu: CpuId, name: &str) -> String {
    format!("cpu/cpu{}/topology/{name}", cpu.index())
}

fn list(file: &dyn Fn(&str) -> Option<String>, path: &str) -> Option<CpuSet> {
    CpuSet::parse_list(&file(path)?).ok()
}

#[cfg(test)]
mod tests {
    use super::read;
    use crate::rng::SplitMix64;
    use crate::{CpuId, CpuSet, CpuTopology, NumaNodeId, SiblingNumbering};
    use std::collections::HashMap;
    use std::num::NonZeroUsize;

    const CORES: u32 = 4;

    fn two_packages_of_two_cores_with_two_threads() -> HashMap<String, String> {
        let mut files = HashMap::new();
        let mut put = |path: String, text: &str| {
            files.insert(path, format!("{text}\n"));
        };
        put("cpu/online".into(), "0-7");
        put("node/online".into(), "0-1");
        for package in 0..2 {
            let cpus = package_list(package);
            put(format!("node/node{package}/cpulist"), &cpus);
        }
        for cpu in 0..2 * CORES {
            let core = cpu % CORES;
            let core_list = format!("{core},{}", core + CORES);
            let package_list = package_list(core / 2);
            let topology = format!("cpu/cpu{cpu}/topology");
            put(format!("{topology}/core_cpus_list"), &core_list);
            put(format!("{topology}/package_cpus_list"), &package_list);
            for (index, level, kind, shared) in [
                (0, "1", "Data", &core_list),
                (1, "1", "Instruction", &core_list),
                (2, "2", "Unified", &core_list),
                (3, "3", "Unified", &package_list),
            ] {
                let cache = format!("cpu/cpu{cpu}/cache/index{index}");
                put(format!("{cache}/level"), level);
                put(format!("{cache}/type"), kind);
                put(format!("{cache}/shared_cpu_list"), shared);
                put(format!("{cache}/coherency_line_size"), "64");
            }
        }
        files
    }

    fn package_list(package: u32) -> String {
        let first = package * 2;
        format!(
            "{first}-{},{}-{}",
            first + 1,
            first + CORES,
            first + CORES + 1
        )
    }

    fn read_files(files: &HashMap<String, String>) -> CpuTopology {
        read(&|path| files.get(path).cloned())
    }

    fn cpu(index: u32) -> CpuId {
        CpuId::new(index).unwrap()
    }

    fn two_by_two_by_two() -> CpuTopology {
        let two = NonZeroUsize::new(2).unwrap();
        CpuTopology::symmetric(two, two, two, SiblingNumbering::Spread).unwrap()
    }

    #[test]
    fn a_full_sysfs_tree_gives_the_whole_shape() {
        assert_eq!(
            read_files(&two_packages_of_two_cores_with_two_threads()),
            two_by_two_by_two()
        );
    }

    #[test]
    fn the_older_file_names_are_read_too() {
        let files: HashMap<String, String> = two_packages_of_two_cores_with_two_threads()
            .into_iter()
            .map(|(path, text)| {
                let path = path
                    .replace("core_cpus_list", "thread_siblings_list")
                    .replace("package_cpus_list", "core_siblings_list");
                (path, text)
            })
            .collect();
        assert_eq!(read_files(&files), two_by_two_by_two());
    }

    #[test]
    fn without_a_node_directory_numa_is_unknown_not_node_zero() {
        let mut files = two_packages_of_two_cores_with_two_threads();
        files.retain(|path, _| !path.starts_with("node/"));
        let topology = read_files(&files);
        assert!(topology.numa_nodes().is_empty());
        assert!(
            topology
                .locations()
                .iter()
                .all(|location| location.numa_node().is_none())
        );
        assert_eq!(topology.physical_cores(), NonZeroUsize::new(4));
    }

    #[test]
    fn offline_cpus_are_left_out() {
        let mut files = two_packages_of_two_cores_with_two_threads();
        files.insert("cpu/online".into(), "0-2,4-7".into());
        files.insert("cpu/cpu7/topology/core_cpus_list".into(), "7".into());
        files.retain(|path, _| !path.starts_with("cpu/cpu3/"));
        let topology = read_files(&files);
        assert!(topology.location(cpu(3)).is_none());
        assert_eq!(topology.logical_cpus(), NonZeroUsize::new(7));
        let seventh = topology.location(cpu(7)).unwrap().core().unwrap();
        assert_eq!(
            topology.core_cpus(seventh),
            CpuSet::parse_list("7").unwrap()
        );
    }

    #[test]
    fn contradictory_core_lists_make_cores_unknown() {
        let mut files = two_packages_of_two_cores_with_two_threads();
        files.insert("cpu/cpu4/topology/core_cpus_list".into(), "4".into());
        let topology = read_files(&files);
        assert!(
            topology
                .locations()
                .iter()
                .all(|location| location.core().is_none())
        );
        assert_eq!(topology.packages(), NonZeroUsize::new(2));
    }

    #[test]
    fn a_core_split_across_packages_keeps_only_the_cores() {
        let mut files = two_packages_of_two_cores_with_two_threads();
        files.insert("cpu/cpu4/topology/package_cpus_list".into(), "4".into());
        files.insert("cpu/cpu0/topology/package_cpus_list".into(), "0-1,5".into());
        files.insert("cpu/cpu1/topology/package_cpus_list".into(), "0-1,5".into());
        files.insert("cpu/cpu5/topology/package_cpus_list".into(), "0-1,5".into());
        let topology = read_files(&files);
        assert_eq!(topology.physical_cores(), NonZeroUsize::new(4));
        assert!(
            topology
                .locations()
                .iter()
                .all(|location| location.package().is_none())
        );
        assert!(
            topology
                .locations()
                .iter()
                .all(|location| location.numa_node().is_none())
        );
    }

    #[test]
    fn missing_or_odd_cache_lines_are_unknown() {
        let mut files = two_packages_of_two_cores_with_two_threads();
        for (path, text) in &mut files {
            if path.ends_with("coherency_line_size") {
                *text = "0\n".into();
            }
        }
        assert_eq!(read_files(&files).cache_line_size(), None);
        for (path, text) in &mut files {
            if path.ends_with("coherency_line_size") {
                *text = "96\n".into();
            }
        }
        let topology = read_files(&files);
        assert_eq!(topology.cache_line_size(), None);
        assert_eq!(topology.physical_cores(), NonZeroUsize::new(4));
    }

    #[test]
    fn a_numa_node_list_must_cover_every_online_cpu_once() {
        let mut files = two_packages_of_two_cores_with_two_threads();
        files.insert("node/node1/cpulist".into(), "0,2-3,6-7".into());
        assert!(read_files(&files).numa_nodes().is_empty());
        let mut files = two_packages_of_two_cores_with_two_threads();
        files.insert("node/node1/cpulist".into(), "2".into());
        assert!(read_files(&files).numa_nodes().is_empty());
        let files = two_packages_of_two_cores_with_two_threads();
        assert_eq!(
            read_files(&files).numa_nodes(),
            [NumaNodeId::new(0), NumaNodeId::new(1)]
        );
    }

    #[test]
    fn without_an_online_list_nothing_is_known() {
        let mut files = two_packages_of_two_cores_with_two_threads();
        files.remove("cpu/online");
        assert_eq!(read_files(&files), CpuTopology::unknown());
        files.insert("cpu/online".into(), "\n".into());
        assert_eq!(read_files(&files), CpuTopology::unknown());
    }

    #[test]
    fn random_file_contents_never_panic() {
        const ALPHABET: &[u8] = b"0123456789--,,\n DataUnified";
        let base = two_packages_of_two_cores_with_two_threads();
        let mut paths: Vec<&String> = base.keys().collect();
        paths.sort();
        let mut rng = SplitMix64::new(64);
        for _ in 0..300 {
            let mut files = base.clone();
            for path in &paths {
                if rng.next_u64().is_multiple_of(4) {
                    let len = (rng.next_u64() % 12) as usize;
                    let text: String = (0..len)
                        .map(|_| {
                            char::from(ALPHABET[(rng.next_u64() % ALPHABET.len() as u64) as usize])
                        })
                        .collect();
                    files.insert((*path).clone(), text);
                }
            }
            let topology = read_files(&files);
            let placed = topology.logical_cpus().map_or(0, NonZeroUsize::get);
            assert_eq!(topology.cpus().len(), placed);
        }
    }
}
