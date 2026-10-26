use crate::btree::{expect_level, shape_of_ref};
use crate::error::StorageError;
use crate::overflow;
use crate::page_cache::{PageCache, PageSource};
use crate::tree_page::Shape;
use eggdb_platform::File;
use std::ops::Bound;

/// The order a [`Cursor`] returns entries in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScanOrder {
    /// Smallest key first.
    Ascending,
    /// Largest key first.
    Descending,
}

/// The entries of a [`BTree`](crate::BTree) with keys in a range, one at a time and in key
/// order, from [`BTree::scan`](crate::BTree::scan) or
/// [`BTree::scan_prefix`](crate::BTree::scan_prefix).
///
/// Leaves have no sibling pointers (D82), so the cursor moves from one leaf to the next
/// through the tree: having read a leaf, it looks up the leaf holding its high fence key going
/// up, or the keys just below its low fence key going down, from the root. It copies the
/// entries in range of one leaf at a time into a buffer it reuses, and holds no page between
/// calls: pages can be evicted, and the tree changed between calls.
///
/// A value on overflow pages is copied whole too, and ends the batch: the cursor copies the
/// leaf's entries up to and including the first such value, and the next batch looks the
/// leaf up again from the root, just past that value's key. So the buffer holds at most one
/// leaf of small entries and one large value, and a buffer grown past a megabyte is released
/// when its batch is done.
///
/// A change reaches the cursor when it lands in a part of the tree the cursor has not copied
/// yet; the entries it has copied are as they were when it copied them. Every page is checked
/// as for [`BTree::get`](crate::BTree::get), so a damaged page fails with an error, and every
/// batch starts strictly past the last one, so a cursor never loops.
#[derive(Debug, Clone)]
pub struct Cursor {
    root: u64,
    order: ScanOrder,
    start: Bound<Vec<u8>>,
    end: Bound<Vec<u8>>,
    seek: Seek,
    seek_key: Vec<u8>,
    bytes: Vec<u8>,
    entries: Vec<Entry>,
    position: usize,
}

pub(crate) type KeyAndValue<'a> = (&'a [u8], &'a [u8]);
type Fence<'a> = (&'a [u8], &'a [u8]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seek {
    AtOrAfter,
    After,
    AtOrBefore,
    Before,
    Last,
    Done,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    key_start: usize,
    value_start: usize,
    value_end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Copied {
    Inline,
    Overflow,
    OutOfRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Batch {
    LeafDone,
    RangeDone,
    StoppedAfterLargeValue,
}

const RETAINED_BUFFER_BYTES: usize = 1 << 20;

impl Cursor {
    pub(crate) fn new(
        root: u64,
        start: Bound<Vec<u8>>,
        end: Bound<Vec<u8>>,
        order: ScanOrder,
    ) -> Self {
        let (seek, seek_key) = match (order, &start, &end) {
            (ScanOrder::Ascending, Bound::Included(key), _) => (Seek::AtOrAfter, key.clone()),
            (ScanOrder::Ascending, Bound::Excluded(key), _) => (Seek::After, key.clone()),
            (ScanOrder::Ascending, Bound::Unbounded, _) => (Seek::AtOrAfter, Vec::new()),
            (ScanOrder::Descending, _, Bound::Included(key)) => (Seek::AtOrBefore, key.clone()),
            (ScanOrder::Descending, _, Bound::Excluded(key)) if key.is_empty() => {
                (Seek::Done, Vec::new())
            }
            (ScanOrder::Descending, _, Bound::Excluded(key)) => (Seek::Before, key.clone()),
            (ScanOrder::Descending, _, Bound::Unbounded) => (Seek::Last, Vec::new()),
        };
        Self {
            root,
            order,
            start,
            end,
            seek,
            seek_key,
            bytes: Vec::new(),
            entries: Vec::new(),
            position: 0,
        }
    }

    /// The next entry's key and value, or `None` once the range is done. The slices stay
    /// valid until the next call.
    ///
    /// An error, such as a damaged page or [`StorageError::PageBorrowed`] for a leaf held for
    /// writing, returns no entry and leaves the cursor where it was, so a later call tries the
    /// same leaf again.
    pub fn next<F: File>(
        &mut self,
        cache: &PageCache<F>,
    ) -> Result<Option<KeyAndValue<'_>>, StorageError> {
        self.next_in(cache)
    }

    pub(crate) fn next_in(
        &mut self,
        pages: &impl PageSource,
    ) -> Result<Option<KeyAndValue<'_>>, StorageError> {
        while self.position == self.entries.len() {
            if self.seek == Seek::Done {
                return Ok(None);
            }
            if self.bytes.capacity() > RETAINED_BUFFER_BYTES {
                self.bytes = Vec::new();
            }
            self.bytes.clear();
            self.entries.clear();
            self.position = 0;
            if let Err(error) = self.read_leaf(pages) {
                self.bytes.clear();
                self.entries.clear();
                return Err(error);
            }
        }
        let entry = self.entries[self.position];
        self.position += 1;
        Ok(Some((
            &self.bytes[entry.key_start..entry.value_start],
            &self.bytes[entry.value_start..entry.value_end],
        )))
    }

    fn read_leaf(&mut self, pages: &impl PageSource) -> Result<(), StorageError> {
        let leaf = self.find_leaf(pages)?;
        let shape = shape_of_ref(&leaf)?;
        let position = self.position_in(&shape, &leaf)?;
        let batch = match self.order {
            ScanOrder::Ascending => self.copy_ascending(pages, &shape, &leaf, position)?,
            ScanOrder::Descending => self.copy_descending(pages, &shape, &leaf, position)?,
        };
        self.seek_key.clear();
        if batch == Batch::StoppedAfterLargeValue {
            if let Some(last) = self.entries.last() {
                self.seek_key
                    .extend_from_slice(&self.bytes[last.key_start..last.value_start]);
            }
            self.seek = match self.order {
                ScanOrder::Ascending => Seek::After,
                ScanOrder::Descending => Seek::Before,
            };
            return Ok(());
        }
        let finished = batch == Batch::RangeDone;
        self.seek_key.extend_from_slice(shape.prefix(&leaf));
        self.seek = match self.order {
            ScanOrder::Ascending => match shape.high_suffix(&leaf) {
                Some(high) if !finished => {
                    self.seek_key.extend_from_slice(high);
                    if above_end(&self.end, &self.seek_key) {
                        Seek::Done
                    } else {
                        Seek::AtOrAfter
                    }
                }
                _ => Seek::Done,
            },
            ScanOrder::Descending => {
                self.seek_key.extend_from_slice(shape.low_suffix(&leaf));
                let start_reached = match &self.start {
                    Bound::Included(start) | Bound::Excluded(start) => *start >= self.seek_key,
                    Bound::Unbounded => false,
                };
                if finished || self.seek_key.is_empty() || start_reached {
                    Seek::Done
                } else {
                    Seek::Before
                }
            }
        };
        Ok(())
    }

    fn find_leaf<'c, P: PageSource>(&self, pages: &'c P) -> Result<P::Page<'c>, StorageError> {
        let mut page_id = self.root;
        let mut level = None;
        let mut parent: Option<(P::Page<'c>, Shape, usize)> = None;
        loop {
            let page = pages.page(page_id)?;
            let shape = shape_of_ref(&page)?;
            expect_level(&shape, level)?;
            if let Some((parent_page, parent_shape, child)) = parent.take() {
                expect_bounds(&parent_shape, &parent_page, child, &shape, &page)?;
            }
            let child = match self.seek {
                Seek::Before => {
                    let suffix = shape.suffix_below(&page, &self.seek_key)?;
                    if shape.is_leaf() {
                        return Ok(page);
                    }
                    shape.child_index_below(&page, suffix)?
                }
                Seek::Last => {
                    if shape.has_high_fence() {
                        return Err(shape
                            .corrupt("a page on the tree's right edge has a high fence key")
                            .into());
                    }
                    if shape.is_leaf() {
                        return Ok(page);
                    }
                    shape.count
                }
                _ => {
                    let suffix = shape.key_suffix(&page, &self.seek_key)?;
                    if shape.is_leaf() {
                        return Ok(page);
                    }
                    shape.child_index_for(&page, suffix)?
                }
            };
            page_id = shape.child_at(&page, child)?;
            level = Some(shape.level - 1);
            parent = Some((page, shape, child));
        }
    }

    fn position_in(&self, shape: &Shape, leaf: &[u8]) -> Result<usize, StorageError> {
        let found = match self.seek {
            Seek::Last | Seek::Done => return Ok(shape.count),
            Seek::Before => shape.search(leaf, shape.suffix_below(leaf, &self.seek_key)?)?,
            Seek::AtOrAfter | Seek::After | Seek::AtOrBefore => {
                shape.search(leaf, shape.key_suffix(leaf, &self.seek_key)?)?
            }
        };
        Ok(match (self.seek, found) {
            (Seek::After | Seek::AtOrBefore, Ok(index)) => index + 1,
            (_, Ok(index) | Err(index)) => index,
        })
    }

    fn copy_ascending(
        &mut self,
        pages: &impl PageSource,
        shape: &Shape,
        leaf: &[u8],
        first: usize,
    ) -> Result<Batch, StorageError> {
        for index in first..shape.count {
            match self.copy_entry(pages, shape, leaf, index)? {
                Copied::Inline => {}
                Copied::OutOfRange => return Ok(Batch::RangeDone),
                Copied::Overflow if index + 1 < shape.count => {
                    return Ok(Batch::StoppedAfterLargeValue);
                }
                Copied::Overflow => {}
            }
        }
        Ok(Batch::LeafDone)
    }

    fn copy_descending(
        &mut self,
        pages: &impl PageSource,
        shape: &Shape,
        leaf: &[u8],
        end: usize,
    ) -> Result<Batch, StorageError> {
        for index in (0..end).rev() {
            match self.copy_entry(pages, shape, leaf, index)? {
                Copied::Inline => {}
                Copied::OutOfRange => return Ok(Batch::RangeDone),
                Copied::Overflow if index > 0 => return Ok(Batch::StoppedAfterLargeValue),
                Copied::Overflow => {}
            }
        }
        Ok(Batch::LeafDone)
    }

    fn copy_entry(
        &mut self,
        pages: &impl PageSource,
        shape: &Shape,
        leaf: &[u8],
        index: usize,
    ) -> Result<Copied, StorageError> {
        let cell = shape.cell_at(leaf, index)?;
        let key_start = self.bytes.len();
        self.bytes.extend_from_slice(shape.prefix(leaf));
        self.bytes.extend_from_slice(cell.key);
        let key = &self.bytes[key_start..];
        let in_range = match self.order {
            ScanOrder::Ascending => !above_end(&self.end, key),
            ScanOrder::Descending => !below_start(&self.start, key),
        };
        if !in_range {
            self.bytes.truncate(key_start);
            return Ok(Copied::OutOfRange);
        }
        let value_start = self.bytes.len();
        let copied = match shape.overflow_reference(&cell)? {
            None => {
                self.bytes.extend_from_slice(cell.payload);
                Copied::Inline
            }
            Some(reference) => {
                overflow::read_value(pages, &reference, &mut self.bytes)?;
                Copied::Overflow
            }
        };
        self.entries.push(Entry {
            key_start,
            value_start,
            value_end: self.bytes.len(),
        });
        Ok(copied)
    }
}

#[cfg(test)]
impl Cursor {
    pub(crate) fn buffer_capacity(&self) -> usize {
        self.bytes.capacity()
    }
}

fn expect_bounds(
    parent: &Shape,
    parent_page: &[u8],
    child: usize,
    shape: &Shape,
    page: &[u8],
) -> Result<(), StorageError> {
    let prefix = parent.prefix(parent_page);
    let low = if child == 0 {
        (prefix, parent.low_suffix(parent_page))
    } else {
        (prefix, parent.cell_at(parent_page, child - 1)?.key)
    };
    let high = if child == parent.count {
        parent.high_suffix(parent_page).map(|high| (prefix, high))
    } else {
        Some((prefix, parent.cell_at(parent_page, child)?.key))
    };
    let own_prefix = shape.prefix(page);
    let own_low = (own_prefix, shape.low_suffix(page));
    let own_high = shape.high_suffix(page).map(|high| (own_prefix, high));
    let same_high = match (high, own_high) {
        (Some(given), Some(own)) => same_key(given, own),
        (None, None) => true,
        _ => false,
    };
    if !same_key(low, own_low) || !same_high {
        return Err(shape
            .corrupt("the fence keys differ from the bounds the parent gives")
            .into());
    }
    Ok(())
}

fn same_key((a_head, a_tail): Fence<'_>, (b_head, b_tail): Fence<'_>) -> bool {
    a_head.len() + a_tail.len() == b_head.len() + b_tail.len()
        && a_head
            .iter()
            .chain(a_tail)
            .zip(b_head.iter().chain(b_tail))
            .all(|(a, b)| a == b)
}

fn above_end(end: &Bound<Vec<u8>>, key: &[u8]) -> bool {
    match end {
        Bound::Included(end) => key > end.as_slice(),
        Bound::Excluded(end) => key >= end.as_slice(),
        Bound::Unbounded => false,
    }
}

fn below_start(start: &Bound<Vec<u8>>, key: &[u8]) -> bool {
    match start {
        Bound::Included(start) => key < start.as_slice(),
        Bound::Excluded(start) => key <= start.as_slice(),
        Bound::Unbounded => false,
    }
}
