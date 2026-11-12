use crate::cql::{
    Assignment, ColumnDefinition, Condition, Operator, Relation, Selection, Statement, TableName,
    Term,
};
use crate::types::CqlType;

pub(crate) const METADATA_DDL: &str = "CREATE SCHEMA IF NOT EXISTS eggdb_cql; CREATE TABLE IF NOT EXISTS eggdb_cql.tables (keyspace_name text, table_name text, partition_key text NOT NULL, clustering_key text NOT NULL, descending text NOT NULL, column_types text NOT NULL, PRIMARY KEY (keyspace_name, table_name))";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableInfo {
    pub(crate) keyspace: String,
    pub(crate) table: String,
    pub(crate) columns: Vec<(String, CqlType)>,
    pub(crate) partition: Vec<String>,
    pub(crate) clustering: Vec<String>,
    pub(crate) descending: Vec<String>,
}

impl TableInfo {
    pub(crate) fn column_type(&self, name: &str) -> Option<CqlType> {
        self.columns
            .iter()
            .find(|(column, _)| column == name)
            .map(|(_, cql)| *cql)
    }

    fn primary_key(&self) -> impl Iterator<Item = &String> {
        self.partition.iter().chain(&self.clustering)
    }
}

/// Where translation learns about keyspaces and tables.
pub(crate) trait Catalog {
    fn table(&mut self, keyspace: &str, table: &str) -> Result<TableInfo, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Void,
    Rows(Box<TableInfo>),
    Applied(String, String),
    SetKeyspace(String),
    SchemaChange {
        change: &'static str,
        target: &'static str,
        keyspace: String,
        name: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Sql(String),
    Marker(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Marker {
    pub(crate) name: String,
    pub(crate) cql: CqlType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Translation {
    pieces: Vec<Piece>,
    pub(crate) markers: Vec<Marker>,
    pub(crate) outcome: Outcome,
    pub(crate) keyspace: String,
    pub(crate) table: String,
}

impl Translation {
    pub(crate) fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }

    /// The SQL with each bind marker replaced by its value, given in the text form of a SQL
    /// parameter.
    pub(crate) fn render(&self, values: &[Option<String>]) -> String {
        let mut out = String::new();
        for piece in &self.pieces {
            match piece {
                Piece::Sql(text) => out.push_str(text),
                Piece::Marker(index) => match (
                    values.get(*index).cloned().flatten(),
                    self.markers.get(*index),
                ) {
                    (Some(value), Some(marker)) => {
                        out.push_str(&literal(&value));
                        out.push_str("::");
                        out.push_str(marker.cql.sql());
                    }
                    _ => out.push_str("NULL"),
                },
            }
        }
        out
    }
}

pub(crate) fn identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub(crate) fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn qualified(keyspace: &str, table: &str) -> String {
    format!("{}.{}", identifier(keyspace), identifier(table))
}

struct Builder {
    pieces: Vec<Piece>,
    markers: Vec<Option<Marker>>,
}

impl Builder {
    fn sql(&mut self, text: &str) {
        match self.pieces.last_mut() {
            Some(Piece::Sql(last)) => last.push_str(text),
            _ => self.pieces.push(Piece::Sql(text.to_owned())),
        }
    }

    fn term(&mut self, term: &Term, name: &str, cql: CqlType) -> Result<(), String> {
        if matches!(term, Term::List(_) | Term::Set(_) | Term::Map(_) | Term::Tuple(_)) {
            if !cql.is_collection() {
                return Err(format!("a collection value cannot go in the {} column {name}", cql.name()));
            }
            let json = term_json(term, cql, 0)?;
            let bytes = crate::types::encode_text(&json, cql)?;
            let canonical = crate::types::parameter_text(Some(&bytes), cql)?.unwrap_or_default();
            self.sql(&format!("{}::text", literal(&canonical)));
            return Ok(());
        }
        match term {
            Term::Marker(index) => {
                let slot = self
                    .markers
                    .get_mut(*index)
                    .ok_or("a bind marker is out of range")?;
                *slot = Some(Marker {
                    name: name.to_owned(),
                    cql,
                });
                self.pieces.push(Piece::Marker(*index));
            }
            Term::Text(text) | Term::Uuid(text) => {
                self.sql(&format!("{}::{}", literal(text), cql.sql()))
            }
            Term::Number(number) if number == "NaN" || number == "Infinity" => {
                self.sql(&format!("{}::{}", literal(number), cql.sql()))
            }
            Term::Number(number) => {
                if !number.bytes().all(|byte| {
                    byte.is_ascii_digit() || matches!(byte, b'.' | b'e' | b'E' | b'+' | b'-')
                }) {
                    return Err(format!("{number} is not a number"));
                }
                self.sql(&format!("{number}::{}", cql.sql()));
            }
            Term::Boolean(flag) => self.sql(if *flag { "TRUE" } else { "FALSE" }),
            Term::Blob(hex) => self.sql(&format!("'\\x{hex}'::bytea")),
            Term::Null => self.sql("NULL"),
            Term::List(_) | Term::Set(_) | Term::Map(_) | Term::Tuple(_) => {}
        }
        Ok(())
    }
}

const MOST_TERM_DEPTH: usize = 16;

/// A collection literal as the JSON text its column stores, before it is checked against the
/// column's type and put in canonical order.
fn term_json(term: &Term, cql: CqlType, depth: usize) -> Result<String, String> {
    if depth > MOST_TERM_DEPTH {
        return Err("collection values are nested too deeply".to_owned());
    }
    let items = |terms: &[Term], element: CqlType| -> Result<String, String> {
        let parts: Result<Vec<String>, String> = terms.iter().map(|item| term_json(item, element, depth + 1)).collect();
        Ok(format!("[{}]", parts?.join(",")))
    };
    match (term, cql) {
        (Term::List(terms), CqlType::List(element)) | (Term::Set(terms), CqlType::Set(element)) => items(terms, *element),
        (Term::Set(terms), CqlType::Map(..)) if terms.is_empty() => Ok("[]".to_owned()),
        (Term::Map(entries), CqlType::Map(key, value)) => {
            let mut parts = Vec::with_capacity(entries.len());
            for (entry_key, entry_value) in entries {
                parts.push(format!("[{},{}]", term_json(entry_key, *key, depth + 1)?, term_json(entry_value, *value, depth + 1)?));
            }
            Ok(format!("[{}]", parts.join(",")))
        }
        (Term::Tuple(terms), CqlType::Tuple(types)) => {
            if terms.len() > types.len() {
                return Err(format!("a {} value has too many items", cql.name()));
            }
            let mut parts = Vec::with_capacity(terms.len());
            for (item, item_type) in terms.iter().zip(types.iter()) {
                parts.push(match item {
                    Term::Null => "null".to_owned(),
                    other => term_json(other, *item_type, depth + 1)?,
                });
            }
            Ok(format!("[{}]", parts.join(",")))
        }
        (Term::Marker(_), _) => Err("bind markers inside collection values are not supported yet".to_owned()),
        (Term::Null, _) => Err("a collection cannot hold null".to_owned()),
        (Term::Text(text) | Term::Uuid(text), _) => Ok(crate::types::quote_json(text)),
        (Term::Blob(hex), _) => Ok(crate::types::quote_json(&format!("\\x{hex}"))),
        (Term::Boolean(flag), _) => Ok(flag.to_string()),
        (Term::Number(number), _) => {
            if number.parse::<f64>().is_ok_and(f64::is_finite) {
                Ok(number.clone())
            } else {
                Ok(crate::types::quote_json(number))
            }
        }
        (_, _) => Err(format!("the value does not fit the CQL type {}", cql.name())),
    }
}

fn keyspace_of(table: &TableName, current: Option<&str>) -> Result<String, String> {
    table.keyspace.clone().or_else(|| current.map(str::to_owned)).ok_or_else(|| "No keyspace has been specified. USE a keyspace, or explicitly specify keyspace.tablename".to_owned())
}

fn column(info: &TableInfo, name: &str) -> Result<CqlType, String> {
    info.column_type(name)
        .ok_or_else(|| format!("Undefined column name {name}"))
}

fn relations(
    builder: &mut Builder,
    info: &TableInfo,
    relations: &[Relation],
) -> Result<(), String> {
    relations_after(builder, info, relations, " WHERE ")
}

fn relations_after(
    builder: &mut Builder,
    info: &TableInfo,
    relations: &[Relation],
    first: &str,
) -> Result<(), String> {
    for (index, relation) in relations.iter().enumerate() {
        builder.sql(if index == 0 { first } else { " AND " });
        let cql = column(info, &relation.column)?;
        if matches!(relation.operator, Operator::Contains | Operator::ContainsKey) {
            contains(builder, relation, cql)?;
            continue;
        }
        builder.sql(&identifier(&relation.column));
        if relation.operator == Operator::In {
            if relation.terms.is_empty() {
                builder.sql(" IS NULL AND FALSE");
                continue;
            }
            builder.sql(" IN (");
            for (at, term) in relation.terms.iter().enumerate() {
                if at > 0 {
                    builder.sql(", ");
                }
                builder.term(term, &relation.column, cql)?;
            }
            builder.sql(")");
            continue;
        }
        builder.sql(match relation.operator {
            Operator::Equal => " = ",
            Operator::Less => " < ",
            Operator::LessOrEqual => " <= ",
            Operator::Greater => " > ",
            Operator::GreaterOrEqual => " >= ",
            Operator::NotEqual | Operator::In | Operator::Contains | Operator::ContainsKey => " <> ",
        });
        let term = relation.terms.first().ok_or("a relation has no value")?;
        builder.term(term, &relation.column, cql)?;
    }
    Ok(())
}

fn deleted_element(builder: &mut Builder, name: &str, cql: CqlType, element: &Term) -> Result<(), String> {
    let column = identifier(name);
    let current = format!("COALESCE({column}, '[]')::jsonb");
    match cql {
        CqlType::Map(key, _) => {
            builder.sql(&format!(
                "{column} = (SELECT COALESCE(jsonb_agg(item.value), '[]'::jsonb) FROM jsonb_array_elements({current}) AS item WHERE item.value -> 0 <> to_jsonb("
            ));
            builder.term(element, name, *key)?;
            builder.sql("))::text");
        }
        CqlType::List(_) => {
            builder.sql(&format!("{column} = ({current} #- ARRAY[("));
            builder.term(element, name, CqlType::Int)?;
            builder.sql(")::text])::text");
        }
        other => {
            return Err(format!("Invalid element deletion from the non-list, non-map {} column {name}", other.name()));
        }
    }
    Ok(())
}

fn contains(builder: &mut Builder, relation: &Relation, cql: CqlType) -> Result<(), String> {
    let term = relation.terms.first().ok_or("a relation has no value")?;
    let column = identifier(&relation.column);
    match (relation.operator, cql) {
        (Operator::Contains, CqlType::List(element) | CqlType::Set(element)) => {
            builder.sql(&format!("{column}::jsonb @> jsonb_build_array(to_jsonb("));
            builder.term(term, &relation.column, *element)?;
            builder.sql("))");
        }
        (Operator::Contains, CqlType::Map(_, item)) | (Operator::ContainsKey, CqlType::Map(item, _)) => {
            let side = if relation.operator == Operator::ContainsKey { 0 } else { 1 };
            builder.sql(&format!(
                "EXISTS (SELECT 1 FROM jsonb_array_elements({column}::jsonb) AS entry WHERE entry.value -> {side} = to_jsonb("
            ));
            builder.term(term, &relation.column, *item)?;
            builder.sql("))");
        }
        (Operator::ContainsKey, _) => {
            return Err(format!("Cannot use CONTAINS KEY on non-map column {}", relation.column));
        }
        _ => return Err(format!("Cannot use CONTAINS on non-collection column {}", relation.column)),
    }
    Ok(())
}

fn key_terms<'r>(
    info: &TableInfo,
    relations: &'r [Relation],
) -> Result<Vec<(&'r str, &'r Term)>, String> {
    for relation in relations {
        if !info.primary_key().any(|key| *key == relation.column) {
            return Err(format!(
                "Non PRIMARY KEY columns found in where clause: {}",
                relation.column
            ));
        }
        if relation.operator != Operator::Equal {
            return Err(format!(
                "Only equality on every PRIMARY KEY column is supported in UPDATE yet, not on {}",
                relation.column
            ));
        }
    }
    info.primary_key()
        .map(|key| {
            relations
                .iter()
                .find(|relation| relation.column == *key)
                .and_then(|relation| {
                    relation
                        .terms
                        .first()
                        .map(|term| (relation.column.as_str(), term))
                })
                .ok_or_else(|| format!("Some primary key parts are missing: {key}"))
        })
        .collect()
}

fn upsert(
    builder: &mut Builder,
    info: &TableInfo,
    target: &str,
    keys: &[(&str, &Term)],
    assignments: &[(String, Assignment)],
) -> Result<(), String> {
    let columns: Vec<String> = keys
        .iter()
        .map(|(name, _)| identifier(name))
        .chain(assignments.iter().map(|(name, _)| identifier(name)))
        .collect();
    builder.sql(&format!(
        "INSERT INTO {target} ({}) VALUES (",
        columns.join(", ")
    ));
    let mut first = true;
    for (name, term) in keys.iter().copied() {
        if !first {
            builder.sql(", ");
        }
        first = false;
        builder.term(term, name, column(info, name)?)?;
    }
    for (name, assignment) in assignments {
        if !first {
            builder.sql(", ");
        }
        first = false;
        inserted_value(builder, name, column(info, name)?, assignment)?;
    }
    let conflict: Vec<String> = info.primary_key().map(|key| identifier(key)).collect();
    builder.sql(&format!(") ON CONFLICT ({}) DO ", conflict.join(", ")));
    if assignments.is_empty() {
        builder.sql("NOTHING");
        return Ok(());
    }
    builder.sql("UPDATE SET ");
    let owner = format!("{}.", identifier(&info.table));
    for (at, (name, assignment)) in assignments.iter().enumerate() {
        if at > 0 {
            builder.sql(", ");
        }
        builder.sql(&format!("{} = ", identifier(name)));
        assigned_value(builder, &owner, name, column(info, name)?, assignment, true)?;
    }
    Ok(())
}

fn inserted_value(builder: &mut Builder, name: &str, cql: CqlType, assignment: &Assignment) -> Result<(), String> {
    match (assignment, cql) {
        (Assignment::Set(term) | Assignment::Add(term) | Assignment::Prepend(term), _) => builder.term(term, name, cql),
        (Assignment::Remove(term), cql) if !cql.is_collection() => {
            builder.sql("-(");
            builder.term(term, name, cql)?;
            builder.sql(")");
            Ok(())
        }
        (Assignment::Element(..), CqlType::Map(..)) => {
            new_json(builder, name, cql, assignment, false)?;
            builder.sql("::text");
            Ok(())
        }
        _ => {
            builder.sql("NULL");
            Ok(())
        }
    }
}

fn new_json(builder: &mut Builder, name: &str, cql: CqlType, assignment: &Assignment, excluded: bool) -> Result<(), String> {
    if excluded {
        builder.sql(&format!("EXCLUDED.{}::jsonb", identifier(name)));
        return Ok(());
    }
    match (assignment, cql) {
        (Assignment::Element(key, value), CqlType::Map(key_type, value_type)) => {
            builder.sql("jsonb_build_array(jsonb_build_array(to_jsonb(");
            builder.term(key, name, *key_type)?;
            builder.sql("), to_jsonb(");
            builder.term(value, name, *value_type)?;
            builder.sql(")))");
        }
        (Assignment::Set(term) | Assignment::Add(term) | Assignment::Prepend(term), _) => {
            builder.sql("(");
            builder.term(term, name, cql)?;
            builder.sql(")::jsonb");
        }
        _ => return Err(format!("this update of the {} column {name} carries no value", cql.name())),
    }
    Ok(())
}

fn assigned_value(
    builder: &mut Builder,
    owner: &str,
    name: &str,
    cql: CqlType,
    assignment: &Assignment,
    excluded: bool,
) -> Result<(), String> {
    let column = format!("{owner}{}", identifier(name));
    let current = format!("COALESCE({column}, '[]')::jsonb");
    match (assignment, cql) {
        (Assignment::Set(term), _) => {
            if excluded {
                builder.sql(&format!("EXCLUDED.{}", identifier(name)));
            } else {
                builder.term(term, name, cql)?;
            }
        }
        (Assignment::Add(_), CqlType::List(_)) => {
            builder.sql(&format!("({current} || "));
            new_json(builder, name, cql, assignment, excluded)?;
            builder.sql(")::text");
        }
        (Assignment::Prepend(_), CqlType::List(_)) => {
            builder.sql("(");
            new_json(builder, name, cql, assignment, excluded)?;
            builder.sql(&format!(" || {current})::text"));
        }
        (Assignment::Add(_), CqlType::Set(_)) => {
            builder.sql(&format!(
                "(SELECT COALESCE(jsonb_agg(merged.value ORDER BY merged.value), '[]'::jsonb) FROM (SELECT DISTINCT item.value FROM jsonb_array_elements({current} || "
            ));
            new_json(builder, name, cql, assignment, excluded)?;
            builder.sql(") AS item) AS merged)::text");
        }
        (Assignment::Add(_) | Assignment::Element(..), CqlType::Map(..)) => {
            builder.sql("(SELECT COALESCE(jsonb_agg(merged.value ORDER BY merged.value -> 0), '[]'::jsonb) FROM (SELECT fresh.value FROM jsonb_array_elements(");
            new_json(builder, name, cql, assignment, excluded)?;
            builder.sql(&format!(
                ") AS fresh UNION ALL SELECT kept.value FROM jsonb_array_elements({current}) AS kept WHERE NOT EXISTS (SELECT 1 FROM jsonb_array_elements("
            ));
            new_json(builder, name, cql, assignment, excluded)?;
            builder.sql(") AS other WHERE other.value -> 0 = kept.value -> 0)) AS merged)::text");
        }
        (Assignment::Remove(term), CqlType::List(_) | CqlType::Set(_)) => {
            builder.sql(&format!(
                "(SELECT COALESCE(jsonb_agg(item.value), '[]'::jsonb) FROM jsonb_array_elements({current}) AS item WHERE NOT (("
            ));
            builder.term(term, name, cql)?;
            builder.sql(")::jsonb @> jsonb_build_array(item.value)))::text");
        }
        (Assignment::Remove(term), CqlType::Map(key, _)) => {
            builder.sql(&format!(
                "(SELECT COALESCE(jsonb_agg(item.value), '[]'::jsonb) FROM jsonb_array_elements({current}) AS item WHERE NOT (("
            ));
            builder.term(term, name, CqlType::Set(key))?;
            builder.sql(")::jsonb @> jsonb_build_array(item.value -> 0)))::text");
        }
        (Assignment::Element(index, value), CqlType::List(element)) => {
            builder.sql(&format!("jsonb_set({current}, ARRAY[("));
            builder.term(index, name, CqlType::Int)?;
            builder.sql(")::text], to_jsonb(");
            builder.term(value, name, *element)?;
            builder.sql("))::text");
        }
        (Assignment::Add(term), cql) if !cql.is_collection() => {
            builder.sql(&format!("COALESCE({column}, 0) + "));
            if excluded {
                builder.sql(&format!("EXCLUDED.{}", identifier(name)));
            } else {
                builder.term(term, name, cql)?;
            }
        }
        (Assignment::Remove(term), cql) if !cql.is_collection() => {
            builder.sql(&format!("COALESCE({column}, 0) - "));
            builder.term(term, name, cql)?;
        }
        (_, cql) => return Err(format!("this update of the {} column {name} is not supported", cql.name())),
    }
    Ok(())
}

struct TableDefinition<'d> {
    columns: &'d [ColumnDefinition],
    partition: &'d [String],
    clustering: &'d [String],
    descending: &'d [String],
}

fn create_table(
    builder: &mut Builder,
    keyspace: &str,
    name: &str,
    if_not_exists: bool,
    definition: &TableDefinition<'_>,
) -> Result<(), String> {
    let TableDefinition {
        columns,
        partition,
        clustering,
        descending,
    } = *definition;
    for key in partition.iter().chain(clustering) {
        if !columns.iter().any(|definition| definition.name == *key) {
            return Err(format!(
                "Unknown definition {key} referenced in PRIMARY KEY"
            ));
        }
    }
    for column in descending {
        if !clustering.contains(column) {
            return Err(format!("Missing CLUSTERING ORDER for column {column}"));
        }
    }
    let definitions: Vec<String> = columns
        .iter()
        .map(|definition| format!("{} {}", identifier(&definition.name), definition.cql.sql()))
        .collect();
    let keys: Vec<String> = partition
        .iter()
        .chain(clustering)
        .map(|key| identifier(key))
        .collect();
    let types: Vec<String> = columns
        .iter()
        .map(|definition| format!("{}\t{}", definition.name, definition.cql.name()))
        .collect();
    builder.sql(&format!(
        "{METADATA_DDL}; CREATE TABLE {}{} ({}, PRIMARY KEY ({})); INSERT INTO eggdb_cql.tables VALUES ({}, {}, {}, {}, {}, {}) ON CONFLICT (keyspace_name, table_name) DO NOTHING",
        if if_not_exists { "IF NOT EXISTS " } else { "" },
        qualified(keyspace, name),
        definitions.join(", "),
        keys.join(", "),
        literal(keyspace),
        literal(name),
        literal(&partition.join("\n")),
        literal(&clustering.join("\n")),
        literal(&descending.join("\n")),
        literal(&types.join("\n"))
    ));
    Ok(())
}

fn select(
    builder: &mut Builder,
    info: &TableInfo,
    selection: &Selection,
    conditions: &[Relation],
    order: &[(String, bool)],
    limit: Option<&Term>,
) -> Result<(), String> {
    let target = qualified(&info.keyspace, &info.table);
    let projection = match selection {
        Selection::All => "*".to_owned(),
        Selection::Count => "count(*) AS \"count\"".to_owned(),
        Selection::Columns(columns) => {
            let mut parts = Vec::new();
            for (name, alias) in columns {
                column(info, name)?;
                parts.push(match alias {
                    Some(alias) => format!("{} AS {}", identifier(name), identifier(alias)),
                    None => identifier(name),
                });
            }
            parts.join(", ")
        }
    };
    builder.sql(&format!("SELECT {projection} FROM {target}"));
    relations(builder, info, conditions)?;
    let ordering: Vec<String> = if order.is_empty() {
        if matches!(selection, Selection::Count) || info.clustering.is_empty() {
            Vec::new()
        } else {
            info.partition
                .iter()
                .map(|key| identifier(key))
                .chain(info.clustering.iter().map(|key| {
                    if info.descending.contains(key) {
                        format!("{} DESC", identifier(key))
                    } else {
                        identifier(key)
                    }
                }))
                .collect()
        }
    } else {
        let mut ordering = Vec::new();
        for (name, descending) in order {
            if !info.clustering.contains(name) {
                return Err(format!(
                    "Order by is currently only supported on the clustered columns of the PRIMARY KEY, got {name}"
                ));
            }
            ordering.push(if *descending {
                format!("{} DESC", identifier(name))
            } else {
                identifier(name)
            });
        }
        ordering
    };
    if !ordering.is_empty() {
        builder.sql(&format!(" ORDER BY {}", ordering.join(", ")));
    }
    if let Some(limit) = limit {
        builder.sql(" LIMIT ");
        builder.term(limit, "[limit]", CqlType::Int)?;
    }
    Ok(())
}

fn statement(
    builder: &mut Builder,
    statement: &Statement,
    current: Option<&str>,
    catalog: &mut dyn Catalog,
) -> Result<(Outcome, String, String), String> {
    Ok(match statement {
        Statement::Use(keyspace) => (
            Outcome::SetKeyspace(keyspace.clone()),
            keyspace.clone(),
            String::new(),
        ),
        Statement::CreateKeyspace {
            name,
            if_not_exists,
        } => {
            builder.sql(&format!(
                "CREATE SCHEMA {}{}",
                if *if_not_exists { "IF NOT EXISTS " } else { "" },
                identifier(name)
            ));
            (
                Outcome::SchemaChange {
                    change: "CREATED",
                    target: "KEYSPACE",
                    keyspace: name.clone(),
                    name: None,
                },
                name.clone(),
                String::new(),
            )
        }
        Statement::DropKeyspace { name, if_exists } => {
            builder.sql(&format!(
                "DROP SCHEMA {}{} CASCADE; {METADATA_DDL}; DELETE FROM eggdb_cql.tables WHERE keyspace_name = {}",
                if *if_exists { "IF EXISTS " } else { "" },
                identifier(name),
                literal(name)
            ));
            (
                Outcome::SchemaChange {
                    change: "DROPPED",
                    target: "KEYSPACE",
                    keyspace: name.clone(),
                    name: None,
                },
                name.clone(),
                String::new(),
            )
        }
        Statement::CreateTable {
            table,
            if_not_exists,
            columns,
            partition,
            clustering,
            descending,
        } => {
            let keyspace = keyspace_of(table, current)?;
            create_table(
                builder,
                &keyspace,
                &table.table,
                *if_not_exists,
                &TableDefinition {
                    columns,
                    partition,
                    clustering,
                    descending,
                },
            )?;
            (
                Outcome::SchemaChange {
                    change: "CREATED",
                    target: "TABLE",
                    keyspace: keyspace.clone(),
                    name: Some(table.table.clone()),
                },
                keyspace,
                table.table.clone(),
            )
        }
        Statement::AlterTable { table, change } => {
            let keyspace = keyspace_of(table, current)?;
            let mut info = catalog.table(&keyspace, &table.table)?;
            let target = qualified(&keyspace, &table.table);
            let in_key = |info: &TableInfo, name: &str| {
                info.partition.iter().chain(&info.clustering).any(|key| key == name)
            };
            let mut sql = Vec::new();
            match change {
                crate::cql::AlterChange::Add(columns) => {
                    for column in columns {
                        if info.columns.iter().any(|(name, _)| *name == column.name) {
                            return Err(format!(
                                "Invalid column name {} because it conflicts with an existing column",
                                column.name
                            ));
                        }
                        sql.push(format!(
                            "ALTER TABLE {target} ADD COLUMN {} {}",
                            identifier(&column.name),
                            column.cql.sql()
                        ));
                        info.columns.push((column.name.clone(), column.cql));
                    }
                }
                crate::cql::AlterChange::Drop(names) => {
                    for name in names {
                        if in_key(&info, name) {
                            return Err(format!("Cannot drop PRIMARY KEY column {name}"));
                        }
                        if !info.columns.iter().any(|(known, _)| known == name) {
                            return Err(format!("Column {name} was not found in table {}", table.table));
                        }
                        sql.push(format!("ALTER TABLE {target} DROP COLUMN {}", identifier(name)));
                        info.columns.retain(|(known, _)| known != name);
                    }
                }
                crate::cql::AlterChange::Rename(pairs) => {
                    for (from, to) in pairs {
                        if !in_key(&info, from) {
                            return Err(format!(
                                "Cannot rename non PRIMARY KEY column {from}"
                            ));
                        }
                        if info.columns.iter().any(|(known, _)| known == to) {
                            return Err(format!(
                                "Cannot rename column {from} to {to} in table {}; another column has that name",
                                table.table
                            ));
                        }
                        sql.push(format!(
                            "ALTER TABLE {target} RENAME COLUMN {} TO {}",
                            identifier(from),
                            identifier(to)
                        ));
                        let renamed = |name: &mut String| {
                            if name == from {
                                *name = to.clone();
                            }
                        };
                        info.columns.iter_mut().for_each(|(name, _)| renamed(name));
                        info.partition.iter_mut().for_each(renamed);
                        info.clustering.iter_mut().for_each(renamed);
                        info.descending.iter_mut().for_each(renamed);
                    }
                }
                crate::cql::AlterChange::Options => {}
            }
            if !sql.is_empty() {
                let types: Vec<String> = info
                    .columns
                    .iter()
                    .map(|(name, cql)| format!("{name}\t{}", cql.name()))
                    .collect();
                sql.push(format!(
                    "{METADATA_DDL}; UPDATE eggdb_cql.tables SET partition_key = {}, clustering_key = {}, descending = {}, column_types = {} WHERE keyspace_name = {} AND table_name = {}",
                    literal(&info.partition.join("\n")),
                    literal(&info.clustering.join("\n")),
                    literal(&info.descending.join("\n")),
                    literal(&types.join("\n")),
                    literal(&keyspace),
                    literal(&table.table)
                ));
                builder.sql(&sql.join("; "));
            }
            (
                Outcome::SchemaChange {
                    change: "UPDATED",
                    target: "TABLE",
                    keyspace: keyspace.clone(),
                    name: Some(table.table.clone()),
                },
                keyspace,
                table.table.clone(),
            )
        }
        Statement::CreateIndex {
            name,
            table,
            column: indexed,
            if_not_exists,
        } => {
            let keyspace = keyspace_of(table, current)?;
            let info = catalog.table(&keyspace, &table.table)?;
            column(&info, indexed)?;
            let name = name
                .clone()
                .unwrap_or_else(|| format!("{}_{indexed}_idx", table.table));
            builder.sql(&format!(
                "CREATE INDEX {}{} ON {} ({})",
                if *if_not_exists { "IF NOT EXISTS " } else { "" },
                identifier(&name),
                qualified(&keyspace, &table.table),
                identifier(indexed)
            ));
            (
                Outcome::SchemaChange {
                    change: "UPDATED",
                    target: "TABLE",
                    keyspace: keyspace.clone(),
                    name: Some(table.table.clone()),
                },
                keyspace,
                table.table.clone(),
            )
        }
        Statement::DropIndex { index, if_exists } => {
            let keyspace = keyspace_of(index, current)?;
            builder.sql(&format!(
                "DROP INDEX {}{}",
                if *if_exists { "IF EXISTS " } else { "" },
                qualified(&keyspace, &index.table)
            ));
            (
                Outcome::SchemaChange {
                    change: "UPDATED",
                    target: "KEYSPACE",
                    keyspace: keyspace.clone(),
                    name: None,
                },
                keyspace,
                index.table.clone(),
            )
        }
        Statement::DropTable { table, if_exists } => {
            let keyspace = keyspace_of(table, current)?;
            builder.sql(&format!(
                "DROP TABLE {}{}; {METADATA_DDL}; DELETE FROM eggdb_cql.tables WHERE keyspace_name = {} AND table_name = {}",
                if *if_exists { "IF EXISTS " } else { "" },
                qualified(&keyspace, &table.table),
                literal(&keyspace),
                literal(&table.table)
            ));
            (
                Outcome::SchemaChange {
                    change: "DROPPED",
                    target: "TABLE",
                    keyspace: keyspace.clone(),
                    name: Some(table.table.clone()),
                },
                keyspace,
                table.table.clone(),
            )
        }
        Statement::Truncate(table) => {
            let keyspace = keyspace_of(table, current)?;
            catalog.table(&keyspace, &table.table)?;
            builder.sql(&format!("TRUNCATE {}", qualified(&keyspace, &table.table)));
            (Outcome::Void, keyspace, table.table.clone())
        }
        Statement::Insert {
            table,
            columns,
            values,
            if_not_exists,
        } => {
            let keyspace = keyspace_of(table, current)?;
            let info = catalog.table(&keyspace, &table.table)?;
            let mut keys = Vec::new();
            for key in info.primary_key() {
                let at = columns
                    .iter()
                    .position(|name| name == key)
                    .ok_or_else(|| format!("Some primary key parts are missing: {key}"))?;
                keys.push((columns[at].as_str(), &values[at]));
            }
            let assignments: Vec<(String, Assignment)> = columns
                .iter()
                .zip(values)
                .filter(|(name, _)| !info.primary_key().any(|key| key == *name))
                .map(|(name, term)| (name.clone(), Assignment::Set(term.clone())))
                .collect();
            let target = qualified(&keyspace, &table.table);
            if *if_not_exists {
                let mut entries: Vec<(&str, &Term)> = keys.clone();
                entries.extend(
                    assignments
                        .iter()
                        .map(|(name, assignment)| match assignment {
                            Assignment::Set(term)
                            | Assignment::Add(term)
                            | Assignment::Prepend(term)
                            | Assignment::Remove(term)
                            | Assignment::Element(_, term) => (name.as_str(), term),
                        }),
                );
                let names: Vec<String> = entries.iter().map(|(name, _)| identifier(name)).collect();
                builder.sql(&format!(
                    "INSERT INTO {target} ({}) VALUES (",
                    names.join(", ")
                ));
                for (at, (name, term)) in entries.iter().enumerate() {
                    if at > 0 {
                        builder.sql(", ");
                    }
                    builder.term(term, name, column(&info, name)?)?;
                }
                builder.sql(") ON CONFLICT DO NOTHING RETURNING TRUE");
                (
                    Outcome::Applied(keyspace.clone(), table.table.clone()),
                    keyspace,
                    table.table.clone(),
                )
            } else {
                upsert(builder, &info, &target, &keys, &assignments)?;
                (Outcome::Void, keyspace, table.table.clone())
            }
        }
        Statement::Update {
            table,
            assignments,
            relations: conditions,
            condition,
        } => {
            let keyspace = keyspace_of(table, current)?;
            let info = catalog.table(&keyspace, &table.table)?;
            for (name, _) in assignments {
                if info.primary_key().any(|key| key == name) {
                    return Err(format!("PRIMARY KEY part {name} found in SET part"));
                }
                column(&info, name)?;
            }
            let keys = key_terms(&info, conditions)?;
            let target = qualified(&keyspace, &table.table);
            let Some(condition) = condition else {
                upsert(builder, &info, &target, &keys, assignments)?;
                return Ok((Outcome::Void, keyspace, table.table.clone()));
            };
            builder.sql(&format!("UPDATE {target} SET "));
            for (at, (name, assignment)) in assignments.iter().enumerate() {
                if at > 0 {
                    builder.sql(", ");
                }
                let cql = column(&info, name)?;
                builder.sql(&format!("{} = ", identifier(name)));
                assigned_value(builder, "", name, cql, assignment, false)?;
            }
            relations(builder, &info, conditions)?;
            if let Condition::Columns(checks) = condition {
                relations_after(builder, &info, checks, " AND ")?;
            }
            builder.sql(" RETURNING 1");
            (
                Outcome::Applied(keyspace.clone(), table.table.clone()),
                keyspace,
                table.table.clone(),
            )
        }
        Statement::Delete {
            table,
            columns,
            elements,
            relations: conditions,
            condition,
        } => {
            let keyspace = keyspace_of(table, current)?;
            let info = catalog.table(&keyspace, &table.table)?;
            let target = qualified(&keyspace, &table.table);
            if columns.is_empty() && elements.is_empty() {
                builder.sql(&format!("DELETE FROM {target}"));
            } else {
                builder.sql(&format!("UPDATE {target} SET "));
                let names = columns.iter().chain(elements.iter().map(|(name, _)| name));
                for name in names {
                    column(&info, name)?;
                    if info.primary_key().any(|key| key == name) {
                        return Err(format!(
                            "Invalid identifier {name} for deletion (should not be a PRIMARY KEY part)"
                        ));
                    }
                }
                let mut first = true;
                for name in columns {
                    if !first {
                        builder.sql(", ");
                    }
                    first = false;
                    builder.sql(&format!("{} = NULL", identifier(name)));
                }
                for (name, element) in elements {
                    if !first {
                        builder.sql(", ");
                    }
                    first = false;
                    deleted_element(builder, name, column(&info, name)?, element)?;
                }
            }
            relations(builder, &info, conditions)?;
            match condition {
                None => (Outcome::Void, keyspace, table.table.clone()),
                Some(condition) => {
                    key_terms(&info, conditions)?;
                    if let Condition::Columns(checks) = condition {
                        relations_after(builder, &info, checks, " AND ")?;
                    }
                    builder.sql(" RETURNING 1");
                    (
                        Outcome::Applied(keyspace.clone(), table.table.clone()),
                        keyspace,
                        table.table.clone(),
                    )
                }
            }
        }
        Statement::Select {
            table,
            selection,
            relations: conditions,
            order,
            limit,
        } => {
            let keyspace = keyspace_of(table, current)?;
            let info = catalog.table(&keyspace, &table.table)?;
            select(builder, &info, selection, conditions, order, limit.as_ref())?;
            (Outcome::Rows(Box::new(info)), keyspace, table.table.clone())
        }
        Statement::Batch(statements) => {
            builder.sql("BEGIN; ");
            let mut last = (String::new(), String::new());
            for child in statements {
                let (_, keyspace, table) = self::statement(builder, child, current, catalog)?;
                builder.sql("; ");
                last = (keyspace, table);
            }
            builder.sql("COMMIT");
            (Outcome::Void, last.0, last.1)
        }
    })
}

/// Translates one CQL statement with `markers` bind markers into SQL in the PostgreSQL dialect.
pub(crate) fn translate(
    cql: &Statement,
    markers: usize,
    current: Option<&str>,
    catalog: &mut dyn Catalog,
) -> Result<Translation, String> {
    let mut builder = Builder {
        pieces: Vec::new(),
        markers: vec![None; markers],
    };
    let (outcome, keyspace, table) = statement(&mut builder, cql, current, catalog)?;
    let markers = builder
        .markers
        .into_iter()
        .map(|marker| {
            marker.unwrap_or(Marker {
                name: String::new(),
                cql: CqlType::Varchar,
            })
        })
        .collect();
    Ok(Translation {
        pieces: builder.pieces,
        markers,
        outcome,
        keyspace,
        table,
    })
}
