use crate::ast::OrderItem;
use crate::error::{SqlError, SqlResult};
use crate::eval::compare_truth;
use crate::execute::compare_sort_keys;
use crate::memory::WorkMemory;
use crate::types::Value;
use std::cmp::Ordering;

const FIRST_CAPACITY: usize = 1024;

struct HeldRow {
    keys: Vec<Value>,
    output: Vec<Value>,
    arrival: u64,
    bytes: usize,
}

pub(crate) struct TopRows<'o> {
    order_by: &'o [OrderItem],
    wanted: usize,
    held: Vec<HeldRow>,
    arrivals: u64,
}

impl<'o> TopRows<'o> {
    pub(crate) fn new(order_by: &'o [OrderItem], wanted: usize) -> Self {
        Self {
            order_by,
            wanted,
            held: Vec::with_capacity(wanted.min(FIRST_CAPACITY)),
            arrivals: 0,
        }
    }

    fn order(&self, left: &HeldRow, right: &HeldRow) -> Ordering {
        compare_sort_keys(self.order_by, &left.keys, &right.keys)
            .then(left.arrival.cmp(&right.arrival))
    }

    fn larger_at(&self, left: usize, right: usize) -> bool {
        match (self.held.get(left), self.held.get(right)) {
            (Some(left), Some(right)) => self.order(left, right) == Ordering::Greater,
            _ => false,
        }
    }

    pub(crate) fn offer(
        &mut self,
        keys: Vec<Value>,
        output: Vec<Value>,
        memory: &mut WorkMemory,
    ) -> SqlResult<()> {
        let arrival = self.arrivals;
        self.arrivals += 1;
        if self.wanted == 0 {
            return Ok(());
        }
        if self.held.len() < self.wanted {
            let bytes = WorkMemory::row_bytes(&keys) + WorkMemory::row_bytes(&output);
            memory.charge(bytes)?;
            self.held.push(HeldRow {
                keys,
                output,
                arrival,
                bytes,
            });
            self.sift_up(self.held.len() - 1);
            return Ok(());
        }
        let Some(largest) = self.held.first() else {
            return Ok(());
        };
        if compare_sort_keys(self.order_by, &keys, &largest.keys) != Ordering::Less {
            return Ok(());
        }
        let bytes = WorkMemory::row_bytes(&keys) + WorkMemory::row_bytes(&output);
        memory.charge(bytes)?;
        let entering = HeldRow {
            keys,
            output,
            arrival,
            bytes,
        };
        if let Some(root) = self.held.first_mut() {
            let evicted = std::mem::replace(root, entering);
            memory.release(evicted.bytes);
        }
        self.sift_down(0);
        Ok(())
    }

    fn sift_up(&mut self, mut at: usize) {
        while at > 0 {
            let parent = (at - 1) / 2;
            if !self.larger_at(at, parent) {
                return;
            }
            self.held.swap(at, parent);
            at = parent;
        }
    }

    fn sift_down(&mut self, mut at: usize) {
        loop {
            let left = 2 * at + 1;
            let right = left + 1;
            let mut largest = at;
            if self.larger_at(left, largest) {
                largest = left;
            }
            if self.larger_at(right, largest) {
                largest = right;
            }
            if largest == at {
                return;
            }
            self.held.swap(at, largest);
            at = largest;
        }
    }

    pub(crate) fn into_sorted(mut self) -> Vec<(Vec<Value>, Vec<Value>)> {
        let mut held = std::mem::take(&mut self.held);
        held.sort_unstable_by(|left, right| self.order(left, right));
        held.into_iter().map(|row| (row.keys, row.output)).collect()
    }
}

pub(crate) struct SortabilityCheck {
    first: Vec<Option<Value>>,
    failures: Vec<Option<SqlError>>,
}

impl SortabilityCheck {
    pub(crate) fn new(columns: usize) -> Self {
        Self {
            first: vec![None; columns],
            failures: (0..columns).map(|_| None).collect(),
        }
    }

    pub(crate) fn check(&mut self, order_by: &[OrderItem], keys: &[Value]) {
        for (index, ((first, failure), value)) in self
            .first
            .iter_mut()
            .zip(self.failures.iter_mut())
            .zip(keys)
            .enumerate()
        {
            if value.is_null() || failure.is_some() {
                continue;
            }
            if first.is_none() {
                *first = Some(value.clone());
                continue;
            }
            if let Some(seen) = first.as_ref()
                && let Err(error) = compare_truth(seen, value, "<")
            {
                let position = order_by.get(index).map_or(0, |item| item.expr.position);
                *failure = Some(error.at(position));
            }
        }
    }

    pub(crate) fn finish(self) -> SqlResult<()> {
        match self.failures.into_iter().flatten().next() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}
