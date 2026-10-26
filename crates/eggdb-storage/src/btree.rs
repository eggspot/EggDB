use crate::error::{FormatError, StorageError};
use crate::format::{NO_PAGE, PageSize};
use crate::page_cache::{PageCache, PageMut, PageRef};
use crate::tree_page::{self, MAX_HEIGHT, Node, Shape};
use eggdb_platform::File;
use std::collections::BTreeSet;

/// A B+tree of byte-string keys and values in the pages of a [`PageCache`] (D77, format.md
/// "B+tree pages").
///
/// Keys compare as unsigned bytes (key scheme 0). Pages are slotted: a sorted array of
/// two-byte slots at the front, searched by binary search, points at cells packed from the
/// back. Each page keeps a low and a high fence key bounding the keys it may hold, and stores
/// the prefix the two share only once, so its keys are stored without it. Leaf splits choose
/// the shortest separator between the halves.
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
    /// Bytes of the leaves' values.
    pub value_bytes: u64,
}

impl BTree {
    /// Creates an empty tree in a newly allocated page, its root for the tree's life. The
    /// caller records [`BTree::root`] where it will find the tree again, such as the
    /// superblock's `catalog_root`, in the same commit.
    pub fn create<F: File>(cache: &PageCache<F>) -> Result<Self, StorageError> {
        let root = new_page(cache, &Node::empty_leaf())?;
        Ok(Self { root })
    }

    /// Opens the tree whose root is page `root`, checking that the page is a tree root.
    pub fn open<F: File>(cache: &PageCache<F>, root: u64) -> Result<Self, StorageError> {
        if root == NO_PAGE {
            return Err(FormatError::CorruptTree {
                page: root,
                problem: "the tree has no root page",
            }
            .into());
        }
        let page = cache.read(root)?;
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

    /// The most bytes a key and its value may have together with pages of `page_size`, so
    /// that at least four entries fit in a leaf: 750 bytes with 4 KiB pages and 12,270 with
    /// 64 KiB pages.
    pub fn max_entry_bytes(page_size: PageSize) -> usize {
        tree_page::max_entry_bytes(page_size.bytes())
    }

    /// The value stored under `key`, copied out, or `None`.
    pub fn get<F: File>(
        &self,
        cache: &PageCache<F>,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, StorageError> {
        self.get_with(cache, key, <[u8]>::to_vec)
    }

    /// Calls `read` with the value stored under `key` while its page is pinned, and returns
    /// what it returns, or `None` when the key is absent. Nothing is copied or allocated.
    pub fn get_with<F: File, R>(
        &self,
        cache: &PageCache<F>,
        key: &[u8],
        read: impl FnOnce(&[u8]) -> R,
    ) -> Result<Option<R>, StorageError> {
        let mut page_id = self.root;
        let mut level = None;
        loop {
            let page = cache.read(page_id)?;
            let shape = shape_of_ref(&page)?;
            expect_level(&shape, level)?;
            let suffix = shape.key_suffix(&page, key)?;
            if shape.is_leaf() {
                return Ok(match shape.search(&page, suffix)? {
                    Ok(index) => Some(read(shape.cell_at(&page, index)?.payload)),
                    Err(_) => None,
                });
            }
            page_id = shape.child_for(&page, suffix)?;
            level = Some(shape.level - 1);
        }
    }

    /// Stores `value` under `key`, inserting it or replacing the value there. Pages split as
    /// needed, up to the root.
    ///
    /// A key longer than [`BTree::max_key_bytes`] fails with [`StorageError::KeyTooLarge`],
    /// and a key and value longer together than [`BTree::max_entry_bytes`] with
    /// [`StorageError::EntryTooLarge`], before anything changes. Any other error, such as
    /// [`StorageError::CacheFull`] in the middle of a split, can leave the transaction's
    /// changes half made: roll the transaction back.
    pub fn put<F: File>(
        &self,
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
        let max_entry = Self::max_entry_bytes(page_size);
        if key.len() + value.len() > max_entry {
            return Err(StorageError::EntryTooLarge {
                bytes: key.len() + value.len(),
                max: max_entry,
            });
        }
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
        let mut page = cache.write(page_id)?;
        let mut shape = shape_of_mut(&page)?;
        let suffix = shape.key_suffix(&page, key)?;
        let index = match shape.search(&page, suffix)? {
            Ok(index) => {
                if shape.cell_at(&page, index)?.payload.len() == value.len() {
                    shape.overwrite_value(&mut page, index, value)?;
                    return Ok(());
                }
                shape.remove(&mut page, index)?;
                index
            }
            Err(index) => index,
        };
        if shape.leaf_fits(suffix.len(), value.len()) {
            shape.insert_leaf(&mut page, index, suffix, value);
            return Ok(());
        }
        let mut node = Node::decode(&shape, &page)?;
        node.keys.insert(index, key.to_vec());
        node.values.insert(index, value.to_vec());
        store(cache, &path[..=depth], page, node, index)
    }

    /// Checks every page of the tree and how they fit together, and counts what it holds.
    /// This is what `eggdb check` runs for each tree.
    ///
    /// Each page must pass the checks of a page read from the file (its layout, slots inside
    /// the page, cells that don't overlap and account for every byte, keys in strictly
    /// increasing order within its fence keys, sizes within the limits). Beyond those, each
    /// child must sit one level below its parent, with fence keys equal to the bounds its
    /// parent gives it, and no page may be reached twice. The first page that fails is
    /// reported as [`FormatError::CorruptTree`].
    pub fn check<F: File>(&self, cache: &PageCache<F>) -> Result<TreeReport, StorageError> {
        let mut report = TreeReport::default();
        let mut visited = BTreeSet::new();
        let bounds = Bounds {
            low: Vec::new(),
            high: None,
        };
        check_page(cache, self.root, None, &bounds, &mut visited, &mut report)?;
        Ok(report)
    }
}

struct Bounds {
    low: Vec<u8>,
    high: Option<Vec<u8>>,
}

fn shape_of_ref(page: &PageRef<'_>) -> Result<Shape, StorageError> {
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

fn expect_level(shape: &Shape, level: Option<u8>) -> Result<(), StorageError> {
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
    split.left.encode(page_id, &mut page)?;
    drop(page);
    let right_id = new_page(cache, &split.right)?;
    let mut parent = cache.write(parent_id)?;
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

fn check_page<F: File>(
    cache: &PageCache<F>,
    page_id: u64,
    level: Option<u8>,
    bounds: &Bounds,
    visited: &mut BTreeSet<u64>,
    report: &mut TreeReport,
) -> Result<(), StorageError> {
    let corrupt = |problem| -> StorageError {
        FormatError::CorruptTree {
            page: page_id,
            problem,
        }
        .into()
    };
    if !visited.insert(page_id) {
        return Err(corrupt("the page is reached twice"));
    }
    let node = {
        let page = cache.read(page_id)?;
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
    if level.is_none() {
        report.height = u32::from(node.level) + 1;
    }
    if node.level == 0 {
        report.leaf_pages += 1;
        report.entries += node.keys.len() as u64;
        report.key_bytes += node.keys.iter().map(|key| key.len() as u64).sum::<u64>();
        report.value_bytes += node
            .values
            .iter()
            .map(|value| value.len() as u64)
            .sum::<u64>();
        return Ok(());
    }
    report.inner_pages += 1;
    for (index, &child) in node.children.iter().enumerate() {
        let child_bounds = Bounds {
            low: if index == 0 {
                node.low.clone()
            } else {
                node.keys[index - 1].clone()
            },
            high: node.keys.get(index).cloned().or_else(|| node.high.clone()),
        };
        check_page(
            cache,
            child,
            Some(node.level - 1),
            &child_bounds,
            visited,
            report,
        )?;
    }
    Ok(())
}
