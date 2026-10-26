use crate::btree::{BTree, TreeReport};
use crate::cursor::{Cursor, KeyAndValue};
use crate::error::StorageError;
use crate::format::Superblock;
use crate::hybrid_clock::{CommitStamp, HybridClock};
use crate::log::{Durability, LogStats};
use crate::page_cache::{CacheSize, CacheStats, PageCache, Snapshot};
use eggdb_platform::{Clock, File};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

/// A database file's pages with transactions over them (D86): one write transaction at a time,
/// any number of read transactions, each reading a snapshot of the last commit before it
/// began, and a hybrid-clock [`CommitStamp`] for every commit.
///
/// The store owns the [`PageCache`]. It can be shared between threads: each operation of a
/// transaction takes the store for its own duration only, so a reader never waits for a
/// writer's transaction to end, nor a writer for a reader's. A commit holds the store while
/// it writes and syncs, which readers wait out.
///
/// **Snapshots.** Before the writer first changes or frees a page that a read transaction may
/// still need, the store keeps the page's committed image in memory, and a reader whose
/// snapshot predates a page's current version reads that image (D77). Nothing of this reaches
/// the file, so checkpoints, recycled log segments and reused free pages never change what a
/// reader sees. The images are bounded by [`Store::set_snapshot_memory`]; when the writer
/// needs more, the oldest snapshots expire, and their transactions fail with
/// [`StorageError::SnapshotTooOld`] from then on, so the writer is never held back.
///
/// **Writes.** [`Store::begin_write`] fails with [`StorageError::WriterActive`] while another
/// write transaction is open, instead of waiting. A write transaction's changes stay in the
/// cache until [`WriteTransaction::commit`] logs them through the write-ahead log and stamps
/// the commit, or until it is rolled back: explicitly, by being dropped, or by itself when a
/// change fails half-way, so an error never leaves a half-applied change to commit.
pub struct Store<F: File> {
    state: Mutex<State<F>>,
}

struct State<F: File> {
    cache: PageCache<F>,
    clock: HybridClock,
    last_commit: CommitStamp,
    writer_active: bool,
}

impl<F: File> Store<F> {
    /// Puts transactions over `cache`, stamping commits with `clock`. The first stamp is above
    /// the file's `clock_high_water`, the stamp of its last commit, whatever `clock` says.
    pub fn new(cache: PageCache<F>, clock: Arc<dyn Clock>) -> Self {
        let high_water = cache.superblock().clock_high_water;
        Self {
            state: Mutex::new(State {
                cache,
                clock: HybridClock::new(clock, high_water),
                last_commit: CommitStamp::from_bits(high_water),
                writer_active: false,
            }),
        }
    }

    /// Begins a read transaction, which sees the last commit before it began, and nothing
    /// after it, for as long as it lives. A write transaction open at the time stays
    /// invisible to it, committed or not.
    pub fn begin_read(&self) -> Result<ReadTransaction<'_, F>, StorageError> {
        let mut state = self.lock()?;
        let version = state.cache.begin_snapshot()?;
        Ok(ReadTransaction {
            store: self,
            version,
            stamp: state.last_commit,
            catalog_root: state.cache.superblock().catalog_root,
        })
    }

    /// Begins the write transaction. While it is open, another fails with
    /// [`StorageError::WriterActive`]. A store over a file opened read-only refuses it with
    /// [`StorageError::ReadOnly`], and one whose last write or sync failed with
    /// [`StorageError::NeedsReopen`].
    pub fn begin_write(&self) -> Result<WriteTransaction<'_, F>, StorageError> {
        let mut state = self.lock()?;
        if state.writer_active {
            return Err(StorageError::WriterActive);
        }
        state.cache.check_writable()?;
        state.writer_active = true;
        Ok(WriteTransaction {
            store: self,
            failed: false,
            finished: false,
            began_after: state.last_commit,
            catalog_root: state.cache.superblock().catalog_root,
        })
    }

    /// The stamp of the last commit, or of the last commit before the file was opened;
    /// [`CommitStamp::NONE`] for a file no transaction has committed to.
    pub fn last_commit(&self) -> Result<CommitStamp, StorageError> {
        Ok(self.lock()?.last_commit)
    }

    /// The superblock of the last commit.
    pub fn superblock(&self) -> Result<Superblock, StorageError> {
        Ok(self.lock()?.cache.superblock())
    }

    /// What the page cache holds and has done, before-images included.
    pub fn cache_stats(&self) -> Result<CacheStats, StorageError> {
        Ok(self.lock()?.cache.stats())
    }

    /// What the log has replayed and checkpointed since the file was opened.
    pub fn log_stats(&self) -> Result<LogStats, StorageError> {
        Ok(self.lock()?.cache.log_stats())
    }

    /// Sets how much memory the before-images of read transactions' snapshots may take; the
    /// default is as many pages as the page cache holds. Lowering it below what is kept now
    /// expires the oldest snapshots at once. While the open write transaction has changed more
    /// pages than this holds, a read transaction that begins fails with
    /// [`StorageError::SnapshotTooOld`] on its first read.
    pub fn set_snapshot_memory(&self, size: CacheSize) -> Result<(), StorageError> {
        let mut state = self.lock()?;
        let pages = size.pages(state.cache.page_size());
        state.cache.set_before_image_budget(pages);
        Ok(())
    }

    /// Sets how later commits wait for the disk (see [`PageCache::set_durability`]).
    pub fn set_durability(&self, durability: Durability) -> Result<(), StorageError> {
        self.lock()?.cache.set_durability(durability)
    }

    /// Sets how much log a commit may leave before it checkpoints automatically (see
    /// [`PageCache::set_auto_checkpoint_bytes`]).
    pub fn set_auto_checkpoint_bytes(&self, log_bytes: Option<u64>) -> Result<(), StorageError> {
        self.lock()?.cache.set_auto_checkpoint_bytes(log_bytes);
        Ok(())
    }

    /// Makes every commit so far durable with one sync (see [`PageCache::sync`]).
    pub fn sync(&self) -> Result<(), StorageError> {
        self.lock()?.cache.sync()
    }

    /// Writes a checkpoint (see [`PageCache::checkpoint`]). Read transactions are unaffected;
    /// an open write transaction with changes refuses it with
    /// [`StorageError::UncommittedChanges`].
    pub fn checkpoint(&self) -> Result<(), StorageError> {
        self.lock()?.cache.checkpoint()
    }

    fn lock(&self) -> Result<MutexGuard<'_, State<F>>, StorageError> {
        self.state.lock().map_err(|_| StorageError::NeedsReopen)
    }
}

impl<F: File> fmt::Debug for Store<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.state.try_lock() {
            Ok(state) => f
                .debug_struct("Store")
                .field("cache", &state.cache)
                .field("clock", &state.clock)
                .field("last_commit", &state.last_commit)
                .field("writer_active", &state.writer_active)
                .finish(),
            Err(_) => f.debug_struct("Store").finish_non_exhaustive(),
        }
    }
}

/// A snapshot of a [`Store`]: what the last commit before [`Store::begin_read`] left, which no
/// later commit changes. Dropping it lets the store forget the before-images it needed.
pub struct ReadTransaction<'s, F: File> {
    store: &'s Store<F>,
    version: u64,
    stamp: CommitStamp,
    catalog_root: u64,
}

impl<F: File> ReadTransaction<'_, F> {
    /// The stamp of the commit this transaction sees, or [`CommitStamp::NONE`] when no
    /// transaction has committed to the file yet.
    pub fn stamp(&self) -> CommitStamp {
        self.stamp
    }

    /// The catalog root as of the snapshot.
    pub fn catalog_root(&self) -> u64 {
        self.catalog_root
    }

    /// Opens the tree whose root is page `root` as of the snapshot (see [`BTree::open`]).
    pub fn open_tree(&self, root: u64) -> Result<BTree, StorageError> {
        let state = self.store.lock()?;
        let snapshot = Snapshot {
            cache: &state.cache,
            version: self.version,
        };
        BTree::open_in(&snapshot, root)
    }

    /// The value stored under `key` in `tree` as of the snapshot, copied out, or `None`.
    pub fn get(&self, tree: &BTree, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        self.get_with(tree, key, <[u8]>::to_vec)
    }

    /// Calls `read` with the value stored under `key` in `tree` as of the snapshot, and
    /// returns what it returns, or `None` when the key is absent. The store is held while
    /// `read` runs, so `read` must not use the store itself: beginning, using or dropping a
    /// transaction of the same store inside it deadlocks.
    pub fn get_with<R>(
        &self,
        tree: &BTree,
        key: &[u8],
        read: impl FnOnce(&[u8]) -> R,
    ) -> Result<Option<R>, StorageError> {
        let state = self.store.lock()?;
        let snapshot = Snapshot {
            cache: &state.cache,
            version: self.version,
        };
        tree.get_in(&snapshot, key, read)
    }

    /// Checks every page of `tree` as of the snapshot, as [`BTree::check`] does, and counts what
    /// it holds. The store is held for the whole walk, so the writer and other readers wait
    /// until it ends.
    pub fn check(&self, tree: &BTree) -> Result<TreeReport, StorageError> {
        let state = self.store.lock()?;
        let snapshot = Snapshot {
            cache: &state.cache,
            version: self.version,
        };
        tree.check_in(&snapshot)
    }

    /// The next entry of `cursor`, from a [`BTree::scan`] or [`BTree::scan_prefix`] of a tree,
    /// as of the snapshot (see [`Cursor::next`]).
    pub fn next<'c>(
        &self,
        cursor: &'c mut Cursor,
    ) -> Result<Option<KeyAndValue<'c>>, StorageError> {
        let state = self.store.lock()?;
        let snapshot = Snapshot {
            cache: &state.cache,
            version: self.version,
        };
        cursor.next_in(&snapshot)
    }
}

impl<F: File> Drop for ReadTransaction<'_, F> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.store.state.lock() {
            state.cache.end_snapshot(self.version);
        }
    }
}

impl<F: File> fmt::Debug for ReadTransaction<'_, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadTransaction")
            .field("stamp", &self.stamp)
            .field("catalog_root", &self.catalog_root)
            .finish_non_exhaustive()
    }
}

/// The one open write transaction of a [`Store`], from [`Store::begin_write`]. It reads its
/// own changes; nobody else sees them before [`WriteTransaction::commit`].
///
/// Dropping it rolls it back. A change that fails half-way, such as a put whose split finds
/// the cache full, rolls the whole transaction back at once and ends it: every later call
/// fails with [`StorageError::TransactionFailed`], and the store takes a new write
/// transaction. A change refused before it starts, such as a key too long, leaves the
/// transaction as it was.
pub struct WriteTransaction<'s, F: File> {
    store: &'s Store<F>,
    failed: bool,
    finished: bool,
    began_after: CommitStamp,
    catalog_root: u64,
}

impl<F: File> WriteTransaction<'_, F> {
    /// The stamp of the last commit before this transaction began.
    pub fn began_after(&self) -> CommitStamp {
        self.began_after
    }

    /// The catalog root this transaction will commit.
    pub fn catalog_root(&self) -> u64 {
        self.catalog_root
    }

    /// Sets the catalog root this transaction will commit, such as the root of a tree it
    /// created.
    pub fn set_catalog_root(&mut self, root: u64) {
        self.catalog_root = root;
    }

    /// Creates an empty tree (see [`BTree::create`]).
    pub fn create_tree(&mut self) -> Result<BTree, StorageError> {
        self.change(|cache| BTree::create(cache))
    }

    /// Opens the tree whose root is page `root`, this transaction's changes included (see
    /// [`BTree::open`]).
    pub fn open_tree(&self, root: u64) -> Result<BTree, StorageError> {
        self.check_open()?;
        let state = self.store.lock()?;
        BTree::open(&state.cache, root)
    }

    /// The value stored under `key` in `tree`, this transaction's changes included.
    pub fn get(&self, tree: &BTree, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        self.get_with(tree, key, <[u8]>::to_vec)
    }

    /// Calls `read` with the value stored under `key` in `tree`, this transaction's changes
    /// included, and returns what it returns, or `None` when the key is absent. The store is
    /// held while `read` runs, so `read` must not use the store itself: beginning, using or
    /// dropping a transaction of the same store inside it deadlocks.
    pub fn get_with<R>(
        &self,
        tree: &BTree,
        key: &[u8],
        read: impl FnOnce(&[u8]) -> R,
    ) -> Result<Option<R>, StorageError> {
        self.check_open()?;
        let state = self.store.lock()?;
        tree.get_in(&state.cache, key, read)
    }

    /// The next entry of `cursor`, this transaction's changes included (see [`Cursor::next`]).
    pub fn next<'c>(
        &self,
        cursor: &'c mut Cursor,
    ) -> Result<Option<KeyAndValue<'c>>, StorageError> {
        self.check_open()?;
        let state = self.store.lock()?;
        cursor.next_in(&state.cache)
    }

    /// Stores `value` under `key` in `tree` (see [`BTree::put`]). A key or entry over the
    /// limits is refused with nothing changed; any other error rolls the transaction back.
    pub fn put(&mut self, tree: &BTree, key: &[u8], value: &[u8]) -> Result<(), StorageError> {
        self.change(|cache| tree.put(cache, key, value))
    }

    /// Removes `key` from `tree` and returns whether it was there (see [`BTree::delete`]). An
    /// error rolls the transaction back.
    pub fn delete(&mut self, tree: &BTree, key: &[u8]) -> Result<bool, StorageError> {
        self.change(|cache| tree.delete(cache, key))
    }

    /// Commits every change atomically through the write-ahead log, with a commit stamp above
    /// every earlier one, and returns the stamp. Readers that begin afterwards see the changes.
    ///
    /// On an error nothing is committed and the transaction is rolled back, except after a
    /// failed write or sync: then the commit may or may not have reached the disk, and the
    /// store refuses everything with [`StorageError::NeedsReopen`] until the file is opened
    /// again (D59).
    pub fn commit(mut self) -> Result<CommitStamp, StorageError> {
        self.check_open()?;
        self.finished = true;
        let mut state = self.store.lock()?;
        let state = &mut *state;
        state.writer_active = false;
        let stamp = match state.clock.next() {
            Ok(stamp) => stamp,
            Err(error) => {
                let _ = state.cache.rollback();
                return Err(error);
            }
        };
        let catalog_root = self.catalog_root;
        let committed = state.cache.commit_stamped(stamp.bits(), |superblock| {
            superblock.catalog_root = catalog_root;
            superblock.clock_high_water = stamp.bits();
        });
        match committed {
            Ok(()) => {
                state.last_commit = stamp;
                Ok(stamp)
            }
            Err(error) => {
                let _ = state.cache.rollback();
                Err(error)
            }
        }
    }

    /// Drops every change of the transaction. Dropping the transaction does the same.
    pub fn rollback(mut self) -> Result<(), StorageError> {
        self.finished = true;
        if self.failed {
            return Ok(());
        }
        let mut state = self.store.lock()?;
        state.writer_active = false;
        state.cache.rollback()
    }

    fn check_open(&self) -> Result<(), StorageError> {
        if self.failed {
            return Err(StorageError::TransactionFailed);
        }
        Ok(())
    }

    fn change<T>(
        &mut self,
        change: impl FnOnce(&PageCache<F>) -> Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        self.check_open()?;
        let mut state = self.store.lock()?;
        let changed = change(&state.cache);
        if let Err(error) = &changed
            && !leaves_the_transaction_intact(error)
        {
            self.failed = true;
            state.writer_active = false;
            let _ = state.cache.rollback();
        }
        changed
    }
}

fn leaves_the_transaction_intact(error: &StorageError) -> bool {
    matches!(
        error,
        StorageError::KeyTooLarge { .. } | StorageError::EntryTooLarge { .. }
    )
}

impl<F: File> Drop for WriteTransaction<'_, F> {
    fn drop(&mut self) {
        if self.finished || self.failed {
            return;
        }
        if let Ok(mut state) = self.store.state.lock() {
            state.writer_active = false;
            let _ = state.cache.rollback();
        }
    }
}

impl<F: File> fmt::Debug for WriteTransaction<'_, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WriteTransaction")
            .field("failed", &self.failed)
            .field("began_after", &self.began_after)
            .field("catalog_root", &self.catalog_root)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
impl<F: File> Store<F> {
    pub(crate) fn with_cache<R>(&self, use_cache: impl FnOnce(&PageCache<F>) -> R) -> R {
        let state = self.state.lock().unwrap();
        use_cache(&state.cache)
    }
}
