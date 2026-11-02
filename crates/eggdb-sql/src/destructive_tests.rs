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

fn reaching_store() -> MemoryStore {
    let store = MemoryStore::default();
    Session::new(&store)
        .execute(
            "CREATE TABLE t (id integer PRIMARY KEY, name text); \
             CREATE TABLE u (id integer PRIMARY KEY); \
             CREATE TABLE log (note text); \
             CREATE FUNCTION wipe_u() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN DELETE FROM u; RETURN NEW; END $$; \
             CREATE FUNCTION wipe_now() RETURNS integer LANGUAGE plpgsql AS $$ BEGIN DELETE FROM u; RETURN 1; END $$; \
             CREATE TABLE inserted (id integer PRIMARY KEY, v integer); \
             CREATE TRIGGER inserted_wipes AFTER INSERT ON inserted FOR EACH ROW EXECUTE FUNCTION wipe_u(); \
             CREATE VIEW t_view AS SELECT id, name FROM t; \
             CREATE TABLE measured (id integer, region text, PRIMARY KEY (id, region)) PARTITION BY LIST (region); \
             CREATE TABLE measured_eu PARTITION OF measured FOR VALUES IN ('eu'); \
             CREATE TRIGGER measured_eu_wipes BEFORE DELETE ON measured_eu FOR EACH ROW EXECUTE FUNCTION wipe_u(); \
             CREATE TABLE ordered (id integer, region text, PRIMARY KEY (id, region)) PARTITION BY LIST (region); \
             CREATE TABLE ordered_eu PARTITION OF ordered FOR VALUES IN ('eu'); \
             CREATE TABLE ordered_lines (id integer PRIMARY KEY, order_id integer, region text, \
                 FOREIGN KEY (order_id, region) REFERENCES ordered_eu (id, region) ON DELETE CASCADE); \
             CREATE TABLE quiet (id integer, region text, PRIMARY KEY (id, region)) PARTITION BY LIST (region); \
             CREATE TABLE quiet_eu PARTITION OF quiet FOR VALUES IN ('eu')",
            &mut |_| {},
        )
        .unwrap();
    store
}

fn classified_with_keys(
    store: &MemoryStore,
    dialect: Dialect,
    text: &str,
) -> Option<DestructiveStatement> {
    Session::with_dialect(store, dialect)
        .destructive_statement(text)
        .unwrap_or_else(|error| panic!("{dialect:?} {text:?}: {error:?}"))
}

#[test]
fn a_key_pinned_delete_of_a_partitioned_table_reaches_its_partitions_triggers_and_foreign_keys() {
    let store = reaching_store();
    for text in [
        "DELETE FROM measured WHERE id = 1 AND region = 'eu'",
        "DELETE FROM ordered WHERE id = 1 AND region = 'eu'",
        "UPDATE measured SET region = 'eu' WHERE id = 1 AND region = 'eu'",
    ] {
        let found = kind_with_keys(&store, Dialect::PostgreSql, text);
        assert!(
            matches!(
                found,
                Some(DestructiveKind::DeleteOfManyRows | DestructiveKind::UpdateOfManyRows)
            ),
            "{text}: {found:?}"
        );
    }
    assert_eq!(
        kind_with_keys(
            &store,
            Dialect::PostgreSql,
            "DELETE FROM quiet WHERE id = 1 AND region = 'eu'"
        ),
        None
    );
}

#[test]
fn an_insert_that_fires_a_trigger_or_cannot_be_looked_up_runs_code() {
    let store = reaching_store();
    for (dialect, text, object) in [
        (
            Dialect::PostgreSql,
            "INSERT INTO inserted VALUES (1, 1)",
            "inserted",
        ),
        (
            Dialect::PostgreSql,
            "INSERT INTO inserted VALUES (1, 1) ON CONFLICT DO NOTHING",
            "inserted",
        ),
        (
            Dialect::MySql,
            "INSERT IGNORE INTO inserted VALUES (1, 1)",
            "inserted",
        ),
        (
            Dialect::Oracle,
            "MERGE INTO inserted USING u ON (inserted.id = u.id) WHEN NOT MATCHED THEN INSERT VALUES (u.id, 1)",
            "inserted",
        ),
        (
            Dialect::PostgreSql,
            "INSERT INTO t_view VALUES (5, 'v')",
            "t_view",
        ),
    ] {
        let found = classified_with_keys(&store, dialect, text)
            .unwrap_or_else(|| panic!("{dialect:?} {text} is not destructive"));
        assert_eq!(found.parts[0].kind, DestructiveKind::RunCode, "{text}");
        assert_eq!(found.parts[0].objects, [object], "{text}");
    }
    for text in [
        "INSERT INTO t VALUES (1, 'a')",
        "INSERT INTO t VALUES (1, 'a') ON CONFLICT DO NOTHING",
        "INSERT INTO quiet VALUES (1, 'eu')",
        "INSERT INTO nowhere VALUES (1)",
    ] {
        assert_eq!(
            classified_with_keys(&store, Dialect::PostgreSql, text),
            None,
            "{text}"
        );
    }
    for dialect in Dialect::ALL {
        match destructive_statement("INSERT INTO t VALUES (1, 'a')", dialect) {
            Ok(Some(found)) => assert_eq!(found.parts[0].kind, DestructiveKind::RunCode),
            Err(OneStatementError::Failed(_)) if dialect != Dialect::PostgreSql => {}
            other => panic!("{dialect:?}: {other:?}"),
        }
    }
}

#[test]
fn transact_sql_exec_and_dynamic_sql_run_code() {
    let store = reaching_store();
    for text in [
        "EXEC clean_up",
        "EXECUTE dbo.clean_up 1",
        "EXEC('DELETE FROM t')",
        "EXEC sp_executesql N'DELETE FROM t'",
    ] {
        for found in [
            statement(Dialect::SqlServer, text),
            classified_with_keys(&store, Dialect::SqlServer, text)
                .unwrap_or_else(|| panic!("{text} is not destructive")),
        ] {
            assert_eq!(found.parts[0].kind, DestructiveKind::RunCode, "{text}");
        }
    }
    assert_eq!(
        statement(Dialect::SqlServer, "EXEC clean_up").parts[0].objects,
        ["clean_up"]
    );
}

#[test]
fn a_select_of_a_function_that_may_write_runs_code() {
    let store = reaching_store();
    for (text, object) in [
        ("SELECT wipe_now()", "wipe_now"),
        ("SELECT * FROM wipe_now()", "wipe_now"),
        ("SELECT 1 UNION ALL SELECT wipe_now()", "wipe_now"),
    ] {
        let found = classified_with_keys(&store, Dialect::PostgreSql, text)
            .unwrap_or_else(|| panic!("{text} is not destructive"));
        assert_eq!(found.parts[0].kind, DestructiveKind::RunCode, "{text}");
        assert_eq!(found.parts[0].objects, [object], "{text}");
    }
    for text in [
        "SELECT id FROM t",
        "SELECT 1",
        "SELECT name FROM t WHERE id = 1",
    ] {
        assert_eq!(
            classified_with_keys(&store, Dialect::PostgreSql, text),
            None,
            "{text}"
        );
    }
}

#[test]
fn a_with_statement_lists_every_modifying_part_and_its_body() {
    let found = statement(
        Dialect::PostgreSql,
        "WITH a AS (DELETE FROM t RETURNING id), b AS (DELETE FROM u RETURNING id) UPDATE log SET note = 'x'",
    );
    assert_eq!(
        found.parts,
        [
            crate::DestructivePart {
                kind: DestructiveKind::DeleteWithoutFilter,
                objects: vec!["t".to_owned(), "u".to_owned()],
            },
            crate::DestructivePart {
                kind: DestructiveKind::UpdateWithoutFilter,
                objects: vec!["log".to_owned()],
            },
        ]
    );
}

#[test]
fn dropping_or_disabling_a_trigger_or_row_security_is_destructive() {
    for (dialect, text, expected, object) in [
        (
            Dialect::PostgreSql,
            "DROP TRIGGER inserted_wipes ON inserted",
            DestructiveKind::DropTrigger,
            "inserted.inserted_wipes",
        ),
        (
            Dialect::PostgreSql,
            "ALTER TABLE inserted DISABLE TRIGGER inserted_wipes",
            DestructiveKind::DisableTrigger,
            "inserted.inserted_wipes",
        ),
        (
            Dialect::PostgreSql,
            "ALTER TABLE inserted DISABLE TRIGGER ALL",
            DestructiveKind::DisableTrigger,
            "inserted",
        ),
        (
            Dialect::PostgreSql,
            "DROP POLICY own_rows ON t",
            DestructiveKind::DropPolicy,
            "t.own_rows",
        ),
        (
            Dialect::PostgreSql,
            "ALTER TABLE t DISABLE ROW LEVEL SECURITY",
            DestructiveKind::DisableRowSecurity,
            "t",
        ),
    ] {
        let found = statement(dialect, text);
        assert_eq!(found.parts[0].kind, expected, "{text}");
        assert_eq!(found.parts[0].objects, [object], "{text}");
    }
    for text in [
        "ALTER TABLE t ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE inserted ENABLE TRIGGER inserted_wipes",
        "CREATE POLICY own_rows ON t USING (id > 0)",
    ] {
        assert_eq!(kind(Dialect::PostgreSql, text), None, "{text}");
    }
}

fn runs_function(found: Option<DestructiveStatement>, function: &str) -> bool {
    found.is_some_and(|found| {
        found.parts.iter().any(|part| {
            part.kind == DestructiveKind::RunCode && part.objects.iter().any(|o| o == function)
        })
    })
}

#[test]
fn a_write_calling_a_user_function_anywhere_in_it_runs_code() {
    let store = reaching_store();
    for (dialect, text) in [
        (
            Dialect::PostgreSql,
            "INSERT INTO t VALUES (5, wipe_now()::text)",
        ),
        (
            Dialect::PostgreSql,
            "INSERT INTO t SELECT 6, wipe_now()::text",
        ),
        (
            Dialect::PostgreSql,
            "INSERT INTO t SELECT id + 10, name FROM t WHERE id = wipe_now()",
        ),
        (
            Dialect::PostgreSql,
            "INSERT INTO t VALUES (7, 'a') RETURNING wipe_now()",
        ),
        (
            Dialect::PostgreSql,
            "INSERT INTO t VALUES (8, 'a') ON CONFLICT (id) DO UPDATE SET name = wipe_now()::text",
        ),
        (
            Dialect::PostgreSql,
            "UPDATE t SET name = wipe_now()::text WHERE id = 1",
        ),
        (
            Dialect::PostgreSql,
            "UPDATE t SET name = 'a' WHERE id = 1 RETURNING wipe_now()",
        ),
        (Dialect::PostgreSql, "DELETE FROM t WHERE id = wipe_now()"),
        (
            Dialect::PostgreSql,
            "DELETE FROM t WHERE id = 1 RETURNING wipe_now()",
        ),
        (
            Dialect::PostgreSql,
            "DELETE FROM t WHERE id = 1 AND EXISTS (SELECT 1 FROM u WHERE u.id = wipe_now())",
        ),
        (Dialect::MySql, "INSERT INTO t VALUES (7, wipe_now())"),
        (
            Dialect::MySql,
            "INSERT INTO t VALUES (9, 'a') ON DUPLICATE KEY UPDATE name = wipe_now()",
        ),
    ] {
        assert!(
            runs_function(classified_with_keys(&store, dialect, text), "wipe_now"),
            "{dialect:?} {text}"
        );
    }
    for text in [
        "INSERT INTO t VALUES (5, upper('a'))",
        "INSERT INTO t SELECT 6, lower('B')",
        "UPDATE t SET name = lower(name) WHERE id = 1",
        "DELETE FROM t WHERE id = 1 RETURNING upper(name)",
    ] {
        assert_eq!(
            classified_with_keys(&store, Dialect::PostgreSql, text),
            None,
            "{text}"
        );
    }
}

#[test]
fn statements_that_run_a_query_classify_the_query_they_run() {
    let store = reaching_store();
    Session::new(&store)
        .execute(
            "CREATE MATERIALIZED VIEW writes_view AS SELECT id, wipe_now() AS w FROM t; \
             CREATE MATERIALIZED VIEW plain_view AS SELECT id FROM t",
            &mut |_| {},
        )
        .unwrap();
    for (dialect, text) in [
        (Dialect::PostgreSql, "EXPLAIN ANALYZE SELECT wipe_now()"),
        (
            Dialect::PostgreSql,
            "DECLARE c CURSOR WITH HOLD FOR SELECT wipe_now()",
        ),
        (
            Dialect::PostgreSql,
            "CREATE MATERIALIZED VIEW mv AS SELECT id, wipe_now() FROM t",
        ),
        (Dialect::PostgreSql, "REFRESH MATERIALIZED VIEW writes_view"),
        (Dialect::MySql, "SET @x = wipe_now()"),
    ] {
        assert!(
            runs_function(classified_with_keys(&store, dialect, text), "wipe_now"),
            "{dialect:?} {text}"
        );
    }
    assert_eq!(
        kind_with_keys(&store, Dialect::PostgreSql, "EXPLAIN ANALYZE DELETE FROM t"),
        Some(DestructiveKind::DeleteWithoutFilter)
    );
    for text in [
        "EXPLAIN DELETE FROM t",
        "EXPLAIN SELECT wipe_now()",
        "DECLARE c CURSOR WITH HOLD FOR SELECT id FROM t",
        "CREATE MATERIALIZED VIEW mv AS SELECT id FROM t",
        "REFRESH MATERIALIZED VIEW plain_view",
    ] {
        assert_eq!(
            classified_with_keys(&store, Dialect::PostgreSql, text),
            None,
            "{text}"
        );
    }
}

#[test]
fn dropping_or_replacing_a_trigger_function_takes_its_triggers_out() {
    let store = reaching_store();
    let dropped = classified_with_keys(
        &store,
        Dialect::PostgreSql,
        "DROP FUNCTION wipe_u() CASCADE",
    )
    .expect("a cascading drop of a trigger function is destructive");
    assert_eq!(dropped.parts[0].kind, DestructiveKind::DropTrigger);
    assert_eq!(
        dropped.parts[0].objects,
        ["inserted.inserted_wipes", "measured_eu.measured_eu_wipes"]
    );
    let replaced = classified_with_keys(
        &store,
        Dialect::PostgreSql,
        "CREATE OR REPLACE FUNCTION wipe_u() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$",
    )
    .expect("replacing a trigger function is destructive");
    assert_eq!(replaced.parts[0].kind, DestructiveKind::DisableTrigger);
    assert_eq!(
        replaced.parts[0].objects,
        ["inserted.inserted_wipes", "measured_eu.measured_eu_wipes"]
    );
    for text in [
        "DROP FUNCTION wipe_now() CASCADE",
        "DROP FUNCTION wipe_u()",
        "CREATE OR REPLACE FUNCTION wipe_now() RETURNS integer LANGUAGE plpgsql AS $$ BEGIN RETURN 2; END $$",
    ] {
        assert_eq!(
            kind_with_keys(&store, Dialect::PostgreSql, text),
            Some(DestructiveKind::ChangeRoutine),
            "{text}"
        );
    }
    assert_eq!(
        classified_with_keys(
            &store,
            Dialect::PostgreSql,
            "CREATE FUNCTION fresh() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$",
        ),
        None
    );
}

fn guarded_store() -> MemoryStore {
    let store = MemoryStore::default();
    Session::new(&store)
        .execute(
            "CREATE TABLE g (id integer PRIMARY KEY); \
             CREATE SCHEMA s2; \
             CREATE FUNCTION helper() RETURNS integer LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$; \
             CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF helper() = 1 THEN RAISE EXCEPTION 'blocked'; END IF; RETURN NEW; END $$; \
             CREATE TRIGGER g_guard BEFORE INSERT ON g FOR EACH ROW EXECUTE FUNCTION tf(); \
             CREATE FUNCTION unused() RETURNS integer LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$",
            &mut |_| {},
        )
        .unwrap();
    store
}

const NEUTERED_TRIGGER_FUNCTION: &str =
    "CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$";

#[test]
fn renaming_moving_or_recreating_a_trigger_function_takes_its_trigger_out() {
    let store = guarded_store();
    for text in [
        "ALTER FUNCTION tf() RENAME TO tf_old",
        "ALTER FUNCTION tf() SET SCHEMA s2",
        "ALTER ROUTINE tf RENAME TO tf_old",
    ] {
        let found = classified_with_keys(&store, Dialect::PostgreSql, text)
            .unwrap_or_else(|| panic!("{text} is not destructive"));
        assert_eq!(
            found.parts[0].kind,
            DestructiveKind::DisableTrigger,
            "{text}"
        );
        assert_eq!(found.parts[0].objects, ["g.g_guard"], "{text}");
    }
    Session::new(&store)
        .execute("ALTER FUNCTION tf() RENAME TO tf_old", &mut |_| {})
        .unwrap();
    let recreated = classified_with_keys(&store, Dialect::PostgreSql, NEUTERED_TRIGGER_FUNCTION)
        .expect("re-creating a function a trigger names is destructive");
    assert_eq!(recreated.parts[0].kind, DestructiveKind::DisableTrigger);
    assert_eq!(recreated.parts[0].objects, ["g.g_guard"]);
}

#[test]
fn replacing_renaming_or_dropping_any_existing_routine_changes_code_that_runs_later() {
    let store = guarded_store();
    for text in [
        "CREATE OR REPLACE FUNCTION helper() RETURNS integer LANGUAGE plpgsql AS $$ BEGIN RETURN 0; END $$",
        "ALTER FUNCTION helper() RENAME TO helper_old",
        "ALTER FUNCTION helper() SET SCHEMA s2",
        "DROP FUNCTION helper()",
        "DROP FUNCTION IF EXISTS helper",
    ] {
        let found = classified_with_keys(&store, Dialect::PostgreSql, text)
            .unwrap_or_else(|| panic!("{text} is not destructive"));
        assert_eq!(
            found.parts[0].kind,
            DestructiveKind::ChangeRoutine,
            "{text}"
        );
        assert_eq!(found.parts[0].objects, ["helper"], "{text}");
    }
    for text in [
        "CREATE FUNCTION brand_new() RETURNS integer LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$",
        "CREATE OR REPLACE FUNCTION brand_new() RETURNS integer LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$",
        "ALTER FUNCTION unused() OWNER TO someone",
        "ALTER FUNCTION missing() RENAME TO other",
        "DROP FUNCTION IF EXISTS missing",
    ] {
        assert_eq!(
            classified_with_keys(&store, Dialect::PostgreSql, text),
            None,
            "{text}"
        );
    }
}

#[test]
fn creating_a_function_named_like_a_built_in_changes_code_that_runs_later() {
    let store = MemoryStore::default();
    Session::new(&store)
        .execute(
            "CREATE TABLE g (name text); \
             CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF length(NEW.name) > 3 THEN RAISE EXCEPTION 'blocked'; END IF; RETURN NEW; END $$; \
             CREATE TRIGGER g_guard BEFORE INSERT ON g FOR EACH ROW EXECUTE FUNCTION tf()",
            &mut |_| {},
        )
        .unwrap();
    for text in [
        "CREATE FUNCTION length(x text) RETURNS integer LANGUAGE plpgsql AS $$ BEGIN RETURN -1; END $$",
        "CREATE FUNCTION public.lower(x text) RETURNS text LANGUAGE plpgsql AS $$ BEGIN RETURN x; END $$",
        "CREATE OR REPLACE FUNCTION LOWER(x text) RETURNS text LANGUAGE plpgsql AS $$ BEGIN RETURN x; END $$",
        "CREATE FUNCTION now() RETURNS integer LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$",
    ] {
        assert_eq!(
            kind_with_keys(&store, Dialect::PostgreSql, text),
            Some(DestructiveKind::ChangeRoutine),
            "{text}"
        );
    }
    assert_eq!(
        kind_with_keys(
            &store,
            Dialect::PostgreSql,
            "CREATE FUNCTION length_of_name(x text) RETURNS integer LANGUAGE plpgsql AS $$ BEGIN RETURN -1; END $$",
        ),
        None
    );
}

#[test]
fn other_dialects_replacing_or_dropping_an_existing_routine_change_code_that_runs_later() {
    let store = MemoryStore::default();
    let mut sqlserver = Session::with_dialect(&store, Dialect::SqlServer);
    sqlserver
        .execute("CREATE PROCEDURE p AS SELECT 1", &mut |_| {})
        .unwrap();
    for text in [
        "ALTER PROCEDURE p AS SELECT 2",
        "CREATE OR ALTER PROCEDURE p AS SELECT 2",
        "DROP PROCEDURE p",
    ] {
        assert_eq!(
            kind_with_keys(&store, Dialect::SqlServer, text),
            Some(DestructiveKind::ChangeRoutine),
            "{text}"
        );
    }
    assert_eq!(
        kind_with_keys(
            &store,
            Dialect::SqlServer,
            "CREATE OR ALTER PROCEDURE q AS SELECT 1"
        ),
        None
    );
    let mut oracle = Session::with_dialect(&store, Dialect::Oracle);
    oracle
        .execute(
            "CREATE OR REPLACE PROCEDURE op IS BEGIN NULL; END op;",
            &mut |_| {},
        )
        .unwrap();
    oracle
        .execute("CREATE PACKAGE pk AS PROCEDURE bump; END pk;", &mut |_| {})
        .unwrap();
    for text in [
        "CREATE OR REPLACE PROCEDURE op IS BEGIN NULL; END op;",
        "CREATE OR REPLACE PACKAGE pk AS PROCEDURE bump; END pk;",
        "DROP PACKAGE pk",
    ] {
        assert_eq!(
            kind_with_keys(&store, Dialect::Oracle, text),
            Some(DestructiveKind::ChangeRoutine),
            "{text}"
        );
    }
    for text in [
        "CREATE OR REPLACE PROCEDURE fresh_op IS BEGIN NULL; END fresh_op;",
        "CREATE PACKAGE BODY pk AS PROCEDURE bump IS BEGIN NULL; END bump; END pk;",
    ] {
        assert_eq!(
            kind_with_keys(&store, Dialect::Oracle, text),
            None,
            "{text}"
        );
    }
}

#[test]
fn a_merge_calling_a_user_function_in_its_source_condition_or_actions_runs_code() {
    let store = reaching_store();
    for text in [
        "MERGE INTO u USING (SELECT wipe_now() AS id FROM dual) s ON (u.id = s.id) WHEN NOT MATCHED THEN INSERT VALUES (s.id)",
        "MERGE INTO u USING t ON (u.id = wipe_now()) WHEN NOT MATCHED THEN INSERT VALUES (t.id)",
        "MERGE INTO u USING t ON (u.id = t.id) WHEN NOT MATCHED THEN INSERT VALUES (wipe_now())",
        "MERGE INTO t USING u ON (t.id = u.id) WHEN MATCHED THEN UPDATE SET t.name = 'x' WHERE wipe_now() = 1",
    ] {
        assert!(
            runs_function(
                classified_with_keys(&store, Dialect::Oracle, text),
                "wipe_now"
            ),
            "{text}"
        );
    }
}
