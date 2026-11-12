pub(crate) const LEDGER_SCRIPT: &str = r"
CREATE SCHEMA eggdb_ledger;
CREATE TABLE eggdb_ledger.chains (
    table_schema text NOT NULL,
    table_name text NOT NULL,
    last_hash text NOT NULL,
    last_seq bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (table_schema, table_name)
);
CREATE FUNCTION eggdb_ledger.append() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    previous text;
    sequence bigint;
BEGIN
    SELECT c.last_hash, c.last_seq INTO previous, sequence FROM eggdb_ledger.chains c WHERE c.table_schema = TG_TABLE_SCHEMA AND c.table_name = TG_TABLE_NAME;
    NEW.ledger_seq := sequence + 1;
    NEW.ledger_previous := previous;
    NEW.ledger_hash := encode(sha256(previous || ((to_jsonb(NEW) - 'ledger_hash') - 'ledger_previous')::text), 'hex');
    UPDATE eggdb_ledger.chains SET last_hash = NEW.ledger_hash, last_seq = NEW.ledger_seq WHERE table_schema = TG_TABLE_SCHEMA AND table_name = TG_TABLE_NAME;
    RETURN NEW;
END
$$;
CREATE FUNCTION eggdb_ledger.refuse() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'table %.% is a ledger: rows can be added, never changed or removed', TG_TABLE_SCHEMA, TG_TABLE_NAME USING ERRCODE = '55000';
END
$$;
CREATE FUNCTION eggdb_ledger.protect(schema_name text, table_name text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    previous text := '0000000000000000000000000000000000000000000000000000000000000000';
    adopted bigint := 0;
    found_row record;
    hash text;
BEGIN
    EXECUTE format('ALTER TABLE %I.%I ADD COLUMN ledger_seq bigint', schema_name, table_name);
    EXECUTE format('ALTER TABLE %I.%I ADD COLUMN ledger_previous text', schema_name, table_name);
    EXECUTE format('ALTER TABLE %I.%I ADD COLUMN ledger_hash text', schema_name, table_name);
    EXECUTE 'CREATE SEQUENCE eggdb_ledger.adoption_order';
    EXECUTE format('UPDATE %I.%I SET ledger_seq = nextval(%L)', schema_name, table_name, 'eggdb_ledger.adoption_order');
    EXECUTE 'DROP SEQUENCE eggdb_ledger.adoption_order';
    FOR found_row IN EXECUTE format('SELECT t.ledger_seq AS seq, ((to_jsonb(t) - %L) - %L)::text AS body FROM %I.%I t ORDER BY t.ledger_seq', 'ledger_hash', 'ledger_previous', schema_name, table_name) LOOP
        hash := encode(sha256(previous || found_row.body), 'hex');
        EXECUTE format('UPDATE %I.%I SET ledger_previous = %L, ledger_hash = %L WHERE ledger_seq = %s', schema_name, table_name, previous, hash, found_row.seq);
        previous := hash;
        adopted := found_row.seq;
    END LOOP;
    EXECUTE format('CREATE TRIGGER eggdb_ledger_append BEFORE INSERT ON %I.%I FOR EACH ROW EXECUTE FUNCTION eggdb_ledger.append()', schema_name, table_name);
    EXECUTE format('CREATE TRIGGER eggdb_ledger_refuse BEFORE UPDATE OR DELETE ON %I.%I FOR EACH ROW EXECUTE FUNCTION eggdb_ledger.refuse()', schema_name, table_name);
    EXECUTE format('CREATE TRIGGER eggdb_ledger_truncate BEFORE TRUNCATE ON %I.%I FOR EACH STATEMENT EXECUTE FUNCTION eggdb_ledger.refuse()', schema_name, table_name);
    INSERT INTO eggdb_ledger.chains (table_schema, table_name, last_hash, last_seq) VALUES (schema_name, table_name, previous, adopted);
    RETURN adopted;
END
$$;
CREATE FUNCTION eggdb_ledger.verify(schema_name text, table_name text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    broken bigint;
BEGIN
    EXECUTE format('SELECT count(*) FROM (SELECT ledger_hash, ledger_previous, lag(ledger_hash, 1, %L) OVER (ORDER BY ledger_seq) AS expected, encode(sha256(ledger_previous || ((to_jsonb(t) - %L) - %L)::text), %L) AS computed FROM %I.%I t) checked WHERE ledger_previous IS DISTINCT FROM expected OR ledger_hash IS DISTINCT FROM computed', '0000000000000000000000000000000000000000000000000000000000000000', 'ledger_hash', 'ledger_previous', 'hex', schema_name, table_name) INTO broken;
    RETURN broken;
END
$$;
";

const CHAINS: &str = "chains";
const LEDGER_SCHEMA: &str = "eggdb_ledger";

pub(crate) fn refuse_dropping(
    reader: &dyn crate::store::KeyValueRead,
    tables: &mut crate::execute::TableCache,
    table: &str,
) -> crate::error::SqlResult<()> {
    let chains_name = crate::schemas::qualified(LEDGER_SCHEMA, CHAINS);
    if table == chains_name {
        return Ok(());
    }
    let Some(chains) = tables.table(reader, &chains_name)? else {
        return Ok(());
    };
    let (Some(schema_column), Some(name_column)) = (
        chains.column_index("table_schema"),
        chains.column_index("table_name"),
    ) else {
        return Ok(());
    };
    let schema = crate::schemas::schema_of(table);
    let name = crate::schemas::base_name(table);
    let protected = crate::execute::every_row(reader, &chains)?
        .into_iter()
        .any(|(_, row)| {
            matches!(row.get(schema_column), Some(crate::types::Value::Text(found)) if found == schema)
                && matches!(row.get(name_column), Some(crate::types::Value::Text(found)) if found == name)
        });
    if protected {
        return Err(crate::error::SqlError::new(
            crate::error::SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
            format!("table {schema}.{name} is a ledger: it can never be dropped"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::memory_store::MemoryStore;
    use crate::tests::render;
    use crate::{Session, SqlError};

    fn run(session: &mut Session<'_>, sql: &str) -> Result<String, SqlError> {
        let mut outputs = Vec::new();
        session.execute(sql, &mut |result| outputs.push(render(&result)))?;
        Ok(outputs.pop().unwrap_or_default())
    }

    #[test]
    fn a_ledger_table_only_grows_and_its_chain_finds_tampering() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_ledger",
            "CREATE TABLE payments (id integer PRIMARY KEY, amount numeric NOT NULL)",
            "SELECT eggdb_ledger.protect('public', 'payments')",
            "INSERT INTO payments (id, amount) VALUES (1, 10), (2, 25.5)",
            "INSERT INTO payments (id, amount) VALUES (3, 7)",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(
            run(
                &mut session,
                "SELECT eggdb_ledger.verify('public', 'payments') AS broken"
            )
            .unwrap(),
            "broken\n0\nSELECT 1"
        );
        assert_eq!(
            run(
                &mut session,
                "UPDATE payments SET amount = 1000 WHERE id = 2"
            )
            .unwrap_err()
            .state()
            .code(),
            "55000"
        );
        assert_eq!(
            run(&mut session, "DELETE FROM payments WHERE id = 1")
                .unwrap_err()
                .state()
                .code(),
            "55000"
        );
        run(
            &mut session,
            "ALTER TABLE payments DISABLE TRIGGER eggdb_ledger_refuse",
        )
        .unwrap();
        run(
            &mut session,
            "UPDATE payments SET amount = 1000 WHERE id = 2",
        )
        .unwrap();
        assert_eq!(
            run(
                &mut session,
                "SELECT eggdb_ledger.verify('public', 'payments') AS broken"
            )
            .unwrap(),
            "broken\n1\nSELECT 1"
        );
        assert_eq!(run(&mut session, "SELECT length(encode(sha256('abc'), 'hex')) AS n, encode(sha256('abc'), 'hex') = 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad' AS right").unwrap(), "n|right\n64|t\nSELECT 1");
    }

    #[test]
    fn a_ledger_table_can_be_neither_truncated_nor_dropped() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_ledger",
            "CREATE TABLE receipts (id integer PRIMARY KEY)",
            "SELECT eggdb_ledger.protect('public', 'receipts')",
            "INSERT INTO receipts (id) VALUES (1)",
            "CREATE TABLE scratch (id integer)",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        for sql in ["TRUNCATE receipts", "DROP TABLE receipts"] {
            assert_eq!(
                run(&mut session, sql).unwrap_err().state().code(),
                "55000",
                "{sql}"
            );
        }
        run(&mut session, "DROP TABLE scratch").unwrap();
        assert_eq!(
            run(&mut session, "SELECT count(*) AS n FROM receipts").unwrap(),
            "n\n1\nSELECT 1"
        );
    }

    #[test]
    fn protecting_a_table_with_rows_chains_them_first() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_ledger",
            "CREATE TABLE payments (id integer, amount numeric NOT NULL)",
            "INSERT INTO payments (id, amount) VALUES (1, 10), (2, 25.5), (2, 25.5)",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(run(&mut session, "SELECT eggdb_ledger.protect('public', 'payments') AS adopted").unwrap(), "adopted\n3\nSELECT 1");
        run(&mut session, "INSERT INTO payments (id, amount) VALUES (4, 7)").unwrap();
        assert_eq!(
            run(&mut session, "SELECT eggdb_ledger.verify('public', 'payments') AS broken, max(ledger_seq) AS last FROM payments").unwrap(),
            "broken|last\n0|4\nSELECT 1"
        );
        assert_eq!(run(&mut session, "UPDATE payments SET amount = 0 WHERE id = 1").unwrap_err().state().code(), "55000");
    }
}
