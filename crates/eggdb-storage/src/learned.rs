//! Learned indexes over the LSM tree's sealed runs (D1336): a piecewise linear model from a
//! key's first eight bytes to the position of the leaf that holds it, so a point get reads
//! one leaf instead of descending the run's inner pages. Runs never change once sealed and
//! committed, so a model serves every reader whose run has the same root page at the same log
//! position; it lives in memory only.

use crate::btree::{BTree, shape_of_ref};
use crate::error::StorageError;
use crate::page_cache::PageSource;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

const ERROR: usize = 4;
const MOST_PROBES: usize = 2 * ERROR + 3;
const MOST_MODELS: usize = 1024;

/// The first eight bytes of `key` as a big-endian number, zero-padded: ordered as keys are.
pub(crate) fn key_number(key: &[u8]) -> u64 {
    let mut bytes = [0u8; 8];
    let length = key.len().min(8);
    bytes[..length].copy_from_slice(&key[..length]);
    u64::from_be_bytes(bytes)
}

#[derive(Debug, Clone, Copy)]
struct Piece {
    start: usize,
    first: u64,
    slope: f64,
}

/// A run's model: its leaves in key order, the number of each leaf's lowest key, and the
/// pieces of the line that predicts a leaf's position from a key's number.
#[derive(Debug)]
pub(crate) struct LearnedRun {
    root_lsn: u64,
    leaves: Vec<u64>,
    pieces: Vec<Piece>,
}

impl LearnedRun {
    /// The model of leaves `leaves`, whose lowest keys have numbers `firsts` (ascending).
    pub(crate) fn build(root_lsn: u64, firsts: &[u64], leaves: Vec<u64>) -> Self {
        let mut pieces = Vec::new();
        let mut at = 0;
        while at < firsts.len() {
            let start = at;
            let first = firsts[at];
            let (mut low, mut high) = (0.0f64, f64::INFINITY);
            at += 1;
            while at < firsts.len() {
                let run = (at - start) as f64;
                let across = firsts[at].saturating_sub(first) as f64;
                if across == 0.0 {
                    if at - start <= ERROR {
                        at += 1;
                        continue;
                    }
                    break;
                }
                let next_low = low.max((run - ERROR as f64) / across);
                let next_high = high.min((run + ERROR as f64) / across);
                if next_low > next_high {
                    break;
                }
                (low, high) = (next_low, next_high);
                at += 1;
            }
            let slope = if high.is_finite() { (low + high) / 2.0 } else { low };
            pieces.push(Piece { start, first, slope });
        }
        Self { root_lsn, leaves, pieces }
    }

    /// Whether this model describes the run whose root page was logged at `root_lsn`.
    pub(crate) fn describes(&self, root_lsn: u64) -> bool {
        self.root_lsn == root_lsn
    }

    fn predict(&self, number: u64) -> usize {
        let index = self.pieces.partition_point(|piece| piece.first <= number).saturating_sub(1);
        let Some(piece) = self.pieces.get(index) else {
            return 0;
        };
        let position = piece.start as f64 + piece.slope * number.saturating_sub(piece.first) as f64;
        (position.max(0.0) as usize).min(self.leaves.len().saturating_sub(1))
    }

    /// The leaves to try for a key with number `number`, nearest the prediction first.
    pub(crate) fn candidates(&self, number: u64) -> impl Iterator<Item = u64> + '_ {
        let predicted = self.predict(number);
        (0..MOST_PROBES).filter_map(move |step| {
            let distance = step.div_ceil(2);
            let position = if step % 2 == 1 { predicted.checked_add(distance) } else { predicted.checked_sub(distance) };
            position.and_then(|position| self.leaves.get(position)).copied()
        })
    }
}

/// The models a page cache keeps, by the root page of their run.
#[derive(Debug, Default)]
pub(crate) struct LearnedRuns {
    models: BTreeMap<u64, LearnedRun>,
}

impl LearnedRuns {
    pub(crate) fn get(&self, root: u64) -> Option<&LearnedRun> {
        self.models.get(&root)
    }

    pub(crate) fn insert(&mut self, root: u64, model: LearnedRun) {
        if self.models.len() >= MOST_MODELS {
            self.models.clear();
        }
        self.models.insert(root, model);
    }

    /// Forgets the model whose run had its root at `page`, freed.
    pub(crate) fn forget(&mut self, page: u64) {
        self.models.remove(&page);
    }

    pub(crate) fn clear(&mut self) {
        self.models.clear();
    }
}

/// Walks run `tree`, whose root page was logged at `root_lsn`, in key order and models its
/// leaves.
pub(crate) fn model_of(pages: &impl PageSource, tree: &BTree, root_lsn: u64) -> Result<LearnedRun, StorageError> {
    let mut firsts = Vec::new();
    let mut leaves = Vec::new();
    let mut stack = alloc::vec![tree.root()];
    while let Some(page_id) = stack.pop() {
        let page = pages.page(page_id)?;
        let shape = shape_of_ref(&page)?;
        if shape.is_leaf() {
            let mut low = shape.prefix(&page).to_vec();
            low.extend_from_slice(shape.low_suffix(&page));
            firsts.push(key_number(&low));
            leaves.push(page_id);
            continue;
        }
        for index in (0..=shape.count).rev() {
            stack.push(shape.child_at(&page, index)?);
        }
    }
    Ok(LearnedRun::build(root_lsn, &firsts, leaves))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predictions_stay_within_the_error_of_each_leaf() {
        let firsts: Vec<u64> = (0..1_000u64).map(|index| index * index * 37).collect();
        let model = LearnedRun::build(1, &firsts, (0..1_000).collect());
        for (position, &first) in firsts.iter().enumerate() {
            let predicted = model.predict(first);
            assert!(predicted.abs_diff(position) <= ERROR, "{position}: {predicted}");
            assert!(model.candidates(first).any(|leaf| leaf == position as u64));
        }
    }

    #[test]
    fn key_numbers_keep_the_order_of_keys() {
        assert!(key_number(b"apple") < key_number(b"apples"));
        assert!(key_number(b"a") < key_number(b"b"));
        assert_eq!(key_number(b""), 0);
    }
}
