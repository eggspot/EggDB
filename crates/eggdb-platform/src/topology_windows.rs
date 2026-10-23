use crate::cpus::{CpuId, CpuSet};
use crate::topology::{CpuTopology, NumaNodeId, TopologyReport, number_groups};
use std::collections::BTreeMap;
use std::num::NonZeroUsize;

const RELATION_PROCESSOR_CORE: u32 = 0;
const RELATION_NUMA_NODE: u32 = 1;
const RELATION_CACHE: u32 = 2;
const RELATION_PROCESSOR_PACKAGE: u32 = 3;
const RELATION_NUMA_NODE_EX: u32 = 6;
const CACHE_UNIFIED: u32 = 0;
const CACHE_DATA: u32 = 2;
const HEADER_SIZE: usize = 8;
const PROCESSOR_MASKS: usize = 32;
const PROCESSOR_GROUP_COUNT: usize = 30;
const NUMA_NODE_NUMBER: usize = 8;
const CACHE_LEVEL: usize = 8;
const CACHE_LINE_SIZE: usize = 10;
const CACHE_TYPE: usize = 16;
const CACHE_GROUP_COUNT: usize = 38;
const CACHE_MASKS: usize = 40;
const GROUP_WIDTH: u32 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AffinityLayout {
    Wide,
    Narrow,
}

impl AffinityLayout {
    #[cfg(windows)]
    pub(crate) const NATIVE: Self = if cfg!(target_pointer_width = "64") {
        Self::Wide
    } else {
        Self::Narrow
    };

    fn size(self) -> usize {
        match self {
            Self::Wide => 16,
            Self::Narrow => 12,
        }
    }

    fn group_offset(self) -> usize {
        match self {
            Self::Wide => 8,
            Self::Narrow => 4,
        }
    }
}

#[derive(Debug, Default)]
struct Records {
    cores: Vec<CpuSet>,
    packages: Vec<CpuSet>,
    numa_nodes: BTreeMap<CpuId, NumaNodeId>,
    numa_conflict: bool,
    caches: Vec<(u8, CpuSet)>,
    cache_line_size: Option<NonZeroUsize>,
}

pub(crate) fn parse(buffer: &[u8], layout: AffinityLayout) -> CpuTopology {
    parse_records(buffer, layout).map_or_else(CpuTopology::unknown, build)
}

fn parse_records(mut buffer: &[u8], layout: AffinityLayout) -> Option<Records> {
    let mut records = Records::default();
    while !buffer.is_empty() {
        let relationship = u32::from_le_bytes(field(buffer, 0)?);
        let size = usize::try_from(u32::from_le_bytes(field(buffer, 4)?)).ok()?;
        if size < HEADER_SIZE || size > buffer.len() {
            return None;
        }
        let (record, rest) = buffer.split_at(size);
        buffer = rest;
        match relationship {
            RELATION_PROCESSOR_CORE => records.cores.push(masks(
                record,
                PROCESSOR_GROUP_COUNT,
                PROCESSOR_MASKS,
                layout,
            )?),
            RELATION_PROCESSOR_PACKAGE => records.packages.push(masks(
                record,
                PROCESSOR_GROUP_COUNT,
                PROCESSOR_MASKS,
                layout,
            )?),
            RELATION_NUMA_NODE | RELATION_NUMA_NODE_EX => {
                let node = NumaNodeId::new(u32::from_le_bytes(field(record, NUMA_NODE_NUMBER)?));
                let cpus = masks(record, PROCESSOR_GROUP_COUNT, PROCESSOR_MASKS, layout)?;
                for cpu in cpus.iter() {
                    let previous = records.numa_nodes.insert(cpu, node);
                    records.numa_conflict |= previous.is_some_and(|previous| previous != node);
                }
            }
            RELATION_CACHE => {
                let kind = u32::from_le_bytes(field(record, CACHE_TYPE)?);
                if kind != CACHE_UNIFIED && kind != CACHE_DATA {
                    continue;
                }
                let [level] = field(record, CACHE_LEVEL)?;
                let line = usize::from(u16::from_le_bytes(field(record, CACHE_LINE_SIZE)?));
                records.cache_line_size = records.cache_line_size.max(NonZeroUsize::new(line));
                let cpus = masks(record, CACHE_GROUP_COUNT, CACHE_MASKS, layout)?;
                records.caches.push((level, cpus));
            }
            _ => {}
        }
    }
    Some(records)
}

fn build(records: Records) -> CpuTopology {
    let cpus: CpuSet = records.cores.iter().flat_map(CpuSet::iter).collect();
    if cpus.is_empty() {
        return CpuTopology::unknown();
    }
    let last_level = records.caches.iter().map(|(level, _)| *level).max();
    TopologyReport {
        cpus,
        cores: number_groups(records.cores),
        packages: number_groups(records.packages),
        numa_nodes: (!records.numa_conflict).then_some(records.numa_nodes),
        last_level_caches: number_groups(
            records
                .caches
                .into_iter()
                .filter(|(level, _)| Some(*level) == last_level)
                .map(|(_, cpus)| cpus)
                .collect(),
        ),
        cache_line_size: records.cache_line_size,
    }
    .into_topology()
}

fn masks(
    record: &[u8],
    count_offset: usize,
    masks_offset: usize,
    layout: AffinityLayout,
) -> Option<CpuSet> {
    let count = usize::from(u16::from_le_bytes(field(record, count_offset)?)).max(1);
    let mut cpus = CpuSet::new();
    for index in 0..count {
        let affinity = masks_offset.checked_add(index.checked_mul(layout.size())?)?;
        let mask = match layout {
            AffinityLayout::Wide => u64::from_le_bytes(field(record, affinity)?),
            AffinityLayout::Narrow => u64::from(u32::from_le_bytes(field(record, affinity)?)),
        };
        let group = u32::from(u16::from_le_bytes(field(
            record,
            affinity.checked_add(layout.group_offset())?,
        )?));
        for bit in (0..u64::BITS).filter(|bit| mask & (1 << bit) != 0) {
            cpus.insert(CpuId::new(group * GROUP_WIDTH + bit).ok()?);
        }
    }
    Some(cpus)
}

fn field<const N: usize>(bytes: &[u8], offset: usize) -> Option<[u8; N]> {
    bytes.get(offset..offset.checked_add(N)?)?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::{AffinityLayout, parse};
    use crate::rng::SplitMix64;
    use crate::{CpuId, CpuSet, CpuTopology, NumaNodeId, SiblingNumbering};
    use std::num::NonZeroUsize;

    const LAYOUTS: [AffinityLayout; 2] = [AffinityLayout::Wide, AffinityLayout::Narrow];

    fn record(
        kind: u32,
        head: &[(usize, &[u8])],
        count_at: usize,
        masks: &[(u16, u32)],
        layout: AffinityLayout,
    ) -> Vec<u8> {
        let masks_at = count_at + 2;
        let size = masks_at + masks.len().max(1) * layout.size();
        let mut bytes = vec![0; size];
        bytes[0..4].copy_from_slice(&kind.to_le_bytes());
        bytes[4..8].copy_from_slice(&u32::try_from(size).unwrap().to_le_bytes());
        for (offset, field) in head {
            bytes[*offset..offset + field.len()].copy_from_slice(field);
        }
        let count = u16::try_from(masks.len()).unwrap();
        bytes[count_at..masks_at].copy_from_slice(&count.to_le_bytes());
        for (index, (group, mask)) in masks.iter().enumerate() {
            let at = masks_at + index * layout.size();
            match layout {
                AffinityLayout::Wide => {
                    bytes[at..at + 8].copy_from_slice(&u64::from(*mask).to_le_bytes())
                }
                AffinityLayout::Narrow => bytes[at..at + 4].copy_from_slice(&mask.to_le_bytes()),
            }
            let group_at = at + layout.group_offset();
            bytes[group_at..group_at + 2].copy_from_slice(&group.to_le_bytes());
        }
        bytes
    }

    fn processor(kind: u32, mask: u32, layout: AffinityLayout) -> Vec<u8> {
        record(kind, &[], 30, &[(0, mask)], layout)
    }

    fn numa_node(node: u32, masks: &[(u16, u32)], layout: AffinityLayout) -> Vec<u8> {
        record(1, &[(8, &node.to_le_bytes())], 30, masks, layout)
    }

    fn cache(level: u8, kind: u32, mask: u32, layout: AffinityLayout) -> Vec<u8> {
        record(
            2,
            &[
                (8, &[level]),
                (10, &64_u16.to_le_bytes()),
                (16, &kind.to_le_bytes()),
            ],
            38,
            &[(0, mask)],
            layout,
        )
    }

    fn two_packages_of_two_cores_with_two_threads(layout: AffinityLayout) -> Vec<u8> {
        let mut buffer = Vec::new();
        for package in 0..2 {
            let package_mask = 0x0F << (4 * package);
            buffer.extend(processor(3, package_mask, layout));
            buffer.extend(numa_node(package, &[(0, package_mask)], layout));
            buffer.extend(cache(3, 0, package_mask, layout));
        }
        for core in 0..4 {
            let core_mask = 0b11 << (2 * core);
            buffer.extend(processor(0, core_mask, layout));
            buffer.extend(cache(1, 2, core_mask, layout));
            buffer.extend(cache(1, 1, core_mask, layout));
            buffer.extend(cache(2, 0, core_mask, layout));
        }
        buffer.extend(record(4, &[], 30, &[], layout));
        buffer
    }

    fn two_by_two_by_two() -> CpuTopology {
        let two = NonZeroUsize::new(2).unwrap();
        CpuTopology::symmetric(two, two, two, SiblingNumbering::Adjacent).unwrap()
    }

    #[test]
    fn records_in_either_layout_give_the_whole_shape() {
        for layout in LAYOUTS {
            let buffer = two_packages_of_two_cores_with_two_threads(layout);
            assert_eq!(parse(&buffer, layout), two_by_two_by_two(), "{layout:?}");
        }
    }

    #[test]
    fn later_groups_number_their_cpus_from_sixty_four() {
        for layout in LAYOUTS {
            let mut buffer = record(0, &[], 30, &[(1, 0b1)], layout);
            buffer.extend(numa_node(3, &[(1, 0b1)], layout));
            let topology = parse(&buffer, layout);
            let location = topology.location(CpuId::new(64).unwrap()).unwrap();
            assert_eq!(location.numa_node(), Some(NumaNodeId::new(3)));
        }
    }

    #[test]
    fn a_zero_group_count_means_one_mask() {
        for layout in LAYOUTS {
            let mut node = numa_node(0, &[(0, 0b11)], layout);
            node[30..32].copy_from_slice(&0_u16.to_le_bytes());
            let mut buffer = processor(0, 0b11, layout);
            buffer.extend(node);
            assert_eq!(parse(&buffer, layout).numa_nodes(), [NumaNodeId::new(0)]);
        }
    }

    #[test]
    fn extended_numa_records_count_as_numa_records() {
        let layout = AffinityLayout::Wide;
        let mut buffer = processor(0, 0b11, layout);
        let mut extended = numa_node(2, &[(0, 0b11)], layout);
        extended[0..4].copy_from_slice(&6_u32.to_le_bytes());
        buffer.extend(numa_node(2, &[(0, 0b11)], layout));
        buffer.extend(extended);
        assert_eq!(parse(&buffer, layout).numa_nodes(), [NumaNodeId::new(2)]);
    }

    #[test]
    fn a_numa_node_split_over_groups_is_read_whole() {
        let layout = AffinityLayout::Wide;
        let mut buffer = record(0, &[], 30, &[(0, 0b1)], layout);
        buffer.extend(record(0, &[], 30, &[(1, 0b1)], layout));
        buffer.extend(numa_node(0, &[(0, 0b1), (1, 0b1)], layout));
        let topology = parse(&buffer, layout);
        assert_eq!(
            topology.numa_node_cpus(NumaNodeId::new(0)),
            CpuSet::parse_list("0,64").unwrap()
        );
    }

    #[test]
    fn broken_buffers_are_unknown() {
        let layout = AffinityLayout::Wide;
        let whole = two_packages_of_two_cores_with_two_threads(layout);
        assert_eq!(
            parse(&whole[..whole.len() - 1], layout),
            CpuTopology::unknown()
        );
        let mut tiny = whole.clone();
        tiny[4..8].copy_from_slice(&4_u32.to_le_bytes());
        assert_eq!(parse(&tiny, layout), CpuTopology::unknown());
        let mut short = processor(0, 1, layout);
        short[4..8].copy_from_slice(&20_u32.to_le_bytes());
        short.truncate(20);
        assert_eq!(parse(&short, layout), CpuTopology::unknown());
        assert_eq!(parse(&[], layout), CpuTopology::unknown());
    }

    #[test]
    fn overlapping_cores_leave_cores_unknown() {
        let layout = AffinityLayout::Wide;
        let mut buffer = processor(0, 0b011, layout);
        buffer.extend(processor(0, 0b110, layout));
        let topology = parse(&buffer, layout);
        assert_eq!(topology.cpus(), CpuSet::parse_list("0-2").unwrap());
        assert_eq!(topology.physical_cores(), None);
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut rng = SplitMix64::new(65);
        let base = two_packages_of_two_cores_with_two_threads(AffinityLayout::Wide);
        for _ in 0..2_000 {
            let mut buffer = base.clone();
            for _ in 0..1 + rng.next_u64() % 8 {
                let at = (rng.next_u64() % buffer.len() as u64) as usize;
                buffer[at] = rng.next_u64().to_le_bytes()[0];
            }
            let cut = (rng.next_u64() % (buffer.len() as u64 + 1)) as usize;
            for layout in LAYOUTS {
                let topology = parse(&buffer[..cut], layout);
                let placed = topology.logical_cpus().map_or(0, NonZeroUsize::get);
                assert_eq!(topology.cpus().len(), placed);
            }
        }
    }
}
