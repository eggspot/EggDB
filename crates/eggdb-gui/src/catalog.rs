use eggdb::Database;
use eggdb::sql::{Dialect, Session, TableSchema};
use eggdb_pgwire::client::PgClient;

use crate::connection::{StatementFailure, file_result};
use crate::dialect::QueryDialect;
use crate::grid::quote_name;

/// The most keys counted in one key space; a key space with more shows `100K+`.
pub const KEY_COUNT_LIMIT: u64 = 100_000;

const USER_SCHEMAS: &str = "NOT IN ('pg_catalog', 'information_schema')";

/// The database's objects the object browser lists (`docs/ux/workspace.md` 4.1), each list
/// in name order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    /// The tables.
    pub tables: Vec<CatalogTable>,
    /// The views.
    pub views: Vec<CatalogView>,
    /// The secondary indexes of every table.
    pub indexes: Vec<CatalogIndex>,
    /// The sequences.
    pub sequences: Vec<CatalogSequence>,
    /// The key spaces: the default one when it holds keys, then each collection.
    pub key_spaces: Vec<KeySpace>,
    /// The triggers of every table.
    pub triggers: Vec<CatalogTrigger>,
}

/// A trigger on a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogTrigger {
    /// The trigger's name.
    pub name: String,
    /// The table it is on.
    pub table: String,
    /// When it fires: `BEFORE`, `AFTER` or `INSTEAD OF`.
    pub timing: String,
    /// The events it fires on, in the catalog's order: `INSERT`, `UPDATE`, `DELETE`.
    pub events: Vec<String>,
    /// What it runs, as `information_schema.triggers.action_statement` gives it.
    pub action: String,
}

impl CatalogTrigger {
    /// The tree row's right text: the events joined, `INSERT, UPDATE`.
    pub fn events_text(&self) -> String {
        self.events.join(", ")
    }

    /// The inspector's line under the name: `Trigger · AFTER INSERT, UPDATE on orders`.
    pub fn summary(&self) -> String {
        format!(
            "Trigger · {} {} on {}",
            self.timing,
            self.events_text(),
            self.table
        )
    }
}

/// One column of a table or a view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogColumn {
    /// The column's name.
    pub name: String,
    /// Its type as the catalog names it, such as `integer`.
    pub type_name: String,
    /// Whether it is part of the primary key.
    pub primary_key: bool,
    /// Whether it refuses NULL.
    pub not_null: bool,
}

/// A table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogTable {
    /// The table's name.
    pub name: String,
    /// Its columns, in order.
    pub columns: Vec<CatalogColumn>,
    /// The statements that make it: `CREATE TABLE`, then its `CREATE INDEX` statements.
    pub ddl: String,
}

/// A view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogView {
    /// The view's name.
    pub name: String,
    /// Its columns, in order.
    pub columns: Vec<CatalogColumn>,
    /// The `CREATE VIEW` statement that makes it.
    pub ddl: String,
}

/// A secondary index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogIndex {
    /// The index's name.
    pub name: String,
    /// The table it indexes.
    pub table: String,
    /// Its columns, in key order.
    pub columns: Vec<String>,
    /// Whether it refuses two rows with the same values.
    pub unique: bool,
    /// The `CREATE INDEX` statement that makes it.
    pub ddl: String,
}

/// A sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogSequence {
    /// The sequence's name.
    pub name: String,
    /// Its current value, `None` before its first `nextval`.
    pub current: Option<String>,
}

/// How many keys a key space holds, counted up to [`KEY_COUNT_LIMIT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCount {
    /// This many.
    Exactly(u64),
    /// More than [`KEY_COUNT_LIMIT`].
    MoreThanLimit,
}

impl KeyCount {
    /// The count as a tree row's right text: `312`, `2.1K`, `100K+`.
    pub fn text(self) -> String {
        match self {
            Self::Exactly(count) => compact_count(count),
            Self::MoreThanLimit => format!("{}+", compact_count(KEY_COUNT_LIMIT)),
        }
    }
}

/// A key-value key space: the default one or a named collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeySpace {
    /// The collection's name; `None` for the default key space.
    pub collection: Option<String>,
    /// How many keys it holds.
    pub keys: KeyCount,
}

impl KeySpace {
    /// The name the tree shows: `default key space` or the collection's.
    pub fn label(&self) -> &str {
        self.collection.as_deref().unwrap_or("default key space")
    }
}

impl Catalog {
    /// A catalog of tables known only by name, with no columns and nothing else.
    pub fn of_tables(names: Vec<String>) -> Self {
        Self {
            tables: names
                .into_iter()
                .map(|name| CatalogTable {
                    name,
                    columns: Vec::new(),
                    ddl: String::new(),
                })
                .collect(),
            ..Self::default()
        }
    }

    /// The triggers on `table`, in name order.
    pub fn triggers_of<'c>(
        &'c self,
        table: &'c str,
    ) -> impl Iterator<Item = &'c CatalogTrigger> + 'c {
        self.triggers
            .iter()
            .filter(move |trigger| trigger.table == table)
    }

    /// The secondary indexes of `table`, in name order.
    pub fn indexes_of<'c>(&'c self, table: &'c str) -> impl Iterator<Item = &'c CatalogIndex> + 'c {
        self.indexes
            .iter()
            .filter(move |index| index.table == table)
    }
}

/// `312`, `1.2K` for 1,204, `2.1M`, `3.4B`: a count in at most four characters and a
/// unit, one decimal under ten units.
pub fn compact_count(count: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(1_000_000_000, "B"), (1_000_000, "M"), (1_000, "K")];
    for (size, unit) in UNITS {
        if count >= size {
            let tenths = count / (size / 10);
            return if tenths < 100 && !tenths.is_multiple_of(10) {
                format!("{}.{}{unit}", tenths / 10, tenths % 10)
            } else {
                format!("{}{unit}", count / size)
            };
        }
    }
    count.to_string()
}

/// Reads the catalog of an open file through the engine: its schema for tables, indexes and
/// views, its `pg_catalog` for view definitions and sequences, and its key spaces.
pub fn read_file(database: &Database) -> Result<Catalog, StatementFailure> {
    let store = database.sql_store();
    let mut session = store.session(Dialect::PostgreSql);
    let read = read_file_schema(&mut session);
    let _ = session.close();
    let mut catalog = read?;
    catalog.key_spaces = read_key_spaces(database).unwrap_or_default();
    Ok(catalog)
}

fn read_file_schema(session: &mut Session<'_>) -> Result<Catalog, StatementFailure> {
    let schemas = session
        .tables()
        .map_err(|error| StatementFailure::from_sql(&error, ""))?;
    let views = session
        .views()
        .map_err(|error| StatementFailure::from_sql(&error, ""))?;
    let definitions = file_rows(session, &view_definitions_query()).unwrap_or_default();
    let sequences = file_rows(session, &sequences_query()).unwrap_or_default();
    let triggers = file_rows(session, TRIGGERS_QUERY).unwrap_or_default();
    let mut catalog = Catalog::default();
    for schema in &schemas {
        catalog.tables.push(file_table(schema));
        for index in &schema.indexes {
            catalog.indexes.push(CatalogIndex {
                name: index.name.clone(),
                table: schema.name.clone(),
                columns: index.columns.clone(),
                unique: index.unique,
                ddl: index.create_statement_in(&schema.name, Dialect::PostgreSql),
            });
        }
    }
    for view in views {
        let definition = definitions
            .iter()
            .find(|row| text_at(row, 0) == view.name)
            .map(|row| text_at(row, 1));
        catalog.views.push(CatalogView {
            ddl: view_ddl(&view.name, definition.as_deref()),
            columns: view
                .columns
                .into_iter()
                .map(|column| CatalogColumn {
                    name: column.name,
                    type_name: column.sql_type.map(|kind| kind.name()).unwrap_or_default(),
                    primary_key: false,
                    not_null: false,
                })
                .collect(),
            name: view.name,
        });
    }
    catalog.sequences = sequence_rows(&sequences);
    catalog.triggers = trigger_rows(&triggers);
    sort_catalog(&mut catalog);
    Ok(catalog)
}

fn file_table(schema: &TableSchema) -> CatalogTable {
    let columns = schema
        .columns
        .iter()
        .map(|column| CatalogColumn {
            name: column.name.clone(),
            type_name: column.sql_type.name(),
            primary_key: schema.primary_key.contains(&column.name),
            not_null: column.not_null,
        })
        .collect();
    let mut ddl = schema.create_statement_in(Dialect::PostgreSql);
    let indexes = schema.index_statements_in(Dialect::PostgreSql);
    if !indexes.is_empty() {
        ddl.push('\n');
        ddl.push_str(indexes.trim_end());
    }
    CatalogTable {
        name: schema.name.clone(),
        columns,
        ddl,
    }
}

fn file_rows(
    session: &mut Session<'_>,
    sql: &str,
) -> Result<Vec<Vec<Option<String>>>, StatementFailure> {
    let mut rows = Vec::new();
    session
        .execute(sql, &mut |result| rows.extend(file_result(result).rows))
        .map_err(|error| StatementFailure::from_sql(&error, sql))?;
    Ok(rows)
}

fn read_key_spaces(database: &Database) -> eggdb::Result<Vec<KeySpace>> {
    let transaction = database.begin_read()?;
    let mut spaces = Vec::new();
    let default_keys = count_keys(transaction.scan(..[0xFF_u8].as_slice()));
    if default_keys != KeyCount::Exactly(0) {
        spaces.push(KeySpace {
            collection: None,
            keys: default_keys,
        });
    }
    for name in transaction.collection_names()? {
        let collection = transaction.collection(&name)?;
        spaces.push(KeySpace {
            keys: count_keys(collection.scan(..)),
            collection: Some(name),
        });
    }
    Ok(spaces)
}

fn count_keys<T>(scan: impl Iterator<Item = eggdb::Result<T>>) -> KeyCount {
    let counted = scan
        .take(
            usize::try_from(KEY_COUNT_LIMIT)
                .unwrap_or(usize::MAX)
                .saturating_add(1),
        )
        .filter(Result::is_ok)
        .count() as u64;
    if counted > KEY_COUNT_LIMIT {
        KeyCount::MoreThanLimit
    } else {
        KeyCount::Exactly(counted)
    }
}

/// Reads a server's catalog with queries on its `pg_catalog` and `information_schema`: the
/// tables must answer; views, indexes, sequences and primary keys are left empty when the
/// server cannot list them.
pub fn read_server(client: &mut PgClient) -> Result<Catalog, StatementFailure> {
    let tables = server_rows(
        client,
        &format!(
            "SELECT tablename FROM pg_catalog.pg_tables WHERE schemaname {USER_SCHEMAS} ORDER BY tablename"
        ),
    )?;
    let columns = server_rows(client, &format!(
        "SELECT table_name, column_name, data_type, is_nullable FROM information_schema.columns WHERE table_schema {USER_SCHEMAS} ORDER BY table_name, ordinal_position"
    ))
    .unwrap_or_default();
    let keys = server_rows(client, "SELECT kcu.table_name, kcu.column_name FROM information_schema.table_constraints tc JOIN information_schema.key_column_usage kcu ON kcu.constraint_name = tc.constraint_name AND kcu.table_name = tc.table_name WHERE tc.constraint_type = 'PRIMARY KEY'")
        .unwrap_or_default();
    let views = server_rows(client, &view_definitions_query()).unwrap_or_default();
    let indexes = server_rows(client, &format!(
        "SELECT ic.relname, tc.relname, i.indisunique, pg_catalog.pg_get_indexdef(i.indexrelid) FROM pg_catalog.pg_index i JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid JOIN pg_catalog.pg_class tc ON tc.oid = i.indrelid JOIN pg_catalog.pg_namespace n ON n.oid = tc.relnamespace WHERE NOT i.indisprimary AND n.nspname {USER_SCHEMAS} ORDER BY ic.relname"
    ))
    .unwrap_or_default();
    let sequences = server_rows(client, &sequences_query()).unwrap_or_default();
    let triggers = server_rows(client, TRIGGERS_QUERY).unwrap_or_default();
    let columns_of = |relation: &str| -> Vec<CatalogColumn> {
        columns
            .iter()
            .filter(|row| text_at(row, 0) == relation)
            .map(|row| {
                let name = text_at(row, 1);
                CatalogColumn {
                    primary_key: keys
                        .iter()
                        .any(|key| text_at(key, 0) == relation && text_at(key, 1) == name),
                    type_name: text_at(row, 2),
                    not_null: text_at(row, 3).eq_ignore_ascii_case("NO"),
                    name,
                }
            })
            .collect()
    };
    let mut catalog = Catalog::default();
    for row in &tables {
        let name = text_at(row, 0);
        let columns = columns_of(&name);
        catalog.tables.push(CatalogTable {
            ddl: table_ddl(&name, &columns),
            columns,
            name,
        });
    }
    for row in &views {
        let name = text_at(row, 0);
        catalog.views.push(CatalogView {
            ddl: view_ddl(&name, Some(&text_at(row, 1))),
            columns: columns_of(&name),
            name,
        });
    }
    for row in &indexes {
        let ddl = text_at(row, 3);
        catalog.indexes.push(CatalogIndex {
            name: text_at(row, 0),
            table: text_at(row, 1),
            columns: index_columns(&ddl),
            unique: matches!(text_at(row, 2).as_str(), "t" | "true"),
            ddl: format!("{};", ddl.trim_end_matches(';')),
        });
    }
    catalog.sequences = sequence_rows(&sequences);
    catalog.triggers = trigger_rows(&triggers);
    sort_catalog(&mut catalog);
    Ok(catalog)
}

fn server_rows(
    client: &mut PgClient,
    sql: &str,
) -> Result<Vec<Vec<Option<String>>>, StatementFailure> {
    let results = client
        .simple_query(sql)
        .map_err(|error| StatementFailure::from_client(&error))?;
    Ok(results.into_iter().flat_map(|result| result.rows).collect())
}

fn view_definitions_query() -> String {
    format!(
        "SELECT viewname, definition FROM pg_catalog.pg_views WHERE schemaname {USER_SCHEMAS} ORDER BY viewname"
    )
}

fn sequences_query() -> String {
    format!(
        "SELECT sequencename, last_value FROM pg_catalog.pg_sequences WHERE schemaname {USER_SCHEMAS} ORDER BY sequencename"
    )
}

fn sequence_rows(rows: &[Vec<Option<String>>]) -> Vec<CatalogSequence> {
    rows.iter()
        .map(|row| CatalogSequence {
            name: text_at(row, 0),
            current: row.get(1).cloned().flatten(),
        })
        .collect()
}

const TRIGGERS_QUERY: &str = "SELECT trigger_name, event_object_table, action_timing, event_manipulation, action_statement FROM information_schema.triggers WHERE trigger_schema NOT IN ('pg_catalog', 'information_schema') ORDER BY trigger_name, event_object_table";

/// The triggers in rows of `information_schema.triggers` (name, table, timing, event, action), one
/// trigger for the rows that share a name and a table, its events in the order listed.
pub fn trigger_rows(rows: &[Vec<Option<String>>]) -> Vec<CatalogTrigger> {
    let mut triggers: Vec<CatalogTrigger> = Vec::new();
    for row in rows {
        let (name, table, event) = (text_at(row, 0), text_at(row, 1), text_at(row, 3));
        match triggers
            .iter_mut()
            .find(|trigger| trigger.name == name && trigger.table == table)
        {
            Some(trigger) => {
                if !trigger.events.contains(&event) {
                    trigger.events.push(event);
                }
            }
            None => triggers.push(CatalogTrigger {
                name,
                table,
                timing: text_at(row, 2),
                events: vec![event],
                action: text_at(row, 4),
            }),
        }
    }
    triggers
}

fn text_at(row: &[Option<String>], column: usize) -> String {
    row.get(column).cloned().flatten().unwrap_or_default()
}

fn sort_catalog(catalog: &mut Catalog) {
    catalog.tables.sort_by(|a, b| a.name.cmp(&b.name));
    catalog.views.sort_by(|a, b| a.name.cmp(&b.name));
    catalog.indexes.sort_by(|a, b| a.name.cmp(&b.name));
    catalog.sequences.sort_by(|a, b| a.name.cmp(&b.name));
    catalog.triggers.sort_by(|a, b| a.name.cmp(&b.name));
}

/// The `CREATE TABLE` statement a server's columns describe, when the server gives no DDL.
pub fn table_ddl(name: &str, columns: &[CatalogColumn]) -> String {
    let quote = |text: &str| quote_name(text, QueryDialect::PostgreSql);
    let mut parts: Vec<String> = columns
        .iter()
        .map(|column| {
            let mut part = format!("    {} {}", quote(&column.name), column.type_name);
            if column.not_null && !column.primary_key {
                part.push_str(" NOT NULL");
            }
            part
        })
        .collect();
    let key: Vec<String> = columns
        .iter()
        .filter(|column| column.primary_key)
        .map(|column| quote(&column.name))
        .collect();
    if !key.is_empty() {
        parts.push(format!("    PRIMARY KEY ({})", key.join(", ")));
    }
    format!("CREATE TABLE {} (\n{}\n);", quote(name), parts.join(",\n"))
}

fn view_ddl(name: &str, definition: Option<&str>) -> String {
    let quoted = quote_name(name, QueryDialect::PostgreSql);
    match definition.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => format!("CREATE VIEW {quoted} AS\n{};", text.trim_end_matches(';')),
        None => format!("CREATE VIEW {quoted};"),
    }
}

fn index_columns(ddl: &str) -> Vec<String> {
    let Some(open) = ddl.rfind('(') else {
        return Vec::new();
    };
    let Some(close) = ddl[open..].find(')') else {
        return Vec::new();
    };
    ddl[open + 1..open + close]
        .split(',')
        .map(|column| column.trim().trim_matches('"').to_owned())
        .filter(|column| !column.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "eggdb-gui-catalog-{}-{name}.eggdb",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn shop(path: &std::path::Path) -> Database {
        let database = Database::open(path).expect("the file opens");
        {
            let store = database.sql_store();
            let mut session = store.session(Dialect::PostgreSql);
            session
                .execute(
                    "CREATE TABLE customers (id integer PRIMARY KEY, name text NOT NULL);
                     CREATE TABLE orders (id integer PRIMARY KEY, customer_id integer, total numeric);
                     CREATE TABLE products (id integer PRIMARY KEY, name text);
                     CREATE INDEX orders_customer_id_idx ON orders (customer_id);
                     CREATE UNIQUE INDEX products_name_key ON products (name);
                     CREATE INDEX customers_name_idx ON customers (name);
                     CREATE VIEW big_orders AS SELECT id, total FROM orders WHERE total > 100;
                     CREATE SEQUENCE invoice_numbers;
                     CREATE SEQUENCE ticket_numbers;
                     INSERT INTO orders VALUES (1, 1, 50), (2, 1, 150), (3, 2, 250);",
                    &mut |_| {},
                )
                .expect("the schema is made");
            let _ = session.close();
        }
        database
            .write(|transaction| {
                transaction.put("greeting", "hello")?;
                transaction.create_collection("sessions")?;
                let mut sessions = transaction.collection("sessions")?;
                sessions.put("s1", "alice")?;
                sessions.put("s2", "bob")?;
                Ok(())
            })
            .expect("the keys are written");
        database
    }

    #[test]
    fn a_file_lists_its_tables_views_indexes_sequences_and_key_spaces() {
        let path = scratch("shop");
        let database = shop(&path);
        let catalog = read_file(&database).expect("the catalog reads");
        let names = |list: Vec<&str>| list.into_iter().map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(
            catalog
                .tables
                .iter()
                .map(|table| table.name.clone())
                .collect::<Vec<_>>(),
            names(vec!["customers", "orders", "products"])
        );
        let customers = &catalog.tables[0];
        assert_eq!(customers.columns.len(), 2);
        assert!(customers.columns[0].primary_key);
        assert_eq!(customers.columns[0].type_name, "integer");
        assert!(customers.columns[1].not_null);
        assert!(
            customers.ddl.starts_with("CREATE TABLE"),
            "{}",
            customers.ddl
        );
        assert!(
            customers.ddl.contains("customers_name_idx"),
            "{}",
            customers.ddl
        );
        assert_eq!(
            catalog
                .views
                .iter()
                .map(|view| view.name.clone())
                .collect::<Vec<_>>(),
            names(vec!["big_orders"])
        );
        assert!(
            catalog.views[0]
                .ddl
                .starts_with("CREATE VIEW big_orders AS"),
            "{}",
            catalog.views[0].ddl
        );
        assert_eq!(catalog.views[0].columns.len(), 2);
        assert_eq!(
            catalog
                .indexes
                .iter()
                .map(|index| index.name.clone())
                .collect::<Vec<_>>(),
            names(vec![
                "customers_name_idx",
                "orders_customer_id_idx",
                "products_name_key"
            ])
        );
        assert!(catalog.indexes[2].unique && !catalog.indexes[0].unique);
        assert_eq!(catalog.indexes[1].table, "orders");
        assert_eq!(catalog.indexes[1].columns, names(vec!["customer_id"]));
        assert_eq!(catalog.indexes_of("orders").count(), 1);
        assert_eq!(
            catalog
                .sequences
                .iter()
                .map(|sequence| sequence.name.clone())
                .collect::<Vec<_>>(),
            names(vec!["invoice_numbers", "ticket_numbers"])
        );
        assert_eq!(catalog.key_spaces.len(), 2);
        assert_eq!(catalog.key_spaces[0].label(), "default key space");
        assert_eq!(catalog.key_spaces[0].keys, KeyCount::Exactly(1));
        assert_eq!(catalog.key_spaces[1].label(), "sessions");
        assert_eq!(catalog.key_spaces[1].keys, KeyCount::Exactly(2));
        drop(database);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_file_lists_its_triggers_with_their_timing_and_events() {
        let path = scratch("triggers");
        let database = shop(&path);
        {
            let store = database.sql_store();
            let mut session = store.session(Dialect::Sqlite);
            for statement in [
                "CREATE TABLE audit (note text)",
                "CREATE TRIGGER orders_audit AFTER UPDATE ON orders BEGIN INSERT INTO audit VALUES ('u'); END",
                "CREATE TRIGGER orders_check BEFORE INSERT ON orders BEGIN SELECT 1; END",
            ] {
                session
                    .execute(statement, &mut |_| {})
                    .expect("the triggers are made");
            }
            let _ = session.close();
        }
        let catalog = read_file(&database).expect("the catalog reads");
        let triggers: Vec<(String, String, String)> = catalog
            .triggers
            .iter()
            .map(|trigger| (trigger.name.clone(), trigger.table.clone(), trigger.summary()))
            .collect();
        assert_eq!(
            triggers,
            vec![
                ("orders_audit".to_owned(), "orders".to_owned(), "Trigger · AFTER UPDATE on orders".to_owned()),
                ("orders_check".to_owned(), "orders".to_owned(), "Trigger · BEFORE INSERT on orders".to_owned()),
            ]
        );
        assert_eq!(catalog.triggers_of("orders").count(), 2);
        assert!(catalog.triggers[0].action.contains("INSERT INTO audit"), "{}", catalog.triggers[0].action);
        assert_eq!(catalog.triggers_of("customers").count(), 0);
        drop(database);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn trigger_rows_join_one_triggers_events() {
        let row = |name: &str, event: &str| {
            vec![
                Some(name.to_owned()),
                Some("orders".to_owned()),
                Some("BEFORE".to_owned()),
                Some(event.to_owned()),
            ]
        };
        let triggers = trigger_rows(&[
            row("stamp", "INSERT"),
            row("stamp", "UPDATE"),
            row("stamp", "UPDATE"),
            vec![None],
        ]);
        assert_eq!(triggers.len(), 2);
        assert_eq!(triggers[0].events_text(), "INSERT, UPDATE");
        assert_eq!(triggers[0].summary(), "Trigger · BEFORE INSERT, UPDATE on orders");
        assert_eq!(triggers[1].name, "");
    }

    #[test]
    fn an_empty_file_has_no_key_spaces_and_no_tables() {
        let path = scratch("empty");
        let database = Database::open(&path).expect("the file opens");
        let catalog = read_file(&database).expect("the catalog reads");
        assert_eq!(catalog, Catalog::default());
        drop(database);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn counts_are_short() {
        assert_eq!(compact_count(312), "312");
        assert_eq!(compact_count(1_204), "1.2K");
        assert_eq!(compact_count(2_000), "2K");
        assert_eq!(compact_count(100_000), "100K");
        assert_eq!(compact_count(2_100_000), "2.1M");
        assert_eq!(compact_count(3_400_000_000), "3.4B");
        assert_eq!(KeyCount::MoreThanLimit.text(), "100K+");
    }

    #[test]
    fn a_server_table_is_described_by_its_columns() {
        let columns = vec![
            CatalogColumn {
                name: "id".into(),
                type_name: "integer".into(),
                primary_key: true,
                not_null: true,
            },
            CatalogColumn {
                name: "order date".into(),
                type_name: "date".into(),
                primary_key: false,
                not_null: true,
            },
        ];
        assert_eq!(
            table_ddl("orders", &columns),
            "CREATE TABLE orders (\n    id integer,\n    \"order date\" date NOT NULL,\n    PRIMARY KEY (id)\n);"
        );
        assert_eq!(
            index_columns("CREATE INDEX i ON public.orders USING btree (customer_id, \"day\")"),
            vec!["customer_id", "day"]
        );
    }
}
