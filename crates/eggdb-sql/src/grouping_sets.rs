use crate::ast::GroupingSets;
use std::collections::HashMap;

pub(crate) const GROUPING: &str = "grouping";
pub(crate) const GROUPING_ID: &str = "grouping_id";
pub(crate) const GROUP_ID: &str = "group_id";
pub(crate) const MAX_GROUPING_ARGUMENTS: usize = 31;

pub(crate) fn is_operation(name: &str) -> bool {
    matches!(name, GROUPING | GROUPING_ID | GROUP_ID)
}

pub(crate) struct ExpandedSets {
    members: Vec<Vec<usize>>,
    same_key_as: Vec<usize>,
    group_ids: Vec<i32>,
    pub(crate) hierarchical: bool,
}

fn first_keys(set: &[usize], order: &[usize]) -> bool {
    order.get(..set.len()) == Some(set)
}

impl ExpandedSets {
    pub(crate) fn new(written: &GroupingSets, same_key_as: &[usize]) -> Self {
        let mut chosen: Vec<Vec<usize>> = Vec::with_capacity(written.sets.len());
        let mut group_ids = Vec::with_capacity(written.sets.len());
        let mut earlier_equal: HashMap<Vec<usize>, i32> = HashMap::new();
        for set in &written.sets {
            let mut members: Vec<usize> = set
                .iter()
                .filter_map(|&key| same_key_as.get(key).copied())
                .collect();
            members.sort_unstable();
            members.dedup();
            let earlier = earlier_equal.entry(members.clone()).or_insert(0);
            if written.distinct && *earlier > 0 {
                continue;
            }
            group_ids.push(*earlier);
            *earlier = earlier.saturating_add(1);
            chosen.push(members);
        }
        let mut order: Vec<usize> = same_key_as.to_vec();
        order.sort_unstable();
        order.dedup();
        let hierarchical = chosen.iter().all(|members| first_keys(members, &order));
        Self {
            members: chosen,
            same_key_as: same_key_as.to_vec(),
            group_ids,
            hierarchical,
        }
    }

    pub(crate) fn count(&self) -> usize {
        self.members.len()
    }

    pub(crate) fn keeps(&self, set: usize, key: usize) -> bool {
        match (self.members.get(set), self.same_key_as.get(key)) {
            (Some(members), Some(key)) => members.binary_search(key).is_ok(),
            _ => true,
        }
    }

    pub(crate) fn keeps_in_every_set(&self, key: usize) -> bool {
        (0..self.count()).all(|set| self.keeps(set, key))
    }

    pub(crate) fn keeps_none(&self, set: usize) -> bool {
        self.members.get(set).is_some_and(Vec::is_empty)
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
