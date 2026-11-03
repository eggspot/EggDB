use crate::ast::GroupingSets;

pub(crate) const GROUPING: &str = "grouping";
pub(crate) const GROUPING_ID: &str = "grouping_id";
pub(crate) const GROUP_ID: &str = "group_id";
pub(crate) const MAX_GROUPING_ARGUMENTS: usize = 31;

pub(crate) fn is_operation(name: &str) -> bool {
    matches!(name, GROUPING | GROUPING_ID | GROUP_ID)
}

pub(crate) struct ExpandedSets {
    present: Vec<Vec<bool>>,
    group_ids: Vec<i32>,
    pub(crate) hierarchical: bool,
}

fn first_keys(set: &[usize], order: &[usize]) -> bool {
    order.get(..set.len()) == Some(set)
}

impl ExpandedSets {
    pub(crate) fn new(written: &GroupingSets, same_key_as: &[usize]) -> Self {
        let mut chosen: Vec<Vec<usize>> = Vec::with_capacity(written.sets.len());
        for set in &written.sets {
            let mut members: Vec<usize> = set
                .iter()
                .filter_map(|&key| same_key_as.get(key).copied())
                .collect();
            members.sort_unstable();
            members.dedup();
            if written.distinct && chosen.contains(&members) {
                continue;
            }
            chosen.push(members);
        }
        let group_ids = chosen
            .iter()
            .enumerate()
            .map(|(at, members)| {
                let earlier = chosen
                    .iter()
                    .take(at)
                    .filter(|other| *other == members)
                    .count();
                i32::try_from(earlier).unwrap_or(i32::MAX)
            })
            .collect();
        let mut order: Vec<usize> = same_key_as.to_vec();
        order.sort_unstable();
        order.dedup();
        let hierarchical = chosen.iter().all(|members| first_keys(members, &order));
        let present = chosen
            .iter()
            .map(|members| {
                same_key_as
                    .iter()
                    .map(|key| members.binary_search(key).is_ok())
                    .collect()
            })
            .collect();
        Self {
            present,
            group_ids,
            hierarchical,
        }
    }

    pub(crate) fn count(&self) -> usize {
        self.present.len()
    }

    pub(crate) fn keeps(&self, set: usize, key: usize) -> bool {
        self.present
            .get(set)
            .and_then(|present| present.get(key))
            .copied()
            .unwrap_or(true)
    }

    pub(crate) fn keeps_in_every_set(&self, key: usize) -> bool {
        (0..self.count()).all(|set| self.keeps(set, key))
    }

    pub(crate) fn keeps_none(&self, set: usize) -> bool {
        self.present
            .get(set)
            .is_some_and(|present| present.iter().all(|kept| !kept))
    }

    pub(crate) fn group_id(&self, set: usize) -> i32 {
        self.group_ids.get(set).copied().unwrap_or(0)
    }
}

pub(crate) fn grouping_bits(sets: Option<&ExpandedSets>, set: usize, keys: &[usize]) -> i32 {
    keys.iter().fold(0, |bits, &key| {
        let rolled_up = sets.is_some_and(|sets| !sets.keeps(set, key));
        (bits << 1) | i32::from(rolled_up)
    })
}
