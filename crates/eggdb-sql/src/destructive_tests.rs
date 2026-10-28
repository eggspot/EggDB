use crate::memory_store::MemoryStore;
use crate::{
    DestructiveKind, DestructiveStatement, Dialect, OneStatementError, OneStatementRefusal,
    Session, destructive_statement,
};

fn kind(dialect: Dialect, text: &str) -> Option<DestructiveKind> {
    match destructive_statement(text, dialect) {
        Ok(found) => found.map(|statement| statement.parts[0].kind),
        Err(error) => panic!("{dialect:?} {text:?}: {error:?}"),
    }
}

fn statement(dialect: Dialect, text: &str) -> DestructiveStatement {
    destructive_statement(text, dialect)
        .unwrap_or_else(|error| panic!("{dialect:?} {text:?}: {error:?}"))
        .unwrap_or_else(|| panic!("{dialect:?} {text:?} is not destructive"))
}

#[test]
fn drops_truncates_and_unfiltered_deletes_and_updates_are_destructive_in_every_dialect() {
    for dialect in Dialect::ALL {
        for (text, expected) in [
            ("DROP TABLE t", DestructiveKind::DropTable),
            ("DROP VIEW v", DestructiveKind::DropView),
            ("DELETE FROM t", DestructiveKind::DeleteWithoutFilter),
            (
                "DELETE FROM t WHERE 1 = 1",
                DestructiveKind::DeleteWithoutFilter,
            ),
            (
                "UPDATE t SET name = 'x'",
                DestructiveKind::UpdateWithoutFilter,
            ),
            (
                "UPDATE t SET name = 'x' WHERE 1 = 1",
                DestructiveKind::UpdateWithoutFilter,
            ),
            (
                "ALTER TABLE t DROP COLUMN name",
                DestructiveKind::DropColumn,
            ),
        ] {
            assert_eq!(kind(dialect, text), Some(expected), "{dialect:?} {text}");
        }
    }
    for dialect in Dialect::ALL {
        let text = if dialect == Dialect::MySql {
            "DROP INDEX t_name ON t"
        } else {
            "DROP INDEX t_name"
        };
        assert_eq!(
            kind(dialect, text),
            Some(DestructiveKind::DropIndex),
            "{dialect:?}"
        );
    }
    for dialect in Dialect::ALL {
        match destructive_statement("TRUNCATE TABLE t", dialect) {
            Ok(Some(found)) => assert_eq!(found.parts[0].kind, DestructiveKind::Truncate),
            Err(OneStatementError::Failed(_)) => assert_ne!(dialect, Dialect::Oracle),
            other => panic!("{dialect:?}: {other:?}"),
        }
    }
    for (dialect, text, expected) in [
        (
            Dialect::PostgreSql,
            "DROP SEQUENCE s",
            DestructiveKind::DropSequence,
        ),
        (
            Dialect::PostgreSql,
            "DROP SCHEMA app CASCADE",
            DestructiveKind::DropSchema,
        ),
        (
            Dialect::PostgreSql,
            "ALTER TABLE t DROP CONSTRAINT t_check",
            DestructiveKind::DropConstraint,
        ),
        (
            Dialect::PostgreSql,
            "ALTER TABLE t ALTER COLUMN name TYPE varchar(10)",
            DestructiveKind::ChangeColumnType,
        ),
        (
            Dialect::MySql,
            "ALTER TABLE t MODIFY name varchar(10)",
            DestructiveKind::ChangeColumnType,
        ),
        (
            Dialect::MySql,
            "ALTER TABLE t CHANGE name full_name text",
            DestructiveKind::ChangeColumnType,
        ),
        (
            Dialect::MySql,
            "ALTER TABLE t DROP PRIMARY KEY",
            DestructiveKind::DropPrimaryKey,
        ),
    ] {
        assert_eq!(kind(dialect, text), Some(expected), "{dialect:?} {text}");
    }
}

#[test]
fn the_objects_are_named_and_cascade_is_kept() {
    let dropped = statement(Dialect::PostgreSql, "DROP TABLE a, app.b CASCADE");
    assert_eq!(dropped.parts[0].kind, DestructiveKind::DropTable);
    assert_eq!(dropped.parts[0].objects, ["a", "app.b"]);
    assert!(dropped.cascade);
    let column = statement(
        Dialect::PostgreSql,
        "ALTER TABLE t DROP COLUMN name CASCADE",
    );
    assert_eq!(column.parts[0].objects, ["t.name"]);
    assert!(column.cascade);
    let two = statement(Dialect::MySql, "ALTER TABLE t DROP COLUMN a, DROP COLUMN b");
    assert_eq!(two.parts.len(), 1);
    assert_eq!(two.parts[0].objects, ["t.a", "t.b"]);
    assert_eq!(
        statement(Dialect::Sqlite, "DELETE FROM t")
            .objects()
            .collect::<Vec<_>>(),
        ["t"]
    );
}

#[test]
fn an_alter_table_doing_several_kinds_of_destruction_names_each_with_its_own_kind() {
    let mixed = statement(
        Dialect::PostgreSql,
        "ALTER TABLE m ALTER COLUMN b TYPE bigint, DROP COLUMN a",
    );
    assert_eq!(mixed.parts.len(), 2);
    assert_eq!(mixed.parts[0].kind, DestructiveKind::ChangeColumnType);
    assert_eq!(mixed.parts[0].objects, ["m.b"]);
    assert_eq!(mixed.parts[1].kind, DestructiveKind::DropColumn);
    assert_eq!(mixed.parts[1].objects, ["m.a"]);
    let mysql = statement(
        Dialect::MySql,
        "ALTER TABLE m MODIFY b bigint, DROP COLUMN a, DROP PRIMARY KEY, DROP COLUMN c",
    );
    let kinds: Vec<DestructiveKind> = mysql.parts.iter().map(|part| part.kind).collect();
    assert_eq!(
        kinds,
        [
            DestructiveKind::ChangeColumnType,
            DestructiveKind::DropColumn,
            DestructiveKind::DropPrimaryKey
        ]
    );
    assert_eq!(mysql.parts[1].objects, ["m.a", "m.c"]);
    assert_eq!(mysql.parts[2].objects, ["m"]);
    assert_eq!(
        mysql.objects().collect::<Vec<_>>(),
        ["m.b", "m.a", "m.c", "m"]
    );
}

#[test]
fn a_where_naming_a_column_of_the_table_is_not_destructive() {
    for dialect in Dialect::ALL {
        for text in [
            "DELETE FROM t WHERE id = 1",
            "DELETE FROM t WHERE t.id > 0",
            "DELETE FROM t WHERE id IN (SELECT id FROM u)",
            "UPDATE t SET name = 'x' WHERE id = 2",
            "UPDATE t SET name = 'x' WHERE name IS NOT NULL",
        ] {
            assert_eq!(kind(dialect, text), None, "{dialect:?} {text}");
        }
        for text in [
            "DELETE FROM t WHERE EXISTS (SELECT 1 FROM u)",
            "DELETE FROM t WHERE u.id = 1",
            "UPDATE t SET name = 'x' WHERE (SELECT count(*) FROM u) > 0",
        ] {
            assert!(kind(dialect, text).is_some(), "{dialect:?} {text}");
        }
    }
}

#[test]
fn creating_inserting_renaming_and_reading_are_not_destructive() {
    for dialect in Dialect::ALL {
        for text in [
            "SELECT id FROM t",
            "INSERT INTO t VALUES (1, 'a')",
            "CREATE TABLE n (id integer PRIMARY KEY)",
            "CREATE INDEX t_n ON t (name)",
            "CREATE VIEW w AS SELECT id FROM t",
            "ALTER TABLE t ADD COLUMN note text",
            "ALTER TABLE t RENAME TO t2",
        ] {
            match destructive_statement(text, dialect) {
                Ok(None) => {}
                Err(OneStatementError::Failed(_)) if dialect != Dialect::PostgreSql => {}
                other => panic!("{dialect:?} {text}: {other:?}"),
            }
        }
    }
    for (dialect, text) in [
        (Dialect::PostgreSql, "EXPLAIN DELETE FROM t"),
        (Dialect::Sqlite, "EXPLAIN QUERY PLAN DELETE FROM t"),
        (
            Dialect::PostgreSql,
            "CREATE OR REPLACE VIEW w AS SELECT id FROM t",
        ),
        (Dialect::PostgreSql, "ALTER SEQUENCE s RESTART"),
        (
            Dialect::PostgreSql,
            "ALTER TABLE t RENAME COLUMN name TO full_name",
        ),
        (Dialect::PostgreSql, "COMMENT ON TABLE t IS 'x'"),
        (Dialect::MySql, "REPLACE INTO t VALUES (1, 'a')"),
        (
            Dialect::MySql,
            "INSERT INTO t VALUES (1, 'a') ON DUPLICATE KEY UPDATE name = 'b'",
        ),
        (Dialect::Sqlite, "INSERT OR REPLACE INTO t VALUES (1, 'a')"),
        (Dialect::Sqlite, "PRAGMA foreign_keys = ON"),
    ] {
        assert_eq!(kind(dialect, text), None, "{dialect:?} {text}");
    }
}

#[test]
fn statements_that_run_other_statements_never_pass_as_harmless() {
    for (dialect, text) in [
        (Dialect::PostgreSql, "CALL clean_up()"),
        (Dialect::PostgreSql, "DO $$ BEGIN DELETE FROM t; END $$"),
        (Dialect::MySql, "CALL clean_up()"),
        (Dialect::Oracle, "BEGIN DELETE FROM t; END;"),
        (Dialect::Oracle, "EXECUTE IMMEDIATE 'DROP TABLE t'"),
        (Dialect::Oracle, "CALL clean_up()"),
    ] {
        match destructive_statement(text, dialect) {
            Ok(Some(_)) | Err(_) => {}
            Ok(None) => panic!(
                "{dialect:?} {text:?} parses as harmless: a statement that runs other statements must be destructive"
            ),
        }
    }
}

#[test]
fn the_one_statement_refusals_come_first() {
    for (text, refusal) in [
        ("", OneStatementRefusal::Empty),
        (
            "DROP TABLE t; DROP TABLE u",
            OneStatementRefusal::MoreThanOne { position: 14 },
        ),
        ("COMMIT", OneStatementRefusal::TransactionControl),
    ] {
        assert_eq!(
            destructive_statement(text, Dialect::PostgreSql),
            Err(OneStatementError::Refused(refusal)),
            "{text}"
        );
    }
    assert!(matches!(
        destructive_statement("DROP TABLE", Dialect::PostgreSql),
        Err(OneStatementError::Failed(_))
    ));
}

fn store() -> MemoryStore {
    let store = MemoryStore::default();
    Session::new(&store)
        .execute(
            "CREATE TABLE t (id integer PRIMARY KEY, name text); CREATE TABLE u (id integer PRIMARY KEY); INSERT INTO t VALUES (1, 'a')",
            &mut |_| {},
        )
        .unwrap();
    store
}

fn tables(store: &MemoryStore) -> Vec<String> {
    Session::new(store)
        .tables()
        .unwrap()
        .into_iter()
        .map(|table| table.name)
        .collect()
}

#[test]
fn only_the_confirmed_destructive_statement_runs() {
    let store = store();
    let mut session = Session::new(&store);
    assert_eq!(
        session.run_one_statement_if_confirmed("DROP TABLE t", None, &mut |_| true),
        Err(OneStatementError::Refused(OneStatementRefusal::Unconfirmed))
    );
    let other = statement(Dialect::PostgreSql, "DROP TABLE u");
    assert_eq!(
        session.run_one_statement_if_confirmed("DROP TABLE t", Some(&other), &mut |_| true),
        Err(OneStatementError::Refused(OneStatementRefusal::Unconfirmed))
    );
    assert_eq!(tables(&store), ["t", "u"]);
    session
        .run_one_statement_if_confirmed("INSERT INTO t VALUES (2, 'b')", None, &mut |_| true)
        .unwrap();
    let confirmed = session.destructive_statement("DROP TABLE t").unwrap();
    assert_eq!(
        confirmed,
        destructive_statement("DROP TABLE t", Dialect::PostgreSql).unwrap()
    );
    session
        .run_one_statement_if_confirmed("DROP TABLE t", confirmed.as_ref(), &mut |_| true)
        .unwrap();
    assert_eq!(tables(&store), ["u"]);
    session
        .run_one_statement("DROP TABLE u", &mut |_| true)
        .unwrap();
    assert!(tables(&store).is_empty());
}
