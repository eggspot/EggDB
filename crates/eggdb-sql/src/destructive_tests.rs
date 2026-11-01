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

fn keyed_store() -> MemoryStore {
    let store = MemoryStore::default();
    Session::new(&store)
        .execute(
            "CREATE TABLE t (id integer PRIMARY KEY, name text, code varchar(10), n integer); \
             CREATE UNIQUE INDEX t_code ON t (code); \
             CREATE TABLE pair (a integer, b integer, note text, PRIMARY KEY (a, b)); \
             CREATE TABLE loose (id integer, note text); \
             CREATE TABLE u (id integer PRIMARY KEY); \
             CREATE TABLE parent (id integer PRIMARY KEY, v integer); \
             CREATE TABLE child (id integer PRIMARY KEY, parent_id integer REFERENCES parent (id) ON DELETE CASCADE ON UPDATE CASCADE); \
             CREATE TABLE guarded (id integer PRIMARY KEY, v integer); \
             CREATE TABLE guarded_child (id integer PRIMARY KEY, guarded_id integer REFERENCES guarded (id)); \
             CREATE TABLE audited (id integer PRIMARY KEY, v integer); \
             CREATE FUNCTION keep_row() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN OLD; END $$; \
             CREATE TRIGGER audited_delete BEFORE DELETE ON audited FOR EACH ROW EXECUTE FUNCTION keep_row()",
            &mut |_| {},
        )
        .unwrap();
    store
}

fn kind_with_keys(store: &MemoryStore, dialect: Dialect, text: &str) -> Option<DestructiveKind> {
    let mut session = Session::with_dialect(store, dialect);
    match session.destructive_statement(text) {
        Ok(found) => found.map(|statement| statement.parts[0].kind),
        Err(error) => panic!("{dialect:?} {text:?}: {error:?}"),
    }
}

#[test]
fn without_the_database_every_filtered_delete_and_update_is_destructive() {
    for dialect in Dialect::ALL {
        for (text, expected) in [
            (
                "DELETE FROM t WHERE id = 1",
                DestructiveKind::DeleteOfManyRows,
            ),
            (
                "DELETE FROM t WHERE t.id > 0",
                DestructiveKind::DeleteOfManyRows,
            ),
            (
                "UPDATE t SET name = 'x' WHERE id = 2",
                DestructiveKind::UpdateOfManyRows,
            ),
            (
                "DELETE FROM t WHERE EXISTS (SELECT 1 FROM u)",
                DestructiveKind::DeleteWithoutFilter,
            ),
            (
                "DELETE FROM t WHERE u.id = 1",
                DestructiveKind::DeleteWithoutFilter,
            ),
            (
                "UPDATE t SET name = 'x' WHERE (SELECT count(*) FROM u) > 0",
                DestructiveKind::UpdateWithoutFilter,
            ),
        ] {
            assert_eq!(kind(dialect, text), Some(expected), "{dialect:?} {text}");
        }
    }
}

#[test]
fn a_where_pinning_every_column_of_a_key_to_values_is_not_destructive_in_any_dialect() {
    let store = keyed_store();
    for dialect in Dialect::ALL {
        for text in [
            "DELETE FROM t WHERE id = 7",
            "DELETE FROM t WHERE t.id = 7",
            "DELETE FROM t WHERE 7 = id",
            "DELETE FROM t WHERE id = -7",
            "DELETE FROM t WHERE id IN (1, 2, 3)",
            "DELETE FROM t WHERE id = 7 AND name = 'x'",
            "DELETE FROM t WHERE id = 7 AND (name = 'x' OR name = 'y')",
            "DELETE FROM t WHERE code = 'a1'",
            "DELETE FROM pair WHERE a = 1 AND b = 2",
            "DELETE FROM pair WHERE b IN (2, 3) AND a = 1",
            "UPDATE t SET name = 'x' WHERE id = 2",
            "UPDATE t SET name = NULL, n = 3 WHERE code = 'a1'",
            "UPDATE parent SET v = 2 WHERE id = 1",
            "DELETE FROM guarded WHERE id = 1",
            "UPDATE audited SET v = 2 WHERE id = 1",
        ] {
            assert_eq!(
                kind_with_keys(&store, dialect, text),
                None,
                "{dialect:?} {text}"
            );
        }
    }
}

#[test]
fn a_where_that_can_match_many_rows_is_destructive_in_every_dialect() {
    let store = keyed_store();
    let long_list: Vec<String> = (1..=101).map(|id| id.to_string()).collect();
    let long_delete = format!("DELETE FROM t WHERE id IN ({})", long_list.join(", "));
    let wide_pair = format!(
        "DELETE FROM pair WHERE a IN ({}) AND b IN (1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11)",
        long_list[..10].join(", ")
    );
    for dialect in Dialect::ALL {
        for (text, expected) in [
            (
                "DELETE FROM t WHERE id = id",
                DestructiveKind::DeleteOfManyRows,
            ),
            (
                "DELETE FROM t WHERE id IS NOT NULL",
                DestructiveKind::DeleteOfManyRows,
            ),
            (
                "DELETE FROM t WHERE id >= 0 OR id < 0",
                DestructiveKind::DeleteOfManyRows,
            ),
            (
                "UPDATE t SET name = NULL WHERE id = id",
                DestructiveKind::UpdateOfManyRows,
            ),
            (
                "DELETE FROM t WHERE id > 5",
                DestructiveKind::DeleteOfManyRows,
            ),
            (
                "DELETE FROM t WHERE id = 1 OR id = 2",
                DestructiveKind::DeleteOfManyRows,
            ),
            (
                "DELETE FROM t WHERE NOT (id <> 1)",
                DestructiveKind::DeleteOfManyRows,
            ),
            (
                "DELETE FROM t WHERE id NOT IN (1)",
                DestructiveKind::DeleteOfManyRows,
            ),
            (
                "DELETE FROM t WHERE name = 'x'",
                DestructiveKind::DeleteOfManyRows,
            ),
            (
                "DELETE FROM t WHERE code = 1",
                DestructiveKind::DeleteOfManyRows,
            ),
            (
                "DELETE FROM t WHERE id IN (SELECT id FROM loose)",
                DestructiveKind::DeleteOfManyRows,
            ),
            (
                "DELETE FROM t WHERE id = (SELECT max(id) FROM loose)",
                DestructiveKind::DeleteOfManyRows,
            ),
            (
                "DELETE FROM pair WHERE a = 1",
                DestructiveKind::DeleteOfManyRows,
            ),
            (
                "DELETE FROM loose WHERE id = 1",
                DestructiveKind::DeleteOfManyRows,
            ),
            (
                "DELETE FROM parent WHERE id = 1",
                DestructiveKind::DeleteOfManyRows,
            ),
            (
                "UPDATE parent SET id = 2 WHERE id = 1",
                DestructiveKind::UpdateOfManyRows,
            ),
            (
                "DELETE FROM audited WHERE id = 1",
                DestructiveKind::DeleteOfManyRows,
            ),
            (&long_delete, DestructiveKind::DeleteOfManyRows),
            (&wide_pair, DestructiveKind::DeleteOfManyRows),
            (
                "DELETE FROM t WHERE 1 = 1",
                DestructiveKind::DeleteWithoutFilter,
            ),
            ("DELETE FROM t", DestructiveKind::DeleteWithoutFilter),
        ] {
            assert_eq!(
                kind_with_keys(&store, dialect, text),
                Some(expected),
                "{dialect:?} {text}"
            );
        }
    }
    for (dialect, text) in [
        (
            Dialect::PostgreSql,
            "DELETE FROM t WHERE id = CAST('7' AS integer)",
        ),
        (
            Dialect::PostgreSql,
            "DELETE FROM t WHERE code = CAST(1 AS text)",
        ),
    ] {
        assert_eq!(kind_with_keys(&store, dialect, text), None, "{text}");
    }
    for (dialect, text) in [
        (
            Dialect::PostgreSql,
            "DELETE FROM t WHERE code = CAST('1' AS integer)",
        ),
        (Dialect::PostgreSql, "DELETE FROM t WHERE id = random()"),
        (
            Dialect::PostgreSql,
            "DELETE FROM t WHERE (id, n) IN ((1, 2))",
        ),
        (Dialect::PostgreSql, "DELETE FROM missing WHERE id = 1"),
    ] {
        assert!(
            !matches!(kind_with_keys_or_error(&store, dialect, text), Ok(None)),
            "{text}"
        );
    }
}

fn kind_with_keys_or_error(
    store: &MemoryStore,
    dialect: Dialect,
    text: &str,
) -> Result<Option<DestructiveKind>, OneStatementError> {
    let mut session = Session::with_dialect(store, dialect);
    session
        .destructive_statement(text)
        .map(|found| found.map(|statement| statement.parts[0].kind))
}

#[test]
fn upserts_and_merges_that_overwrite_rows_are_destructive() {
    let store = keyed_store();
    for (dialect, text, expected) in [
        (
            Dialect::MySql,
            "REPLACE INTO t VALUES (1, 'a', 'c1', 1)",
            DestructiveKind::InsertOverwritingRows,
        ),
        (
            Dialect::MySql,
            "INSERT INTO t VALUES (1, 'a', 'c1', 1) ON DUPLICATE KEY UPDATE name = 'b'",
            DestructiveKind::InsertOverwritingRows,
        ),
        (
            Dialect::Sqlite,
            "INSERT OR REPLACE INTO t VALUES (1, 'a', 'c1', 1)",
            DestructiveKind::InsertOverwritingRows,
        ),
        (
            Dialect::Sqlite,
            "REPLACE INTO t VALUES (1, 'a', 'c1', 1)",
            DestructiveKind::InsertOverwritingRows,
        ),
        (
            Dialect::Sqlite,
            "INSERT INTO t VALUES (1, 'a', 'c1', 1) ON CONFLICT (id) DO UPDATE SET name = 'b'",
            DestructiveKind::InsertOverwritingRows,
        ),
        (
            Dialect::PostgreSql,
            "INSERT INTO t VALUES (1, 'a', 'c1', 1) ON CONFLICT (id) DO UPDATE SET name = 'b'",
            DestructiveKind::InsertOverwritingRows,
        ),
        (
            Dialect::PostgreSql,
            "INSERT INTO t SELECT id, note, note, id FROM loose WHERE id > 0 ON CONFLICT (id) DO UPDATE SET name = 'b'",
            DestructiveKind::InsertOverwritingRows,
        ),
        (
            Dialect::Oracle,
            "MERGE INTO t USING u ON (t.id = u.id) WHEN MATCHED THEN UPDATE SET t.name = 'b'",
            DestructiveKind::UpdateOfManyRows,
        ),
    ] {
        assert_eq!(kind(dialect, text), Some(expected), "{dialect:?} {text}");
        assert_eq!(
            kind_with_keys(&store, dialect, text),
            Some(expected),
            "{dialect:?} {text}"
        );
    }
    for (dialect, text) in [
        (
            Dialect::PostgreSql,
            "INSERT INTO t VALUES (1, 'a', 'c1', 1)",
        ),
        (
            Dialect::PostgreSql,
            "INSERT INTO t VALUES (1, 'a', 'c1', 1) ON CONFLICT DO NOTHING",
        ),
        (
            Dialect::MySql,
            "INSERT IGNORE INTO t VALUES (1, 'a', 'c1', 1)",
        ),
        (
            Dialect::Sqlite,
            "INSERT OR IGNORE INTO t VALUES (1, 'a', 'c1', 1)",
        ),
        (Dialect::PostgreSql, "INSERT INTO u SELECT id FROM loose"),
    ] {
        assert_eq!(
            kind_with_keys(&store, dialect, text),
            None,
            "{dialect:?} {text}"
        );
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

#[test]
fn transact_sql_brackets_and_transactions_are_classified_as_in_the_other_dialects() {
    for (text, expected) in [
        (
            "DELETE FROM [t]",
            Some(DestructiveKind::DeleteWithoutFilter),
        ),
        ("TRUNCATE TABLE [dbo].[t]", Some(DestructiveKind::Truncate)),
        ("DROP TABLE [dbo].[t]", Some(DestructiveKind::DropTable)),
        (
            "DELETE FROM [t] WHERE [id] = 1",
            Some(DestructiveKind::DeleteOfManyRows),
        ),
        ("SELECT TOP 1 [id] FROM [dbo].[t]", None),
    ] {
        assert_eq!(kind(Dialect::SqlServer, text), expected, "{text}");
    }
    let keyed = keyed_store();
    for (text, expected) in [
        ("DELETE FROM [t] WHERE [id] = 1", None),
        ("DELETE FROM [dbo].[t] WHERE [Id] = 1", None),
        (
            "DELETE FROM [t] WHERE [name] = N'x'",
            Some(DestructiveKind::DeleteOfManyRows),
        ),
    ] {
        assert_eq!(
            kind_with_keys(&keyed, Dialect::SqlServer, text),
            expected,
            "{text}"
        );
    }
    assert_eq!(
        statement(Dialect::SqlServer, "TRUNCATE TABLE [dbo].[T]").parts[0].objects,
        ["t"]
    );
    for text in [
        "BEGIN TRAN",
        "BEGIN TRANSACTION loading",
        "COMMIT TRAN",
        "COMMIT",
        "ROLLBACK TRANSACTION",
        "ROLLBACK WORK",
        "SAVE TRAN point",
        "ROLLBACK TRANSACTION point",
    ] {
        assert_eq!(
            destructive_statement(text, Dialect::SqlServer),
            Err(OneStatementError::Refused(
                OneStatementRefusal::TransactionControl
            )),
            "{text}"
        );
    }
    for text in ["SAVE point", "BEGIN DELETE FROM t END"] {
        assert!(
            matches!(
                destructive_statement(text, Dialect::SqlServer),
                Err(OneStatementError::Failed(_))
            ),
            "{text}"
        );
    }
    assert_eq!(
        destructive_statement("DELETE FROM t DELETE FROM u", Dialect::SqlServer),
        Err(OneStatementError::Refused(
            OneStatementRefusal::MoreThanOne { position: 14 }
        ))
    );
}
