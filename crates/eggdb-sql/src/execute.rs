use crate::aggregate::{self, shape_of};
use crate::ast::{
    BinaryOperator, ConstraintKind, CreateTable, Delete, Expr, ExprKind, Insert, InsertSource,
    InsertValue, Name, OnDuplicate, OrderItem, Overriding, Select, SelectItem, TableReference,
    Update,
};
use crate::constraints::{self, PreparedChecks, RowChange};
use crate::dialect::Dialect;
use crate::encoding::{
    Column, IdentityGeneration, IdentitySeed, MAX_COLUMNS, TableDefinition, ViewDefinition, decode_meta,
    decode_row, decode_row_into,
    encode_key_value, encode_meta, encode_row, encode_row_into, is_view_record, meta_key,
    prefix_end, rows_prefix, table_key,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{
    Evaluator, Scope, Source, check_columns, children_mut, compare_truth, output_name,
    reject_aggregates,
};
use crate::indexes;
use crate::join::{self, JoinedRows, RowCountHints};
use crate::lexer::tokenize_in;
use crate::memory::WorkMemory;
use crate::mysql_errors::Warning;
use crate::parser::{LiteralRows, LiteralToken, Parser};
use crate::plan::{self, Access, OrderRequest, Path};
use crate::session::{OutputColumn, QueryResult};
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
use crate::subquery::Correlation;
use crate::triggers::{Firing, RowOutcome, TriggerEvent};
use crate::types::{Conversion, SqlType, Value, compare_values, convert, equality_key};
use crate::typing;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const DELETE_BATCH: usize = 1024;

pub(crate) fn undefined_table(name: &Name) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_TABLE,
        format!("relation \"{}\" does not exist", name.text),
    )
    .at(name.position)
}

const CACHED_TABLES: usize = 256;

struct CachedTable {
    bytes: Vec<u8>,
    relation: Relation,
    checked_at: Option<u64>,
    needs_catalog: bool,
    needs_user_types: bool,
}

fn stored_expressions_need_user_types(relation: &Relation) -> bool {
    match relation {
        Relation::Table(table) => crate::user_types::table_needs_registry(table),
        Relation::View(_) => true,
    }
}

pub(crate) fn column_storage(sql_type: SqlType, position: usize) -> SqlResult<(SqlType, Option<u32>)> {
    match sql_type {
        SqlType::Defined(defined) if defined.kind == crate::types::DefinedKind::Domain => {
            let base = crate::user_types::domain_base(defined.oid).ok_or_else(|| {
                SqlError::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!("type with OID {} does not exist", defined.oid),
                )
                .at(position)
            })?;
            Ok((base, Some(defined.oid)))
        }
        SqlType::Defined(defined) if defined.oid == crate::records::RECORD_OID => Err(SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            "column cannot be of type record",
        )
        .at(position)),
        SqlType::Array(element) if !element.is_storable() => {
            Err(crate::arrays::unsupported_element(element.sql_type()).at(position))
        }
        other => Ok((other, None)),
    }
}

fn stored_expressions_need_catalog(relation: &Relation) -> bool {
    match relation {
        Relation::Table(table) => table_needs_catalog(table),
        Relation::View(view) => crate::pg_catalog::text_needs_catalog(&view.query),
    }
}

pub(crate) fn table_needs_catalog(table: &TableDefinition) -> bool {
    table
        .columns
        .iter()
        .filter_map(|column| column.default.as_deref())
        .chain(table.checks.iter().map(|check| check.expression.as_str()))
        .any(crate::pg_catalog::text_needs_catalog)
}

#[derive(Clone)]
pub(crate) enum Relation {
    Table(Arc<TableDefinition>),
    View(Arc<ViewDefinition>),
}

#[derive(Default)]
pub(crate) struct TableCache {
    entries: HashMap<String, CachedTable>,
    row_counts: RowCountHints,
    pub(crate) triggers: crate::triggers::TriggerCache,
    pub(crate) routines: crate::routines::RoutineCache,
}

impl TableCache {
    pub(crate) fn relation(
        &mut self,
        reader: &dyn KeyValueRead,
        name: &str,
    ) -> SqlResult<Option<Relation>> {
        crate::pg_catalog::load_if_wanted(reader)?;
        if crate::temp_tables::is_temp_table(name) {
            let Some((definition, needs_catalog)) =
                crate::temp_tables::temp_definition_and_catalog_need(name)
            else {
                return Ok(None);
            };
            if needs_catalog {
                crate::pg_catalog::load_for_stored_expressions(reader)?;
            }
            return Ok(Some(Relation::Table(definition)));
        }
        let version = reader.committed_version();
        if let Some(cached) = self.entries.get(name)
            && version.is_some()
            && cached.checked_at == version
        {
            return Self::loaded(reader, cached);
        }
        let Some(bytes) = reader.get(&table_key(name))? else {
            return Ok(None);
        };
        if let Some(cached) = self.entries.get_mut(name)
            && cached.bytes == bytes
        {
            cached.checked_at = version;
            return Self::loaded(reader, cached);
        }
        let relation = if is_view_record(&bytes) {
            Relation::View(Arc::new(ViewDefinition::decode(name, &bytes)?))
        } else {
            Relation::Table(Arc::new(TableDefinition::decode(name, &bytes)?))
        };
        if self.entries.len() >= CACHED_TABLES {
            self.entries.clear();
        }
        let cached = CachedTable {
            bytes,
            needs_catalog: stored_expressions_need_catalog(&relation),
            needs_user_types: stored_expressions_need_user_types(&relation),
            relation,
            checked_at: version,
        };
        let found = Self::loaded(reader, &cached);
        self.entries.insert(name.to_owned(), cached);
        found
    }

    fn loaded(reader: &dyn KeyValueRead, cached: &CachedTable) -> SqlResult<Option<Relation>> {
        if cached.needs_catalog {
            crate::pg_catalog::load_for_stored_expressions(reader)?;
        }
        if cached.needs_user_types {
            crate::user_types::ensure_loaded(reader)?;
        }
        Ok(Some(cached.relation.clone()))
    }

    pub(crate) fn table(
        &mut self,
        reader: &dyn KeyValueRead,
        name: &str,
    ) -> SqlResult<Option<Arc<TableDefinition>>> {
        match self.relation(reader, name)? {
            Some(Relation::Table(table)) => Ok(Some(table)),
            Some(Relation::View(_)) => Err(crate::views::not_a_table(name)),
            None => Ok(None),
        }
    }

    pub(crate) fn require_target(
        &mut self,
        reader: &dyn KeyValueRead,
        name: &Name,
        verb: &str,
    ) -> SqlResult<Arc<TableDefinition>> {
        match self
            .relation(reader, &name.text)
            .map_err(|error| error.at_if_unplaced(name.position))?
        {
            Some(Relation::Table(table)) => Ok(table),
            Some(Relation::View(_)) => {
                Err(crate::views::not_updatable(&name.text, verb).at(name.position))
            }
            None => Err(undefined_table(name)),
        }
    }

    pub(crate) fn require(
        &mut self,
        reader: &dyn KeyValueRead,
        name: &Name,
    ) -> SqlResult<Arc<TableDefinition>> {
        self.table(reader, &name.text)
            .map_err(|error| error.at_if_unplaced(name.position))?
            .ok_or_else(|| undefined_table(name))
    }
}

fn scope_of<'a>(table: &'a TableDefinition, reference: &'a TableReference) -> Scope<'a> {
    Scope::One(Source {
        table,
        label: reference.alias.as_deref().unwrap_or(&table.name),
        offset: 0,
    })
}

pub(crate) fn create_table(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    create: &CreateTable,
    dialect: Dialect,
) -> SqlResult<QueryResult> {
    if create.columns.len() > MAX_COLUMNS {
        return Err(SqlError::new(
            SqlState::TOO_MANY_COLUMNS,
            format!("tables can have at most {MAX_COLUMNS} columns"),
        ));
    }
    constraints::check_identifier(&create.name)?;
    let mut columns: Vec<Column> = Vec::new();
    let mut domains: Vec<crate::encoding::ColumnDomain> = Vec::new();
    let mut primary_key = Vec::new();
    let mut primary_key_position = None;
    for (index, definition) in create.columns.iter().enumerate() {
        constraints::check_identifier(&definition.name)?;
        if let Some((_, text)) = &definition.default {
            crate::sequences::check_default_sequences(writer, text)
                .map_err(|error| error.at_if_unplaced(definition.name.position))?;
        }
        if columns
            .iter()
            .any(|column| column.name == definition.name.text)
        {
            return Err(SqlError::new(
                SqlState::DUPLICATE_COLUMN,
                format!(
                    "column \"{}\" specified more than once",
                    definition.name.text
                ),
            )
            .at(definition.name.position));
        }
        if let Some((expr, _)) = &definition.default {
            check_default(expr)?;
        }
        if definition.primary_key {
            if primary_key_position.is_some() {
                return Err(multiple_primary_keys(create, definition.name.position));
            }
            primary_key_position = Some(definition.name.position);
            primary_key.push(index);
        }
        let (sql_type, domain) = column_storage(definition.sql_type, definition.name.position)?;
        if let Some(domain) = domain {
            domains.push(crate::encoding::ColumnDomain {
                column: index,
                oid: domain,
            });
        }
        columns.push(Column {
            name: definition.name.text.clone(),
            sql_type,
            not_null: definition.not_null,
            default: definition.default.as_ref().map(|(_, text)| text.clone()),
            auto_increment: definition.auto_increment,
        });
    }
    if let Some((names, position)) = &create.primary_key {
        if primary_key_position.is_some() {
            return Err(multiple_primary_keys(create, *position));
        }
        for name in names {
            let Some(index) = columns.iter().position(|column| column.name == name.text) else {
                return Err(SqlError::new(
                    SqlState::UNDEFINED_COLUMN,
                    format!("column \"{}\" named in key does not exist", name.text),
                )
                .at(name.position));
            };
            if !primary_key.contains(&index) {
                primary_key.push(index);
            }
        }
    }
    for &index in &primary_key {
        columns[index].not_null = true;
        let position = create
            .columns
            .get(index)
            .map_or(create.name.position, |column| column.name.position);
        indexes::check_orderable(columns[index].sql_type, position)?;
    }
    check_auto_increment(create, &columns, &primary_key)?;
    if crate::sequences::relation_exists(writer, &create.name.text)? {
        if create.if_not_exists {
            return Ok(QueryResult::command("CREATE TABLE").with_notice(format!(
                "NOTICE:  relation \"{}\" already exists, skipping",
                create.name.text
            )));
        }
        return Err(SqlError::new(
            SqlState::DUPLICATE_TABLE,
            format!("relation \"{}\" already exists", create.name.text),
        )
        .at(create.name.position));
    }
    let temporary = crate::temporary::is_temporary_name(&create.name.text);
    let (next_id, following) = if temporary {
        (crate::temporary::next_id(writer)?, None)
    } else {
        let next_id = match writer.get(&meta_key())? {
            Some(bytes) => decode_meta(&bytes)?,
            None => 1,
        };
        let following = next_id.checked_add(1).ok_or_else(|| {
            SqlError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                "the database has used every table identifier",
            )
        })?;
        (next_id, Some(following))
    };
    if crate::user_types::load(writer)?
        .named(crate::schemas::normalized(&create.name.text))
        .is_some()
    {
        return Err(SqlError::new(
            SqlState::DUPLICATE_OBJECT,
            format!(
                "type \"{}\" already exists",
                crate::schemas::base_name(&create.name.text)
            ),
        )
        .with_hint("A relation has an associated type of the same name, so you must use a name that doesn't conflict with any existing type.")
        .at(create.name.position));
    }
    let mut table = TableDefinition {
        id: next_id,
        name: create.name.text.clone(),
        columns,
        primary_key,
        domains,
        ..TableDefinition::default()
    };
    for (index, definition) in create.columns.iter().enumerate() {
        if definition.generated_always && definition.auto_increment {
            table.set_identity(index, Some(IdentityGeneration::Always));
        }
        if definition.auto_increment {
            table.identity_seed = definition.identity_seed;
        } else if definition.default.is_some() {
            table.set_default_dialect(index, definition.default_dialect);
        }
    }
    let specs = unique_columns(create).chain(create.indexes.iter().cloned());
    let mut others = None;
    for spec in specs {
        if others.is_none() {
            others = Some(indexes::every_table(writer)?);
        }
        indexes::define_index(
            others.as_deref().unwrap_or(&[]),
            &mut table,
            &spec,
            dialect,
            spec.unique,
        )?;
    }
    if let Some(name) = &create.primary_key_name
        && !table.primary_key.is_empty()
        && *name.text != table.primary_key_constraint_name()
    {
        if table.constraint_name_taken(&name.text) {
            return Err(SqlError::new(
                SqlState::DUPLICATE_TABLE,
                format!("relation \"{}\" already exists", name.text),
            )
            .at(name.position));
        }
        table.primary_key_name = Some(name.text.clone());
    }
    let specs = create
        .columns
        .iter()
        .flat_map(|column| column.constraints.iter())
        .chain(create.constraints.iter());
    let mut foreign_keys = Vec::new();
    for spec in specs {
        match &spec.kind {
            ConstraintKind::Check { expr, text } => {
                constraints::define_check(&mut table, spec, expr, text, dialect)?;
            }
            ConstraintKind::ForeignKey(key) => foreign_keys.push((spec, key)),
            ConstraintKind::PrimaryKey(_) | ConstraintKind::Unique(_) => {}
        }
    }
    for (spec, key) in foreign_keys {
        if temporary != crate::temporary::is_temporary_name(&key.parent.text) {
            return Err(SqlError::new(
                SqlState::INVALID_TABLE_DEFINITION,
                if temporary {
                    "constraints on temporary tables may reference only temporary tables"
                } else {
                    "constraints on permanent tables may reference only permanent tables"
                },
            )
            .at(spec.position));
        }
        constraints::define_foreign_key(writer, tables, &mut table, spec, key, dialect)?;
    }
    table.referenced = !constraints::referencing_tables(writer, &table.name)?.is_empty();
    if let Some(following) = following {
        writer.put(&meta_key(), &encode_meta(following))?;
    }
    if temporary {
        crate::temporary::record_action(
            writer,
            &table.name,
            create
                .temporary
                .unwrap_or(crate::ast::OnCommit::PreserveRows),
        )?;
    }
    crate::comments::forget_owner(writer, &table.name)?;
    crate::triggers::remove_table_triggers(writer, &table.name)?;
    crate::comments::store_created(writer, &table.name, create)?;
    writer.put(&table_key(&table.name), &table.encode())?;
    constraints::register_references(writer, tables, &table)?;
    Ok(QueryResult::command("CREATE TABLE"))
}

fn unique_columns(create: &CreateTable) -> impl Iterator<Item = crate::ast::IndexSpec> + '_ {
    create.columns.iter().filter_map(|column| {
        column.unique.as_ref().map(|name| crate::ast::IndexSpec {
            name: name.clone(),
            unique: true,
            columns: vec![column.name.clone()],
            position: column.name.position,
            method: crate::encoding::IndexMethod::BTree,
            sort_options: crate::encoding::SortOptions::default(),
        })
    })
}

fn check_auto_increment(
    create: &CreateTable,
    columns: &[Column],
    primary_key: &[usize],
) -> SqlResult<()> {
    let auto: Vec<usize> = (0..columns.len())
        .filter(|&index| columns[index].auto_increment)
        .collect();
    let Some(&first) = auto.first() else {
        return Ok(());
    };
    let integer = matches!(
        columns[first].sql_type,
        SqlType::SmallInt | SqlType::Integer | SqlType::BigInt
    );
    if auto.len() > 1
        || primary_key.first() != Some(&first)
        || !integer
        || columns[first].default.is_some()
    {
        return Err(SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            "Incorrect table definition; there can be only one auto column and it must be defined as a key",
        )
        .with_hint("EggDB needs the AUTO_INCREMENT column to be an integer, without a DEFAULT, and the first column of the primary key.")
        .at(create.columns[first].name.position));
    }
    Ok(())
}

fn multiple_primary_keys(create: &CreateTable, position: usize) -> SqlError {
    SqlError::new(
        SqlState::INVALID_TABLE_DEFINITION,
        format!(
            "multiple primary keys for table \"{}\" are not allowed",
            create.name.text
        ),
    )
    .at(position)
}

pub(crate) fn check_default_expr(expr: &Expr) -> SqlResult<()> {
    check_default(expr)
}

fn check_default(expr: &Expr) -> SqlResult<()> {
    reject_aggregates(expr, "DEFAULT expressions")?;
    if crate::subquery::mentions_subquery(expr) {
        return Err(
            SqlError::not_supported("cannot use subquery in DEFAULT expression").at(expr.position),
        );
    }
    check_columns(expr, Scope::empty()).map_err(|error| {
        if error.state() == SqlState::UNDEFINED_COLUMN || error.state() == SqlState::UNDEFINED_TABLE
        {
            SqlError::not_supported("cannot use column reference in DEFAULT expression")
                .at(expr.position)
        } else {
            error
        }
    })
}

pub(crate) fn drop_tables(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    names: &[Name],
    if_exists: bool,
    cascade: bool,
    dialect: Dialect,
) -> SqlResult<QueryResult> {
    let mut result = QueryResult::command("DROP TABLE");
    for name in names {
        let Some(dropped) = tables.table(writer, &name.text)? else {
            continue;
        };
        if dialect == Dialect::PostgreSql {
            crate::views::drop_dependent_views(
                writer,
                tables,
                &dropped.name,
                "table",
                cascade,
                &mut result,
            )?;
        }
        if dropped.owns_sequence {
            result = release_owned_sequences(writer, tables, &dropped, names, cascade, result)?;
        }
        for child in constraints::referencing_tables(writer, &name.text)? {
            if child == name.text || names.iter().any(|dropped| dropped.text == child) {
                continue;
            }
            let Some(definition) = tables.table(writer, &child)? else {
                continue;
            };
            let mut definition = (*definition).clone();
            let dependent: Vec<String> = definition
                .foreign_keys
                .iter()
                .filter(|key| key.parent == name.text)
                .map(|key| key.name.clone())
                .collect();
            if dependent.is_empty() || dialect == Dialect::Sqlite {
                continue;
            }
            if !cascade {
                return Err(dependent_table(&name.text, &child, &dependent[0], dialect));
            }
            constraints::unregister_references(writer, tables, &definition)?;
            definition
                .foreign_keys
                .retain(|key| key.parent != name.text);
            writer.put(&table_key(&definition.name), &definition.encode())?;
            constraints::register_references(writer, tables, &definition)?;
            for constraint in &dependent {
                result = result.with_notice(format!(
                    "NOTICE:  drop cascades to constraint {constraint} on table {child}"
                ));
            }
        }
    }
    for name in names {
        let Some(table) = tables.table(writer, &name.text)? else {
            if if_exists {
                result = result.with_notice(format!(
                    "NOTICE:  table \"{}\" does not exist, skipping",
                    name.text
                ));
                continue;
            }
            return Err(undefined_table(name));
        };
        let prefix = rows_prefix(table.id);
        let end = prefix_end(&prefix);
        loop {
            let mut keys = Vec::new();
            writer.scan(&prefix, &end, ScanDirection::Forward, &mut |key, _| {
                keys.push(key.to_vec());
                Ok(keys.len() < DELETE_BATCH)
            })?;
            if keys.is_empty() {
                break;
            }
            for key in &keys {
                writer.delete(key)?;
            }
        }
        indexes::delete_every_index_entry(writer, &table)?;
        constraints::unregister_references(writer, tables, &table)?;
        crate::sequences::drop_owned_by(writer, tables, &table.name, None)?;
        crate::comments::forget_owner(writer, &table.name)?;
        crate::triggers::remove_table_triggers(writer, &table.name)?;
        writer.delete(&table_key(&table.name))?;
    }
    Ok(result)
}

#[cold]
#[inline(never)]
fn release_owned_sequences(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    dropped: &TableDefinition,
    names: &[Name],
    cascade: bool,
    mut result: QueryResult,
) -> SqlResult<QueryResult> {
    for (sequence, definition) in crate::sequences::every_sequence(writer)? {
        if definition
            .owned_by
            .as_ref()
            .is_none_or(|(owner, _)| *owner != dropped.name)
        {
            continue;
        }
        for (table, column) in crate::sequences::every_default_referring(writer, &sequence)? {
            if names.iter().any(|name| name.text == table) {
                continue;
            }
            if !cascade {
                return Err(SqlError::new(
                    SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop table {} because other objects depend on it",
                        dropped.name
                    ),
                )
                .with_detail(format!(
                    "default value for column {column} of table {table} depends on sequence {sequence}"
                ))
                .with_hint("Use DROP ... CASCADE to drop the dependent objects too."));
            }
            if let Some(user) = tables.table(writer, &table)? {
                let mut user = (*user).clone();
                if let Some(index) = user.column_index(&column) {
                    user.columns[index].default = None;
                }
                writer.put(&table_key(&user.name), &user.encode())?;
                result = result.with_notice(format!(
                    "NOTICE:  drop cascades to default value for column {column} of table {table}"
                ));
            }
        }
    }
    Ok(result)
}

pub(crate) fn dependent_table(
    parent: &str,
    child: &str,
    constraint: &str,
    dialect: Dialect,
) -> SqlError {
    match dialect {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_GENERAL,
            format!(
                "Cannot drop table '{parent}' referenced by a foreign key constraint '{constraint}' on table '{child}'."
            ),
        )
        .with_vendor_code(3730),
        Dialect::Oracle => SqlError::new(
            SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
            "unique/primary keys in table referenced by foreign keys",
        )
        .with_oracle_code(2449),
        _ => SqlError::new(
            SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
            format!("cannot drop table {parent} because other objects depend on it"),
        )
        .with_detail(format!(
            "constraint {constraint} on table {child} depends on table {parent}"
        ))
        .with_hint("Use DROP ... CASCADE to drop the dependent objects too."),
    }
}

pub(crate) fn row_key(table: &TableDefinition, row: &[Value]) -> Vec<u8> {
    let mut key = rows_prefix(table.id);
    for &index in &table.primary_key {
        encode_key_value(&mut key, &row[index]);
    }
    key
}

fn next_hidden_row_id(reader: &dyn KeyValueRead, table: &TableDefinition) -> SqlResult<u64> {
    let prefix = rows_prefix(table.id);
    let mut last = None;
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Backward,
        &mut |key, _| {
            last = Some(key.to_vec());
            Ok(false)
        },
    )?;
    let Some(key) = last else {
        return Ok(1);
    };
    let suffix: [u8; 8] = key
        .get(prefix.len()..)
        .and_then(|suffix| suffix.try_into().ok())
        .ok_or_else(|| crate::encoding::corrupt("row key"))?;
    u64::from_be_bytes(suffix)
        .checked_add(1)
        .ok_or_else(|| row_identifiers_used(table))
}

fn row_identifiers_used(table: &TableDefinition) -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("table \"{}\" has used every row identifier", table.name),
    )
}

fn default_expression(column: &Column, dialect: Dialect) -> SqlResult<Option<Expr>> {
    let Some(text) = &column.default else {
        return Ok(None);
    };
    let tokens = if dialect == Dialect::PostgreSql {
        tokenize_in(text, dialect)?
    } else {
        tokenize_in(text, dialect).map_err(|_| crate::encoding::corrupt("column default"))?
    };
    let mut parser = Parser::new(text, &tokens).in_dialect(dialect);
    let expr = if dialect == Dialect::PostgreSql {
        parser.expression()?
    } else {
        parser
            .expression()
            .map_err(|_| crate::encoding::corrupt("column default"))?
    };
    if !parser.at_end() {
        return Err(crate::encoding::corrupt("column default"));
    }
    Ok(Some(expr))
}

fn evaluated_default(expr: Option<&Expr>, column: &Column, dialect: Dialect) -> SqlResult<Value> {
    let Some(expr) = expr else {
        return Ok(Value::Null);
    };
    let evaluator = Evaluator {
        scope: Scope::empty(),
        row: &[],
        correlation: None,
    };
    let value = evaluator
        .eval(expr, Some(column.sql_type))
        .map_err(|error| SqlError::new(error.state(), error.message().to_owned()))?;
    match dialect {
        Dialect::PostgreSql => assign(value, column),
        written_in => assignment_rule(written_in)(value, column),
    }
}

fn with_domain_default(table: &TableDefinition, index: usize, column: &Column) -> Option<Column> {
    if column.default.is_some() || column.auto_increment {
        return None;
    }
    let domain = table.domain_of(index)?;
    let default = crate::user_types::domain_default(domain)?;
    Some(Column {
        default: Some(default),
        ..column.clone()
    })
}

fn default_value(table: &TableDefinition, index: usize) -> SqlResult<Value> {
    let Some(column) = table.columns.get(index) else {
        return Ok(Value::Null);
    };
    let dialect = table.default_dialect(index);
    let domain_default = with_domain_default(table, index, column);
    let column = domain_default.as_ref().unwrap_or(column);
    evaluated_default(
        default_expression(column, dialect)?.as_ref(),
        column,
        dialect,
    )
}

pub(crate) struct ColumnDefaults {
    expressions: Vec<Option<Expr>>,
    dialects: Vec<Dialect>,
}

impl ColumnDefaults {
    pub(crate) fn of(table: &TableDefinition) -> SqlResult<Self> {
        let dialects: Vec<Dialect> = (0..table.columns.len())
            .map(|index| table.default_dialect(index))
            .collect();
        Ok(Self {
            expressions: table
                .columns
                .iter()
                .enumerate()
                .zip(&dialects)
                .map(|((index, column), &dialect)| {
                    match with_domain_default(table, index, column) {
                        Some(defaulted) => default_expression(&defaulted, dialect),
                        None => default_expression(column, dialect),
                    }
                })
                .collect::<SqlResult<_>>()?,
            dialects,
        })
    }

    pub(crate) fn value(&self, index: usize, column: &Column) -> SqlResult<Value> {
        evaluated_default(
            self.expressions.get(index).and_then(Option::as_ref),
            column,
            self.dialects
                .get(index)
                .copied()
                .unwrap_or(Dialect::PostgreSql),
        )
    }
}

fn assign(value: Value, column: &Column) -> SqlResult<Value> {
    let source = value.sql_type();
    convert(value, column.sql_type, Conversion::Assignment).map_err(|error| {
        let source = source.map_or_else(|| "unknown".to_owned(), SqlType::name);
        if error.state() == SqlState::CANNOT_COERCE {
            SqlError::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "column \"{}\" is of type {} but expression is of type {source}",
                    column.name,
                    column.sql_type.name()
                ),
            )
            .with_hint("You will need to rewrite or cast the expression.")
        } else {
            error
        }
    })
}

type Assignment = fn(Value, &Column) -> SqlResult<Value>;

pub(crate) fn assignment_rule(dialect: Dialect) -> Assignment {
    match dialect {
        Dialect::SqlServer => crate::sqlserver_conversion::assign,
        Dialect::Oracle => crate::oracle_conversion::assign,
        _ => assign,
    }
}

fn assigned_value(
    evaluator: &Evaluator<'_>,
    value: &InsertValue,
    table: &TableDefinition,
    index: usize,
    rule: Assignment,
) -> SqlResult<Value> {
    let column = &table.columns[index];
    match value {
        InsertValue::Default => default_value(table, index),
        InsertValue::Expr(expr) => {
            let evaluated = evaluator.eval(expr, Some(column.sql_type))?;
            rule(evaluated, column).map_err(|error| error.at_if_unplaced(expr.position))
        }
    }
}

fn check_not_null(table: &TableDefinition, row: &[Value]) -> SqlResult<()> {
    for (column, value) in table.columns.iter().zip(row) {
        if column.not_null && value.is_null() {
            let shown: Vec<String> = row
                .iter()
                .map(|value| value.to_text().unwrap_or_else(|| "null".into()))
                .collect();
            return Err(SqlError::new(
                SqlState::NOT_NULL_VIOLATION,
                format!(
                    "null value in column \"{}\" of relation \"{}\" violates not-null constraint",
                    column.name, table.name
                ),
            )
            .with_detail(format!("Failing row contains ({}).", shown.join(", "))));
        }
    }
    Ok(())
}

fn duplicate_key(table: &TableDefinition, row: &[Value]) -> SqlError {
    let names: Vec<&str> = table
        .primary_key
        .iter()
        .map(|&index| table.columns[index].name.as_str())
        .collect();
    let values: Vec<String> = table
        .primary_key
        .iter()
        .map(|&index| row[index].to_text().unwrap_or_else(|| "null".into()))
        .collect();
    SqlError::new(
        SqlState::UNIQUE_VIOLATION,
        format!(
            "duplicate key value violates unique constraint \"{}\"",
            table.primary_key_constraint_name()
        ),
    )
    .with_detail(format!(
        "Key ({})=({}) already exists.",
        names.join(", "),
        values.join(", ")
    ))
    .with_table(&table.name)
}

pub(crate) fn target_columns(table: &TableDefinition, names: &[Name]) -> SqlResult<Vec<usize>> {
    let mut indexes = Vec::with_capacity(names.len());
    for name in names {
        let index = table.column_index(&name.text).ok_or_else(|| {
            SqlError::new(
                SqlState::UNDEFINED_COLUMN,
                format!(
                    "column \"{}\" of relation \"{}\" does not exist",
                    name.text, table.name
                ),
            )
            .at(name.position)
        })?;
        if indexes.contains(&index) {
            return Err(SqlError::new(
                SqlState::DUPLICATE_COLUMN,
                format!("column \"{}\" specified more than once", name.text),
            )
            .at(name.position));
        }
        indexes.push(index);
    }
    Ok(indexes)
}

fn returning_exprs(items: &mut Option<Vec<SelectItem>>) -> impl Iterator<Item = &mut Expr> {
    items.iter_mut().flatten().filter_map(|item| match item {
        SelectItem::Expr { expr, .. } => Some(expr),
        SelectItem::Wildcard { .. } => None,
    })
}

fn returning_mentions_subquery(items: &Option<Vec<SelectItem>>) -> bool {
    items.iter().flatten().any(|item| match item {
        SelectItem::Expr { expr, .. } => crate::subquery::mentions_subquery(expr),
        SelectItem::Wildcard { .. } => false,
    })
}

fn value_mentions_subquery(value: &InsertValue) -> bool {
    matches!(value, InsertValue::Expr(expr) if crate::subquery::mentions_subquery(expr))
}

fn insert_mentions_subquery(insert: &Insert) -> bool {
    let values = match &insert.source {
        InsertSource::Values(rows) => rows.iter().flatten().any(value_mentions_subquery),
        InsertSource::Query(_) | InsertSource::DefaultValues => false,
    };
    values || returning_mentions_subquery(&insert.returning)
}

fn update_mentions_subquery(update: &Update) -> bool {
    update
        .assignments
        .iter()
        .any(|(_, value)| value_mentions_subquery(value))
        || update.filter.iter().any(crate::subquery::mentions_subquery)
        || returning_mentions_subquery(&update.returning)
}

pub(crate) fn insert_exprs(insert: &mut Insert) -> Vec<&mut Expr> {
    let mut exprs: Vec<&mut Expr> = Vec::new();
    if let InsertSource::Values(rows) = &mut insert.source {
        for value in rows.iter_mut().flatten() {
            if let InsertValue::Expr(expr) = value {
                exprs.push(expr);
            }
        }
    }
    exprs.extend(returning_exprs(&mut insert.returning));
    exprs
}

pub(crate) fn update_exprs(update: &mut Update) -> Vec<&mut Expr> {
    let mut exprs: Vec<&mut Expr> = Vec::new();
    for (_, value) in &mut update.assignments {
        if let InsertValue::Expr(expr) = value {
            exprs.push(expr);
        }
    }
    exprs.extend(update.filter.iter_mut());
    exprs.extend(returning_exprs(&mut update.returning));
    exprs
}

pub(crate) fn delete_exprs(delete: &mut Delete) -> Vec<&mut Expr> {
    let mut exprs: Vec<&mut Expr> = delete.filter.iter_mut().collect();
    exprs.extend(returning_exprs(&mut delete.returning));
    exprs
}

fn with_subqueries_run<T: Clone>(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    statement: &T,
    target: &TableReference,
    values_only: bool,
    exprs: fn(&mut T) -> Vec<&mut Expr>,
) -> SqlResult<(T, crate::subquery::RowSubqueries)> {
    let mut copy = statement.clone();
    let found = exprs(&mut copy);
    let table = tables.require(reader, &target.name)?;
    let scope = if values_only {
        Scope::empty()
    } else {
        scope_of(&table, target)
    };
    let subqueries = crate::subquery::prepare_in_place(reader, tables, found, scope)?;
    Ok((copy, subqueries))
}

fn rows_being_changed(position: usize) -> SqlError {
    SqlError::not_supported("a subquery that refers to the rows being changed is not supported yet")
        .with_hint("Compute the values with a SELECT first, or use a subquery that stands alone.")
        .at(position)
}

fn joined_conjuncts(mut parts: Vec<Expr>, position: usize) -> Option<Expr> {
    match parts.len() {
        0 => None,
        1 => parts.pop(),
        _ => Some(Expr {
            kind: ExprKind::And(parts),
            position,
        }),
    }
}

fn split_row_filter(filter: Option<&Expr>) -> (Option<Expr>, Option<Expr>) {
    let Some(filter) = filter else {
        return (None, None);
    };
    if !crate::subquery::mentions_correlated(filter) {
        return (Some(filter.clone()), None);
    }
    let parts = match &filter.kind {
        ExprKind::And(parts) => parts.clone(),
        _ => vec![filter.clone()],
    };
    let (late, early): (Vec<Expr>, Vec<Expr>) = parts
        .into_iter()
        .partition(crate::subquery::mentions_correlated);
    (
        joined_conjuncts(early, filter.position),
        joined_conjuncts(late, filter.position),
    )
}

fn with_selected_rows(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    insert: &Insert,
    query: &crate::ast::Query,
) -> SqlResult<Insert> {
    let selected = self::query(writer, tables, query, false)?;
    let columns = selected.columns.unwrap_or_default();
    let rows = selected
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .enumerate()
                .map(|(at, value)| {
                    let sql_type = columns
                        .get(at)
                        .and_then(|column| column.sql_type)
                        .or_else(|| value.sql_type());
                    InsertValue::Expr(Expr {
                        kind: ExprKind::Constant { value, sql_type },
                        position: query.position,
                    })
                })
                .collect()
        })
        .collect();
    Ok(Insert {
        table: insert.table.clone(),
        columns: insert.columns.clone(),
        source: InsertSource::Values(rows),
        returning: insert.returning.clone(),
        on_duplicate: insert.on_duplicate.clone(),
        rollback_on_conflict: insert.rollback_on_conflict,
        overriding: insert.overriding,
    })
}

pub(crate) fn insert(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    insert: &Insert,
) -> SqlResult<QueryResult> {
    if crate::updatable_views::is_view(writer, tables, &insert.table)? {
        if let Some(result) = crate::triggers::insert_instead_of_view(writer, tables, insert)? {
            return Ok(result);
        }
        return crate::updatable_views::insert(writer, tables, insert);
    }
    if let InsertSource::Query(query) = &insert.source {
        let selected = with_selected_rows(writer, tables, insert, query)?;
        return self::insert(writer, tables, &selected);
    }
    let prepared = if insert_mentions_subquery(insert) {
        let (prepared, subqueries) =
            with_subqueries_run(writer, tables, insert, &insert.table, true, insert_exprs)?;
        if subqueries.is_correlated() {
            return Err(rows_being_changed(insert.table.name.position));
        }
        Some(prepared)
    } else {
        None
    };
    let insert = prepared.as_ref().unwrap_or(insert);
    let table = tables.require_target(writer, &insert.table.name, "insert into")?;
    if crate::sequences::mentions_nextval(&table) {
        crate::sequences::load_all(writer)?;
    }
    let checks = PreparedChecks::of(&table)?;
    let scope = scope_of(&table, &insert.table);
    let dialect = crate::constraints::current_dialect();
    let sqlserver = dialect == Dialect::SqlServer;
    let identity = sqlserver.then(|| table.auto_increment_column()).flatten();
    let identity_insert =
        identity.is_some() && constraints::identity_insert_is_on(&insert.table.name.text);
    let rule = assignment_rule(dialect);
    let targets = match &insert.columns {
        Some(names) => {
            let targets = target_columns(&table, names)?;
            if let Some(identity) = identity
                && targets.contains(&identity) != identity_insert
            {
                return Err(if identity_insert {
                    identity_value_missing(&insert.table.name)
                } else {
                    identity_insert_off(&insert.table.name)
                });
            }
            targets
        }
        None if identity_insert => {
            return Err(match insert.source {
                InsertSource::Values(_) | InsertSource::Query(_) => {
                    identity_needs_column_list(&insert.table.name)
                }
                InsertSource::DefaultValues => identity_value_missing(&insert.table.name),
            });
        }
        None => (0..table.columns.len())
            .filter(|&index| Some(index) != identity)
            .collect(),
    };
    let returning = returning(&insert.returning, scope)?;
    let empty_row: Vec<Vec<InsertValue>> = vec![Vec::new()];
    let rows = match &insert.source {
        InsertSource::Values(rows) => rows,
        InsertSource::Query(_) | InsertSource::DefaultValues => &empty_row,
    };
    if let Some(first) = rows.first() {
        if rows.iter().any(|row| row.len() != first.len()) {
            return Err(SqlError::syntax(
                "VALUES lists must all be the same length",
                insert.table.name.position,
            ));
        }
        if first.len() > targets.len() {
            return Err(SqlError::syntax(
                "INSERT has more expressions than target columns",
                insert.table.name.position,
            ));
        }
        let listed_values = matches!(insert.source, InsertSource::Values(_));
        if (insert.columns.is_some() || sqlserver && listed_values) && first.len() < targets.len() {
            return Err(SqlError::syntax(
                "INSERT has more target columns than expressions",
                insert.table.name.position,
            ));
        }
    }
    if let InsertSource::Values(rows) = &insert.source {
        for value in rows.iter().flatten() {
            if let InsertValue::Expr(expr) = value {
                reject_aggregates(expr, "VALUES")?;
            }
        }
    }
    check_generated_always(&table, &targets, rows, insert.overriding)?;
    let given_ignored: Vec<bool> = (0..table.columns.len())
        .map(|index| {
            insert.overriding == Some(Overriding::UserValue) && table.identity_of(index).is_some()
        })
        .collect();
    let constants = Evaluator {
        scope: Scope::empty(),
        row: &[],
        correlation: None,
    };
    let updates = match &insert.on_duplicate {
        OnDuplicate::Update(assignments) => Some(resolved_updates(&table, scope, assignments)?),
        _ => None,
    };
    let mut firing = Firing::for_table(&*writer, tables, &table, TriggerEvent::Insert, &[])?;
    let mut update_firing = match &updates {
        Some(updates) => {
            let assigned: Vec<usize> = updates.iter().map(|(index, _)| *index).collect();
            Firing::for_table(&*writer, tables, &table, TriggerEvent::Update, &assigned)?
        }
        None => None,
    };
    let mut stored = StoredRows::new(&table);
    stored.checks = Some(&checks);
    stored.identity_insert = identity_insert;
    stored.track = !matches!(insert.on_duplicate, OnDuplicate::Fail | OnDuplicate::Ignore)
        && constraints::is_referenced(&table);
    let mut inserted_children = ChildRows::new(&table);
    let mut result_rows = Vec::new();
    let mut affected = 0usize;
    let mut warnings = Vec::new();
    for values in rows {
        let mut row = Vec::with_capacity(table.columns.len());
        for index in 0..table.columns.len() {
            let given = targets
                .iter()
                .position(|&target| target == index)
                .and_then(|slot| values.get(slot))
                .filter(|_| !given_ignored[index]);
            row.push(match given {
                Some(value) => assigned_value(&constants, value, &table, index, rule)?,
                None => default_value(&table, index)?,
            });
        }
        if let Some(firing) = firing.as_mut()
            && firing.has_before()
        {
            let outcome = firing.before_row(writer, tables, None, Some(&mut row))?;
            stored.forget_generated_ids();
            if outcome == RowOutcome::Skip {
                continue;
            }
        }
        match stored.try_insert(writer, &mut row)? {
            None => {
                affected += 1;
                inserted_children.push(&row)?;
                if let Some(firing) = firing.as_mut() {
                    firing.after_row(writer, tables, None, Some(row.as_slice()))?;
                    stored.forget_generated_ids();
                }
            }
            Some(conflict) => match (&insert.on_duplicate, &updates) {
                (OnDuplicate::Ignore, _) => warnings.push(Warning {
                    level: "Warning",
                    code: 1062,
                    message: crate::mysql_errors::translate(conflict.error(&table, &row), "", "")
                        .message()
                        .to_owned(),
                }),
                (OnDuplicate::Replace, _) => {
                    let mut conflict = Some(conflict);
                    while let Some(found) = conflict {
                        let removed = delete_row(writer, &table, &found.row_key)?;
                        if stored.track {
                            stored.changes.push(RowChange {
                                table: Arc::clone(&table),
                                old: removed,
                                new: None,
                            });
                        }
                        affected += 1;
                        conflict = stored.try_insert(writer, &mut row)?;
                    }
                    inserted_children.push(&row)?;
                    affected += 1;
                    if let Some(firing) = firing.as_mut() {
                        firing.after_row(writer, tables, None, Some(row.as_slice()))?;
                        stored.forget_generated_ids();
                    }
                }
                (OnDuplicate::Update(_), Some(updates)) => {
                    let upsert = Upsert {
                        updates,
                        inserted: &row,
                        existing_key: &conflict.row_key,
                    };
                    let fired = update_firing.as_mut().map(|firing| (firing, &mut *tables));
                    affected += stored.update_existing(writer, scope, upsert, fired)?;
                    stored.forget_generated_ids();
                }
                _ => return Err(conflict.error(&table, &row)),
            },
        }
        if let Some(projection) = &returning {
            result_rows.push(projection.project(&row)?);
        }
    }
    let changes = std::mem::take(&mut stored.changes);
    let first_generated = stored.first_generated;
    let last_rowid = stored.last_rowid;
    let last_identity = table.auto_increment_column().and(last_rowid);
    drop(stored);
    if let Some(firing) = firing {
        firing.finish();
    }
    if let Some(firing) = update_firing {
        firing.finish();
    }
    for change in &changes {
        if let Some(new) = &change.new {
            constraints::check_parents(writer, tables, &table, new, Some(&change.old))?;
        }
    }
    constraints::propagate(writer, tables, changes)?;
    inserted_children.check(writer, tables)?;
    let tag = format!("INSERT 0 {affected}");
    let mut result = match returning {
        Some(projection) => QueryResult::rows(tag, projection.columns, result_rows),
        None => QueryResult::command(tag),
    };
    result.warnings = warnings;
    result.last_insert_id = first_generated;
    result.last_insert_rowid = last_rowid;
    result.last_identity_value = last_identity;
    Ok(result)
}

fn check_generated_always(
    table: &TableDefinition,
    targets: &[usize],
    rows: &[Vec<InsertValue>],
    overriding: Option<Overriding>,
) -> SqlResult<()> {
    if overriding.is_some() || constraints::keeps_identity_values() {
        return Ok(());
    }
    for (slot, &target) in targets.iter().enumerate() {
        let given = rows
            .iter()
            .any(|values| matches!(values.get(slot), Some(InsertValue::Expr(_))));
        if given && table.is_generated_always(target) {
            return Err(generated_always_given(table, target));
        }
    }
    Ok(())
}

fn generated_always_given(table: &TableDefinition, column: usize) -> SqlError {
    let name = &table.columns[column].name;
    SqlError::new(
        SqlState::GENERATED_ALWAYS,
        format!("cannot insert a non-DEFAULT value into column \"{name}\""),
    )
    .with_detail(generated_always_detail(name))
    .with_hint("Use OVERRIDING SYSTEM VALUE to override.")
}

fn generated_always_detail(name: &str) -> String {
    format!("Column \"{name}\" is an identity column defined as GENERATED ALWAYS.")
}

fn check_updated_to_default(
    table: &TableDefinition,
    targets: &[usize],
    assignments: &[(Name, InsertValue)],
) -> SqlResult<()> {
    for (&target, (_, value)) in targets.iter().zip(assignments) {
        if matches!(value, InsertValue::Expr(_)) && table.is_generated_always(target) {
            let name = &table.columns[target].name;
            return Err(SqlError::new(
                SqlState::GENERATED_ALWAYS,
                format!("column \"{name}\" can only be updated to DEFAULT"),
            )
            .with_detail(generated_always_detail(name)));
        }
    }
    Ok(())
}

fn identity_column_error(code: u16, message: String, table: &Name) -> SqlError {
    SqlError::new(SqlState::INVALID_COLUMN_REFERENCE, message)
        .with_sqlserver_code(code)
        .at(table.position)
}

fn identity_insert_off(table: &Name) -> SqlError {
    identity_column_error(
        544,
        format!(
            "Cannot insert explicit value for identity column in table '{}' when IDENTITY_INSERT is set to OFF.",
            crate::schemas::display(&table.text)
        ),
        table,
    )
}

fn identity_value_missing(table: &Name) -> SqlError {
    identity_column_error(
        545,
        format!(
            "Explicit value must be specified for identity column in table '{}' either when IDENTITY_INSERT is set to ON or when a replication user is inserting into a NOT FOR REPLICATION identity column.",
            crate::schemas::display(&table.text)
        ),
        table,
    )
}

fn identity_needs_column_list(table: &Name) -> SqlError {
    identity_column_error(
        8101,
        format!(
            "An explicit value for the identity column in table '{}' can only be specified when a column list is used and IDENTITY_INSERT is ON.",
            crate::schemas::display(&table.text)
        ),
        table,
    )
}

pub(crate) struct ChildRows<'t> {
    table: &'t TableDefinition,
    rows: Vec<Vec<Value>>,
    memory: WorkMemory,
}

impl<'t> ChildRows<'t> {
    pub(crate) fn new(table: &'t TableDefinition) -> Self {
        Self {
            table,
            rows: Vec::new(),
            memory: WorkMemory::new(),
        }
    }

    pub(crate) fn push(&mut self, row: &[Value]) -> SqlResult<()> {
        if self.table.foreign_keys.is_empty() {
            return Ok(());
        }
        self.memory.charge_row(row)?;
        self.rows.push(row.to_vec());
        Ok(())
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(crate) fn check(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
    ) -> SqlResult<()> {
        for row in &self.rows {
            constraints::check_parents(reader, tables, self.table, row, None)?;
        }
        Ok(())
    }

    pub(crate) fn check_and_forget(
        &mut self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
    ) -> SqlResult<()> {
        self.check(reader, tables)?;
        self.rows.clear();
        let used = self.memory.used();
        self.memory.release(used);
        Ok(())
    }
}

fn resolved_updates(
    table: &TableDefinition,
    scope: Scope<'_>,
    assignments: &[(Name, Expr)],
) -> SqlResult<Vec<(usize, Expr)>> {
    let mut updates = Vec::with_capacity(assignments.len());
    for (name, expr) in assignments {
        let index = target_columns(table, std::slice::from_ref(name))?[0];
        let mut expr = expr.clone();
        point_inserted_values(&mut expr, table)?;
        reject_aggregates(&expr, "ON DUPLICATE KEY UPDATE")?;
        check_columns(&expr, scope)?;
        updates.push((index, expr));
    }
    Ok(updates)
}

fn point_inserted_values(expr: &mut Expr, table: &TableDefinition) -> SqlResult<()> {
    if let ExprKind::Function { name, arguments } = &expr.kind
        && name == "values"
        && let [argument] = arguments.as_slice()
        && let ExprKind::Column { name: column, .. } = &argument.kind
    {
        let index = table.column_index(column).ok_or_else(|| {
            SqlError::new(
                SqlState::UNDEFINED_COLUMN,
                format!("column \"{column}\" does not exist"),
            )
            .at(argument.position)
        })?;
        expr.kind = ExprKind::Slot {
            index: table.columns.len() + index,
            sql_type: Some(table.columns[index].sql_type),
        };
        return Ok(());
    }
    for child in children_mut(expr) {
        point_inserted_values(child, table)?;
    }
    Ok(())
}

struct Conflict {
    index: Option<usize>,
    row_key: Vec<u8>,
}

impl Conflict {
    fn error(&self, table: &TableDefinition, row: &[Value]) -> SqlError {
        match self.index {
            None => duplicate_key(table, row),
            Some(position) => indexes::unique_violation(table, &table.indexes[position], row),
        }
    }
}

fn delete_row(
    writer: &mut dyn KeyValueWrite,
    table: &TableDefinition,
    row_key: &[u8],
) -> SqlResult<Vec<Value>> {
    let bytes = writer
        .get(row_key)?
        .ok_or_else(|| crate::encoding::corrupt("row"))?;
    let row = decode_row(&bytes, &table.columns)?;
    if !table.indexes.is_empty() {
        indexes::delete_entries(writer, table, &row, row_key)?;
    }
    writer.delete(row_key)?;
    Ok(row)
}

fn put_row_checked(
    writer: &mut dyn KeyValueWrite,
    table: &TableDefinition,
    row: &[Value],
    row_key: &[u8],
) -> SqlResult<()> {
    if let Some((position, _)) = indexes::first_unique_conflict(writer, table, row, row_key)? {
        return Err(indexes::unique_violation(
            table,
            &table.indexes[position],
            row,
        ));
    }
    writer.put(row_key, &encode_row(row))?;
    indexes::put_entries(writer, table, row, row_key)
}

pub(crate) struct StoredRows<'a> {
    table: &'a TableDefinition,
    key: Vec<u8>,
    prefix_length: usize,
    encoded: Vec<u8>,
    next_row_id: NextRowId,
    auto_increment: Option<usize>,
    identity_seed: IdentitySeed,
    identity_insert: bool,
    next_auto_value: Option<i64>,
    first_generated: Option<u64>,
    last_rowid: Option<i64>,
    checks: Option<&'a PreparedChecks>,
    track: bool,
    changes: Vec<RowChange>,
}

enum NextRowId {
    Unknown,
    Next(u64),
    Exhausted,
}

impl<'a> StoredRows<'a> {
    pub(crate) fn checked(table: &'a TableDefinition, checks: &'a PreparedChecks) -> Self {
        let mut stored = Self::new(table);
        stored.checks = Some(checks);
        stored
    }

    pub(crate) fn set_identity_insert(&mut self, on: bool) {
        self.identity_insert = on;
    }

    pub(crate) fn forget_generated_ids(&mut self) {
        self.next_row_id = NextRowId::Unknown;
        self.next_auto_value = None;
    }

    pub(crate) fn new(table: &'a TableDefinition) -> Self {
        let key = rows_prefix(table.id);
        Self {
            table,
            prefix_length: key.len(),
            key,
            encoded: Vec::new(),
            next_row_id: NextRowId::Unknown,
            auto_increment: table.auto_increment_column(),
            identity_seed: table.identity_seed(),
            identity_insert: false,
            next_auto_value: None,
            first_generated: None,
            last_rowid: None,
            checks: None,
            track: false,
            changes: Vec::new(),
        }
    }

    fn furthest_auto_value(
        &self,
        reader: &dyn KeyValueRead,
        index: usize,
    ) -> SqlResult<Option<i64>> {
        let prefix = rows_prefix(self.table.id);
        let direction = if self.identity_seed.scans_backward() {
            ScanDirection::Backward
        } else {
            ScanDirection::Forward
        };
        let mut last = None;
        reader.scan(&prefix, &prefix_end(&prefix), direction, &mut |_, value| {
            last = Some(value.to_vec());
            Ok(false)
        })?;
        let Some(bytes) = last else {
            return Ok(None);
        };
        let row = decode_row(&bytes, &self.table.columns)?;
        Ok(row.get(index).and_then(Value::as_integer))
    }

    fn fill_auto_increment(
        &mut self,
        reader: &dyn KeyValueRead,
        row: &mut [Value],
    ) -> SqlResult<()> {
        let Some(index) = self.auto_increment else {
            return Ok(());
        };
        let column = &self.table.columns[index];
        let given = row[index].as_integer();
        if self.identity_insert && given.is_none() {
            return Err(SqlError::new(
                SqlState::NOT_NULL_VIOLATION,
                "DEFAULT or NULL are not allowed as explicit identity values.",
            )
            .with_sqlserver_code(339));
        }
        let seed = self.identity_seed;
        let next = match self.next_auto_value {
            Some(next) => next,
            None => seed
                .next_after(self.furthest_auto_value(reader, index)?)
                .ok_or_else(|| auto_increment_exhausted(column))?,
        };
        let kept = match given {
            Some(0) => self.identity_insert,
            other => other.is_some(),
        };
        match given {
            Some(value) if kept => {
                self.next_auto_value = match seed.next_after(Some(value)) {
                    Some(after) if seed.is_beyond(after, next) => Some(after),
                    Some(_) => Some(next),
                    None => None,
                };
            }
            _ => {
                row[index] = convert(Value::BigInt(next), column.sql_type, Conversion::Assignment)
                    .map_err(|_| auto_increment_exhausted(column))?;
                self.first_generated.get_or_insert(next as u64);
                self.next_auto_value = next.checked_add(seed.increment);
            }
        }
        Ok(())
    }

    fn update_existing(
        &mut self,
        writer: &mut dyn KeyValueWrite,
        scope: Scope<'_>,
        upsert: Upsert<'_>,
        mut firing: Option<(&mut Firing, &mut TableCache)>,
    ) -> SqlResult<usize> {
        let Upsert {
            updates,
            inserted,
            existing_key,
        } = upsert;
        let table = self.table;
        let existing_bytes = writer
            .get(existing_key)?
            .ok_or_else(|| crate::encoding::corrupt("row"))?;
        let existing = decode_row(&existing_bytes, &table.columns)?;
        let mut combined = existing.clone();
        combined.extend_from_slice(inserted);
        for (index, expr) in updates {
            let column = &table.columns[*index];
            let value = Evaluator {
                scope,
                row: &combined,
                correlation: None,
            }
            .eval(expr, Some(column.sql_type))?;
            combined[*index] =
                assign(value, column).map_err(|error| error.at_if_unplaced(expr.position))?;
        }
        combined.truncate(table.columns.len());
        if let Some((firing, tables)) = firing.as_mut()
            && firing.has_before()
            && firing.before_row(writer, tables, Some(existing.as_slice()), Some(&mut combined))?
                == RowOutcome::Skip
        {
            return Ok(0);
        }
        if combined == existing {
            if let Some((firing, tables)) = firing {
                firing.after_row(writer, tables, Some(existing.as_slice()), Some(combined.as_slice()))?;
            }
            return Ok(0);
        }
        check_not_null(table, &combined)?;
        if let Some(checks) = self.checks {
            checks.check(table, &combined)?;
        }
        if self.track || !table.foreign_keys.is_empty() {
            self.changes.push(RowChange {
                table: Arc::new(table.clone()),
                old: existing.clone(),
                new: Some(combined.clone()),
            });
        }
        let new_key = if table.primary_key.is_empty() {
            existing_key.to_vec()
        } else {
            row_key(table, &combined)
        };
        if !table.indexes.is_empty() {
            indexes::delete_entries(writer, table, &existing, existing_key)?;
            if new_key != existing_key {
                writer.delete(existing_key)?;
                if writer.get(&new_key)?.is_some() {
                    return Err(duplicate_key(table, &combined));
                }
            }
            put_row_checked(writer, table, &combined, &new_key)?;
        } else {
            let encoded = encode_row(&combined);
            if new_key == existing_key {
                writer.put(existing_key, &encoded)?;
            } else {
                writer.delete(existing_key)?;
                if !writer.put_if_absent(&new_key, &encoded)? {
                    return Err(duplicate_key(table, &combined));
                }
            }
        }
        if let Some((firing, tables)) = firing {
            firing.after_row(writer, tables, Some(existing.as_slice()), Some(combined.as_slice()))?;
        }
        Ok(2)
    }

    pub(crate) fn insert(
        &mut self,
        writer: &mut dyn KeyValueWrite,
        row: &mut [Value],
    ) -> SqlResult<()> {
        match self.try_insert(writer, row)? {
            None => Ok(()),
            Some(conflict) => Err(conflict.error(self.table, row)),
        }
    }

    fn try_insert(
        &mut self,
        writer: &mut dyn KeyValueWrite,
        row: &mut [Value],
    ) -> SqlResult<Option<Conflict>> {
        let table = self.table;
        if self.auto_increment.is_some() {
            self.fill_auto_increment(writer, row)?;
        }
        check_not_null(table, row)?;
        if let Some(checks) = self.checks {
            checks.check(table, row)?;
        }
        self.encoded.clear();
        encode_row_into(&mut self.encoded, row);
        self.key.truncate(self.prefix_length);
        if table.primary_key.is_empty() {
            let id = match self.next_row_id {
                NextRowId::Unknown => next_hidden_row_id(writer, table)?,
                NextRowId::Next(id) => id,
                NextRowId::Exhausted => return Err(row_identifiers_used(table)),
            };
            self.next_row_id = id
                .checked_add(1)
                .map_or(NextRowId::Exhausted, NextRowId::Next);
            self.key.extend_from_slice(&id.to_be_bytes());
            if let Some(conflict) = self.unique_conflict(writer, row)? {
                return Ok(Some(conflict));
            }
            writer.put(&self.key, &self.encoded)?;
            indexes::put_entries(writer, table, row, &self.key)?;
            self.last_rowid = i64::try_from(id).ok();
            return Ok(None);
        }
        for &index in &table.primary_key {
            encode_key_value(&mut self.key, &row[index]);
        }
        let inserted = if table.indexes.is_empty() {
            writer.put_if_absent(&self.key, &self.encoded)?
        } else if writer.get(&self.key)?.is_some() {
            false
        } else if let Some(conflict) = self.unique_conflict(writer, row)? {
            return Ok(Some(conflict));
        } else {
            writer.put(&self.key, &self.encoded)?;
            indexes::put_entries(writer, table, row, &self.key)?;
            true
        };
        if !inserted {
            return Ok(Some(Conflict {
                index: None,
                row_key: self.key.clone(),
            }));
        }
        if let Some(index) = self.auto_increment {
            self.last_rowid = row[index].as_integer();
        }
        Ok(None)
    }

    fn unique_conflict(
        &self,
        reader: &dyn KeyValueRead,
        row: &[Value],
    ) -> SqlResult<Option<Conflict>> {
        Ok(
            indexes::first_unique_conflict(reader, self.table, row, &self.key)?.map(
                |(position, row_key)| Conflict {
                    index: Some(position),
                    row_key,
                },
            ),
        )
    }
}

fn auto_increment_exhausted(column: &Column) -> SqlError {
    SqlError::new(
        SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
        format!(
            "the AUTO_INCREMENT column \"{}\" has used every value of type {}",
            column.name,
            column.sql_type.name()
        ),
    )
}

fn literal_value(literal: &LiteralToken<'_>, column: &Column, position: usize) -> SqlResult<Value> {
    let hint = Some(column.sql_type);
    let value = match literal {
        LiteralToken::Null => Ok(Value::Null),
        LiteralToken::Boolean(flag) => Ok(Value::Boolean(*flag)),
        LiteralToken::Integer(number) => {
            crate::eval::literal_value(&crate::ast::Literal::Integer(*number), hint)
        }
        LiteralToken::Decimal(text) => crate::eval::decimal_constant(text, hint),
        LiteralToken::String(text) => crate::eval::string_constant(text, hint),
    }
    .map_err(|error| error.at_if_unplaced(position))?;
    assign(value, column).map_err(|error| error.at_if_unplaced(position))
}

pub(crate) fn insert_literals(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    insert: &LiteralRows<'_>,
) -> SqlResult<QueryResult> {
    let table = tables.require_target(writer, &insert.table.name, "insert into")?;
    if insert.width > table.columns.len() {
        return Err(SqlError::syntax(
            "INSERT has more expressions than target columns",
            insert.table.name.position,
        ));
    }
    if !constraints::keeps_identity_values()
        && let Some(column) = (0..insert.width).find(|&column| table.is_generated_always(column))
    {
        return Err(generated_always_given(&table, column));
    }
    if crate::sequences::mentions_nextval(&table) {
        crate::sequences::load_all(writer)?;
    }
    let checks = PreparedChecks::of(&table)?;
    let mut firing = Firing::for_table(&*writer, tables, &table, TriggerEvent::Insert, &[])?;
    let mut stored = StoredRows::new(&table);
    stored.checks = Some(&checks);
    let mut children = ChildRows::new(&table);
    let mut literals = Vec::with_capacity(insert.width);
    let mut row = Vec::with_capacity(table.columns.len());
    let mut at = insert.first_row();
    let mut inserted = 0usize;
    for _ in 0..insert.rows {
        insert.next_row(&mut at, &mut literals);
        row.clear();
        for (index, column) in table.columns.iter().enumerate() {
            row.push(match literals.get(index) {
                Some((position, literal)) => literal_value(literal, column, *position)?,
                None => default_value(&table, index)?,
            });
        }
        if let Some(firing) = firing.as_mut()
            && firing.has_before()
        {
            let outcome = firing.before_row(writer, tables, None, Some(&mut row))?;
            stored.forget_generated_ids();
            if outcome == RowOutcome::Skip {
                continue;
            }
        }
        stored.insert(writer, &mut row)?;
        children.push(&row)?;
        inserted += 1;
        if let Some(firing) = firing.as_mut() {
            firing.after_row(writer, tables, None, Some(row.as_slice()))?;
            stored.forget_generated_ids();
        }
    }
    let first_generated = stored.first_generated;
    let last_rowid = stored.last_rowid;
    drop(stored);
    if let Some(firing) = firing {
        firing.finish();
    }
    children.check(writer, tables)?;
    let mut result = QueryResult::command(format!("INSERT 0 {inserted}"));
    result.last_insert_id = first_generated;
    result.last_insert_rowid = last_rowid;
    Ok(result)
}

struct Projection<'a> {
    columns: Vec<OutputColumn>,
    items: Vec<ProjectedItem<'a>>,
    scope: Scope<'a>,
    each_column_once: bool,
    correlation: Option<&'a Correlation<'a>>,
}

enum ProjectedItem<'a> {
    Column(usize),
    Expr(&'a Expr),
}

pub(crate) fn project_rows(
    items: &[SelectItem],
    scope: Scope<'_>,
    rows: &[Vec<Value>],
) -> SqlResult<(Vec<OutputColumn>, Vec<Vec<Value>>)> {
    for item in items {
        if let SelectItem::Expr { expr, .. } = item {
            reject_aggregates(expr, "OUTPUT")?;
        }
    }
    let projection = Projection::new(items, scope)?;
    let mut projected = Vec::with_capacity(rows.len());
    for row in rows {
        projected.push(projection.project(row)?);
    }
    Ok((projection.columns, projected))
}

fn returning<'a>(
    items: &'a Option<Vec<SelectItem>>,
    scope: Scope<'a>,
) -> SqlResult<Option<Projection<'a>>> {
    let Some(items) = items else {
        return Ok(None);
    };
    for item in items {
        if let SelectItem::Expr { expr, .. } = item {
            reject_aggregates(expr, "RETURNING")?;
        }
    }
    Projection::new(items, scope).map(Some)
}

impl<'a> Projection<'a> {
    fn new(items: &'a [SelectItem], scope: Scope<'a>) -> SqlResult<Self> {
        let mut columns = Vec::with_capacity(items.len());
        let mut projected = Vec::with_capacity(items.len());
        for item in items {
            match item {
                SelectItem::Wildcard {
                    qualifier,
                    position,
                } => {
                    if matches!(scope, Scope::Empty) {
                        return Err(SqlError::syntax(
                            "SELECT * with no tables specified is not valid",
                            *position,
                        ));
                    }
                    let offsets: Vec<usize> = match qualifier {
                        None => scope.star(),
                        Some(qualifier) => {
                            let source = scope
                                .sources()
                                .iter()
                                .find(|source| source.label == qualifier)
                                .ok_or_else(|| {
                                    SqlError::new(
                                        SqlState::UNDEFINED_TABLE,
                                        format!(
                                            "missing FROM-clause entry for table \"{qualifier}\""
                                        ),
                                    )
                                    .at(*position)
                                })?;
                            (source.offset..source.offset + source.table.columns.len())
                            .filter(|&offset| !scope.is_hidden(offset))
                            .collect()
                        }
                    };
                    for offset in offsets {
                        if let Some((source, index)) = scope.source_of(offset) {
                            let column = &source.table.columns[index];
                            columns.push(OutputColumn {
                                name: crate::temp_tables::display_name(&column.name).to_owned(),
                                sql_type: Some(column.sql_type),
                            });
                            projected.push(ProjectedItem::Column(offset));
                        }
                    }
                }
                SelectItem::Expr { expr, alias } => {
                    check_columns(expr, scope)?;
                    let column = match &expr.kind {
                        ExprKind::Column { qualifier, name } => scope
                            .column(qualifier.as_deref(), name, expr.position)
                            .ok()
                            .map(|offset| (offset, scope.column_type(offset))),
                        ExprKind::Slot { index, sql_type } => Some((*index, *sql_type)),
                        _ => None,
                    };
                    columns.push(OutputColumn {
                        name: alias.clone().unwrap_or_else(|| output_name(expr)),
                        sql_type: match column {
                            Some((_, sql_type)) => sql_type,
                            None => typing::static_type(expr, scope),
                        },
                    });
                    projected.push(match column {
                        Some((index, _)) => ProjectedItem::Column(index),
                        None => ProjectedItem::Expr(expr),
                    });
                }
            }
        }
        let width = projected
            .iter()
            .filter_map(|item| match item {
                ProjectedItem::Column(index) => Some(index + 1),
                ProjectedItem::Expr(_) => None,
            })
            .max()
            .unwrap_or(0);
        let mut taken = vec![false; width];
        let each_column_once = projected.iter().all(|item| match item {
            ProjectedItem::Column(index) => !std::mem::replace(&mut taken[*index], true),
            ProjectedItem::Expr(_) => false,
        });
        Ok(Self {
            columns,
            items: projected,
            scope,
            each_column_once,
            correlation: None,
        })
    }

    fn output_named(&self, name: &str, position: usize) -> SqlResult<usize> {
        let mut found: Option<(usize, Option<usize>)> = None;
        for (index, column) in self.columns.iter().enumerate() {
            if column.name != name {
                continue;
            }
            let source = self.source_column(index);
            match found {
                None => found = Some((index, source)),
                Some((_, earlier)) if earlier.is_some() && earlier == source => {}
                Some(_) => {
                    return Err(SqlError::new(
                        SqlState::AMBIGUOUS_COLUMN,
                        format!("ORDER BY \"{name}\" is ambiguous"),
                    )
                    .at(position));
                }
            }
        }
        found.map(|(index, _)| index).ok_or_else(|| {
            SqlError::new(
                SqlState::UNDEFINED_COLUMN,
                format!("column \"{name}\" does not exist"),
            )
            .at(position)
        })
    }

    fn source_column(&self, index: usize) -> Option<usize> {
        match &self.items[index] {
            ProjectedItem::Column(column) => Some(*column),
            ProjectedItem::Expr(_) => None,
        }
    }

    fn output_of(&self, expr: &Expr) -> Option<usize> {
        let column = match &expr.kind {
            ExprKind::Column { qualifier, name } => self
                .scope
                .column(qualifier.as_deref(), name, expr.position)
                .ok(),
            ExprKind::Slot { index, .. } => Some(*index),
            _ => None,
        };
        if let Some(column) = column {
            return (0..self.items.len()).find(|&index| self.source_column(index) == Some(column));
        }
        let shape = shape_of(expr);
        self.items.iter().position(|item| match item {
            ProjectedItem::Expr(projected) => shape_of(projected) == shape,
            ProjectedItem::Column(_) => false,
        })
    }

    fn project_owned(&self, mut row: Vec<Value>) -> SqlResult<Vec<Value>> {
        if !self.each_column_once {
            return self.project(&row);
        }
        Ok(self
            .items
            .iter()
            .map(|item| match item {
                ProjectedItem::Column(index) => row
                    .get_mut(*index)
                    .map_or(Value::Null, |value| std::mem::replace(value, Value::Null)),
                ProjectedItem::Expr(_) => Value::Null,
            })
            .collect())
    }

    fn project(&self, row: &[Value]) -> SqlResult<Vec<Value>> {
        let evaluator = Evaluator {
            scope: self.scope,
            row,
            correlation: self.correlation,
        };
        let mut bytes = crate::memory::RowBytes::default();
        let mut output = Vec::with_capacity(self.items.len());
        for item in &self.items {
            let value = match item {
                ProjectedItem::Column(index) => {
                    let value = row.get(*index).unwrap_or(&Value::Null);
                    bytes.add(value)?;
                    value.clone()
                }
                ProjectedItem::Expr(expr) => {
                    let value = evaluator.eval(expr, None)?;
                    bytes.add(&value)?;
                    value
                }
            };
            output.push(value);
        }
        Ok(output)
    }
}

fn filter_matches(scope: Scope<'_>, filter: Option<&Expr>, row: &[Value]) -> SqlResult<bool> {
    let Some(filter) = filter else {
        return Ok(true);
    };
    let evaluator = Evaluator {
        scope,
        row,
        correlation: None,
    };
    Ok(evaluator.boolean(filter, "WHERE")? == Some(true))
}

fn is_constant(expr: &Expr) -> bool {
    check_columns(expr, Scope::empty()).is_ok()
}

fn key_equality<'e>(expr: &'e Expr, scope: Scope<'_>, index: usize) -> Option<&'e Expr> {
    match &expr.kind {
        ExprKind::And(operands) => operands
            .iter()
            .find_map(|operand| key_equality(operand, scope, index)),
        ExprKind::Binary(BinaryOperator::Equal, left, right) => {
            let names_column = |expr: &Expr| {
                matches!(&expr.kind, ExprKind::Column { qualifier, name }
                    if scope.column(qualifier.as_deref(), name, 0).ok() == Some(index))
            };
            if names_column(left) && is_constant(right) {
                Some(right)
            } else if names_column(right) && is_constant(left) {
                Some(left)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn point_key(table: &TableDefinition, scope: Scope<'_>, filter: &Expr) -> Option<Vec<u8>> {
    if table.primary_key.is_empty() {
        return None;
    }
    let constants = Evaluator {
        scope: Scope::empty(),
        row: &[],
        correlation: None,
    };
    let mut key = rows_prefix(table.id);
    for &index in &table.primary_key {
        let column = &table.columns[index];
        let found = key_equality(filter, scope, index)?;
        let original = constants.eval(found, Some(column.sql_type)).ok()?;
        let converted = convert(original.clone(), column.sql_type, Conversion::Assignment).ok()?;
        if converted.is_null() || compare_values(&converted, &original) != Ordering::Equal {
            return None;
        }
        encode_key_value(&mut key, &converted);
    }
    Some(key)
}

type RowVisitor<'v> = dyn FnMut(&[u8], Vec<Value>) -> SqlResult<bool> + 'v;
pub(crate) fn for_each_row_with(
    reader: &dyn KeyValueRead,
    table: &TableDefinition,
    pairs: &[(usize, Value)],
    visit: &mut RowVisitor<'_>,
) -> SqlResult<()> {
    let equalities: Vec<Expr> = pairs
        .iter()
        .filter_map(|(index, value)| {
            let column = table.columns.get(*index)?;
            Some(Expr {
                kind: ExprKind::Binary(
                    BinaryOperator::Equal,
                    Box::new(Expr {
                        kind: ExprKind::Column {
                            qualifier: None,
                            name: column.name.clone(),
                        },
                        position: 0,
                    }),
                    Box::new(Expr {
                        kind: ExprKind::Constant {
                            sql_type: value.sql_type(),
                            value: value.clone(),
                        },
                        position: 0,
                    }),
                ),
                position: 0,
            })
        })
        .collect();
    let filter = Expr {
        kind: ExprKind::And(equalities),
        position: 0,
    };
    let scope = Scope::One(Source {
        table,
        label: &table.name,
        offset: 0,
    });
    let access = planned_access(table, scope, Some(&filter), None);
    for_each_matching_row(reader, table, scope, Some(&filter), &access, visit)
}

pub(crate) fn delete_stored_row(
    writer: &mut dyn KeyValueWrite,
    table: &TableDefinition,
    row_key: &[u8],
    row: &[Value],
) -> SqlResult<()> {
    if !table.indexes.is_empty() {
        indexes::delete_entries(writer, table, row, row_key)?;
    }
    writer.delete(row_key)?;
    Ok(())
}

pub(crate) fn replace_stored_row(
    writer: &mut dyn KeyValueWrite,
    table: &TableDefinition,
    old_key: &[u8],
    old_row: &[Value],
    new_row: &[Value],
) -> SqlResult<()> {
    let new_key = if table.primary_key.is_empty() {
        old_key.to_vec()
    } else {
        row_key(table, new_row)
    };
    if !table.indexes.is_empty() {
        indexes::delete_entries(writer, table, old_row, old_key)?;
    }
    if new_key != old_key {
        writer.delete(old_key)?;
        if writer.get(&new_key)?.is_some() {
            return Err(duplicate_key(table, new_row));
        }
    }
    if table.indexes.is_empty() {
        writer.put(&new_key, &encode_row(new_row))
    } else {
        put_row_checked(writer, table, new_row, &new_key)
    }
}

pub(crate) fn assign_to(value: Value, column: &Column) -> SqlResult<Value> {
    assign(value, column)
}

pub(crate) fn default_of(table: &TableDefinition, index: usize) -> SqlResult<Value> {
    default_value(table, index)
}

pub(crate) fn check_not_null_of(table: &TableDefinition, row: &[Value]) -> SqlResult<()> {
    check_not_null(table, row)
}

fn for_each_matching_row(
    reader: &dyn KeyValueRead,
    table: &TableDefinition,
    scope: Scope<'_>,
    filter: Option<&Expr>,
    access: &Access,
    visit: &mut RowVisitor<'_>,
) -> SqlResult<()> {
    if let Path::Point(key) = &access.path {
        let mut found = None;
        reader.get_with(key, &mut |bytes| {
            found = Some(decode_row(bytes, &table.columns)?);
            Ok(())
        })?;
        if let Some(row) = found {
            let filter_is_the_key = table.primary_key.len() == 1
                && filter.is_some_and(|filter| {
                    matches!(filter.kind, ExprKind::Binary(BinaryOperator::Equal, ..))
                });
            if filter_is_the_key || filter_matches(scope, filter, &row)? {
                visit(key, row)?;
            }
        }
        return Ok(());
    }
    if let Path::Ranges {
        key,
        ranges,
        backward,
    } = &access.path
    {
        return plan::read_ranges(reader, table, *key, ranges, *backward, &mut |key, row| {
            crate::cancel::check()?;
            if filter_matches(scope, filter, &row)? {
                visit(key, row)
            } else {
                Ok(true)
            }
        });
    }
    let prefix = rows_prefix(table.id);
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |key, bytes| {
            crate::cancel::check()?;
            let row = decode_row(bytes, &table.columns)?;
            if filter_matches(scope, filter, &row)? {
                return visit(key, row);
            }
            Ok(true)
        },
    )
}

fn planned_access(
    table: &TableDefinition,
    scope: Scope<'_>,
    filter: Option<&Expr>,
    order: Option<&OrderRequest>,
) -> Access {
    if let Some(key) = filter.and_then(|filter| point_key(table, scope, filter)) {
        return Access::point(key);
    }
    match (filter, order) {
        (None, None) => Access::scan(),
        _ => plan::plan(table, scope, filter, order),
    }
}

fn matching_rows(
    reader: &dyn KeyValueRead,
    table: &TableDefinition,
    scope: Scope<'_>,
    filter: Option<&Expr>,
) -> SqlResult<Vec<(Vec<u8>, Vec<Value>)>> {
    let access = planned_access(table, scope, filter, None);
    let mut memory = WorkMemory::new();
    let mut rows = Vec::new();
    for_each_matching_row(reader, table, scope, filter, &access, &mut |key, row| {
        memory.charge(WorkMemory::row_bytes(&row) + key.len())?;
        rows.push((key.to_vec(), row));
        Ok(true)
    })?;
    Ok(rows)
}

pub(crate) fn row_count(expr: Option<&Expr>, clause: &str) -> SqlResult<Option<u64>> {
    let Some(expr) = expr else {
        return Ok(None);
    };
    let constants = Evaluator {
        scope: Scope::empty(),
        row: &[],
        correlation: None,
    };
    reject_aggregates(expr, clause)?;
    check_columns(expr, Scope::empty()).map_err(|_| {
        SqlError::syntax(
            format!("argument of {clause} must not contain variables"),
            expr.position,
        )
    })?;
    let value = convert(
        constants.eval(expr, Some(SqlType::BigInt))?,
        SqlType::BigInt,
        Conversion::Assignment,
    )
    .map_err(|error| error.at_if_unplaced(expr.position))?;
    match value {
        Value::Null => Ok(None),
        Value::BigInt(count) if count < 0 => Err(if clause == "LIMIT" {
            SqlError::new(
                SqlState::INVALID_ROW_COUNT_IN_LIMIT_CLAUSE,
                "LIMIT must not be negative",
            )
        } else {
            SqlError::new(
                SqlState::INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE,
                "OFFSET must not be negative",
            )
        }),
        Value::BigInt(count) => Ok(Some(count as u64)),
        _ => Ok(None),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RowLimit {
    pub(crate) count: Option<u64>,
    pub(crate) percent: Option<f64>,
    pub(crate) with_ties: bool,
}

impl RowLimit {
    pub(crate) fn is_limited(self) -> bool {
        self.count.is_some() || self.percent.is_some()
    }

    pub(crate) fn rows_to_take(self, total: usize) -> usize {
        match (self.percent, self.count) {
            (Some(percent), _) => percent_of(total, percent),
            (None, Some(count)) => usize::try_from(count).unwrap_or(usize::MAX),
            (None, None) => usize::MAX,
        }
    }
}

fn percent_of(total: usize, percent: f64) -> usize {
    if percent.is_nan() || percent <= 0.0 {
        return 0;
    }
    let wanted = (total as f64 * percent.min(100.0) / 100.0).ceil();
    if wanted >= total as f64 {
        total
    } else {
        wanted as usize
    }
}

fn fetch_percent(expr: &Expr) -> SqlResult<f64> {
    let constants = Evaluator {
        scope: Scope::empty(),
        row: &[],
        correlation: None,
    };
    reject_aggregates(expr, "FETCH")?;
    let value = constants.eval(expr, None)?;
    Ok(crate::oracle_functions::decimal_of(&value)?.map_or(0.0, |percent| percent.to_f64()))
}

pub(crate) fn row_limit(expr: Option<&Expr>) -> SqlResult<RowLimit> {
    let unlimited = RowLimit {
        count: None,
        percent: None,
        with_ties: false,
    };
    let Some(expr) = expr else {
        return Ok(unlimited);
    };
    if let ExprKind::OracleFunction { name, arguments } = &expr.kind
        && let [count] = arguments.as_slice()
    {
        match *name {
            "oracle_fetch_with_ties" => {
                return Ok(RowLimit {
                    count: row_count(Some(count), "LIMIT")?,
                    with_ties: true,
                    ..unlimited
                });
            }
            "oracle_fetch_percent" | "oracle_fetch_percent_with_ties" => {
                return Ok(RowLimit {
                    count: None,
                    percent: Some(fetch_percent(count)?),
                    with_ties: *name == "oracle_fetch_percent_with_ties",
                });
            }
            _ => {}
        }
    }
    Ok(RowLimit {
        count: row_count(Some(expr), "LIMIT")?,
        ..unlimited
    })
}

pub(crate) fn rows_within_limit<K>(
    rows: impl Iterator<Item = (K, Vec<Value>)>,
    skip: usize,
    take: usize,
    ties: Option<&dyn Fn(&K, &K) -> bool>,
) -> Vec<Vec<Value>> {
    let mut output = Vec::new();
    let mut last: Option<K> = None;
    for (keys, row) in rows.skip(skip) {
        if output.len() < take {
            output.push(row);
            if ties.is_some() {
                last = Some(keys);
            }
            continue;
        }
        let tied = match (ties, &last) {
            (Some(tied), Some(last)) => tied(last, &keys),
            _ => false,
        };
        if !tied {
            break;
        }
        output.push(row);
    }
    output
}

enum SortKey<'a> {
    Output(usize),
    Expr(&'a Expr),
}

fn sort_keys<'e>(
    order_by: &[OrderItem],
    expressions: &[&'e Expr],
    projection: &Projection<'_>,
    scope: Scope<'_>,
    distinct: bool,
) -> SqlResult<Vec<SortKey<'e>>> {
    let mut keys = Vec::with_capacity(order_by.len());
    for (item, &expr) in order_by.iter().zip(expressions) {
        let key = match &item.expr.kind {
            ExprKind::Literal(crate::ast::Literal::Integer(position)) => {
                let index = usize::try_from(*position)
                    .ok()
                    .filter(|&index| index >= 1 && index <= projection.columns.len())
                    .ok_or_else(|| {
                        SqlError::new(
                            SqlState::INVALID_COLUMN_REFERENCE,
                            format!("ORDER BY position {position} is not in select list"),
                        )
                        .at(item.expr.position)
                    })?;
                SortKey::Output(index - 1)
            }
            ExprKind::Column {
                qualifier: None,
                name,
            } if projection.columns.iter().any(|column| &column.name == name) => {
                SortKey::Output(projection.output_named(name, item.expr.position)?)
            }
            _ => {
                check_columns(expr, scope)?;
                if distinct {
                    let index = projection.output_of(expr).ok_or_else(|| {
                        SqlError::new(
                            SqlState::INVALID_COLUMN_REFERENCE,
                            "for SELECT DISTINCT, ORDER BY expressions must appear in select list",
                        )
                        .at(item.expr.position)
                    })?;
                    SortKey::Output(index)
                } else {
                    SortKey::Expr(expr)
                }
            }
        };
        keys.push(key);
    }
    Ok(keys)
}

pub(crate) fn returning_columns(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    table: &TableReference,
    items: &Option<Vec<SelectItem>>,
) -> SqlResult<Option<Vec<OutputColumn>>> {
    if crate::updatable_views::is_view(reader, tables, table)? {
        return crate::updatable_views::returning_columns(reader, tables, table, items);
    }
    let definition = tables.require(reader, &table.name)?;
    let scope = scope_of(&definition, table);
    Ok(returning(items, scope)?.map(|projection| projection.columns))
}

pub(crate) fn select(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    select: &Select,
    columns_only: bool,
) -> SqlResult<QueryResult> {
    if crate::subquery::needs_preparation(select) {
        return crate::subquery::run_select(reader, tables, select, columns_only);
    }
    select_or_explain(reader, tables, select, columns_only, None, None)
}

pub(crate) fn query(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    query: &crate::ast::Query,
    columns_only: bool,
) -> SqlResult<QueryResult> {
    crate::subquery::run_statement(reader, tables, query, columns_only)
}

pub(crate) fn select_prepared(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    select: &Select,
    columns_only: bool,
    correlation: Option<&Correlation<'_>>,
) -> SqlResult<QueryResult> {
    select_or_explain(reader, tables, select, columns_only, None, correlation)
}

fn correlated_unsupported(where_: &str, position: usize) -> SqlError {
    SqlError::not_supported(format!(
        "a subquery that refers to the outer query in {where_} is not supported yet"
    ))
    .at(position)
}

fn split_correlated(select: &Select) -> SqlResult<Option<(Select, Expr)>> {
    let correlated = crate::subquery::mentions_correlated;
    if let Some(from) = &select.from {
        for join in &from.joins {
            if let crate::ast::JoinCondition::On(on) = &join.condition
                && correlated(on)
            {
                return Err(correlated_unsupported("a JOIN condition", on.position));
            }
        }
    }
    if let Some(key) = select.group_by.iter().find(|key| correlated(key)) {
        return Err(correlated_unsupported("GROUP BY", key.position));
    }
    let Some(filter) = &select.filter else {
        return Ok(None);
    };
    if !correlated(filter) {
        return Ok(None);
    }
    let position = filter.position;
    let parts = match &filter.kind {
        ExprKind::And(parts) => parts.clone(),
        _ => vec![filter.clone()],
    };
    let (late, early): (Vec<Expr>, Vec<Expr>) = parts.into_iter().partition(correlated);
    let joined = |mut parts: Vec<Expr>| match parts.len() {
        0 => None,
        1 => parts.pop(),
        _ => Some(Expr {
            kind: ExprKind::And(parts),
            position,
        }),
    };
    let mut early_select = select.clone();
    early_select.filter = joined(early);
    match joined(late) {
        Some(late) => Ok(Some((early_select, late))),
        None => Ok(None),
    }
}

pub(crate) struct ExplainedSource {
    pub(crate) table: Arc<TableDefinition>,
    pub(crate) label: String,
    pub(crate) access: Access,
    pub(crate) join: Option<join::JoinMethod>,
}

#[derive(Default)]
pub(crate) struct ExplainedQuery {
    pub(crate) sources: Vec<ExplainedSource>,
    pub(crate) sorted: bool,
    pub(crate) limited: bool,
    pub(crate) filtered: bool,
}

pub(crate) fn explain(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    statement: &crate::ast::Statement,
) -> SqlResult<ExplainedQuery> {
    let mut out = ExplainedQuery::default();
    let (reference, filter) = match statement {
        crate::ast::Statement::Select(query) if crate::subquery::needs_preparation(query) => {
            return Err(SqlError::not_supported(
                "EXPLAIN of a query with subqueries or derived tables",
            ));
        }
        crate::ast::Statement::Select(query) => {
            select_or_explain(reader, tables, query, false, Some(&mut out), None)?;
            return Ok(out);
        }
        crate::ast::Statement::Update(update) => (&update.table, update.filter.as_ref()),
        crate::ast::Statement::Delete(delete) => (&delete.table, delete.filter.as_ref()),
        _ => {
            return Err(SqlError::not_supported(
                "EXPLAIN of a statement other than SELECT, UPDATE or DELETE",
            ));
        }
    };
    let table = tables.require(reader, &reference.name)?;
    let scope = scope_of(&table, reference);
    if let Some(filter) = filter {
        reject_aggregates(filter, "WHERE")?;
        check_columns(filter, scope)?;
    }
    let access = plan::plan(&table, scope, filter, None);
    out.filtered = filter.is_some();
    out.sources.push(ExplainedSource {
        label: reference
            .alias
            .clone()
            .unwrap_or_else(|| table.name.clone()),
        table: Arc::clone(&table),
        access,
        join: None,
    });
    Ok(out)
}

fn select_or_explain(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    select: &Select,
    columns_only: bool,
    explain: Option<&mut ExplainedQuery>,
    correlation: Option<&Correlation<'_>>,
) -> SqlResult<QueryResult> {
    let split = match correlation {
        Some(_) => split_correlated(select)?,
        None => None,
    };
    let (select, late) = match &split {
        Some((early, late)) => (early, Some(late)),
        None => (select, None),
    };
    if let Some(late) = late {
        reject_aggregates(late, "WHERE")?;
    }
    let first = match &select.from {
        Some(from) => Some(tables.require(reader, &from.first.name)?),
        None => None,
    };
    let mut definitions: Vec<Arc<TableDefinition>> = Vec::new();
    if let (Some(from), Some(first)) = (&select.from, &first)
        && !from.joins.is_empty()
    {
        definitions.push(Arc::clone(first));
        for join in &from.joins {
            definitions.push(tables.require(reader, &join.table.name)?);
        }
    }
    let joined = match &select.from {
        Some(from) if !from.joins.is_empty() => {
            Some(join::resolve(from, &definitions, select.filter.as_ref())?)
        }
        _ => None,
    };
    let scope = match (&select.from, &joined, &first) {
        (_, Some(joined), _) => Scope::Many(&joined.sources),
        (Some(from), None, Some(table)) => scope_of(table, &from.first),
        _ => Scope::empty(),
    };
    if let Some(filter) = &select.filter {
        reject_aggregates(filter, "WHERE")?;
        check_columns(filter, scope)?;
    }
    let grouping = if aggregate::is_aggregate_query(select) {
        Some(aggregate::analyze(select, scope)?)
    } else {
        None
    };
    let (items, post_scope) = match &grouping {
        Some(grouping) => (grouping.items.as_slice(), Scope::empty()),
        None => (select.items.as_slice(), scope),
    };
    let mut projection = Projection::new(items, post_scope)?;
    projection.correlation = correlation;
    if let Some(late) = late {
        check_columns(late, scope)?;
    }
    let order_expressions: Vec<&Expr> = select
        .order_by
        .iter()
        .enumerate()
        .map(|(index, item)| {
            grouping
                .as_ref()
                .and_then(|grouping| grouping.order[index].as_ref())
                .unwrap_or(&item.expr)
        })
        .collect();
    let sort_keys = sort_keys(
        &select.order_by,
        &order_expressions,
        &projection,
        post_scope,
        select.distinct,
    )?;
    if columns_only {
        return Ok(QueryResult::rows(
            "SELECT 0".to_owned(),
            projection.columns,
            Vec::new(),
        ));
    }
    let limit = row_limit(select.limit.as_ref())?;
    let offset = row_count(select.offset.as_ref(), "OFFSET")?.unwrap_or(0);
    if let Some(out) = explain {
        out.limited = limit.is_limited();
        out.filtered = select.filter.is_some();
        out.sorted = !sort_keys.is_empty();
        if let Some(joined) = &joined {
            let methods = joined.methods();
            for (position, source) in joined.sources.list.iter().enumerate() {
                let access = match (position, &select.filter) {
                    (0, Some(filter))
                        if point_key(source.table, Scope::One(*source), filter).is_some() =>
                    {
                        plan::plan(source.table, Scope::One(*source), Some(filter), None)
                    }
                    _ => Access::scan(),
                };
                out.sources.push(ExplainedSource {
                    table: Arc::clone(&definitions[position]),
                    label: source.label.to_owned(),
                    access,
                    join: position
                        .checked_sub(1)
                        .and_then(|step| methods.get(step).copied()),
                });
            }
        } else if let (Some(from), Some(table)) = (&select.from, &first) {
            let order = if grouping.is_none() && select.source_row_limit.is_none() {
                order_request(select, &sort_keys, &projection, scope, table)
            } else {
                None
            };
            let access = plan::plan(table, scope, select.filter.as_ref(), order.as_ref());
            out.sorted &= !(access.ordered && order.is_some());
            out.sources.push(ExplainedSource {
                table: Arc::clone(table),
                label: from
                    .first
                    .alias
                    .clone()
                    .unwrap_or_else(|| table.name.clone()),
                access,
                join: None,
            });
        }
        return Ok(QueryResult::command("EXPLAIN"));
    }
    let window = Window {
        skip: usize::try_from(offset).unwrap_or(usize::MAX),
        limit,
        distinct: select.distinct,
    };
    let mut memory = WorkMemory::new();
    let bound_filter = match (&joined, &select.filter) {
        (Some(_), Some(filter)) => Some(aggregate::bind(filter, scope)?),
        _ => None,
    };
    let mut ordered = false;
    let rows = match (&joined, first.as_deref()) {
        (Some(joined), _) => {
            let first = joined.sources.list[0];
            let first_key = select
                .filter
                .as_ref()
                .filter(|_| !joined.preserves_right())
                .and_then(|filter| point_key(first.table, Scope::One(first), filter));
            RowSource::Joined(JoinedRows::new(
                reader,
                joined,
                first_key,
                joined.needed_columns(select),
                joined.filters_by_level(bound_filter.as_ref()),
                &mut tables.row_counts,
                &mut memory,
            )?)
        }
        (None, Some(table)) => {
            let order = if grouping.is_none() && select.source_row_limit.is_none() {
                order_request(select, &sort_keys, &projection, scope, table)
            } else {
                None
            };
            let access = planned_access(table, scope, select.filter.as_ref(), order.as_ref());
            ordered = access.ordered && order.is_some();
            RowSource::Table(table, access)
        }
        (None, None) => RowSource::Nothing,
    };
    let mut source = SourceRows {
        reader,
        rows,
        scope,
        filter: if joined.is_some() {
            None
        } else {
            select.filter.as_ref()
        },
        late,
        correlation,
    };
    if let Some(count) = select.source_row_limit {
        let wanted = usize::try_from(count).unwrap_or(usize::MAX);
        let kept = first_rows(|visit| source.for_each(visit), wanted, &mut memory)?;
        source = SourceRows {
            reader,
            rows: RowSource::Rows(kept),
            scope,
            filter: None,
            late: None,
            correlation,
        };
    }
    if let Some(grouping) = &grouping {
        let groups = grouping.run(
            scope,
            &mut |visit| source.for_each_borrowed(visit),
            &mut memory,
            correlation,
        )?;
        source = SourceRows {
            reader,
            rows: RowSource::Rows(groups),
            scope: post_scope,
            filter: None,
            late: None,
            correlation,
        };
    }
    let output = if sort_keys.is_empty() || (ordered && !limit.with_ties) {
        unsorted_rows(&mut source, &projection, window, &mut memory)?
    } else {
        sorted_rows(
            &mut source,
            &select.order_by,
            &projection,
            &sort_keys,
            window,
            &mut memory,
        )?
    };
    let tag = format!("SELECT {}", output.len());
    Ok(QueryResult::rows(tag, projection.columns, output))
}

fn order_request(
    select: &Select,
    sort_keys: &[SortKey<'_>],
    projection: &Projection<'_>,
    scope: Scope<'_>,
    table: &TableDefinition,
) -> Option<OrderRequest> {
    if sort_keys.is_empty() {
        return None;
    }
    let mut columns = Vec::with_capacity(sort_keys.len());
    for (item, key) in select.order_by.iter().zip(sort_keys) {
        let column = match key {
            SortKey::Output(index) => projection.source_column(*index)?,
            SortKey::Expr(expr) => match &expr.kind {
                ExprKind::Column { qualifier, name } => scope
                    .column(qualifier.as_deref(), name, expr.position)
                    .ok()?,
                _ => return None,
            },
        };
        let nulls_first = item.nulls_first.unwrap_or(item.descending);
        if nulls_first != item.descending && !table.columns.get(column)?.not_null {
            return None;
        }
        columns.push((column, item.descending));
    }
    Some(OrderRequest {
        columns,
        limited: select.limit.is_some(),
    })
}

pub(crate) fn first_rows(
    for_each: impl FnOnce(&mut dyn FnMut(Vec<Value>) -> SqlResult<bool>) -> SqlResult<()>,
    wanted: usize,
    memory: &mut WorkMemory,
) -> SqlResult<Vec<Vec<Value>>> {
    let mut kept = Vec::new();
    if wanted > 0 {
        for_each(&mut |row| {
            memory.charge_row(&row)?;
            kept.push(row);
            Ok(kept.len() < wanted)
        })?;
    }
    Ok(kept)
}

#[derive(Clone, Copy)]
struct Window {
    skip: usize,
    limit: RowLimit,
    distinct: bool,
}

impl Window {
    fn streamed_take(self) -> usize {
        if self.limit.percent.is_some() {
            usize::MAX
        } else {
            self.limit.rows_to_take(0)
        }
    }
}

enum RowSource<'a> {
    Nothing,
    Table(&'a TableDefinition, Access),
    Joined(JoinedRows<'a>),
    Rows(Vec<Vec<Value>>),
}

struct SourceRows<'a> {
    reader: &'a dyn KeyValueRead,
    rows: RowSource<'a>,
    scope: Scope<'a>,
    filter: Option<&'a Expr>,
    late: Option<&'a Expr>,
    correlation: Option<&'a Correlation<'a>>,
}

impl SourceRows<'_> {
    fn has_rows_to_read(&self) -> bool {
        !matches!(self.rows, RowSource::Nothing)
    }

    fn for_each(&mut self, visit: &mut dyn FnMut(Vec<Value>) -> SqlResult<bool>) -> SqlResult<()> {
        let Some(late) = self.late else {
            return self.for_each_early(visit);
        };
        let (scope, correlation) = (self.scope, self.correlation);
        self.for_each_early(&mut |row| {
            let evaluator = Evaluator {
                scope,
                row: &row,
                correlation,
            };
            if evaluator.boolean(late, "WHERE")? == Some(true) {
                visit(row)
            } else {
                Ok(true)
            }
        })
    }

    fn for_each_early(
        &mut self,
        visit: &mut dyn FnMut(Vec<Value>) -> SqlResult<bool>,
    ) -> SqlResult<()> {
        let (reader, scope, filter) = (self.reader, self.scope, self.filter);
        match &mut self.rows {
            RowSource::Table(table, access) => {
                for_each_matching_row(reader, table, scope, filter, access, &mut |_, row| {
                    visit(row)
                })
            }
            RowSource::Joined(joined) => joined.for_each(&mut |row| {
                if filter_matches(scope, filter, row)? {
                    visit(row.to_vec())
                } else {
                    Ok(true)
                }
            }),
            RowSource::Rows(rows) => {
                for row in std::mem::take(rows) {
                    if !visit(row)? {
                        break;
                    }
                }
                Ok(())
            }
            RowSource::Nothing => {
                if filter_matches(scope, filter, &[])? {
                    visit(Vec::new())?;
                }
                Ok(())
            }
        }
    }
}

impl SourceRows<'_> {
    fn for_each_borrowed(
        &mut self,
        visit: &mut dyn FnMut(&[Value]) -> SqlResult<bool>,
    ) -> SqlResult<()> {
        if self.late.is_some() {
            return self.for_each(&mut |row| visit(&row));
        }
        let (reader, scope, filter) = (self.reader, self.scope, self.filter);
        match &mut self.rows {
            RowSource::Joined(joined) => joined.for_each(&mut |row| {
                if filter_matches(scope, filter, row)? {
                    visit(row)
                } else {
                    Ok(true)
                }
            }),
            RowSource::Table(table, access) if access.path == Path::Scan => {
                let prefix = rows_prefix(table.id);
                let mut row = Vec::with_capacity(table.columns.len());
                reader.scan(
                    &prefix,
                    &prefix_end(&prefix),
                    ScanDirection::Forward,
                    &mut |_, bytes| {
                        row.clear();
                        decode_row_into(bytes, &table.columns, None, &mut row)?;
                        if filter_matches(scope, filter, &row)? {
                            visit(&row)
                        } else {
                            Ok(true)
                        }
                    },
                )
            }
            _ => self.for_each(&mut |row| visit(&row)),
        }
    }
}

fn first_time_seen(
    seen: &mut HashSet<Vec<u8>>,
    row: &[Value],
    memory: &mut WorkMemory,
) -> SqlResult<bool> {
    let mut key = Vec::new();
    for value in row {
        equality_key(&mut key, value);
    }
    if seen.contains(&key) {
        return Ok(false);
    }
    memory.charge(key.len() + 32)?;
    seen.insert(key);
    Ok(true)
}

fn unsorted_rows(
    source: &mut SourceRows<'_>,
    projection: &Projection<'_>,
    window: Window,
    memory: &mut WorkMemory,
) -> SqlResult<Vec<Vec<Value>>> {
    let mut output = Vec::new();
    let take = window.streamed_take();
    if take == 0 && source.has_rows_to_read() {
        return Ok(output);
    }
    let mut skipped = 0;
    let mut seen = window.distinct.then(HashSet::new);
    source.for_each(&mut |row| {
        let projected = projection.project_owned(row)?;
        if let Some(seen) = &mut seen
            && !first_time_seen(seen, &projected, memory)?
        {
            return Ok(true);
        }
        if skipped < window.skip {
            skipped += 1;
            return Ok(true);
        }
        if output.len() < take {
            memory.charge_row(&projected)?;
            output.push(projected);
        }
        Ok(output.len() < take)
    })?;
    if window.limit.percent.is_some() {
        output.truncate(window.limit.rows_to_take(skipped + output.len()));
    }
    Ok(output)
}

fn sorted_rows(
    source: &mut SourceRows<'_>,
    order_by: &[OrderItem],
    projection: &Projection<'_>,
    sort_keys: &[SortKey<'_>],
    window: Window,
    memory: &mut WorkMemory,
) -> SqlResult<Vec<Vec<Value>>> {
    let scope = source.scope;
    let correlation = source.correlation;
    let mut rows = Vec::new();
    let mut seen = window.distinct.then(HashSet::new);
    source.for_each(&mut |row| {
        let output = projection.project(&row)?;
        if let Some(seen) = &mut seen
            && !first_time_seen(seen, &output, memory)?
        {
            return Ok(true);
        }
        let evaluator = Evaluator {
            scope,
            row: &row,
            correlation,
        };
        let mut keys = Vec::with_capacity(sort_keys.len());
        let mut bytes = crate::memory::RowBytes::default();
        for value in &output {
            bytes.add(value)?;
        }
        for key in sort_keys {
            let value = match key {
                SortKey::Output(index) => output[*index].clone(),
                SortKey::Expr(expr) => evaluator.eval(expr, None)?,
            };
            bytes.add(&value)?;
            keys.push(value);
        }
        memory.charge_row(&output)?;
        memory.charge_row(&keys)?;
        rows.push((keys, output));
        Ok(true)
    })?;
    check_sortable(order_by, &rows)?;
    rows.sort_by(|(left, _), (right, _)| compare_sort_keys(order_by, left, right));
    let take = window.limit.rows_to_take(rows.len());
    let tied = |left: &Vec<Value>, right: &Vec<Value>| {
        compare_sort_keys(order_by, left, right) == Ordering::Equal
    };
    let ties: Option<&dyn Fn(&Vec<Value>, &Vec<Value>) -> bool> = if window.limit.with_ties {
        Some(&tied)
    } else {
        None
    };
    Ok(rows_within_limit(rows.into_iter(), window.skip, take, ties))
}

pub(crate) fn order_rows(rows: &mut Vec<Vec<Value>>, order_by: &[OrderItem], keys: &[usize]) {
    let mut keyed: Vec<(Vec<Value>, Vec<Value>)> = std::mem::take(rows)
        .into_iter()
        .map(|row| {
            let picked = keys
                .iter()
                .map(|&index| row.get(index).cloned().unwrap_or(Value::Null))
                .collect();
            (picked, row)
        })
        .collect();
    keyed.sort_by(|(left, _), (right, _)| compare_sort_keys(order_by, left, right));
    *rows = keyed.into_iter().map(|(_, row)| row).collect();
}

pub(crate) fn compare_sort_keys(
    order_by: &[OrderItem],
    left: &[Value],
    right: &[Value],
) -> Ordering {
    for (index, item) in order_by.iter().enumerate() {
        let nulls_first = item.nulls_first.unwrap_or(item.descending);
        let ordering = match (&left[index], &right[index]) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => {
                if nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (_, Value::Null) => {
                if nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (a, b) => {
                let ordering = compare_values(a, b);
                if item.descending {
                    ordering.reverse()
                } else {
                    ordering
                }
            }
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

fn check_sortable(order_by: &[OrderItem], rows: &[(Vec<Value>, Vec<Value>)]) -> SqlResult<()> {
    for (index, item) in order_by.iter().enumerate() {
        let mut seen: Option<&Value> = None;
        for (keys, _) in rows {
            let value = &keys[index];
            if value.is_null() {
                continue;
            }
            if let Some(previous) = seen {
                compare_truth(previous, value, "<")
                    .map_err(|error| error.at(item.expr.position))?;
            } else {
                seen = Some(value);
            }
        }
    }
    Ok(())
}

pub(crate) fn update(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    update: &Update,
) -> SqlResult<QueryResult> {
    if crate::updatable_views::is_view(writer, tables, &update.table)? {
        if let Some(result) = crate::triggers::update_instead_of_view(writer, tables, update)? {
            return Ok(result);
        }
        return crate::updatable_views::update(writer, tables, update);
    }
    let (prepared, subqueries) = if update_mentions_subquery(update) {
        let (prepared, subqueries) =
            with_subqueries_run(writer, tables, update, &update.table, false, update_exprs)?;
        (Some(prepared), Some(subqueries))
    } else {
        (None, None)
    };
    let update = prepared.as_ref().unwrap_or(update);
    let table = tables.require_target(writer, &update.table.name, "update")?;
    if crate::sequences::mentions_nextval(&table) {
        crate::sequences::load_all(writer)?;
    }
    let checks = PreparedChecks::of(&table)?;
    let rule = assignment_rule(constraints::current_dialect());
    let referenced = constraints::is_referenced(&table);
    let keep_old = !table.indexes.is_empty() || referenced || !table.foreign_keys.is_empty();
    let scope = scope_of(&table, &update.table);
    let names: Vec<Name> = update
        .assignments
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    let targets = target_columns(&table, &names).map_err(|error| {
        if error.state() == SqlState::DUPLICATE_COLUMN {
            SqlError::syntax(
                format!(
                    "multiple assignments to same column \"{}\"",
                    error
                        .message()
                        .trim_start_matches("column \"")
                        .split('"')
                        .next()
                        .unwrap_or("")
                ),
                error.position().unwrap_or(0),
            )
        } else {
            error
        }
    })?;
    for (_, value) in &update.assignments {
        if let InsertValue::Expr(expr) = value {
            reject_aggregates(expr, "UPDATE")?;
            check_columns(expr, scope)?;
        }
    }
    check_updated_to_default(&table, &targets, &update.assignments)?;
    let mut firing = Firing::for_table(&*writer, tables, &table, TriggerEvent::Update, &targets)?;
    let keep_old = keep_old || firing.is_some();
    if let Some(filter) = &update.filter {
        reject_aggregates(filter, "WHERE")?;
        check_columns(filter, scope)?;
    }
    let (early, late) = split_row_filter(update.filter.as_ref());
    let matched = matching_rows(writer, &table, scope, early.as_ref())?;
    let mut changes: Vec<(Vec<u8>, Vec<u8>, Vec<Value>)> = Vec::with_capacity(matched.len());
    let mut old_rows = Vec::with_capacity(if keep_old { matched.len() } else { 0 });
    let mut result_rows = Vec::new();
    let columns = {
        let reader: &dyn KeyValueRead = &*writer;
        let correlation = subqueries
            .as_ref()
            .filter(|found| found.is_correlated())
            .map(|found| found.correlation(reader));
        let mut returning = returning(&update.returning, scope)?;
        if let Some(projection) = &mut returning {
            projection.correlation = correlation.as_ref();
        }
        for (old_key, old_row) in matched {
            let evaluator = Evaluator {
                scope,
                row: &old_row,
                correlation: correlation.as_ref(),
            };
            if let Some(late) = &late
                && evaluator.boolean(late, "WHERE")? != Some(true)
            {
                continue;
            }
            let mut new_row = old_row.clone();
            for ((_, value), &index) in update.assignments.iter().zip(&targets) {
                new_row[index] = assigned_value(&evaluator, value, &table, index, rule)?;
            }
            check_not_null(&table, &new_row)?;
            if !checks.is_empty() {
                checks.check(&table, &new_row)?;
            }
            if let Some(projection) = &returning {
                result_rows.push(projection.project(&new_row)?);
            }
            let new_key = if table.primary_key.is_empty() {
                old_key.clone()
            } else {
                row_key(&table, &new_row)
            };
            if keep_old {
                old_rows.push(old_row);
            }
            changes.push((old_key, new_key, new_row));
        }
        returning.map(|projection| projection.columns)
    };
    if let Some(firing) = firing.as_mut()
        && firing.has_before()
    {
        let mut kept_changes = Vec::with_capacity(changes.len());
        let mut kept_old = Vec::with_capacity(old_rows.len());
        let mut kept_results = Vec::new();
        let mut projected = std::mem::take(&mut result_rows).into_iter();
        for ((old_key, _, mut new_row), old_row) in changes.into_iter().zip(old_rows) {
            let result_row = if columns.is_some() {
                projected.next()
            } else {
                None
            };
            if firing.before_row(writer, tables, Some(old_row.as_slice()), Some(&mut new_row))?
                == RowOutcome::Skip
            {
                continue;
            }
            check_not_null(&table, &new_row)?;
            if !checks.is_empty() {
                checks.check(&table, &new_row)?;
            }
            let new_key = if table.primary_key.is_empty() {
                old_key.clone()
            } else {
                row_key(&table, &new_row)
            };
            kept_changes.push((old_key, new_key, new_row));
            kept_old.push(old_row);
            kept_results.extend(result_row);
        }
        changes = kept_changes;
        old_rows = kept_old;
        result_rows = kept_results;
    }
    write_updated_rows(writer, tables, &table, &changes, &old_rows, &mut |_| Ok(()))?;
    if let Some(mut firing) = firing {
        for ((_, _, new_row), old_row) in changes.iter().zip(&old_rows) {
            firing.after_row(writer, tables, Some(old_row.as_slice()), Some(new_row.as_slice()))?;
        }
        firing.finish();
    }
    let tag = format!("UPDATE {}", changes.len());
    Ok(match columns {
        Some(columns) => QueryResult::rows(tag, columns, result_rows),
        None => QueryResult::command(tag),
    })
}

pub(crate) type RowUpdate = (Vec<u8>, Vec<u8>, Vec<Value>);

struct Upsert<'u> {
    updates: &'u [(usize, Expr)],
    inserted: &'u [Value],
    existing_key: &'u [u8],
}

pub(crate) fn write_updated_rows(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    table: &Arc<TableDefinition>,
    changes: &[RowUpdate],
    old_rows: &[Vec<Value>],
    on_written: &mut dyn FnMut(&[Value]) -> SqlResult<()>,
) -> SqlResult<()> {
    for (position, (old_key, new_key, _)) in changes.iter().enumerate() {
        if !table.indexes.is_empty()
            && let Some(old_row) = old_rows.get(position)
        {
            indexes::delete_entries(writer, table, old_row, old_key)?;
        }
        if old_key != new_key {
            writer.delete(old_key)?;
        }
    }
    for (old_key, new_key, new_row) in changes {
        if old_key != new_key && writer.get(new_key)?.is_some() {
            return Err(duplicate_key(table, new_row));
        }
        if table.indexes.is_empty() {
            writer.put(new_key, &encode_row(new_row))?;
        } else {
            put_row_checked(writer, table, new_row, new_key)?;
        }
        on_written(new_row)?;
    }
    if constraints::is_referenced(table) {
        let propagated: Vec<RowChange> = changes
            .iter()
            .zip(old_rows)
            .map(|((_, _, new_row), old_row)| RowChange {
                table: Arc::clone(table),
                old: old_row.clone(),
                new: Some(new_row.clone()),
            })
            .collect();
        constraints::propagate(writer, tables, propagated)?;
    }
    if !table.foreign_keys.is_empty() {
        for ((_, _, new_row), old_row) in changes.iter().zip(old_rows) {
            constraints::check_parents(writer, tables, table, new_row, Some(old_row))?;
        }
    }
    Ok(())
}

pub(crate) fn delete_rows(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    table: &Arc<TableDefinition>,
    rows: Vec<(Vec<u8>, Vec<Value>)>,
    on_deleted: &mut dyn FnMut(&[Value]) -> SqlResult<()>,
) -> SqlResult<usize> {
    for (key, row) in &rows {
        if !table.indexes.is_empty() {
            indexes::delete_entries(writer, table, row, key)?;
        }
        writer.delete(key)?;
        on_deleted(row)?;
    }
    let count = rows.len();
    if constraints::is_referenced(table) {
        let changes: Vec<RowChange> = rows
            .into_iter()
            .map(|(_, row)| RowChange {
                table: Arc::clone(table),
                old: row,
                new: None,
            })
            .collect();
        constraints::propagate(writer, tables, changes)?;
    }
    Ok(count)
}

pub(crate) fn every_row(
    reader: &dyn KeyValueRead,
    table: &TableDefinition,
) -> SqlResult<Vec<(Vec<u8>, Vec<Value>)>> {
    let reference = TableReference::named(
        Name {
            text: table.name.clone(),
            position: 0,
        },
        None,
    );
    matching_rows(reader, table, scope_of(table, &reference), None)
}

pub(crate) fn truncate(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    table: &TableReference,
) -> SqlResult<QueryResult> {
    let definition = tables.require_target(writer, &table.name, "truncate")?;
    if constraints::is_referenced(&definition) {
        for child in constraints::referencing_tables(writer, &definition.name)? {
            if child != definition.name {
                return Err(SqlError::not_supported(
                    "cannot truncate a table referenced in a foreign key constraint",
                )
                .with_detail(format!(
                    "Table \"{child}\" references \"{}\".",
                    definition.name
                ))
                .with_hint(format!(
                    "Truncate table \"{child}\" at the same time, or use TRUNCATE ... CASCADE."
                )));
            }
        }
    }
    let every_row = Delete {
        table: table.clone(),
        filter: None,
        returning: None,
    };
    delete_matching(writer, tables, &every_row, false)?;
    Ok(QueryResult::command("TRUNCATE TABLE".to_owned()))
}

pub(crate) fn delete(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    delete: &Delete,
) -> SqlResult<QueryResult> {
    delete_matching(writer, tables, delete, true)
}

fn delete_matching(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    delete: &Delete,
    fires_triggers: bool,
) -> SqlResult<QueryResult> {
    if crate::updatable_views::is_view(writer, tables, &delete.table)? {
        if fires_triggers
            && let Some(result) = crate::triggers::delete_instead_of_view(writer, tables, delete)?
        {
            return Ok(result);
        }
        return crate::updatable_views::delete(writer, tables, delete);
    }
    let (prepared, subqueries) = if delete.filter.iter().any(crate::subquery::mentions_subquery)
        || returning_mentions_subquery(&delete.returning)
    {
        let (prepared, subqueries) =
            with_subqueries_run(writer, tables, delete, &delete.table, false, delete_exprs)?;
        (Some(prepared), Some(subqueries))
    } else {
        (None, None)
    };
    let delete = prepared.as_ref().unwrap_or(delete);
    let table = tables.require_target(writer, &delete.table.name, "delete from")?;
    let scope = scope_of(&table, &delete.table);
    if let Some(filter) = &delete.filter {
        reject_aggregates(filter, "WHERE")?;
        check_columns(filter, scope)?;
    }
    let (early, late) = split_row_filter(delete.filter.as_ref());
    let mut matched = matching_rows(writer, &table, scope, early.as_ref())?;
    let mut result_rows = Vec::new();
    let columns = {
        let reader: &dyn KeyValueRead = &*writer;
        let correlation = subqueries
            .as_ref()
            .filter(|found| found.is_correlated())
            .map(|found| found.correlation(reader));
        let mut returning = returning(&delete.returning, scope)?;
        if let Some(projection) = &mut returning {
            projection.correlation = correlation.as_ref();
        }
        if let Some(late) = &late {
            let mut kept = Vec::with_capacity(matched.len());
            for (key, row) in matched {
                let evaluator = Evaluator {
                    scope,
                    row: &row,
                    correlation: correlation.as_ref(),
                };
                if evaluator.boolean(late, "WHERE")? == Some(true) {
                    kept.push((key, row));
                }
            }
            matched = kept;
        }
        if let Some(projection) = &returning {
            for (_, row) in &matched {
                result_rows.push(projection.project(row)?);
            }
        }
        returning.map(|projection| projection.columns)
    };
    let mut firing = if fires_triggers {
        Firing::for_table(&*writer, tables, &table, TriggerEvent::Delete, &[])?
    } else {
        None
    };
    if let Some(firing) = firing.as_mut()
        && firing.has_before()
    {
        let mut kept = Vec::with_capacity(matched.len());
        let mut kept_results = Vec::new();
        let mut projected = std::mem::take(&mut result_rows).into_iter();
        for (key, row) in matched {
            let result_row = if columns.is_some() {
                projected.next()
            } else {
                None
            };
            if firing.before_row(writer, tables, Some(row.as_slice()), None)? == RowOutcome::Skip {
                continue;
            }
            kept.push((key, row));
            kept_results.extend(result_row);
        }
        matched = kept;
        result_rows = kept_results;
    }
    let deleted: Vec<Vec<Value>> = match &firing {
        Some(_) => matched.iter().map(|(_, row)| row.clone()).collect(),
        None => Vec::new(),
    };
    let count = delete_rows(writer, tables, &table, matched, &mut |_| Ok(()))?;
    if let Some(mut firing) = firing {
        for row in &deleted {
            firing.after_row(writer, tables, Some(row.as_slice()), None)?;
        }
        firing.finish();
    }
    let tag = format!("DELETE {count}");
    Ok(match columns {
        Some(columns) => QueryResult::rows(tag, columns, result_rows),
        None => QueryResult::command(tag),
    })
}
