use crate::ast::Name;
use crate::encoding::{NAMESPACE, prefix_end};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::TableCache;
use crate::logical_decoding::{DecodedChange, DecodedOperation};
use crate::session::QueryResult;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};

pub(crate) const PUBLICATION_SPACE: u8 = 0x21;
const RECORD_VERSION: u8 = 1;
const ALL_TABLES: u8 = 1;
const PUBLISHES_INSERT: u8 = 2;
const PUBLISHES_UPDATE: u8 = 4;
const PUBLISHES_DELETE: u8 = 8;
const PUBLISHES_TRUNCATE: u8 = 16;
const EVERY_ACTION: u8 = PUBLISHES_INSERT | PUBLISHES_UPDATE | PUBLISHES_DELETE | PUBLISHES_TRUNCATE;

/// How `ALTER PUBLICATION` changes a publication's table list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TableChange {
    Add,
    Drop,
    Set,
}

/// A table a publication lists, with the columns it sends (all of them when empty).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublishedTable {
    pub(crate) name: Name,
    pub(crate) columns: Vec<Name>,
    /// The row filter's text, `WHERE (...)`'s expression (D1249).
    pub(crate) filter: Option<String>,
}

/// What a publication publishes from: every table, or the tables and schemas listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PublishedFrom {
    AllTables,
    Listed { tables: Vec<PublishedTable>, schemas: Vec<Name> },
}

/// A publication statement (PostgreSQL's `CREATE`, `ALTER` and `DROP PUBLICATION`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PublicationCommand {
    Create { name: Name, from: PublishedFrom, publish: Option<String> },
    AlterTables { name: Name, change: TableChange, tables: Vec<PublishedTable>, schemas: Vec<Name> },
    AlterPublish { name: Name, publish: String },
    Rename { name: Name, to: Name },
    Owner { name: Name },
    Drop { names: Vec<Name>, if_exists: bool },
}

impl PublicationCommand {
    pub(crate) fn tag(&self) -> &'static str {
        match self {
            Self::Create { .. } => "CREATE PUBLICATION",
            Self::Drop { .. } => "DROP PUBLICATION",
            _ => "ALTER PUBLICATION",
        }
    }
}

/// A stored publication: its tables by internal name, its schemas, and the actions it sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredPublication {
    pub(crate) name: String,
    pub(crate) all_tables: bool,
    pub(crate) tables: Vec<String>,
    pub(crate) schemas: Vec<String>,
    actions: u8,
    /// The tables listed with a column list, and the columns each sends (D1248).
    pub(crate) column_lists: Vec<(String, Vec<String>)>,
    /// The tables listed with a row filter, and its expression's text (D1249).
    pub(crate) row_filters: Vec<(String, String)>,
}

impl StoredPublication {
    pub(crate) fn publishes_insert(&self) -> bool {
        self.actions & PUBLISHES_INSERT != 0
    }

    pub(crate) fn publishes_update(&self) -> bool {
        self.actions & PUBLISHES_UPDATE != 0
    }

    pub(crate) fn publishes_delete(&self) -> bool {
        self.actions & PUBLISHES_DELETE != 0
    }

    pub(crate) fn publishes_truncate(&self) -> bool {
        self.actions & PUBLISHES_TRUNCATE != 0
    }

    /// The columns this publication sends of the table `internal`: `None` for all of them.
    fn columns_of(&self, internal: &str) -> Option<&[String]> {
        if self.all_tables || self.schemas.iter().any(|schema| schema == crate::schemas::schema_of(internal)) {
            return None;
        }
        self.column_lists
            .iter()
            .find(|(table, _)| table == internal)
            .map(|(_, columns)| columns.as_slice())
    }

    fn covers(&self, internal: &str) -> bool {
        self.all_tables
            || self.tables.iter().any(|table| table == internal)
            || self.schemas.iter().any(|schema| schema == crate::schemas::schema_of(internal))
    }
}

fn key(name: &str) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(PUBLICATION_SPACE);
    key.extend_from_slice(name.as_bytes());
    key
}

fn push_texts(out: &mut Vec<u8>, texts: &[String]) {
    out.extend_from_slice(&u32::try_from(texts.len()).unwrap_or(u32::MAX).to_be_bytes());
    for text in texts {
        out.extend_from_slice(&u32::try_from(text.len()).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(text.as_bytes());
    }
}

fn read_texts(bytes: &[u8], at: &mut usize) -> SqlResult<Vec<String>> {
    let corrupt = || crate::encoding::corrupt("publication");
    let number = |at: &mut usize| -> SqlResult<usize> {
        let word = bytes.get(*at..*at + 4).ok_or_else(corrupt)?;
        *at += 4;
        Ok(u32::from_be_bytes([word[0], word[1], word[2], word[3]]) as usize)
    };
    let count = number(at)?;
    let mut texts = Vec::with_capacity(count.min(bytes.len()));
    for _ in 0..count {
        let length = number(at)?;
        let text = bytes.get(*at..*at + length).ok_or_else(corrupt)?;
        *at += length;
        texts.push(String::from_utf8(text.to_vec()).map_err(|_| corrupt())?);
    }
    Ok(texts)
}

fn encode(publication: &StoredPublication) -> Vec<u8> {
    let flags = publication.actions | if publication.all_tables { ALL_TABLES } else { 0 };
    let mut out = vec![RECORD_VERSION, flags];
    push_texts(&mut out, &publication.tables);
    push_texts(&mut out, &publication.schemas);
    if !publication.column_lists.is_empty() || !publication.row_filters.is_empty() {
        let tables: Vec<String> = publication.column_lists.iter().map(|(table, _)| table.clone()).collect();
        push_texts(&mut out, &tables);
        for (_, columns) in &publication.column_lists {
            push_texts(&mut out, columns);
        }
        let (filtered, filters): (Vec<String>, Vec<String>) = publication.row_filters.iter().cloned().unzip();
        push_texts(&mut out, &filtered);
        push_texts(&mut out, &filters);
    }
    out
}

fn decode(name: &str, bytes: &[u8]) -> SqlResult<StoredPublication> {
    let [RECORD_VERSION, flags, rest @ ..] = bytes else {
        return Err(crate::encoding::corrupt("publication"));
    };
    let mut at = 0;
    let tables = read_texts(rest, &mut at)?;
    let schemas = read_texts(rest, &mut at)?;
    let mut column_lists = Vec::new();
    if at < rest.len() {
        for table in read_texts(rest, &mut at)? {
            column_lists.push((table, read_texts(rest, &mut at)?));
        }
    }
    let mut row_filters = Vec::new();
    if at < rest.len() {
        let filtered = read_texts(rest, &mut at)?;
        let filters = read_texts(rest, &mut at)?;
        row_filters = filtered.into_iter().zip(filters).collect();
    }
    Ok(StoredPublication {
        name: name.to_owned(),
        all_tables: flags & ALL_TABLES != 0,
        tables,
        schemas,
        actions: flags & EVERY_ACTION,
        column_lists,
        row_filters,
    })
}

/// Every publication of the database, by name.
pub(crate) fn every_publication(reader: &dyn KeyValueRead) -> SqlResult<Vec<StoredPublication>> {
    let mut prefix = NAMESPACE.to_vec();
    prefix.push(PUBLICATION_SPACE);
    let mut found = Vec::new();
    let mut failure = None;
    reader.scan(&prefix, &prefix_end(&prefix), ScanDirection::Forward, &mut |key, value| {
        let name = String::from_utf8_lossy(key.get(prefix.len()..).unwrap_or_default()).into_owned();
        match decode(&name, value) {
            Ok(publication) => {
                found.push(publication);
                Ok(true)
            }
            Err(error) => {
                failure = Some(error);
                Ok(false)
            }
        }
    })?;
    match failure {
        Some(error) => Err(error),
        None => Ok(found),
    }
}

fn stored(reader: &dyn KeyValueRead, name: &str) -> SqlResult<Option<StoredPublication>> {
    reader.get(&key(name))?.map(|bytes| decode(name, &bytes)).transpose()
}

fn missing(name: &Name) -> SqlError {
    SqlError::new(SqlState::UNDEFINED_OBJECT, format!("publication \"{}\" does not exist", name.text)).at(name.position)
}

fn actions_of(publish: Option<&str>) -> SqlResult<u8> {
    let Some(publish) = publish else {
        return Ok(EVERY_ACTION);
    };
    let mut actions = 0;
    for action in publish.split(',').map(str::trim).filter(|action| !action.is_empty()) {
        actions |= match action.to_ascii_lowercase().as_str() {
            "insert" => PUBLISHES_INSERT,
            "update" => PUBLISHES_UPDATE,
            "delete" => PUBLISHES_DELETE,
            "truncate" => PUBLISHES_TRUNCATE,
            other => {
                return Err(SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("unrecognized value for publication option \"publish\": \"{other}\""),
                ));
            }
        };
    }
    Ok(actions)
}

struct Listed {
    tables: Vec<String>,
    column_lists: Vec<(String, Vec<String>)>,
    row_filters: Vec<(String, String)>,
}

fn parse_filter(text: &str) -> SqlResult<crate::ast::Expr> {
    let damaged = || crate::encoding::corrupt("publication row filter");
    let tokens = crate::lexer::tokenize_in(text, crate::dialect::Dialect::PostgreSql).map_err(|_| damaged())?;
    let mut parser = crate::parser::Parser::new(text, &tokens);
    let expr = parser.expression().map_err(|_| damaged())?;
    if !parser.at_end() {
        return Err(damaged());
    }
    Ok(expr)
}

fn passes(table: &crate::encoding::TableDefinition, expr: &crate::ast::Expr, row: &[crate::Value]) -> SqlResult<bool> {
    let evaluator = crate::eval::Evaluator {
        scope: crate::eval::Scope::One(crate::eval::Source {
            table,
            label: &table.name,
            offset: 0,
        }),
        row,
        correlation: None,
    };
    Ok(evaluator.boolean(expr, "WHERE")? == Some(true))
}

fn table_names(reader: &dyn KeyValueRead, tables: &mut TableCache, listed: &[PublishedTable]) -> SqlResult<Listed> {
    let mut resolved = Vec::with_capacity(listed.len());
    let mut column_lists = Vec::new();
    let mut row_filters = Vec::new();
    for table in listed {
        let definition = tables.require(reader, &table.name)?;
        if !resolved.contains(&definition.name) {
            resolved.push(definition.name.clone());
        }
        if let Some(filter) = &table.filter {
            let expr = parse_filter(filter)?;
            let nulls = vec![crate::Value::Null; definition.columns.len()];
            passes(&definition, &expr, &nulls)?;
            row_filters.push((definition.name.clone(), filter.clone()));
        }
        if table.columns.is_empty() {
            continue;
        }
        let mut columns = Vec::with_capacity(table.columns.len());
        for column in &table.columns {
            if !definition.columns.iter().any(|own| own.name == column.text) {
                return Err(SqlError::new(
                    SqlState::UNDEFINED_COLUMN,
                    format!(
                        "column \"{}\" of relation \"{}\" does not exist",
                        column.text,
                        crate::schemas::base_name(&definition.name)
                    ),
                )
                .at(column.position));
            }
            if columns.contains(&column.text) {
                return Err(SqlError::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!("duplicate column \"{}\" in publication column list", column.text),
                )
                .at(column.position));
            }
            columns.push(column.text.clone());
        }
        column_lists.push((definition.name.clone(), columns));
    }
    Ok(Listed {
        tables: resolved,
        column_lists,
        row_filters,
    })
}

fn schema_names(reader: &dyn KeyValueRead, names: &[Name]) -> SqlResult<Vec<String>> {
    let mut resolved = Vec::with_capacity(names.len());
    for name in names {
        if !crate::schemas::schema_exists(reader, &name.text)? {
            return Err(SqlError::new(SqlState::INVALID_SCHEMA_NAME, format!("schema \"{}\" does not exist", name.text))
                .at(name.position));
        }
        if !resolved.contains(&name.text) {
            resolved.push(name.text.clone());
        }
    }
    Ok(resolved)
}

/// Runs a publication statement.
pub(crate) fn run(writer: &mut dyn KeyValueWrite, tables: &mut TableCache, command: &PublicationCommand) -> SqlResult<QueryResult> {
    match command {
        PublicationCommand::Create { name, from, publish } => {
            if stored(writer, &name.text)?.is_some() {
                return Err(SqlError::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!("publication \"{}\" already exists", name.text),
                )
                .at(name.position));
            }
            let (all_tables, listed, schemas) = match from {
                PublishedFrom::AllTables => (
                    true,
                    Listed {
                        tables: Vec::new(),
                        column_lists: Vec::new(),
                        row_filters: Vec::new(),
                    },
                    Vec::new(),
                ),
                PublishedFrom::Listed { tables: listed, schemas } => {
                    (false, table_names(writer, tables, listed)?, schema_names(writer, schemas)?)
                }
            };
            let publication = StoredPublication {
                name: name.text.clone(),
                all_tables,
                tables: listed.tables,
                schemas,
                actions: actions_of(publish.as_deref())?,
                column_lists: listed.column_lists,
                row_filters: listed.row_filters,
            };
            writer.put(&key(&name.text), &encode(&publication))?;
        }
        PublicationCommand::AlterTables { name, change, tables: listed, schemas } => {
            let mut publication = stored(writer, &name.text)?.ok_or_else(|| missing(name))?;
            if publication.all_tables {
                return Err(SqlError::new(
                    SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!("publication \"{}\" is defined as FOR ALL TABLES", name.text),
                )
                .with_detail("Tables cannot be added to or dropped from FOR ALL TABLES publications.")
                .at(name.position));
            }
            let Listed {
                tables: listed,
                column_lists,
                row_filters,
            } = table_names(writer, tables, listed)?;
            let schemas = schema_names(writer, schemas)?;
            match change {
                TableChange::Set => {
                    publication.tables = listed;
                    publication.schemas = schemas;
                    publication.column_lists = column_lists;
                    publication.row_filters = row_filters;
                }
                TableChange::Add => {
                    for table in listed {
                        if publication.tables.contains(&table) {
                            return Err(SqlError::new(
                                SqlState::DUPLICATE_OBJECT,
                                format!(
                                    "relation \"{}\" is already member of publication \"{}\"",
                                    crate::schemas::base_name(&table),
                                    name.text
                                ),
                            ));
                        }
                        publication.tables.push(table);
                    }
                    publication.column_lists.extend(column_lists);
                    publication.row_filters.extend(row_filters);
                    for schema in schemas {
                        if !publication.schemas.contains(&schema) {
                            publication.schemas.push(schema);
                        }
                    }
                }
                TableChange::Drop => {
                    for table in &listed {
                        if !publication.tables.contains(table) {
                            return Err(SqlError::new(
                                SqlState::UNDEFINED_OBJECT,
                                format!(
                                    "relation \"{}\" is not part of the publication",
                                    crate::schemas::base_name(table)
                                ),
                            ));
                        }
                    }
                    publication.tables.retain(|table| !listed.contains(table));
                    publication.column_lists.retain(|(table, _)| !listed.contains(table));
                    publication.row_filters.retain(|(table, _)| !listed.contains(table));
                    publication.schemas.retain(|schema| !schemas.contains(schema));
                }
            }
            writer.put(&key(&name.text), &encode(&publication))?;
        }
        PublicationCommand::AlterPublish { name, publish } => {
            let mut publication = stored(writer, &name.text)?.ok_or_else(|| missing(name))?;
            publication.actions = actions_of(Some(publish))?;
            writer.put(&key(&name.text), &encode(&publication))?;
        }
        PublicationCommand::Rename { name, to } => {
            let mut publication = stored(writer, &name.text)?.ok_or_else(|| missing(name))?;
            if stored(writer, &to.text)?.is_some() {
                return Err(SqlError::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!("publication \"{}\" already exists", to.text),
                )
                .at(to.position));
            }
            writer.delete(&key(&name.text))?;
            publication.name = to.text.clone();
            writer.put(&key(&to.text), &encode(&publication))?;
        }
        PublicationCommand::Owner { name } => {
            stored(writer, &name.text)?.ok_or_else(|| missing(name))?;
        }
        PublicationCommand::Drop { names, if_exists } => {
            let mut notices = Vec::new();
            for name in names {
                if stored(writer, &name.text)?.is_none() {
                    if *if_exists {
                        notices.push(format!("publication \"{}\" does not exist, skipping", name.text));
                        continue;
                    }
                    return Err(missing(name));
                }
                writer.delete(&key(&name.text))?;
            }
            return Ok(notices
                .into_iter()
                .fold(QueryResult::command(command.tag()), QueryResult::with_notice));
        }
    }
    Ok(QueryResult::command(command.tag()))
}

/// Which changes the publications a subscriber names send: a change goes out when one of
/// them covers its table (every table, the table listed, or its schema listed) and sends its
/// kind of change.
#[derive(Debug, Clone)]
pub struct PublicationFilter {
    publications: Vec<StoredPublication>,
    row_filters: Vec<RowFilter>,
}

#[derive(Debug, Clone)]
struct RowFilter {
    publication: usize,
    table: std::sync::Arc<crate::encoding::TableDefinition>,
    expr: crate::ast::Expr,
}

impl PublicationFilter {
    fn row_passes(&self, publication: usize, internal: &str, change: &DecodedChange) -> bool {
        let filters: Vec<&RowFilter> = self
            .row_filters
            .iter()
            .filter(|filter| filter.publication == publication && filter.table.name == internal)
            .collect();
        if filters.is_empty() {
            return true;
        }
        let row = match change.operation {
            DecodedOperation::Truncate => return true,
            DecodedOperation::Delete => change.before.as_deref(),
            _ => change.after.as_deref(),
        };
        let Some(row) = row else {
            return false;
        };
        filters
            .iter()
            .any(|filter| passes(&filter.table, &filter.expr, row).unwrap_or(false))
    }

    /// Whether `change` is sent: a publication the subscriber named covers its table, sends
    /// its kind of change and, when it filters the table's rows, keeps this row (D1249; the
    /// new row of an insert or update, the old one of a delete).
    pub fn publishes(&self, change: &DecodedChange) -> bool {
        let internal = crate::schemas::qualified(&change.schema, &change.table);
        self.publications.iter().enumerate().any(|(at, publication)| {
            publication.covers(&internal)
                && match change.operation {
                    DecodedOperation::Insert => publication.publishes_insert(),
                    DecodedOperation::Update => publication.publishes_update(),
                    DecodedOperation::Delete => publication.publishes_delete(),
                    DecodedOperation::Truncate => publication.publishes_truncate(),
                }
                && self.row_passes(at, &internal, change)
        })
    }

    /// `change` with only the columns the publications covering its table send (D1248): all
    /// of them when any covering publication has no column list for it, else the union of
    /// the lists, in the table's order, the key columns always kept.
    pub fn narrowed(&self, change: &DecodedChange) -> DecodedChange {
        let internal = crate::schemas::qualified(&change.schema, &change.table);
        let mut wanted: Vec<&str> = Vec::new();
        for publication in self.publications.iter().filter(|publication| publication.covers(&internal)) {
            match publication.columns_of(&internal) {
                None => return change.clone(),
                Some(columns) => wanted.extend(columns.iter().map(String::as_str)),
            }
        }
        let kept: Vec<usize> = (0..change.columns.len())
            .filter(|at| change.key_columns.contains(at) || wanted.contains(&change.columns[*at].0.as_str()))
            .collect();
        if kept.len() == change.columns.len() {
            return change.clone();
        }
        let pick = |row: &Option<Vec<crate::Value>>| {
            row.as_ref()
                .map(|values| kept.iter().filter_map(|at| values.get(*at).cloned()).collect())
        };
        DecodedChange {
            table_id: change.table_id,
            schema: change.schema.clone(),
            table: change.table.clone(),
            columns: kept.iter().map(|at| change.columns[*at].clone()).collect(),
            key_columns: change
                .key_columns
                .iter()
                .filter_map(|key| kept.iter().position(|at| at == key))
                .collect(),
            operation: change.operation,
            before: pick(&change.before),
            after: pick(&change.after),
        }
    }
}

/// The filter of the publications `names`.
///
/// # Errors
///
/// A publication that does not exist, with PostgreSQL's message.
pub(crate) fn filter(reader: &dyn KeyValueRead, names: &[String]) -> SqlResult<PublicationFilter> {
    let mut publications = Vec::with_capacity(names.len());
    let mut row_filters = Vec::new();
    let mut tables = TableCache::default();
    for name in names {
        let publication = stored(reader, name)?.ok_or_else(|| {
            SqlError::new(SqlState::UNDEFINED_OBJECT, format!("publication \"{name}\" does not exist"))
        })?;
        for (table, text) in &publication.row_filters {
            if let Some(definition) = tables.table(reader, table)? {
                row_filters.push(RowFilter {
                    publication: publications.len(),
                    table: definition,
                    expr: parse_filter(text)?,
                });
            }
        }
        publications.push(publication);
    }
    Ok(PublicationFilter {
        publications,
        row_filters,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_actions_parse() {
        let publication = StoredPublication {
            name: "orders_feed".to_owned(),
            all_tables: false,
            tables: vec!["orders".to_owned(), "sales\0items".to_owned()],
            schemas: vec!["audit".to_owned()],
            actions: actions_of(Some("insert, update")).unwrap(),
            column_lists: vec![("orders".to_owned(), vec!["id".to_owned(), "total".to_owned()])],
            row_filters: vec![("orders".to_owned(), "total > 10".to_owned())],
        };
        let decoded = decode("orders_feed", &encode(&publication)).unwrap();
        assert_eq!(decoded, publication);
        assert!(decoded.publishes_insert() && decoded.publishes_update() && !decoded.publishes_delete());
        assert!(decoded.covers("orders") && decoded.covers("audit\0log") && !decoded.covers("customers"));
        assert!(actions_of(Some("insert, upsert")).is_err());
        assert_eq!(actions_of(None).unwrap(), EVERY_ACTION);
        assert!(decode("x", &[9]).is_err());
    }

    fn run_sql(session: &mut crate::Session<'_>, sql: &str) -> Result<String, crate::SqlError> {
        let mut outputs = Vec::new();
        session.execute(sql, &mut |result| outputs.push(crate::tests::render(&result)))?;
        Ok(outputs.pop().unwrap_or_default())
    }

    fn change(table: &str, operation: DecodedOperation) -> DecodedChange {
        DecodedChange {
            table_id: 1,
            schema: crate::schemas::DEFAULT_SCHEMA.to_owned(),
            table: table.to_owned(),
            columns: Vec::new(),
            key_columns: Vec::new(),
            operation,
            before: None,
            after: None,
        }
    }

    #[test]
    fn publications_are_created_altered_listed_and_filter_changes() {
        let store = crate::memory_store::MemoryStore::default();
        let mut session = crate::Session::new(&store);
        for sql in [
            "CREATE TABLE orders (id integer PRIMARY KEY)",
            "CREATE TABLE items (id integer PRIMARY KEY)",
            "CREATE TABLE notes (id integer PRIMARY KEY)",
            "CREATE PUBLICATION feed FOR TABLE orders WITH (publish = 'insert, update')",
            "ALTER PUBLICATION feed ADD TABLE items",
            "CREATE PUBLICATION everything FOR ALL TABLES",
        ] {
            run_sql(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(
            run_sql(&mut session, "SELECT pubname, puballtables, pubdelete FROM pg_publication ORDER BY pubname").unwrap(),
            "pubname|puballtables|pubdelete\neverything|t|t\nfeed|f|f\nSELECT 2"
        );
        assert_eq!(run_sql(&mut session, "SELECT count(*) AS listed FROM pg_publication_rel").unwrap(), "listed\n2\nSELECT 1");
        assert!(run_sql(&mut session, "CREATE PUBLICATION feed FOR TABLE notes").is_err());
        assert!(run_sql(&mut session, "ALTER PUBLICATION everything ADD TABLE notes").is_err());
        assert!(run_sql(&mut session, "CREATE PUBLICATION broken FOR TABLE nowhere").is_err());
        let feed = session.publication_filter(&["feed".to_owned()]).unwrap();
        assert!(feed.publishes(&change("orders", DecodedOperation::Insert)));
        assert!(!feed.publishes(&change("orders", DecodedOperation::Delete)));
        assert!(!feed.publishes(&change("notes", DecodedOperation::Insert)));
        let both = session.publication_filter(&["feed".to_owned(), "everything".to_owned()]).unwrap();
        assert!(both.publishes(&change("notes", DecodedOperation::Delete)));
        assert!(session.publication_filter(&["missing".to_owned()]).is_err());
        run_sql(&mut session, "ALTER PUBLICATION feed RENAME TO orders_feed").unwrap();
        run_sql(&mut session, "DROP PUBLICATION IF EXISTS orders_feed, missing").unwrap();
        assert_eq!(run_sql(&mut session, "SELECT count(*) AS left_over FROM pg_publication").unwrap(), "left_over\n1\nSELECT 1");
    }

    #[test]
    fn a_column_list_narrows_the_changes_sent() {
        let store = crate::memory_store::MemoryStore::default();
        let mut session = crate::Session::new(&store);
        for sql in [
            "CREATE TABLE orders (id integer PRIMARY KEY, total integer, secret text)",
            "CREATE PUBLICATION totals FOR TABLE orders (id, total)",
        ] {
            run_sql(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(run_sql(&mut session, "SELECT prattrs FROM pg_publication_rel").unwrap(), "prattrs\n1 2\nSELECT 1");
        assert!(run_sql(&mut session, "CREATE PUBLICATION broken FOR TABLE orders (nowhere)").is_err());
        assert!(run_sql(&mut session, "CREATE PUBLICATION twice FOR TABLE orders (id, id)").is_err());
        let full = DecodedChange {
            columns: vec![
                ("id".to_owned(), crate::SqlType::Integer),
                ("total".to_owned(), crate::SqlType::Integer),
                ("secret".to_owned(), crate::SqlType::Text),
            ],
            key_columns: vec![0],
            after: Some(vec![crate::Value::Integer(1), crate::Value::Integer(20), crate::Value::Text("x".to_owned())]),
            ..change("orders", DecodedOperation::Insert)
        };
        let totals = session.publication_filter(&["totals".to_owned()]).unwrap();
        let sent = totals.narrowed(&full);
        assert_eq!(sent.columns.len(), 2);
        assert_eq!(sent.after, Some(vec![crate::Value::Integer(1), crate::Value::Integer(20)]));
        assert_eq!(sent.key_columns, vec![0]);
        run_sql(&mut session, "CREATE PUBLICATION everything FOR ALL TABLES").unwrap();
        let both = session.publication_filter(&["totals".to_owned(), "everything".to_owned()]).unwrap();
        assert_eq!(both.narrowed(&full), full);
        run_sql(&mut session, "CREATE PUBLICATION big FOR TABLE orders WHERE (total > 10)").unwrap();
        assert!(run_sql(&mut session, "CREATE PUBLICATION wrong FOR TABLE orders WHERE (nowhere > 1)").is_err());
        let big = session.publication_filter(&["big".to_owned()]).unwrap();
        assert!(big.publishes(&full));
        let small = DecodedChange {
            after: Some(vec![crate::Value::Integer(2), crate::Value::Integer(5), crate::Value::Null]),
            ..full.clone()
        };
        assert!(!big.publishes(&small));
    }
}
