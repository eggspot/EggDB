use crate::ast::OnCommit;
use crate::encoding::{
    TableDefinition, is_temporary_key, is_view_record, prefix_end, rows_prefix,
    table_indexes_prefix, table_key, temporary_action_key, temporary_actions_prefix,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::schemas::QUALIFIER;
use crate::store::{EntryVisitor, KeyValueRead, KeyValueWrite, ScanDirection};
use std::cell::Cell;
use std::collections::BTreeMap;

pub(crate) const SCHEMA: &str = "pg_temp";
const FIRST_ID: u32 = u32::MAX - (1 << 25) + 1;
const LAST_ID: u32 = u32::MAX - (1 << 24);
const MOST_BYTES: usize = 1 << 30;
const ENTRY_OVERHEAD: usize = 64;
const DELETE_ROWS: u8 = 1;
const DROP: u8 = 2;

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn is_temporary_id(id: u32) -> bool {
    (FIRST_ID..=LAST_ID).contains(&id)
}

pub(crate) fn is_temporary_name(name: &str) -> bool {
    name.strip_prefix(SCHEMA)
        .is_some_and(|rest| rest.starts_with(QUALIFIER))
}

pub(crate) fn internal_name(name: &str) -> String {
    crate::schemas::written_qualified(SCHEMA, name)
}

pub(crate) fn is_sqlserver_temporary(name: &str) -> bool {
    name.starts_with('#')
}

pub(crate) struct Entered {
    previous: bool,
}

impl Drop for Entered {
    fn drop(&mut self) {
        ACTIVE.with(|active| active.set(self.previous));
    }
}

pub(crate) fn enter(active: bool) -> Entered {
    Entered {
        previous: ACTIVE.with(|flag| flag.replace(active)),
    }
}

pub(crate) fn active() -> bool {
    ACTIVE.with(Cell::get)
}

fn too_large() -> SqlError {
    SqlError::new(
        SqlState::OUT_OF_MEMORY,
        format!("a session's temporary tables can hold at most {MOST_BYTES} bytes"),
    )
    .with_hint("Temporary tables live in the session's memory; keep large data in ordinary tables.")
}

pub(crate) fn action_code(action: OnCommit) -> Option<u8> {
    match action {
        OnCommit::PreserveRows => None,
        OnCommit::DeleteRows => Some(DELETE_ROWS),
        OnCommit::Drop => Some(DROP),
    }
}

pub(crate) fn record_action(
    writer: &mut dyn KeyValueWrite,
    table: &str,
    action: OnCommit,
) -> SqlResult<()> {
    let key = temporary_action_key(table);
    match action_code(action) {
        Some(code) => writer.put(&key, &[code]),
        None => writer.delete(&key).map(drop),
    }
}

pub(crate) fn next_id(reader: &dyn KeyValueRead) -> SqlResult<u32> {
    let prefix = table_key(&internal_name(""));
    let name_start = table_key("").len();
    let mut used = Vec::new();
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |key, value| {
            if !is_view_record(value)
                && let Some(name) = key
                    .get(name_start..)
                    .and_then(|name| std::str::from_utf8(name).ok())
                && let Ok(table) = TableDefinition::decode(name, value)
            {
                used.push(table.id);
            }
            Ok(true)
        },
    )?;
    (FIRST_ID..=LAST_ID)
        .find(|id| !used.contains(id))
        .ok_or_else(|| {
            SqlError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                "the session has used every temporary table identifier",
            )
        })
}

#[derive(Default)]
pub(crate) struct TemporaryObjects {
    entries: BTreeMap<Vec<u8>, Vec<u8>>,
    undo: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    bytes: usize,
}

fn entry_bytes(key: &[u8], value: &[u8]) -> usize {
    key.len() + value.len() + ENTRY_OVERHEAD
}

impl TemporaryObjects {
    pub(crate) fn has_objects(&self) -> bool {
        !self.entries.is_empty()
    }

    fn put(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()> {
        let added = entry_bytes(key, value);
        let removed = self
            .entries
            .get(key)
            .map_or(0, |previous| entry_bytes(key, previous));
        if self.bytes.saturating_sub(removed).saturating_add(added) > MOST_BYTES {
            return Err(too_large());
        }
        let previous = self.entries.insert(key.to_vec(), value.to_vec());
        self.bytes = self.bytes.saturating_sub(removed).saturating_add(added);
        self.undo.push((key.to_vec(), previous));
        Ok(())
    }

    fn delete(&mut self, key: &[u8]) -> bool {
        match self.entries.remove(key) {
            Some(previous) => {
                self.bytes = self.bytes.saturating_sub(entry_bytes(key, &previous));
                self.undo.push((key.to_vec(), Some(previous)));
                true
            }
            None => false,
        }
    }

    fn restore(&mut self, key: Vec<u8>, previous: Option<Vec<u8>>) {
        if let Some(current) = self.entries.remove(&key) {
            self.bytes = self.bytes.saturating_sub(entry_bytes(&key, &current));
        }
        if let Some(value) = previous {
            self.bytes = self.bytes.saturating_add(entry_bytes(&key, &value));
            self.entries.insert(key, value);
        }
    }

    pub(crate) fn rollback(&mut self) {
        while let Some((key, previous)) = self.undo.pop() {
            self.restore(key, previous);
        }
    }

    fn remove_prefix(&mut self, prefix: &[u8]) {
        let keys: Vec<Vec<u8>> = self
            .entries
            .range(prefix.to_vec()..prefix_end(prefix))
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            if let Some(value) = self.entries.remove(&key) {
                self.bytes = self.bytes.saturating_sub(entry_bytes(&key, &value));
            }
        }
    }

    fn remove(&mut self, key: &[u8]) {
        if let Some(value) = self.entries.remove(key) {
            self.bytes = self.bytes.saturating_sub(entry_bytes(key, &value));
        }
    }

    pub(crate) fn commit(&mut self) {
        self.undo.clear();
        let prefix = temporary_actions_prefix();
        let actions: Vec<(String, u8)> = self
            .entries
            .range(prefix.clone()..prefix_end(&prefix))
            .filter_map(|(key, value)| {
                let name = std::str::from_utf8(key.get(prefix.len()..)?).ok()?;
                Some((name.to_owned(), *value.first()?))
            })
            .collect();
        for (name, action) in actions {
            let definition = self
                .entries
                .get(&table_key(&name))
                .filter(|bytes| !is_view_record(bytes))
                .and_then(|bytes| TableDefinition::decode(&name, bytes).ok());
            let Some(table) = definition else {
                self.remove(&temporary_action_key(&name));
                continue;
            };
            self.remove_prefix(&rows_prefix(table.id));
            self.remove_prefix(&table_indexes_prefix(table.id));
            if action == DROP {
                self.remove(&table_key(&name));
                self.remove(&temporary_action_key(&name));
            }
        }
    }

    pub(crate) fn discard(&mut self) {
        let keys: Vec<Vec<u8>> = self.entries.keys().cloned().collect();
        for key in keys {
            self.delete(&key);
        }
    }
}

pub(crate) struct TemporaryRead<'r> {
    pub(crate) inner: &'r dyn KeyValueRead,
    pub(crate) objects: &'r TemporaryObjects,
}

impl KeyValueRead for TemporaryRead<'_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        if is_temporary_key(key) {
            return Ok(self.objects.entries.get(key).cloned());
        }
        self.inner.get(key)
    }

    fn get_with(
        &self,
        key: &[u8],
        read: &mut dyn FnMut(&[u8]) -> SqlResult<()>,
    ) -> SqlResult<bool> {
        if is_temporary_key(key) {
            return match self.objects.entries.get(key) {
                Some(value) => read(value).map(|()| true),
                None => Ok(false),
            };
        }
        self.inner.get_with(key, read)
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        crate::temp_tables::merged_scan(
            &self.objects.entries,
            self.inner,
            start,
            end,
            direction,
            visit,
        )
    }

    fn committed_version(&self) -> Option<u64> {
        if self.objects.entries.is_empty() {
            self.inner.committed_version()
        } else {
            None
        }
    }
}

pub(crate) struct TemporaryWrite<'w> {
    pub(crate) inner: &'w mut dyn KeyValueWrite,
    pub(crate) objects: &'w mut TemporaryObjects,
}

impl KeyValueRead for TemporaryWrite<'_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        if is_temporary_key(key) {
            return Ok(self.objects.entries.get(key).cloned());
        }
        self.inner.get(key)
    }

    fn get_with(
        &self,
        key: &[u8],
        read: &mut dyn FnMut(&[u8]) -> SqlResult<()>,
    ) -> SqlResult<bool> {
        if is_temporary_key(key) {
            return match self.objects.entries.get(key) {
                Some(value) => read(value).map(|()| true),
                None => Ok(false),
            };
        }
        self.inner.get_with(key, read)
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        let base: &dyn KeyValueRead = &*self.inner;
        crate::temp_tables::merged_scan(&self.objects.entries, base, start, end, direction, visit)
    }

    fn committed_version(&self) -> Option<u64> {
        if self.objects.entries.is_empty() {
            self.inner.committed_version()
        } else {
            None
        }
    }
}

impl KeyValueWrite for TemporaryWrite<'_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()> {
        if is_temporary_key(key) {
            return self.objects.put(key, value);
        }
        self.inner.put(key, value)
    }

    fn put_if_absent(&mut self, key: &[u8], value: &[u8]) -> SqlResult<bool> {
        if is_temporary_key(key) {
            if self.objects.entries.contains_key(key) {
                return Ok(false);
            }
            self.objects.put(key, value)?;
            return Ok(true);
        }
        self.inner.put_if_absent(key, value)
    }

    fn delete(&mut self, key: &[u8]) -> SqlResult<bool> {
        if is_temporary_key(key) {
            return Ok(self.objects.delete(key));
        }
        self.inner.delete(key)
    }

    fn commit(self) -> SqlResult<()> {
        Ok(())
    }

    fn rollback(self) -> SqlResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{TemporaryObjects, TemporaryWrite, internal_name, is_temporary_id};
    use crate::encoding::{is_temporary_key, rows_prefix, table_key};
    use crate::memory_store::MemoryStore;
    use crate::store::{KeyValueRead, KeyValueStore, KeyValueWrite};

    #[test]
    fn temporary_keys_stay_in_the_session_and_roll_back() {
        let store = MemoryStore::default();
        let mut objects = TemporaryObjects::default();
        let temporary = table_key(&internal_name("t"));
        let permanent = table_key("t");
        assert!(is_temporary_key(&temporary));
        assert!(!is_temporary_key(&permanent));
        let mut writer = store.begin_write().unwrap();
        {
            let mut routed = TemporaryWrite {
                inner: &mut writer,
                objects: &mut objects,
            };
            routed.put(&temporary, b"x").unwrap();
            routed.put(&permanent, b"y").unwrap();
            assert_eq!(routed.get(&temporary).unwrap(), Some(b"x".to_vec()));
        }
        assert_eq!(writer.get(&temporary).unwrap(), None);
        assert_eq!(writer.get(&permanent).unwrap(), Some(b"y".to_vec()));
        writer.commit().unwrap();
        objects.rollback();
        assert!(!objects.has_objects());
    }

    #[test]
    fn temporary_table_ids_never_meet_ordinary_or_intermediate_ones() {
        assert!(!is_temporary_id(1));
        assert!(!is_temporary_id(u32::MAX));
        assert!(is_temporary_id(u32::MAX - (1 << 24)));
        assert!(is_temporary_key(&rows_prefix(u32::MAX - (1 << 24))));
        assert!(!is_temporary_key(&rows_prefix(7)));
    }
}
