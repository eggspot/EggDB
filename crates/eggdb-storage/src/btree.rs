use crate::cursor::{Cursor, ScanOrder};
use crate::error::{FormatError, StorageError};
use crate::format::{NO_PAGE, PageSize};
use crate::overflow;
use crate::page_cache::{PageCache, PageMut, PageSource, TreePage};
use crate::page_set::PageSet;
use crate::tree_page::{self, CellRef, LeafValue, MAX_HEIGHT, Node, Shape, StoredValue};
use eggdb_platform::File;
use std::ops::Bound;

/// A B+tree of byte-string keys and values in the pages of a [`PageCache`] (D77, format.md
/// "B+tree pages").
///
/// Keys compare as unsigned bytes (key scheme 0). Pages are slotted: a sorted array of
/// two-byte slots at the front, searched by binary search, points at cells packed from the
/// back. Each page keeps a low and a high fence key bounding the keys it may hold, and stores
/// the prefix the two share only once, so its keys are stored without it. Leaf splits choose
/// the shortest separator between the halves, and deletes merge pages left less than a quarter
/// full with the page beside them (D83). Leaves have no sibling pointers: a [`Cursor`] moves
/// between them through the tree, using their fence keys.
///
/// A value too long to fit in a leaf beside its key, longer together with it than
/// [`BTree::max_inline_entry_bytes`], is stored on a chain of overflow pages of its own, and its
/// leaf entry holds the key and a 16-byte reference to the chain (format.md, "Overflow pages").
/// A value is at most [`BTree::MAX_VALUE_BYTES`] long; keys always stay in the leaves.
///
/// The root page stays the same for the tree's life: a root split moves its entries into two
/// new pages and turns the root into an inner page one level up. So the page that names the
/// tree is written once, when the tree is created.
///
/// Every change goes through the cache and reaches the file with its next commit, through the
/// write-ahead log. Every page is checked in full when the cache reads it from the file, and
/// every step of a search checks that the child sits one level below its parent and that the
/// key lies within the page's fence keys, so a damaged or hostile file returns
/// [`FormatError::CorruptTree`] instead of a wrong answer, a panic or a loop (D9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BTree {
    root: u64,
}

/// What [`BTree::check`] found in a tree whose every page passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct TreeReport {
    /// Levels from the root to the leaves, both included.
    pub height: u32,
    /// Inner pages, the root included when it is one.
    pub inner_pages: u64,
    /// Leaf pages.
    pub leaf_pages: u64,
    /// Entries in the leaves.
    pub entries: u64,
    /// Bytes of the leaves' keys, as given to [`BTree::put`].
    pub key_bytes: u64,
    /// Bytes of the values, those on overflow pages included.
    pub value_bytes: u64,
    /// Values stored on overflow pages.
    pub overflow_values: u64,
    /// Overflow pages holding those values.
    pub overflow_pages: u64,
}

/// A value found by a lookup: borrowed from its leaf, or assembled from its overflow pages.
pub(crate) enum FoundValue<'a> {
    Borrowed(&'a [u8]),
    Assembled(Vec<u8>),
}

impl FoundValue<'_> {
    pub(crate) fn as_slice(&self) -> &[u8] {
        match self {
            Self::Borrowed(value) => value,
            Self::Assembled(value) => value,
        }
    }

    pub(crate) fn into_vec(self) -> Vec<u8> {
        match self {
            Self::Borrowed(value) => value.to_vec(),
            Self::Assembled(value) => value,
        }
    }
}

impl BTree {
    /// The longest value a tree stores, in bytes: 1 GiB, whatever the page size. A value is
    /// read and written whole, so it takes its length in memory while it is used.
    pub const MAX_VALUE_BYTES: usize = overflow::MAX_VALUE_BYTES;

    /// Creates an empty tree in a newly allocated page, its root for the tree's life. The
    /// caller records [`BTree::root`] where it will find the tree again, such as the
    /// superblock's `catalog_root`, in the same commit.
    pub fn create<F: File>(cache: &PageCache<F>) -> Result<Self, StorageError> {
        let root = new_page(cache, &Node::empty_leaf())?;
        Ok(Self { root })
    }

    /// Opens the tree whose root is page `root`, checking that the page is a tree root.
    pub fn open<F: File>(cache: &PageCache<F>, root: u64) -> Result<Self, StorageError> {
        Self::open_in(cache, root)
    }

    pub(crate) fn open_in(pages: &impl PageSource, root: u64) -> Result<Self, StorageError> {
        if root == NO_PAGE {
            return Err(FormatError::CorruptTree {
                page: root,
                problem: "the tree has no root page",
            }
            .into());
        }
        let page = pages.page(root)?;
        expect_level(&shape_of_ref(&page)?, None)?;
        Ok(Self { root })
    }

    /// The tree's root page.
    pub fn root(&self) -> u64 {
        self.root
    }

    /// The longest key a tree stores with pages of `page_size`: an eighth of a page, 512
    /// bytes with 4 KiB pages and 8 KiB with 64 KiB pages.
    pub fn max_key_bytes(page_size: PageSize) -> usize {
        tree_page::max_key_bytes(page_size.bytes())
    }

    /// The most bytes a key and its value may have together and still be stored in a leaf
    /// with pages of `page_size`, so that at least four entries fit in a leaf: 750 bytes with
    /// 4 KiB pages and 12,270 with 64 KiB pages. A longer value goes to overflow pages.
    pub fn max_inline_entry_bytes(page_size: PageSize) -> usize {
        tree_page::max_inline_entry_bytes(page_size.bytes())
    }

    /// Checks that [`BTree::put`] would accept `key` and `value` in `cache`, without changing
    /// anything: a key over [`BTree::max_key_bytes`] fails with [`StorageError::KeyTooLarge`],
    /// a value over [`BTree::MAX_VALUE_BYTES`] with [`StorageError::ValueTooLarge`], and a value
    /// whose overflow pages and leaf need more pages than the cache has frames with
    /// [`StorageError::ValueExceedsCache`].
    pub fn check_fits<F: File>(
        cache: &PageCache<F>,
        key: &[u8],
        value: &[u8],
    ) -> Result<(), StorageError> {
        let page_size = cache.page_size();
        let max_key = Self::max_key_bytes(page_size);
        if key.len() > max_key {
            return Err(StorageError::KeyTooLarge {
                bytes: key.len(),
                max: max_key,
            });
        }
        if value.len() > Self::MAX_VALUE_BYTES {
            return Err(StorageError::ValueTooLarge {
                bytes: value.len(),
                max: Self::MAX_VALUE_BYTES,
            });
        }
        if fits_inline(page_size, key, value) {
            return Ok(());
        }
        let pages = overflow::pages_for(value.len() as u64, page_size) + 1;
        let frames = cache.frame_count();
        if pages > frames as u64 {
            return Err(StorageError::ValueExceedsCache {
                bytes: value.len(),
                pages,
                frames,
            });
        }
        Ok(())
    }

    /// The value stored under `key`, copied out, or `None`.
    pub fn get<F: File>(
        &self,
        cache: &PageCache<F>,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, StorageError> {
        self.get_in(cache, key, |value| value.into_vec())
    }

    /// Calls `read` with the value stored under `key`, and returns what it returns, or `None`
    /// when the key is absent. A value stored in its leaf is read where it lies while the page
    /// is pinned, with nothing copied or allocated; a value on overflow pages is first copied
    /// whole into a buffer.
    pub fn get_with<F: File, R>(
        &self,
        cache: &PageCache<F>,
        key: &[u8],
        read: impl FnOnce(&[u8]) -> R,
    ) -> Result<Option<R>, StorageError> {
        self.get_in(cache, key, |value| read(value.as_slice()))
    }

    pub(crate) fn get_in<R>(
        &self,
        pages: &impl PageSource,
        key: &[u8],
        read: impl FnOnce(FoundValue<'_>) -> R,
    ) -> Result<Option<R>, StorageError> {
        let mut page_id = self.root;
        let mut level = None;
        loop {
            let page = pages.page(page_id)?;
            let shape = shape_of_ref(&page)?;
            expect_level(&shape, level)?;
            let suffix = shape.key_suffix(&page, key)?;
            if shape.is_leaf() {
                let Ok(index) = shape.search(&page, suffix)? else {
                    return Ok(None);
                };
                let cell = shape.cell_at(&page, index)?;
                if !cell.overflow {
                    return Ok(Some(read(FoundValue::Borrowed(cell.payload))));
                }
                let value = assembled_value(pages, &shape, &cell)?;
                return Ok(Some(read(FoundValue::Assembled(value))));
            }
            page_id = shape.child_for(&page, suffix)?;
            level = Some(shape.level - 1);
        }
    }

    /// Stores `value` under `key`, inserting it or replacing the value there. Pages split as
    /// needed, up to the root. A value that does not fit in a leaf beside its key goes to a new
    /// chain of overflow pages, and the chain of a value it replaces goes back to the free
    /// list, to be reused after the commit.
    ///
    /// A key or value refused by [`BTree::check_fits`] fails before anything changes. Any other
    /// error, such as [`StorageError::CacheFull`] in the middle of a split, can leave the
    /// transaction's changes half made: roll the transaction back, as
    /// [`WriteTransaction::put`](crate::WriteTransaction::put) does by itself.
    pub fn put<F: File>(
        &self,
        cache: &PageCache<F>,
        key: &[u8],
        value: &[u8],
    ) -> Result<(), StorageError> {
        Self::check_fits(cache, key, value)?;
        let written = if fits_inline(cache.page_size(), key, value) {
            None
        } else {
            Some(overflow::write_value(cache, value)?)
        };
        let stored = match &written {
            None => LeafValue::Inline(value),
            Some(reference) => LeafValue::Overflow(reference.encode()),
        };
        let mut path = [NO_PAGE; MAX_HEIGHT as usize];
        let mut depth = 0;
        let mut page_id = self.root;
        let mut level = None;
        loop {
            path[depth] = page_id;
            let page = cache.read(page_id)?;
            let shape = shape_of_ref(&page)?;
            expect_level(&shape, level)?;
            let suffix = shape.key_suffix(&page, key)?;
            if shape.is_leaf() {
                break;
            }
            page_id = shape.child_for(&page, suffix)?;
            level = Some(shape.level - 1);
            depth += 1;
        }
        let mut page = cache.write_keeping_check(page_id)?;
        let mut shape = shape_of_mut(&page)?;
        let suffix = shape.key_suffix(&page, key)?;
        let mut replaced = None;
        let index = match shape.search(&page, suffix)? {
            Ok(index) => {
                let cell = shape.cell_at(&page, index)?;
                replaced = shape.overflow_reference(&cell)?;
                if written.is_none() && replaced.is_none() && cell.payload.len() == value.len() {
                    shape.overwrite_value(&mut page, index, value)?;
                    return Ok(());
                }
                shape.remove(&mut page, index)?;
                index
            }
            Err(index) => index,
        };
        if shape.leaf_fits(suffix.len(), &stored) {
            shape.insert_leaf(&mut page, index, suffix, &stored);
            drop(page);
        } else {
            let mut node = Node::decode(&shape, &page)?;
            node.keys.insert(index, key.to_vec());
            node.values.insert(
                index,
                match written {
                    None => StoredValue::Inline(value.to_vec()),
                    Some(reference) => StoredValue::Overflow(reference),
                },
            );
            store(cache, &path[..=depth], page, node, index)?;
        }
        match replaced {
            Some(reference) => overflow::free_value(cache, &reference),
            None => Ok(()),
        }
    }

    /// Removes `key` and its value, and returns whether the key was there. A key that is not
    /// there changes nothing.
    ///
    /// A page left less than a quarter full is merged with the page beside it under the same
    /// parent, the left one first, when the two fit in one page; the emptied page goes back to
    /// the file's free list, and merges go on up the tree while parents are left under-filled.
    /// A root left with a single child takes that child's entries, so the tree gets shorter.
    /// A delete never allocates a page and never splits one. As with [`BTree::put`], an error
    /// such as [`StorageError::CacheFull`] in the middle of a merge can leave the
    /// transaction's changes half made: roll the transaction back, as
    /// [`WriteTransaction::delete`](crate::WriteTransaction::delete) does by itself.
    pub fn delete<F: File>(&self, cache: &PageCache<F>, key: &[u8]) -> Result<bool, StorageError> {
        if key.len() > Self::max_key_bytes(cache.page_size()) {
            return Ok(false);
        }
        let mut path = [Step::default(); MAX_HEIGHT as usize];
        let mut depth = 0;
        let mut page_id = self.root;
        let mut level = None;
        loop {
            let page = cache.read(page_id)?;
            let shape = shape_of_ref(&page)?;
            expect_level(&shape, level)?;
            let suffix = shape.key_suffix(&page, key)?;
            if shape.is_leaf() {
                if shape.search(&page, suffix)?.is_err() {
                    return Ok(false);
                }
                break;
            }
            let child = shape.child_index_for(&page, suffix)?;
            path[depth] = Step {
                page: page_id,
                child,
            };
            page_id = shape.child_at(&page, child)?;
            level = Some(shape.level - 1);
            depth += 1;
        }
        let removed = {
            let mut page = cache.write_keeping_check(page_id)?;
            let mut shape = shape_of_mut(&page)?;
            let suffix = shape.key_suffix(&page, key)?;
            let Ok(index) = shape.search(&page, suffix)? else {
                return Err(shape
                    .corrupt("a key found in a leaf is gone when the leaf is changed")
                    .into());
            };
            let removed = shape.overflow_reference(&shape.cell_at(&page, index)?)?;
            shape.remove(&mut page, index)?;
            removed
        };
        if let Some(reference) = removed {
            overflow::free_value(cache, &reference)?;
        }
        rebalance(cache, &path[..depth], page_id)?;
        Ok(true)
    }

    /// A cursor over the entries with keys from `start` to `end`, in `order`. It reads the
    /// tree as [`Cursor::next`] is called, and holds no page between calls. A range with
    /// `start` above `end` holds no entries.
    pub fn scan(&self, start: Bound<&[u8]>, end: Bound<&[u8]>, order: ScanOrder) -> Cursor {
        Cursor::new(
            self.root,
            start.map(<[u8]>::to_vec),
            end.map(<[u8]>::to_vec),
            order,
        )
    }

    /// A cursor over the entries whose keys start with `prefix`, in `order`. The empty prefix
    /// scans the whole tree.
    pub fn scan_prefix(&self, prefix: &[u8], order: ScanOrder) -> Cursor {
        Cursor::new(
            self.root,
            Bound::Included(prefix.to_vec()),
            keys_after_prefix(prefix),
            order,
        )
    }

    /// Checks every page of the tree and how they fit together, and counts what it holds.
    /// This is what `eggdb check` runs for each tree.
    ///
    /// Each page must pass the checks of a page read from the file (its layout, slots inside
    /// the page, cells that don't overlap and account for every byte, keys in strictly
    /// increasing order within its fence keys, sizes within the limits). Beyond those, each
    /// child must sit one level below its parent, with fence keys equal to the bounds its
    /// parent gives it. No two children anywhere get the same bounds, so a page reached twice
    /// fails that check, and the walk reads each page at most once. The first page that fails
    /// is reported as [`FormatError::CorruptTree`].
    ///
    /// Every chain of overflow pages is read whole: each page must be an overflow page of
    /// that value, at its place in the chain, and the chain must end exactly where the value
    /// does. A page reached twice, by two values or by a value and the tree, fails with
    /// [`FormatError::CorruptOverflow`]. The pages reached are remembered in a set of one bit
    /// per page of the file.
    pub fn check<F: File>(&self, cache: &PageCache<F>) -> Result<TreeReport, StorageError> {
        self.check_in(cache)
    }

    pub(crate) fn check_in(&self, pages: &impl PageSource) -> Result<TreeReport, StorageError> {
        let mut used = PageSet::for_pages(pages.page_count())?;
        self.check_marking(pages, &mut used)
    }

    pub(crate) fn check_marking(
        &self,
        pages: &impl PageSource,
        used: &mut PageSet,
    ) -> Result<TreeReport, StorageError> {
        let mut report = TreeReport::default();
        let bounds = Bounds {
            low: Vec::new(),
            high: None,
        };
        let mut walk = CheckWalk {
            pages,
            used,
            report: &mut report,
        };
        walk.check_page(self.root, None, &bounds)?;
        Ok(report)
    }
}

#[cold]
#[inline(never)]
fn assembled_value(
    pages: &impl PageSource,
    shape: &Shape,
    cell: &CellRef<'_>,
) -> Result<Vec<u8>, StorageError> {
    let mut value = Vec::new();
    if let Some(reference) = shape.overflow_reference(cell)? {
        overflow::read_value(pages, &reference, &mut value)?;
    }
    Ok(value)
}

fn fits_inline(page_size: PageSize, key: &[u8], value: &[u8]) -> bool {
    key.len() + value.len() <= BTree::max_inline_entry_bytes(page_size)
}

struct Bounds {
    low: Vec<u8>,
    high: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, Default)]
struct Step {
    page: u64,
    child: usize,
}

const UNDER_FILLED_DIVISOR: usize = 4;

fn keys_after_prefix(prefix: &[u8]) -> Bound<Vec<u8>> {
    let Some(last) = prefix.iter().rposition(|&byte| byte != 0xFF) else {
        return Bound::Unbounded;
    };
    let mut end = prefix[..=last].to_vec();
    end[last] += 1;
    Bound::Excluded(end)
}

fn rebalance<F: File>(
    cache: &PageCache<F>,
    parents: &[Step],
    changed: u64,
) -> Result<(), StorageError> {
    let under_filled = cache.page_size().bytes() / UNDER_FILLED_DIVISOR;
    let mut page_id = changed;
    for &step in parents.iter().rev() {
        let mut child = step.child;
        let mut merged = false;
        while used_bytes(cache, page_id)? < under_filled {
            let Some((index, page)) = merge_with_a_sibling(cache, step.page, child)? else {
                break;
            };
            (child, page_id, merged) = (index, page, true);
        }
        if !merged {
            return Ok(());
        }
        page_id = step.page;
    }
    collapse_root(cache, page_id)
}

fn used_bytes<F: File>(cache: &PageCache<F>, page_id: u64) -> Result<usize, StorageError> {
    let page = cache.read(page_id)?;
    Ok(shape_of_ref(&page)?.used_bytes())
}

fn merge_with_a_sibling<F: File>(
    cache: &PageCache<F>,
    parent: u64,
    child: usize,
) -> Result<Option<(usize, u64)>, StorageError> {
    let separators = {
        let page = cache.read(parent)?;
        shape_of_ref(&page)?.count
    };
    if child > 0
        && let Some(merged) = merge_children(cache, parent, child - 1)?
    {
        return Ok(Some((child - 1, merged)));
    }
    if child < separators
        && let Some(merged) = merge_children(cache, parent, child)?
    {
        return Ok(Some((child, merged)));
    }
    Ok(None)
}

fn merge_children<F: File>(
    cache: &PageCache<F>,
    parent_id: u64,
    separator: usize,
) -> Result<Option<u64>, StorageError> {
    let (level, key, left_id, right_id) = {
        let parent = cache.read(parent_id)?;
        let shape = shape_of_ref(&parent)?;
        let key = [
            shape.prefix(&parent),
            shape.cell_at(&parent, separator)?.key,
        ]
        .concat();
        let left = shape.child_at(&parent, separator)?;
        let right = shape.child_at(&parent, separator + 1)?;
        (shape.level - 1, key, left, right)
    };
    let page_size = cache.page_size().bytes();
    let fits = {
        let left = cache.read(left_id)?;
        let left_shape = shape_of_ref(&left)?;
        expect_level(&left_shape, Some(level))?;
        let right = cache.read(right_id)?;
        let right_shape = shape_of_ref(&right)?;
        expect_level(&right_shape, Some(level))?;
        left_shape.merged_size(&left, &key, &right_shape, &right) <= page_size
    };
    if !fits {
        return Ok(None);
    }
    let left = read_node(cache, left_id, level)?;
    let right = read_node(cache, right_id, level)?;
    if left.high.as_deref() != Some(&key[..]) || right.low != key {
        return Err(FormatError::CorruptTree {
            page: parent_id,
            problem: "a separator differs from the fence keys of the pages beside it",
        }
        .into());
    }
    let merged = left.merge(key, right);
    if merged.size() > page_size {
        return Ok(None);
    }
    merged.encode(right_id, &mut cache.write_keeping_check(right_id)?)?;
    {
        let mut parent = cache.write_keeping_check(parent_id)?;
        let mut shape = shape_of_mut(&parent)?;
        shape.remove(&mut parent, separator)?;
    }
    cache.free(left_id)?;
    Ok(Some(right_id))
}

fn collapse_root<F: File>(cache: &PageCache<F>, root: u64) -> Result<(), StorageError> {
    loop {
        let (child, level) = {
            let page = cache.read(root)?;
            let shape = shape_of_ref(&page)?;
            if shape.is_leaf() || shape.count > 0 {
                return Ok(());
            }
            (shape.child_at(&page, 0)?, shape.level - 1)
        };
        let node = read_node(cache, child, level)?;
        if !node.low.is_empty() || node.high.is_some() {
            return Err(FormatError::CorruptTree {
                page: child,
                problem: "the only child of the root has fence keys",
            }
            .into());
        }
        {
            let mut page = cache.write_keeping_check(root)?;
            let lsn = page.header().lsn;
            page.set_header(tree_page::header_for(node.level, lsn))?;
            node.encode(root, &mut page)?;
        }
        cache.free(child)?;
    }
}

fn read_node<F: File>(cache: &PageCache<F>, page_id: u64, level: u8) -> Result<Node, StorageError> {
    let page = cache.read(page_id)?;
    let shape = shape_of_ref(&page)?;
    expect_level(&shape, Some(level))?;
    Ok(Node::decode(&shape, &page)?)
}

pub(crate) fn shape_of_ref(page: &impl TreePage) -> Result<Shape, StorageError> {
    let shape = Shape::parse(page.page_id(), page.header(), page)?;
    if !page.contents_checked() {
        shape.validate(page)?;
        page.set_contents_checked();
    }
    Ok(shape)
}

fn shape_of_mut(page: &PageMut<'_>) -> Result<Shape, StorageError> {
    let shape = Shape::parse(page.page_id(), page.header(), page)?;
    if !page.contents_checked() {
        shape.validate(page)?;
        page.set_contents_checked();
    }
    Ok(shape)
}

pub(crate) fn expect_level(shape: &Shape, level: Option<u8>) -> Result<(), StorageError> {
    match level {
        None if shape.has_fences() => Err(shape.corrupt("the root page has fence keys").into()),
        Some(level) if shape.level != level => Err(shape
            .corrupt("a child is not exactly one level below its parent")
            .into()),
        _ => Ok(()),
    }
}

fn new_page<F: File>(cache: &PageCache<F>, node: &Node) -> Result<u64, StorageError> {
    let mut page = cache.allocate(tree_page::header_for(node.level, 0))?;
    let page_id = page.page_id();
    node.encode(page_id, &mut page)?;
    page.set_contents_checked();
    Ok(page_id)
}

fn store<F: File>(
    cache: &PageCache<F>,
    path: &[u64],
    mut page: PageMut<'_>,
    node: Node,
    inserted: usize,
) -> Result<(), StorageError> {
    let page_id = page.page_id();
    let page_size = cache.page_size().bytes();
    if node.size() <= page_size {
        node.encode(page_id, &mut page)?;
        return Ok(());
    }
    let split = node
        .split(page_size, inserted)
        .ok_or(FormatError::CorruptTree {
            page: page_id,
            problem: "no split point leaves both halves within a page",
        })?;
    let parents = &path[..path.len().saturating_sub(1)];
    let Some(&parent_id) = parents.last() else {
        return split_root(cache, page, node.level, split);
    };
    let right_id = new_page(cache, &split.right)?;
    split.left.encode(page_id, &mut page)?;
    drop(page);
    let mut parent = cache.write_keeping_check(parent_id)?;
    let mut shape = shape_of_mut(&parent)?;
    let suffix = shape.key_suffix(&parent, &split.separator)?;
    let index = match shape.search(&parent, suffix)? {
        Ok(_) => return Err(shape.corrupt("a separator is already in the parent").into()),
        Err(index) => index,
    };
    if suffix <= shape.low_suffix(&parent) {
        return Err(shape
            .corrupt("a separator is not above the parent's low fence key")
            .into());
    }
    if shape.child_at(&parent, index)? != page_id {
        return Err(shape
            .corrupt("the parent does not point to the page the search came through")
            .into());
    }
    if shape.inner_fits(suffix.len()) {
        shape.insert_inner(&mut parent, index, suffix, page_id);
        shape.set_child(&mut parent, index + 1, right_id)?;
        return Ok(());
    }
    let mut node = Node::decode(&shape, &parent)?;
    node.keys.insert(index, split.separator);
    node.children.insert(index + 1, right_id);
    store(cache, parents, parent, node, index)
}

fn split_root<F: File>(
    cache: &PageCache<F>,
    mut root: PageMut<'_>,
    level: u8,
    split: tree_page::Split,
) -> Result<(), StorageError> {
    let root_id = root.page_id();
    let level = level + 1;
    if level >= MAX_HEIGHT {
        return Err(FormatError::CorruptTree {
            page: root_id,
            problem: "a root split would pass the height limit",
        }
        .into());
    }
    let left = new_page(cache, &split.left)?;
    let right = new_page(cache, &split.right)?;
    let node = Node {
        level,
        low: Vec::new(),
        high: None,
        keys: vec![split.separator],
        values: Vec::new(),
        children: vec![left, right],
    };
    root.set_header(tree_page::header_for(level, root.header().lsn))?;
    node.encode(root_id, &mut root)?;
    Ok(())
}

struct CheckWalk<'w, P: PageSource> {
    pages: &'w P,
    used: &'w mut PageSet,
    report: &'w mut TreeReport,
}

impl<P: PageSource> CheckWalk<'_, P> {
    fn check_page(
        &mut self,
        page_id: u64,
        level: Option<u8>,
        bounds: &Bounds,
    ) -> Result<(), StorageError> {
        let corrupt = |problem| -> StorageError {
            FormatError::CorruptTree {
                page: page_id,
                problem,
            }
            .into()
        };
        let node = {
            let page = self.pages.page(page_id)?;
            let shape = Shape::parse(page_id, page.header(), &page)?;
            shape.validate(&page)?;
            page.set_contents_checked();
            expect_level(&shape, level)?;
            Node::decode(&shape, &page)?
        };
        if node.low != bounds.low || node.high != bounds.high {
            return Err(corrupt(
                "the fence keys differ from the bounds the parent gives",
            ));
        }
        if !self.used.insert(page_id)? {
            return Err(corrupt("the page is reached twice"));
        }
        if level.is_none() {
            if node.level != 0 && node.keys.is_empty() {
                return Err(corrupt("the root is an inner page with a single child"));
            }
            self.report.height = u32::from(node.level) + 1;
        }
        if node.level == 0 {
            return self.check_leaf(&node);
        }
        self.report.inner_pages += 1;
        for (index, &child) in node.children.iter().enumerate() {
            let child_bounds = Bounds {
                low: if index == 0 {
                    node.low.clone()
                } else {
                    node.keys[index - 1].clone()
                },
                high: node.keys.get(index).cloned().or_else(|| node.high.clone()),
            };
            self.check_page(child, Some(node.level - 1), &child_bounds)?;
        }
        Ok(())
    }

    fn check_leaf(&mut self, node: &Node) -> Result<(), StorageError> {
        let report = &mut *self.report;
        report.leaf_pages += 1;
        report.entries += node.keys.len() as u64;
        report.key_bytes += node.keys.iter().map(|key| key.len() as u64).sum::<u64>();
        for value in &node.values {
            report.value_bytes += value.value_bytes();
            if let StoredValue::Overflow(reference) = value {
                report.overflow_values += 1;
                report.overflow_pages += overflow::check_value(self.pages, reference, self.used)?;
            }
        }
        Ok(())
    }
}
