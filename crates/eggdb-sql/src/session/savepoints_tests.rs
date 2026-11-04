use super::{Undo, UndoEntry, UndoWrite};
use crate::error::SqlState;
use crate::memory_store::MemoryStore;
use crate::store::{KeyValueRead, KeyValueStore, KeyValueWrite};

#[test]
fn a_statement_whose_earlier_values_outgrow_the_limit_fails_before_writing_more() {
    let store = MemoryStore::default();
    let mut writer = store.begin_write().expect("write");
    writer.put(b"a", &[1; 100]).expect("seed a");
    writer.put(b"b", &[2; 100]).expect("seed b");
    let mut log = Undo::new();
    let mut undoable = UndoWrite::with_most_bytes(&mut writer, &mut log, 300);
    undoable.put(b"a", b"new a").expect("first change fits");
    undoable
        .put(b"c", b"new c")
        .expect("a new key holds only its name");
    let refused = undoable
        .put(b"b", b"new b")
        .expect_err("the third change passes the limit");
    assert_eq!(refused.state(), SqlState::OUT_OF_MEMORY);
    assert_eq!(undoable.get(b"b").expect("read b"), Some(vec![2; 100]));
    let refused_delete = undoable.delete(b"b").expect_err("a delete is counted too");
    assert_eq!(refused_delete.state(), SqlState::OUT_OF_MEMORY);
    assert_eq!(log.len(), 2);
    assert!(matches!(
        &log[0],
        UndoEntry::Statement { key, previous: Some(previous) } if key == b"a" && previous.len() == 100
    ));
}

#[test]
fn the_default_limit_is_the_statement_work_memory() {
    let store = MemoryStore::default();
    let mut writer = store.begin_write().expect("write");
    let mut log = Undo::new();
    let undoable = UndoWrite::new(&mut writer, &mut log);
    assert_eq!(undoable.most_bytes, crate::memory::WORK_MEMORY_LIMIT);
}
