use crate::error::{SqlError, SqlResult, SqlState};
use crate::store::{EntryVisitor, KeyValueRead, ScanDirection, SnapshotHandle};
use std::cell::RefCell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) const EXPORT_FUNCTION: &str = "pg_export_snapshot";

struct Exported {
    store: usize,
    id: String,
    handle: SnapshotHandle,
}

static EXPORTED: Mutex<Vec<Exported>> = Mutex::new(Vec::new());
static NEXT_EXPORT: AtomicU64 = AtomicU64::new(1);

struct ExportContext {
    store: usize,
    handle: Option<SnapshotHandle>,
    exported: Vec<String>,
}

thread_local! {
    static CONTEXT: RefCell<Option<ExportContext>> = const { RefCell::new(None) };
}

pub(crate) struct ExportScope;

impl Drop for ExportScope {
    fn drop(&mut self) {
        CONTEXT.with(|context| {
            if let Ok(mut context) = context.try_borrow_mut() {
                *context = None;
            }
        });
    }
}

pub(crate) fn enter(store: usize, handle: Option<SnapshotHandle>) -> ExportScope {
    CONTEXT.with(|context| {
        if let Ok(mut context) = context.try_borrow_mut() {
            *context = Some(ExportContext {
                store,
                handle,
                exported: Vec::new(),
            });
        }
    });
    ExportScope
}

pub(crate) fn take_exported() -> Vec<String> {
    CONTEXT.with(|context| {
        context
            .try_borrow_mut()
            .ok()
            .and_then(|mut context| context.as_mut().map(|found| std::mem::take(&mut found.exported)))
            .unwrap_or_default()
    })
}

fn not_exportable() -> SqlError {
    SqlError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        "pg_export_snapshot() needs a READ ONLY transaction block in EggDB",
    )
    .with_hint("Start the transaction with BEGIN ISOLATION LEVEL REPEATABLE READ, READ ONLY.")
}

pub(crate) fn export() -> SqlResult<String> {
    let (store, handle) = CONTEXT
        .with(|context| {
            context.try_borrow().ok().and_then(|context| {
                context
                    .as_ref()
                    .and_then(|found| found.handle.clone().map(|handle| (found.store, handle)))
            })
        })
        .ok_or_else(not_exportable)?;
    let number = NEXT_EXPORT.fetch_add(1, Ordering::Relaxed);
    let id = format!("{:08X}-{:08X}-1", std::process::id(), number & 0xFFFF_FFFF);
    let mut exported = EXPORTED
        .lock()
        .map_err(|_| SqlError::new(SqlState::INTERNAL_ERROR, "the exported snapshots are unusable"))?;
    exported.push(Exported {
        store,
        id: id.clone(),
        handle,
    });
    drop(exported);
    CONTEXT.with(|context| {
        if let Ok(mut context) = context.try_borrow_mut()
            && let Some(found) = context.as_mut()
        {
            found.exported.push(id.clone());
        }
    });
    Ok(id)
}

pub(crate) fn invalid_identifier(id: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("invalid snapshot identifier: \"{id}\""),
    )
}

pub(crate) fn find(store: usize, id: &str) -> SqlResult<SnapshotHandle> {
    let exported = EXPORTED
        .lock()
        .map_err(|_| SqlError::new(SqlState::INTERNAL_ERROR, "the exported snapshots are unusable"))?;
    exported
        .iter()
        .find(|entry| entry.store == store && entry.id == id)
        .map(|entry| entry.handle.clone())
        .ok_or_else(|| invalid_identifier(id))
}

fn forget(ids: &[String]) {
    if let Ok(mut exported) = EXPORTED.lock() {
        exported.retain(|entry| !ids.contains(&entry.id));
    }
}

pub(crate) struct ExportingRead<'s> {
    pub(crate) inner: Box<dyn KeyValueRead + 's>,
    pub(crate) ids: Vec<String>,
}

impl Drop for ExportingRead<'_> {
    fn drop(&mut self) {
        forget(&self.ids);
    }
}

impl KeyValueRead for ExportingRead<'_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        self.inner.get(key)
    }

    fn get_with(
        &self,
        key: &[u8],
        read: &mut dyn FnMut(&[u8]) -> SqlResult<()>,
    ) -> SqlResult<bool> {
        self.inner.get_with(key, read)
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        self.inner.scan(start, end, direction, visit)
    }

    fn committed_version(&self) -> Option<u64> {
        self.inner.committed_version()
    }

    fn snapshot_handle(&self) -> Option<SnapshotHandle> {
        self.inner.snapshot_handle()
    }

    fn collection_names(&self) -> SqlResult<Vec<String>> {
        self.inner.collection_names()
    }

    fn has_collection(&self, name: &str) -> SqlResult<bool> {
        self.inner.has_collection(name)
    }

    fn collection_get(&self, name: &str, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        self.inner.collection_get(name, key)
    }

    fn collection_scan(
        &self,
        name: &str,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        self.inner.collection_scan(name, start, end, direction, visit)
    }
}
