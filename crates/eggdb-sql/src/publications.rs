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

/// What a publication publishes from: every table, or the tables and schemas listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PublishedFrom {
    AllTables,
    Listed { tables: Vec<Name>, schemas: Vec<Name> },
}

/// A publication statement (PostgreSQL's `CREATE`, `ALTER` and `DROP PUBLICATION`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PublicationCommand {
    Create { name: Name, from: PublishedFrom, publish: Option<String> },
    AlterTables { name: Name, change: TableChange, tables: Vec<Name>, schemas: Vec<Name> },
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
    out
}

fn decode(name: &str, bytes: &[u8]) -> SqlResult<StoredPublication> {
    let [RECORD_VERSION, flags, rest @ ..] = bytes else {
        return Err(crate::encoding::corrupt("publication"));
    };
    let mut at = 0;
    let tables = read_texts(rest, &mut at)?;
    let schemas = read_texts(rest, &mut at)?;
    Ok(StoredPublication {
        name: name.to_owned(),
        all_tables: flags & ALL_TABLES != 0,
        tables,
        schemas,
        actions: flags & EVERY_ACTION,
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

fn table_names(reader: &dyn KeyValueRead, tables: &mut TableCache, names: &[Name]) -> SqlResult<Vec<String>> {
    let mut resolved = Vec::with_capacity(names.len());
    for name in names {
        let definition = tables.require(reader, name)?;
        if !resolved.contains(&definition.name) {
            resolved.push(definition.name.clone());
        }
    }
    Ok(resolved)
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
                PublishedFrom::AllTables => (true, Vec::new(), Vec::new()),
                PublishedFrom::Listed { tables: listed, schemas } => {
                    (false, table_names(writer, tables, listed)?, schema_names(writer, schemas)?)
                }
            };
            let publication = StoredPublication {
                name: name.text.clone(),
                all_tables,
                tables: listed,
                schemas,
                actions: actions_of(publish.as_deref())?,
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
            let listed = table_names(writer, tables, listed)?;
            let schemas = schema_names(writer, schemas)?;
            match change {
                TableChange::Set => {
                    publication.tables = listed;
                    publication.schemas = schemas;
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationFilter {
    publications: Vec<StoredPublication>,
}

impl PublicationFilter {
    /// Whether `change` is sent.
    pub fn publishes(&self, change: &DecodedChange) -> bool {
        let internal = crate::schemas::qualified(&change.schema, &change.table);
        self.publications.iter().any(|publication| {
            publication.covers(&internal)
                && match change.operation {
                    DecodedOperation::Insert => publication.publishes_insert(),
                    DecodedOperation::Update => publication.publishes_update(),
                    DecodedOperation::Delete => publication.publishes_delete(),
                }
        })
    }
}

/// The filter of the publications `names`.
///
/// # Errors
///
/// A publication that does not exist, with PostgreSQL's message.
pub(crate) fn filter(reader: &dyn KeyValueRead, names: &[String]) -> SqlResult<PublicationFilter> {
    let mut publications = Vec::with_capacity(names.len());
    for name in names {
        let publication = stored(reader, name)?.ok_or_else(|| {
            SqlError::new(SqlState::UNDEFINED_OBJECT, format!("publication \"{name}\" does not exist"))
        })?;
        publications.push(publication);
    }
    Ok(PublicationFilter { publications })
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
}
