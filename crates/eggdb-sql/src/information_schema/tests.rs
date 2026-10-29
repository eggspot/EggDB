use super::{InformationRelation, describe_type};
use crate::interval::IntervalFields;
use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::types::{NumericModifier, SqlType};
use crate::{Session, SqlError};

fn session(store: &MemoryStore) -> Session<'_, MemoryStore> {
    let mut session = Session::new(store);
    session.identify("eggdb", "shapes");
    session
}

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> Result<Vec<String>, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs)
}

fn last(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    match run(session, sql) {
        Ok(outputs) => outputs.last().cloned().unwrap_or_default(),
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn fails(session: &mut Session<'_, MemoryStore>, sql: &str) -> SqlError {
    match run(session, sql) {
        Ok(outputs) => panic!("{sql} succeeded: {outputs:?}"),
        Err(error) => error,
    }
}

fn rows(session: &mut Session<'_, MemoryStore>, sql: &str) -> Vec<String> {
    let rendered = last(session, sql);
    let lines: Vec<&str> = rendered.lines().collect();
    lines
        .get(1..lines.len().saturating_sub(1))
        .unwrap_or(&[])
        .iter()
        .map(|line| (*line).to_owned())
        .collect()
}

fn shop(store: &MemoryStore) -> Session<'_, MemoryStore> {
    let mut session = session(store);
    for statement in [
        "CREATE SCHEMA shop",
        "CREATE TABLE customers (id serial PRIMARY KEY, name varchar(40) NOT NULL, \
         email text UNIQUE, price numeric(12,2) CHECK (price >= 0), born date, seen timestamptz(3))",
        "CREATE TABLE shop.orders (id bigint PRIMARY KEY, \
         customer_id integer REFERENCES customers (id) ON DELETE CASCADE, line smallint, \
         CONSTRAINT positive CHECK (line > 0 AND customer_id > 0))",
        "CREATE VIEW named AS SELECT id, name FROM customers",
        "CREATE SEQUENCE counter START WITH 5 INCREMENT BY 2",
    ] {
        last(&mut session, statement);
    }
    session
}

#[test]
fn tables_lists_tables_and_views_of_every_schema_with_their_types() {
    let store = MemoryStore::default();
    let mut session = shop(&store);
    assert_eq!(
        rows(
            &mut session,
            "SELECT table_catalog, table_schema, table_name, table_type, is_insertable_into, is_typed \
             FROM information_schema.tables WHERE table_schema IN ('public', 'shop') ORDER BY 2, 3"
        ),
        [
            "shapes|public|customers|BASE TABLE|YES|NO",
            "shapes|public|named|VIEW|NO|NO",
            "shapes|shop|orders|BASE TABLE|YES|NO",
        ]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT table_type FROM information_schema.tables \
             WHERE table_schema = 'information_schema' AND table_name = 'columns'"
        ),
        ["VIEW"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT table_type FROM information_schema.tables \
             WHERE table_schema = 'pg_catalog' AND table_name = 'pg_class'"
        ),
        ["BASE TABLE"]
    );
}

#[test]
fn columns_describe_each_column_as_postgresql_does() {
    let store = MemoryStore::default();
    let mut session = shop(&store);
    assert_eq!(
        rows(
            &mut session,
            "SELECT column_name, ordinal_position, is_nullable, data_type, character_maximum_length, \
             character_octet_length, numeric_precision, numeric_precision_radix, numeric_scale, \
             datetime_precision, udt_schema, udt_name, dtd_identifier, is_identity, is_generated, is_updatable \
             FROM information_schema.columns WHERE table_name = 'customers' ORDER BY ordinal_position"
        ),
        [
            "id|1|NO|integer|NULL|NULL|32|2|0|NULL|pg_catalog|int4|1|NO|NEVER|YES",
            "name|2|NO|character varying|40|160|NULL|NULL|NULL|NULL|pg_catalog|varchar|2|NO|NEVER|YES",
            "email|3|YES|text|NULL|1073741824|NULL|NULL|NULL|NULL|pg_catalog|text|3|NO|NEVER|YES",
            "price|4|YES|numeric|NULL|NULL|12|10|2|NULL|pg_catalog|numeric|4|NO|NEVER|YES",
            "born|5|YES|date|NULL|NULL|NULL|NULL|NULL|0|pg_catalog|date|5|NO|NEVER|YES",
            "seen|6|YES|timestamp with time zone|NULL|NULL|NULL|NULL|NULL|3|pg_catalog|timestamptz|6|NO|NEVER|YES",
        ]
    );
    let defaults = rows(
        &mut session,
        "SELECT column_default FROM information_schema.columns \
         WHERE table_name = 'customers' AND column_name = 'id'",
    );
    assert!(
        defaults
            .first()
            .is_some_and(|default| default.starts_with("nextval(")),
        "{defaults:?}"
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT column_name, is_nullable, is_updatable FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = 'named' ORDER BY ordinal_position"
        ),
        ["id|YES|NO", "name|YES|NO"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT data_type, udt_name FROM information_schema.columns \
             WHERE table_schema = 'pg_catalog' AND table_name = 'pg_class' AND column_name IN ('relname', 'relacl') \
             ORDER BY ordinal_position"
        ),
        ["name|name", "ARRAY|_aclitem"]
    );
}

#[test]
fn constraints_answer_in_every_constraint_view() {
    let store = MemoryStore::default();
    let mut session = shop(&store);
    assert_eq!(
        rows(
            &mut session,
            "SELECT constraint_schema, constraint_name, table_name, constraint_type, is_deferrable, \
             initially_deferred, enforced, nulls_distinct FROM information_schema.table_constraints \
             WHERE constraint_type <> 'CHECK' AND table_schema IN ('public', 'shop') ORDER BY 2"
        ),
        [
            "public|customers_email_key|customers|UNIQUE|NO|NO|YES|YES",
            "public|customers_pkey|customers|PRIMARY KEY|NO|NO|YES|NULL",
            "shop|orders_customer_id_fkey|orders|FOREIGN KEY|NO|NO|YES|NULL",
            "shop|orders_pkey|orders|PRIMARY KEY|NO|NO|YES|NULL",
        ]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT constraint_name, table_schema, table_name, column_name, ordinal_position, \
             position_in_unique_constraint FROM information_schema.key_column_usage \
             WHERE table_schema IN ('public', 'shop') ORDER BY 1, 5"
        ),
        [
            "customers_email_key|public|customers|email|1|NULL",
            "customers_pkey|public|customers|id|1|NULL",
            "orders_customer_id_fkey|shop|orders|customer_id|1|1",
            "orders_pkey|shop|orders|id|1|NULL",
        ]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT constraint_schema, constraint_name, unique_constraint_schema, unique_constraint_name, \
             match_option, update_rule, delete_rule FROM information_schema.referential_constraints"
        ),
        ["shop|orders_customer_id_fkey|public|customers_pkey|NONE|NO ACTION|CASCADE"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT constraint_name, table_schema, table_name, column_name \
             FROM information_schema.constraint_column_usage \
             WHERE constraint_schema IN ('public', 'shop') ORDER BY 1, 4"
        ),
        [
            "customers_email_key|public|customers|email",
            "customers_pkey|public|customers|id",
            "customers_price_check|public|customers|price",
            "orders_customer_id_fkey|public|customers|id",
            "orders_pkey|shop|orders|id",
            "positive|shop|orders|customer_id",
            "positive|shop|orders|line",
        ]
    );
    let clauses = rows(
        &mut session,
        "SELECT check_clause FROM information_schema.check_constraints WHERE constraint_name = 'positive'",
    );
    assert!(
        clauses
            .first()
            .is_some_and(|clause| clause.starts_with('(') && clause.contains("line > 0")),
        "{clauses:?}"
    );
}

#[test]
fn not_null_columns_show_as_check_constraints_named_by_object_identifiers() {
    let store = MemoryStore::default();
    let mut session = shop(&store);
    assert_eq!(
        rows(
            &mut session,
            "SELECT check_clause FROM information_schema.check_constraints \
             WHERE constraint_name LIKE '%not_null' ORDER BY 1"
        ),
        ["id IS NOT NULL", "id IS NOT NULL", "name IS NOT NULL"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT count(*) FROM information_schema.table_constraints t \
             JOIN information_schema.check_constraints c USING (constraint_schema, constraint_name) \
             WHERE t.constraint_type = 'CHECK' AND t.table_name = 'customers'"
        ),
        ["3"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT t.constraint_name = n.oid::text || '_' || c.oid::text || '_2_not_null' \
             FROM information_schema.table_constraints t, pg_catalog.pg_class c, pg_catalog.pg_namespace n \
             WHERE c.relname = 'customers' AND n.oid = c.relnamespace AND t.table_name = 'customers' \
             AND t.constraint_name LIKE '%_2_not_null'"
        ),
        ["t"]
    );
}

#[test]
fn views_sequences_and_schemata_answer() {
    let store = MemoryStore::default();
    let mut session = shop(&store);
    assert_eq!(
        rows(
            &mut session,
            "SELECT table_name, view_definition IS NOT NULL, check_option, is_updatable, is_insertable_into, \
             is_trigger_updatable FROM information_schema.views WHERE table_schema = 'public'"
        ),
        ["named|t|NONE|NO|NO|NO"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT sequence_catalog, sequence_schema, data_type, numeric_precision, numeric_precision_radix, \
             numeric_scale, start_value, minimum_value, maximum_value, increment, cycle_option \
             FROM information_schema.sequences WHERE sequence_name = 'counter'"
        ),
        ["shapes|public|bigint|64|2|0|5|1|9223372036854775807|2|NO"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT catalog_name, schema_name, schema_owner, sql_path FROM information_schema.schemata ORDER BY 2"
        ),
        [
            "shapes|information_schema|eggdb|NULL",
            "shapes|pg_catalog|eggdb|NULL",
            "shapes|pg_toast|eggdb|NULL",
            "shapes|public|pg_database_owner|NULL",
            "shapes|shop|eggdb|NULL",
        ]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT (SELECT count(*) FROM information_schema.routines), \
             (SELECT count(*) FROM information_schema.parameters)"
        ),
        ["0|0"]
    );
}

#[test]
fn information_schema_relations_are_views_of_the_catalog() {
    let store = MemoryStore::default();
    let mut session = shop(&store);
    for relation in InformationRelation::ALL {
        let name = relation.name();
        assert_eq!(
            rows(
                &mut session,
                &format!(
                    "SELECT c.relkind, n.nspname, c.relnatts = {} FROM pg_catalog.pg_class c \
                     JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                     WHERE c.oid = 'information_schema.{name}'::regclass",
                    relation.columns().len()
                )
            ),
            ["v|information_schema|t"],
            "{name}"
        );
        assert_eq!(
            rows(
                &mut session,
                &format!(
                    "SELECT count(*) = {} FROM pg_catalog.pg_attribute \
                     WHERE attrelid = 'information_schema.{name}'::regclass",
                    relation.columns().len()
                )
            ),
            ["t"],
            "{name}"
        );
    }
    assert_eq!(
        rows(
            &mut session,
            "SELECT count(*) FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 'public' AND c.relname = 'tables'"
        ),
        ["0"]
    );
}

#[test]
fn unqualified_names_reach_information_schema_only_through_the_search_path() {
    let store = MemoryStore::default();
    let mut session = shop(&store);
    assert_eq!(
        fails(&mut session, "SELECT table_name FROM tables")
            .state()
            .code(),
        "42P01"
    );
    last(&mut session, "CREATE TABLE views (a integer)");
    last(&mut session, "INSERT INTO views VALUES (7)");
    assert_eq!(rows(&mut session, "SELECT a FROM views"), ["7"]);
    last(&mut session, "SET search_path = information_schema, public");
    assert_eq!(
        rows(
            &mut session,
            "SELECT table_name FROM tables WHERE table_name = 'customers'"
        ),
        ["customers"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT count(*) > 0 FROM views WHERE table_name = 'named'"
        ),
        ["t"]
    );
    last(&mut session, "SET search_path = public, information_schema");
    assert_eq!(rows(&mut session, "SELECT a FROM views"), ["7"]);
    for sql in [
        "DELETE FROM information_schema.tables",
        "INSERT INTO information_schema.schemata (schema_name) VALUES ('x')",
        "UPDATE information_schema.columns SET column_name = 'x'",
    ] {
        assert_eq!(fails(&mut session, sql).state().code(), "42501", "{sql}");
    }
}

#[test]
fn types_are_described_with_their_precision_and_interval_fields() {
    let numeric = describe_type(SqlType::Numeric(Some(NumericModifier {
        precision: 9,
        scale: 3,
    })));
    assert_eq!(numeric.numeric_precision, Some(9));
    assert_eq!(numeric.numeric_scale, Some(3));
    assert_eq!(numeric.numeric_precision_radix, Some(10));
    let unbounded = describe_type(SqlType::Numeric(None));
    assert_eq!(unbounded.numeric_precision, None);
    assert_eq!(unbounded.numeric_scale, None);
    let character = describe_type(SqlType::Bpchar(Some(2)));
    assert_eq!(character.data_type, "character");
    assert_eq!(character.character_maximum_length, Some(2));
    assert_eq!(character.character_octet_length, Some(8));
    let restricted = describe_type(SqlType::Interval {
        fields: Some(IntervalFields::DayToSecond),
        precision: Some(3),
    });
    assert!(
        restricted
            .interval_type
            .as_deref()
            .is_some_and(|fields| fields.starts_with("DAY TO SECOND")),
        "{:?}",
        restricted.interval_type
    );
    assert_eq!(restricted.datetime_precision, Some(3));
    let plain = describe_type(SqlType::Interval {
        fields: None,
        precision: None,
    });
    assert_eq!(plain.interval_type, None);
    assert_eq!(plain.datetime_precision, Some(6));
    let precise = describe_type(SqlType::Interval {
        fields: None,
        precision: Some(2),
    });
    assert_eq!(precise.interval_type, None);
    assert_eq!(precise.datetime_precision, Some(2));
    assert_eq!(describe_type(SqlType::Real).numeric_precision, Some(24));
    assert_eq!(describe_type(SqlType::Double).numeric_precision, Some(53));
    assert_eq!(
        describe_type(SqlType::Time(None)).datetime_precision,
        Some(6)
    );
    assert_eq!(describe_type(SqlType::Uuid).numeric_precision, None);
}
