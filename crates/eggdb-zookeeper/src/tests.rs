use crate::tree::{self, Change, Operation, Outcome, Tree, code};
use eggdb_keyspace::MemoryKeyspace;

fn create(path: &str, owner: i64, sequential: bool) -> Operation {
    Operation::Create { path: path.to_owned(), data: b"v".to_vec(), acl: Vec::new(), ephemeral_owner: owner, sequential }
}

#[test]
fn znodes_keep_versions_children_and_sequence_numbers() {
    let keyspace = MemoryKeyspace::default();
    let tree = Tree::new(&keyspace);
    let (outcomes, changes) = tree.run(&[create("/app", 0, false)]).unwrap();
    assert!(matches!(&outcomes[0], Ok(Outcome::Created(path, _)) if path == "/app"));
    assert_eq!(changes, vec![Change::Created("/app".to_owned()), Change::ChildrenChanged("/".to_owned())]);
    let (outcomes, _) = tree.run(&[create("/app/lock-", 7, true), create("/app/lock-", 7, true)]).unwrap();
    assert!(matches!(&outcomes[0], Ok(Outcome::Created(path, _)) if path == "/app/lock-0000000000"));
    assert!(matches!(&outcomes[1], Ok(Outcome::Created(path, _)) if path == "/app/lock-0000000001"));
    let (outcomes, _) = tree.run(&[Operation::SetData { path: "/app".to_owned(), data: b"w".to_vec(), version: 5 }]).unwrap();
    assert_eq!(outcomes[0], Err(code::BAD_VERSION));
    let (outcomes, _) = tree.run(&[Operation::Delete { path: "/app".to_owned(), version: -1 }]).unwrap();
    assert_eq!(outcomes[0], Err(code::NOT_EMPTY));
    let owned = tree::read(&keyspace, |keys| tree::ephemerals(keys, 7)).unwrap();
    assert_eq!(owned.len(), 2);
}

#[test]
fn a_failed_multi_changes_nothing_and_reports_each_operation() {
    let keyspace = MemoryKeyspace::default();
    let tree = Tree::new(&keyspace);
    let (outcomes, changes) = tree
        .run(&[create("/a", 0, false), Operation::Check { path: "/missing".to_owned(), version: -1 }, create("/b", 0, false)])
        .unwrap();
    assert_eq!(outcomes, vec![Err(code::OK), Err(code::NO_NODE), Err(code::RUNTIME_INCONSISTENCY)]);
    assert!(changes.is_empty());
    assert!(tree::read(&keyspace, |keys| tree::get(keys, "/a")).unwrap().is_none());
}
