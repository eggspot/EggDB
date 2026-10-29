use crate::encoding::{decode_row, prefix_end, rows_prefix};
use crate::error::{SqlError, SqlState};
use crate::schema::{TableSchema, ViewSchema, quoted};
use crate::store::ScanDirection;
use crate::types::Value;
use crate::{KeyValueStore, Session, SqlResult};

/// The most rows a description counts one by one; a table with more says only that it has
/// more ([`Relation::bounded_count_statement`]).
pub const EXACT_ROW_COUNT_UP_TO: i64 = 10_000;

/// A table or a view of the catalog, named by its schema and its name within that schema, as
/// the table commands and the MCP tools list, describe and sample them.
#[derive(Debug, Clone, PartialEq)]
pub struct Relation {
    /// Its schema, such as `public`.
    pub schema: String,
    /// Its name within the schema, without the schema.
    pub name: String,
    /// Whether it is a table or a view, with its definition.
    pub kind: RelationKind,
}

/// Whether a [`Relation`] is a table or a view, with its definition.
#[derive(Debug, Clone, PartialEq)]
pub enum RelationKind {
    /// A table.
    Table(TableSchema),
    /// A view.
    View(ViewSchema),
}

impl Relation {
    fn of_table(table: TableSchema) -> Self {
        Self {
            name: name_within(&table.name, &table.schema),
            schema: table.schema.clone(),
            kind: RelationKind::Table(table),
        }
    }

    fn of_view(view: ViewSchema) -> Self {
        Self {
            name: name_within(&view.name, &view.schema),
            schema: view.schema.clone(),
            kind: RelationKind::View(view),
        }
    }

    /// `table` or `view`.
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            RelationKind::Table(_) => "table",
            RelationKind::View(_) => "view",
        }
    }

    /// How many columns it has.
    pub fn column_count(&self) -> usize {
        match &self.kind {
            RelationKind::Table(table) => table.columns.len(),
            RelationKind::View(view) => view.columns.len(),
        }
    }

    /// Its comment, if it has one.
    pub fn comment(&self) -> Option<&str> {
        match &self.kind {
            RelationKind::Table(table) => table.comment.as_deref(),
            RelationKind::View(view) => view.comment.as_deref(),
        }
    }

    /// Its name as PostgreSQL's SQL writes it: quoted where needed, with its schema unless it
    /// is `public`.
    pub fn sql_name(&self) -> String {
        if self.schema == "public" {
            quoted(&self.name)
        } else {
            format!("{}.{}", quoted(&self.schema), quoted(&self.name))
        }
    }

    /// A PostgreSQL statement that counts its rows up to `up_to + 1`, so a count above
    /// `up_to` means "more than `up_to`" without reading every row.
    pub fn bounded_count_statement(&self, up_to: i64) -> String {
        format!(
            "SELECT count(*) FROM (SELECT 1 FROM {} LIMIT {}) AS counted",
            self.sql_name(),
            up_to.saturating_add(1)
        )
    }

    /// A PostgreSQL statement that reads its first `rows` rows as they are stored.
    pub fn first_rows_statement(&self, rows: u64) -> String {
        format!("SELECT * FROM {} LIMIT {rows}", self.sql_name())
    }
}

fn name_within(name: &str, schema: &str) -> String {
    if schema == "public" {
        return name.to_owned();
    }
    name.strip_prefix(schema)
        .and_then(|rest| rest.strip_prefix('.'))
        .unwrap_or(name)
        .to_owned()
}

impl<'s, S: KeyValueStore + 's> Session<'s, S> {
    /// Every table and view in the catalog, as this session sees them, in order of schema and
    /// then name.
    pub fn relations(&mut self) -> SqlResult<Vec<Relation>> {
        let tables = self.tables()?;
        let views = self.views()?;
        let mut relations: Vec<Relation> = tables
            .into_iter()
            .map(Relation::of_table)
            .chain(views.into_iter().map(Relation::of_view))
            .collect();
        relations.sort_by(|a, b| (&a.schema, &a.name).cmp(&(&b.schema, &b.name)));
        Ok(relations)
    }

    /// Reads the rows of `relation`, a table, in the order they are stored (primary-key order,
    /// or the order they were added in a table without a primary key), all from one snapshot.
    /// It starts just after the row at the position `after`, or at the first row, and hands
    /// `visit` each row's position and its values in column order until `visit` returns
    /// `false`.
    ///
    /// A position is opaque bytes, read back only by a later call on the same table: rows added
    /// or removed between two calls move no other row's position, so pages read this way
    /// neither repeat nor skip a row that stays. A view has no stored order and fails with
    /// SQLSTATE `42809`; a table dropped since the catalog was read fails with `42P01`.
    pub fn read_stored_rows(
        &mut self,
        relation: &Relation,
        after: Option<&[u8]>,
        visit: &mut dyn FnMut(&[u8], Vec<Value>) -> SqlResult<bool>,
    ) -> SqlResult<()> {
        let RelationKind::Table(table) = &relation.kind else {
            return Err(SqlError::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{}\" is a view, which has no stored rows", relation.name),
            ));
        };
        let stored_name = table.name.clone();
        self.read(|reader, tables| {
            let Some(crate::execute::Relation::Table(definition)) =
                tables.relation(reader, &stored_name)?
            else {
                return Err(SqlError::new(
                    SqlState::UNDEFINED_TABLE,
                    format!("relation \"{stored_name}\" does not exist"),
                ));
            };
            let prefix = rows_prefix(definition.id);
            let end = prefix_end(&prefix);
            let mut start = prefix.clone();
            if let Some(position) = after {
                start.extend_from_slice(position);
                start.push(0);
            }
            if start >= end {
                return Ok(());
            }
            reader.scan(&start, &end, ScanDirection::Forward, &mut |key, bytes| {
                let position = key.get(prefix.len()..).unwrap_or_default();
                visit(position, decode_row(bytes, &definition.columns)?)
            })
        })
    }
}

/// The relation `wanted` names, `name` or `schema.name` (`public` when no schema is given):
/// the one spelled exactly so, or else the one spelled so in lower case, as an unquoted SQL
/// name would be; `None` when there is neither.
pub fn find_relation(relations: Vec<Relation>, wanted: &str) -> Option<Relation> {
    let (schema, name) = wanted.split_once('.').unwrap_or(("public", wanted));
    let named = |relation: &Relation, schema: &str, name: &str| {
        relation.schema == schema && relation.name == name
    };
    let lower = (schema.to_lowercase(), name.to_lowercase());
    let position = relations
        .iter()
        .position(|relation| named(relation, schema, name))
        .or_else(|| {
            relations
                .iter()
                .position(|relation| named(relation, &lower.0, &lower.1))
        })?;
    relations.into_iter().nth(position)
}

#[cfg(test)]
mod tests {
    use super::{EXACT_ROW_COUNT_UP_TO, find_relation};
    use crate::memory_store::MemoryStore;
    use crate::{Session, SqlState, Value};

    #[test]
    fn relations_come_in_schema_and_name_order_and_are_found_as_sql_names_them() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        session
            .execute(
                "CREATE TABLE zoo (id integer PRIMARY KEY); CREATE TABLE \"Apes\" (id integer PRIMARY KEY); \
                 CREATE SCHEMA app; CREATE TABLE app.items (id integer PRIMARY KEY, name text); \
                 CREATE VIEW names AS SELECT name FROM app.items",
                &mut |_| {},
            )
            .unwrap();
        let relations = session.relations().unwrap();
        let listed: Vec<(String, String, &str, usize)> = relations
            .iter()
            .map(|relation| {
                (
                    relation.schema.clone(),
                    relation.name.clone(),
                    relation.kind_name(),
                    relation.column_count(),
                )
            })
            .collect();
        assert_eq!(
            listed,
            vec![
                ("app".to_owned(), "items".to_owned(), "table", 2),
                ("public".to_owned(), "Apes".to_owned(), "table", 1),
                ("public".to_owned(), "names".to_owned(), "view", 1),
                ("public".to_owned(), "zoo".to_owned(), "table", 1),
            ]
        );
        let items = find_relation(relations.clone(), "APP.ITEMS").unwrap();
        assert_eq!(items.sql_name(), "app.items");
        assert_eq!(
            items.bounded_count_statement(EXACT_ROW_COUNT_UP_TO),
            "SELECT count(*) FROM (SELECT 1 FROM app.items LIMIT 10001) AS counted"
        );
        let apes = find_relation(relations.clone(), "Apes").unwrap();
        assert_eq!(apes.first_rows_statement(11), "SELECT * FROM \"Apes\" LIMIT 11");
        assert!(find_relation(relations.clone(), "apes").is_none());
        assert!(find_relation(relations, "nothing").is_none());
    }

    #[test]
    fn stored_rows_come_in_key_order_and_a_position_resumes_after_its_row_while_others_write() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        session
            .execute(
                "CREATE TABLE t (id integer PRIMARY KEY, name text); \
                 INSERT INTO t VALUES (3, 'c'), (1, 'a'), (2, 'b'); \
                 CREATE TABLE bag (word text); INSERT INTO bag VALUES ('y'), ('x'); \
                 CREATE VIEW ids AS SELECT id FROM t",
                &mut |_| {},
            )
            .unwrap();
        let relations = session.relations().unwrap();
        let table = find_relation(relations.clone(), "t").unwrap();
        let mut first: Vec<(Vec<u8>, Vec<Value>)> = Vec::new();
        session
            .read_stored_rows(&table, None, &mut |position, values| {
                first.push((position.to_vec(), values));
                Ok(first.len() < 2)
            })
            .unwrap();
        let ids: Vec<Value> = first.iter().map(|(_, values)| values[0].clone()).collect();
        assert_eq!(ids, vec![Value::Integer(1), Value::Integer(2)]);
        assert!(first[0].0 < first[1].0);
        session
            .execute(
                "INSERT INTO t VALUES (0, 'z'), (5, 'e'); DELETE FROM t WHERE id = 3",
                &mut |_| {},
            )
            .unwrap();
        let mut rest = Vec::new();
        session
            .read_stored_rows(&table, Some(&first[1].0), &mut |_, values| {
                rest.push(values);
                Ok(true)
            })
            .unwrap();
        assert_eq!(rest, vec![vec![Value::Integer(5), Value::Text("e".to_owned())]]);
        let mut beyond = 0;
        session
            .read_stored_rows(&table, Some(&[0xff; 40]), &mut |_, _| {
                beyond += 1;
                Ok(true)
            })
            .unwrap();
        assert_eq!(beyond, 0);

        let bag = find_relation(relations.clone(), "bag").unwrap();
        let mut words = Vec::new();
        session
            .read_stored_rows(&bag, None, &mut |_, values| {
                words.push(values);
                Ok(true)
            })
            .unwrap();
        assert_eq!(
            words,
            vec![
                vec![Value::Text("y".to_owned())],
                vec![Value::Text("x".to_owned())]
            ]
        );

        let view = find_relation(relations, "ids").unwrap();
        let refused = session
            .read_stored_rows(&view, None, &mut |_, _| Ok(true))
            .unwrap_err();
        assert_eq!(refused.state(), SqlState::WRONG_OBJECT_TYPE);
    }
}
