use crate::bytes::{read_u16, read_u32, read_u64, write_u16, write_u32, write_u64};
use crate::error::FormatError;
use crate::format::{NO_PAGE, PAGE_HEADER_SIZE, PageHeader, PageType};
use std::cmp::Ordering;

pub(crate) const LAYOUT: u8 = 1;
pub(crate) const MAX_HEIGHT: u8 = 48;

const fn at(absolute: usize) -> usize {
    absolute - PAGE_HEADER_SIZE
}

const COUNT: usize = at(16);
const PREFIX_LEN: usize = at(18);
const LOW_LEN: usize = at(20);
const HIGH_LEN: usize = at(22);
const LEVEL: usize = at(24);
const FIRST_RESERVED: usize = at(25);
const CELL_START: usize = at(28);
const RIGHT_CHILD: usize = at(32);
const GARBAGE: usize = at(40);
const SECOND_RESERVED: usize = at(44);
const FENCES: usize = at(48);
const TREE_HEADER_END: usize = 48;
const SLOT: usize = 2;
const LEAF_CELL_HEADER: usize = 4;
const INNER_CELL_HEADER: usize = 10;
const OUTSIDE_VALUE_FLAG: u16 = 0x8000;

pub(crate) fn max_key_bytes(page_size: usize) -> usize {
    page_size / 8
}

pub(crate) fn max_entry_bytes(page_size: usize) -> usize {
    page_size * 3 / 16 - 18
}

pub(crate) fn header_for(level: u8, lsn: u64) -> PageHeader {
    PageHeader {
        page_type: if level == 0 {
            PageType::BTreeLeaf
        } else {
            PageType::BTreeInner
        },
        layout: LAYOUT,
        flags: 0,
        lsn,
    }
}

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

fn fence_prefix(low: &[u8], high: Option<&[u8]>) -> usize {
    high.map_or(0, |high| common_prefix(low, high))
}

pub(crate) fn shortest_separator(left: &[u8], right: &[u8]) -> Vec<u8> {
    let shared = common_prefix(left, right);
    right[..(shared + 1).min(right.len())].to_vec()
}

pub(crate) struct CellRef<'a> {
    pub(crate) key: &'a [u8],
    pub(crate) payload: &'a [u8],
    offset: usize,
    len: usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Shape {
    pub(crate) page: u64,
    pub(crate) level: u8,
    pub(crate) count: usize,
    pub(crate) prefix_len: usize,
    low_len: usize,
    high_len: Option<usize>,
    cell_start: usize,
    garbage: usize,
    right: u64,
    body_len: usize,
}

impl Shape {
    pub(crate) fn parse(page: u64, header: PageHeader, body: &[u8]) -> Result<Self, FormatError> {
        let corrupt = |problem| FormatError::CorruptTree { page, problem };
        if body.len() < FENCES {
            return Err(corrupt("the page is too small for a tree page"));
        }
        let level = body[LEVEL];
        let expected_type = if level == 0 {
            PageType::BTreeLeaf
        } else {
            PageType::BTreeInner
        };
        if header.page_type != expected_type {
            return Err(corrupt("the page type does not match the page's level"));
        }
        if header.layout != LAYOUT {
            return Err(corrupt("the page has an unknown layout"));
        }
        if level >= MAX_HEIGHT {
            return Err(corrupt("the page's level is above the height limit"));
        }
        if body[FIRST_RESERVED..CELL_START] != [0; 3] || body[SECOND_RESERVED..FENCES] != [0; 4] {
            return Err(corrupt("a reserved field is not zero"));
        }
        let page_size = body.len() + PAGE_HEADER_SIZE;
        let max_key = max_key_bytes(page_size);
        let count = usize::from(read_u16(body, COUNT));
        let prefix_len = usize::from(read_u16(body, PREFIX_LEN));
        let low_len = usize::from(read_u16(body, LOW_LEN));
        let high_len = match read_u16(body, HIGH_LEN) {
            0 => None,
            len => Some(usize::from(len)),
        };
        if low_len > max_key || high_len.is_some_and(|len| len > max_key) {
            return Err(corrupt("a fence key is longer than the key limit"));
        }
        if prefix_len > low_len || prefix_len > high_len.unwrap_or(0) {
            return Err(corrupt("the prefix is longer than a fence key"));
        }
        let cell_start = (read_u32(body, CELL_START) as usize)
            .checked_sub(PAGE_HEADER_SIZE)
            .ok_or(corrupt("the cell area starts inside the page header"))?;
        let right = read_u64(body, RIGHT_CHILD);
        if (level == 0) != (right == NO_PAGE) {
            return Err(corrupt(
                "the rightmost child does not match the page's level",
            ));
        }
        let shape = Self {
            page,
            level,
            count,
            prefix_len,
            low_len,
            high_len,
            cell_start,
            garbage: read_u32(body, GARBAGE) as usize,
            right,
            body_len: body.len(),
        };
        if shape.slots_end() > cell_start || cell_start > body.len() {
            return Err(corrupt(
                "the slots and the cell area overlap or leave the page",
            ));
        }
        if shape.garbage > body.len() - cell_start {
            return Err(corrupt("the unused cell bytes exceed the cell area"));
        }
        Ok(shape)
    }

    pub(crate) fn corrupt(&self, problem: &'static str) -> FormatError {
        FormatError::CorruptTree {
            page: self.page,
            problem,
        }
    }

    pub(crate) fn is_leaf(&self) -> bool {
        self.level == 0
    }

    pub(crate) fn has_fences(&self) -> bool {
        self.low_len != 0 || self.high_len.is_some()
    }

    fn high_suffix_len(&self) -> usize {
        self.high_len.map_or(0, |len| len - self.prefix_len)
    }

    fn slots_start(&self) -> usize {
        FENCES + self.low_len + self.high_suffix_len()
    }

    fn slots_end(&self) -> usize {
        self.slots_start() + self.count * SLOT
    }

    fn free(&self) -> usize {
        self.cell_start - self.slots_end()
    }

    pub(crate) fn prefix<'a>(&self, body: &'a [u8]) -> &'a [u8] {
        &body[FENCES..FENCES + self.prefix_len]
    }

    pub(crate) fn low_suffix<'a>(&self, body: &'a [u8]) -> &'a [u8] {
        &body[FENCES + self.prefix_len..FENCES + self.low_len]
    }

    pub(crate) fn high_suffix<'a>(&self, body: &'a [u8]) -> Option<&'a [u8]> {
        let start = FENCES + self.low_len;
        self.high_len
            .map(|_| &body[start..start + self.high_suffix_len()])
    }

    pub(crate) fn key_suffix<'k>(
        &self,
        body: &[u8],
        key: &'k [u8],
    ) -> Result<&'k [u8], FormatError> {
        let outside = || self.corrupt("the key lies outside the page's fence keys");
        if key.get(..self.prefix_len) != Some(self.prefix(body)) {
            return Err(outside());
        }
        let suffix = &key[self.prefix_len..];
        if suffix < self.low_suffix(body) {
            return Err(outside());
        }
        if self.high_suffix(body).is_some_and(|high| suffix >= high) {
            return Err(outside());
        }
        Ok(suffix)
    }

    pub(crate) fn cell_at<'a>(
        &self,
        body: &'a [u8],
        index: usize,
    ) -> Result<CellRef<'a>, FormatError> {
        let slot = self.slots_start() + index * SLOT;
        if index >= self.count {
            return Err(self.corrupt("a slot past the slot count was read"));
        }
        let outside = || self.corrupt("a cell lies outside the cell area");
        let offset = usize::from(read_u16(body, slot))
            .checked_sub(PAGE_HEADER_SIZE)
            .filter(|&offset| offset >= self.cell_start)
            .ok_or_else(outside)?;
        let header_len = if self.is_leaf() {
            LEAF_CELL_HEADER
        } else {
            INNER_CELL_HEADER
        };
        let header = body.get(offset..offset + header_len).ok_or_else(outside)?;
        let key_len = usize::from(read_u16(header, 0));
        let payload_len = if self.is_leaf() {
            let value_len = read_u16(header, 2);
            if value_len & OUTSIDE_VALUE_FLAG != 0 {
                return Err(self.corrupt("a value is flagged as stored outside the tree"));
            }
            usize::from(value_len)
        } else {
            0
        };
        let key_start = offset + header_len;
        let len = header_len + key_len + payload_len;
        let key = body
            .get(key_start..key_start + key_len)
            .ok_or_else(outside)?;
        let payload = if self.is_leaf() {
            body.get(key_start + key_len..offset + len)
                .ok_or_else(outside)?
        } else {
            &header[2..10]
        };
        Ok(CellRef {
            key,
            payload,
            offset,
            len,
        })
    }

    pub(crate) fn child_at(&self, body: &[u8], index: usize) -> Result<u64, FormatError> {
        if index == self.count {
            return Ok(self.right);
        }
        let child = read_u64(self.cell_at(body, index)?.payload, 0);
        if child == NO_PAGE {
            return Err(self.corrupt("a child pointer is zero"));
        }
        Ok(child)
    }

    pub(crate) fn search(
        &self,
        body: &[u8],
        suffix: &[u8],
    ) -> Result<Result<usize, usize>, FormatError> {
        let (mut low, mut high) = (0, self.count);
        while low < high {
            let middle = low + (high - low) / 2;
            match self.cell_at(body, middle)?.key.cmp(suffix) {
                Ordering::Less => low = middle + 1,
                Ordering::Greater => high = middle,
                Ordering::Equal => return Ok(Ok(middle)),
            }
        }
        Ok(Err(low))
    }

    pub(crate) fn child_for(&self, body: &[u8], suffix: &[u8]) -> Result<u64, FormatError> {
        let index = match self.search(body, suffix)? {
            Ok(index) => index + 1,
            Err(index) => index,
        };
        self.child_at(body, index)
    }

    pub(crate) fn validate(&self, body: &[u8]) -> Result<(), FormatError> {
        let low = self.low_suffix(body);
        if let Some(high) = self.high_suffix(body) {
            let ordered = match (low.first(), high.first()) {
                (None, Some(_)) => true,
                (Some(low), Some(high)) => low < high,
                _ => false,
            };
            if !ordered {
                return Err(
                    self.corrupt("the fence keys are out of order or share more than the prefix")
                );
            }
        }
        let page_size = self.body_len + PAGE_HEADER_SIZE;
        let max_key = max_key_bytes(page_size);
        let max_entry = max_entry_bytes(page_size);
        let mut cells = Vec::new();
        cells
            .try_reserve_exact(self.count)
            .map_err(|_| self.corrupt("the page has too many slots"))?;
        let mut previous: Option<&[u8]> = None;
        for index in 0..self.count {
            let cell = self.cell_at(body, index)?;
            let key_len = self.prefix_len + cell.key.len();
            if key_len > max_key {
                return Err(self.corrupt("a key is longer than the key limit"));
            }
            if self.is_leaf() {
                if key_len + cell.payload.len() > max_entry {
                    return Err(self.corrupt("an entry is longer than the entry limit"));
                }
            } else if read_u64(cell.payload, 0) == NO_PAGE {
                return Err(self.corrupt("a child pointer is zero"));
            }
            let in_order = match previous {
                Some(previous) => previous < cell.key,
                None if self.is_leaf() => cell.key >= low,
                None => cell.key > low,
            };
            if !in_order {
                return Err(self.corrupt(
                    "the keys are not in strictly increasing order within the fence keys",
                ));
            }
            previous = Some(cell.key);
            cells.push((cell.offset, cell.len));
        }
        if let (Some(last), Some(high)) = (previous, self.high_suffix(body))
            && last >= high
        {
            return Err(self.corrupt("a key is not below the high fence key"));
        }
        cells.sort_unstable();
        let mut end = self.cell_start;
        let mut used = 0;
        for (offset, len) in cells {
            if offset < end {
                return Err(self.corrupt("two cells overlap"));
            }
            end = offset + len;
            used += len;
        }
        if used + self.garbage != self.body_len - self.cell_start {
            return Err(self.corrupt("the cell area's used and unused bytes do not add up"));
        }
        Ok(())
    }

    pub(crate) fn leaf_fits(&self, suffix_len: usize, value_len: usize) -> bool {
        self.free() >= SLOT + LEAF_CELL_HEADER + suffix_len + value_len
    }

    pub(crate) fn inner_fits(&self, suffix_len: usize) -> bool {
        self.free() >= SLOT + INNER_CELL_HEADER + suffix_len
    }

    pub(crate) fn insert_leaf(
        &mut self,
        body: &mut [u8],
        index: usize,
        suffix: &[u8],
        value: &[u8],
    ) {
        let len = LEAF_CELL_HEADER + suffix.len() + value.len();
        let offset = self.open_slot(body, index, len);
        write_u16(body, offset, suffix.len() as u16);
        write_u16(body, offset + 2, value.len() as u16);
        let key_start = offset + LEAF_CELL_HEADER;
        body[key_start..key_start + suffix.len()].copy_from_slice(suffix);
        body[key_start + suffix.len()..offset + len].copy_from_slice(value);
    }

    pub(crate) fn insert_inner(
        &mut self,
        body: &mut [u8],
        index: usize,
        suffix: &[u8],
        child: u64,
    ) {
        let len = INNER_CELL_HEADER + suffix.len();
        let offset = self.open_slot(body, index, len);
        write_u16(body, offset, suffix.len() as u16);
        write_u64(body, offset + 2, child);
        body[offset + INNER_CELL_HEADER..offset + len].copy_from_slice(suffix);
    }

    fn open_slot(&mut self, body: &mut [u8], index: usize, len: usize) -> usize {
        self.cell_start -= len;
        let slot = self.slots_start() + index * SLOT;
        body.copy_within(slot..self.slots_end(), slot + SLOT);
        write_u16(body, slot, (self.cell_start + PAGE_HEADER_SIZE) as u16);
        self.count += 1;
        self.store_header(body);
        self.cell_start
    }

    pub(crate) fn remove(&mut self, body: &mut [u8], index: usize) -> Result<(), FormatError> {
        let cell = self.cell_at(body, index)?;
        let (offset, len) = (cell.offset, cell.len);
        body[offset..offset + len].fill(0);
        let slot = self.slots_start() + index * SLOT;
        body.copy_within(slot + SLOT..self.slots_end(), slot);
        self.count -= 1;
        let end = self.slots_end();
        body[end..end + SLOT].fill(0);
        if offset == self.cell_start {
            self.cell_start += len;
        } else {
            self.garbage += len;
        }
        self.store_header(body);
        Ok(())
    }

    pub(crate) fn overwrite_value(
        &self,
        body: &mut [u8],
        index: usize,
        value: &[u8],
    ) -> Result<(), FormatError> {
        let cell = self.cell_at(body, index)?;
        if cell.payload.len() != value.len() {
            return Err(self.corrupt("a value was overwritten with one of another length"));
        }
        let start = cell.offset + cell.len - value.len();
        body[start..start + value.len()].copy_from_slice(value);
        Ok(())
    }

    pub(crate) fn set_child(
        &mut self,
        body: &mut [u8],
        index: usize,
        child: u64,
    ) -> Result<(), FormatError> {
        if index == self.count {
            self.right = child;
            write_u64(body, RIGHT_CHILD, child);
            return Ok(());
        }
        let offset = self.cell_at(body, index)?.offset;
        write_u64(body, offset + 2, child);
        Ok(())
    }

    fn store_header(&self, body: &mut [u8]) {
        write_u16(body, COUNT, self.count as u16);
        write_u32(
            body,
            CELL_START,
            (self.cell_start + PAGE_HEADER_SIZE) as u32,
        );
        write_u32(body, GARBAGE, self.garbage as u32);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Node {
    pub(crate) level: u8,
    pub(crate) low: Vec<u8>,
    pub(crate) high: Option<Vec<u8>>,
    pub(crate) keys: Vec<Vec<u8>>,
    pub(crate) values: Vec<Vec<u8>>,
    pub(crate) children: Vec<u64>,
}

pub(crate) struct Split {
    pub(crate) left: Node,
    pub(crate) separator: Vec<u8>,
    pub(crate) right: Node,
}

impl Node {
    pub(crate) fn empty_leaf() -> Self {
        Self {
            level: 0,
            low: Vec::new(),
            high: None,
            keys: Vec::new(),
            values: Vec::new(),
            children: Vec::new(),
        }
    }

    pub(crate) fn decode(shape: &Shape, body: &[u8]) -> Result<Self, FormatError> {
        let prefix = shape.prefix(body);
        let with_prefix = |suffix: &[u8]| [prefix, suffix].concat();
        let mut node = Self {
            level: shape.level,
            low: with_prefix(shape.low_suffix(body)),
            high: shape.high_suffix(body).map(with_prefix),
            keys: Vec::with_capacity(shape.count + 1),
            values: Vec::new(),
            children: Vec::new(),
        };
        for index in 0..shape.count {
            let cell = shape.cell_at(body, index)?;
            node.keys.push(with_prefix(cell.key));
            if shape.is_leaf() {
                node.values.push(cell.payload.to_vec());
            } else {
                node.children.push(shape.child_at(body, index)?);
            }
        }
        if !shape.is_leaf() {
            node.children.push(shape.right);
        }
        Ok(node)
    }

    pub(crate) fn size(&self) -> usize {
        size_of(
            self.level,
            &self.low,
            self.high.as_deref(),
            &self.keys,
            &self.values,
        )
    }

    pub(crate) fn encode(&self, page: u64, body: &mut [u8]) -> Result<(), FormatError> {
        let corrupt = |problem| FormatError::CorruptTree { page, problem };
        if self.size() > body.len() + PAGE_HEADER_SIZE {
            return Err(corrupt("the page's entries do not fit in it"));
        }
        let prefix = fence_prefix(&self.low, self.high.as_deref());
        if self
            .keys
            .iter()
            .any(|key| key.get(..prefix) != Some(&self.low[..prefix]))
        {
            return Err(corrupt("a key does not share the page's prefix"));
        }
        body[..FENCES].fill(0);
        write_u16(body, COUNT, self.keys.len() as u16);
        write_u16(body, PREFIX_LEN, prefix as u16);
        write_u16(body, LOW_LEN, self.low.len() as u16);
        write_u16(
            body,
            HIGH_LEN,
            self.high.as_ref().map_or(0, |high| high.len() as u16),
        );
        body[LEVEL] = self.level;
        if self.level != 0 {
            write_u64(
                body,
                RIGHT_CHILD,
                self.children.last().copied().unwrap_or(NO_PAGE),
            );
        }
        let mut at = FENCES;
        for part in [
            &self.low[..],
            self.high.as_deref().map_or(&[][..], |high| &high[prefix..]),
        ] {
            body[at..at + part.len()].copy_from_slice(part);
            at += part.len();
        }
        let slots = at;
        let mut cell_start = body.len();
        for (index, key) in self.keys.iter().enumerate() {
            let suffix = &key[prefix..];
            if self.level == 0 {
                let value = &self.values[index];
                cell_start -= LEAF_CELL_HEADER + suffix.len() + value.len();
                write_u16(body, cell_start, suffix.len() as u16);
                write_u16(body, cell_start + 2, value.len() as u16);
                let key_start = cell_start + LEAF_CELL_HEADER;
                body[key_start..key_start + suffix.len()].copy_from_slice(suffix);
                body[key_start + suffix.len()..key_start + suffix.len() + value.len()]
                    .copy_from_slice(value);
            } else {
                cell_start -= INNER_CELL_HEADER + suffix.len();
                write_u16(body, cell_start, suffix.len() as u16);
                write_u64(body, cell_start + 2, self.children[index]);
                body[cell_start + INNER_CELL_HEADER..cell_start + INNER_CELL_HEADER + suffix.len()]
                    .copy_from_slice(suffix);
            }
            write_u16(
                body,
                slots + index * SLOT,
                (cell_start + PAGE_HEADER_SIZE) as u16,
            );
        }
        body[slots + self.keys.len() * SLOT..cell_start].fill(0);
        write_u32(body, CELL_START, (cell_start + PAGE_HEADER_SIZE) as u32);
        Ok(())
    }

    pub(crate) fn split(&self, page_size: usize, inserted: usize) -> Option<Split> {
        let count = self.keys.len();
        if count < 2 {
            return None;
        }
        let mut candidates = Vec::with_capacity(count + 2);
        if self.level == 0 {
            if inserted + 1 == count && self.high.is_none() {
                candidates.push(count - 1);
            }
            if inserted == 0 && self.low.is_empty() {
                candidates.push(1);
            }
        }
        let middle = self.middle().clamp(1, count - 1);
        candidates.push(middle);
        candidates.extend(1..count);
        if self.level != 0 {
            candidates.push(0);
        }
        candidates
            .into_iter()
            .find_map(|at| self.split_at(at, page_size))
    }

    fn middle(&self) -> usize {
        let sizes: Vec<usize> = self
            .keys
            .iter()
            .enumerate()
            .map(|(index, key)| key.len() + self.values.get(index).map_or(8, Vec::len))
            .collect();
        let total: usize = sizes.iter().sum();
        let mut running = 0;
        for (index, size) in sizes.iter().enumerate() {
            running += size;
            if running * 2 >= total {
                return index + 1;
            }
        }
        self.keys.len() / 2
    }

    fn split_at(&self, at: usize, page_size: usize) -> Option<Split> {
        let (left, separator, right) = if self.level == 0 {
            let separator = shortest_separator(&self.keys[at - 1], &self.keys[at]);
            let left = self.part(self.low.clone(), Some(separator.clone()), 0..at, Vec::new());
            let right = self.part(
                separator.clone(),
                self.high.clone(),
                at..self.keys.len(),
                Vec::new(),
            );
            (left, separator, right)
        } else {
            let separator = self.keys[at].clone();
            let left = self.part(
                self.low.clone(),
                Some(separator.clone()),
                0..at,
                self.children[..=at].to_vec(),
            );
            let right = self.part(
                separator.clone(),
                self.high.clone(),
                at + 1..self.keys.len(),
                self.children[at + 1..].to_vec(),
            );
            (left, separator, right)
        };
        (left.size() <= page_size && right.size() <= page_size).then_some(Split {
            left,
            separator,
            right,
        })
    }

    fn part(
        &self,
        low: Vec<u8>,
        high: Option<Vec<u8>>,
        range: std::ops::Range<usize>,
        children: Vec<u64>,
    ) -> Self {
        Self {
            level: self.level,
            low,
            high,
            keys: self.keys[range.clone()].to_vec(),
            values: if self.level == 0 {
                self.values[range].to_vec()
            } else {
                Vec::new()
            },
            children,
        }
    }
}

fn size_of(
    level: u8,
    low: &[u8],
    high: Option<&[u8]>,
    keys: &[Vec<u8>],
    values: &[Vec<u8>],
) -> usize {
    let prefix = fence_prefix(low, high);
    let fences = low.len() + high.map_or(0, |high| high.len() - prefix);
    let cells: usize = keys
        .iter()
        .enumerate()
        .map(|(index, key)| {
            let suffix = key.len().saturating_sub(prefix);
            if level == 0 {
                SLOT + LEAF_CELL_HEADER + suffix + values.get(index).map_or(0, Vec::len)
            } else {
                SLOT + INNER_CELL_HEADER + suffix
            }
        })
        .sum();
    TREE_HEADER_END + fences + cells
}
