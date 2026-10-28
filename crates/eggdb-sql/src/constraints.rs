use crate::ast::{ConstraintSpec, Expr, ForeignKeySpec, ReferentialAction};
use crate::dialect::Dialect;
use crate::encoding::{
    CheckConstraint, ForeignKey, MAX_CONSTRAINTS, TableDefinition, decode_row, prefix_end,
    reference_key, references_prefix, rows_prefix,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope, Source, check_columns, reject_aggregates};
use crate::execute::{self, TableCache};
use crate::lexer::{TokenKind, swap_uniform_case, tokenize_in};
use crate::memory::WorkMemory;
use crate::parser::Parser;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
use crate::types::{SqlType, Value, compare_values};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::Arc;

const CHILD_BATCH: usize = 1024;
const MAX_CASCADE_DEPTH: usize = 100_000;

#[derive(Clone)]
struct Context {
    dialect: Dialect,
    foreign_keys: bool,
    database: String,
    owner: String,
}

thread_local! {
    static CONTEXT: RefCell<Option<Context>> = const { RefCell::new(None) };
}

pub(crate) struct ContextGuard(Option<Context>);

impl Drop for ContextGuard {
    fn drop(&mut self) {
        let previous = self.0.take();
        CONTEXT.with(|context| *context.borrow_mut() = previous);
    }
}

pub(crate) fn enter(
    dialect: Dialect,
    foreign_keys: bool,
    database: String,
    owner: String,
) -> ContextGuard {
    let context = Context {
        dialect,
        foreign_keys,
        database,
        owner,
    };
    ContextGuard(CONTEXT.with(|current| current.borrow_mut().replace(context)))
}

fn context() -> Context {
    CONTEXT.with(|context| {
        context.borrow().clone().unwrap_or(Context {
            dialect: Dialect::PostgreSql,
            foreign_keys: true,
            database: String::new(),
            owner: String::new(),
        })
    })
}

pub(crate) fn current_dialect() -> Dialect {
    context().dialect
}

fn foreign_keys_enforced() -> bool {
    context().foreign_keys
}

fn shown_values(row: &[Value], columns: &[usize]) -> String {
    columns
        .iter()
        .map(|&index| {
            row.get(index)
                .and_then(Value::to_text)
                .unwrap_or_else(|| "null".into())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn shown_row(row: &[Value]) -> String {
    row.iter()
        .map(|value| value.to_text().unwrap_or_else(|| "null".into()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn column_names(table: &TableDefinition, columns: &[usize]) -> Vec<String> {
    columns
        .iter()
        .filter_map(|&index| table.columns.get(index))
        .map(|column| column.name.clone())
        .collect()
}

#[cold]
#[inline(never)]
fn mysql_definition(child: &TableDefinition, key: &ForeignKey) -> String {
    let quote = |name: &str| format!("`{name}`");
    let columns: Vec<String> = column_names(child, &key.columns)
        .iter()
        .map(|name| quote(name))
        .collect();
    let parents: Vec<String> = key.parent_columns.iter().map(|name| quote(name)).collect();
    let context = context();
    format!(
        "(`{}`.`{}`, CONSTRAINT `{}` FOREIGN KEY ({}) REFERENCES `{}` ({}))",
        context.database,
        child.name,
        key.name,
        columns.join(", "),
        key.parent,
        parents.join(", ")
    )
}

fn oracle_name(name: &str) -> String {
    let context = context();
    format!("{}.{}", context.owner, swap_uniform_case(name))
}

#[cold]
#[inline(never)]
fn missing_parent(child: &TableDefinition, key: &ForeignKey, row: &[Value]) -> SqlError {
    let error = SqlError::new(
        SqlState::FOREIGN_KEY_VIOLATION,
        format!(
            "insert or update on table \"{}\" violates foreign key constraint \"{}\"",
            child.name, key.name
        ),
    );
    match context().dialect {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_INTEGRITY,
            format!(
                "Cannot add or update a child row: a foreign key constraint fails {}",
                mysql_definition(child, key)
            ),
        )
        .with_vendor_code(1452),
        Dialect::Sqlite => SqlError::new(
            SqlState::FOREIGN_KEY_VIOLATION,
            "FOREIGN KEY constraint failed",
        )
        .with_sqlite_code(787),
        Dialect::Oracle => SqlError::new(
            SqlState::FOREIGN_KEY_VIOLATION,
            format!(
                "integrity constraint ({}) violated - parent key not found",
                oracle_name(&key.name)
            ),
        )
        .with_oracle_code(2291),
        Dialect::PostgreSql | Dialect::SqlServer => error
            .with_detail(format!(
                "Key ({})=({}) is not present in table \"{}\".",
                column_names(child, &key.columns).join(", "),
                shown_values(row, &key.columns),
                key.parent
            ))
            .with_table(&child.name),
    }
}

#[cold]
#[inline(never)]
fn still_referenced(
    parent: &TableDefinition,
    child: &TableDefinition,
    key: &ForeignKey,
    parent_row: &[Value],
    parent_columns: &[usize],
) -> SqlError {
    match context().dialect {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_INTEGRITY,
            format!(
                "Cannot delete or update a parent row: a foreign key constraint fails {}",
                mysql_definition(child, key)
            ),
        )
        .with_vendor_code(1451),
        Dialect::Sqlite => SqlError::new(
            SqlState::FOREIGN_KEY_VIOLATION,
            "FOREIGN KEY constraint failed",
        )
        .with_sqlite_code(787),
        Dialect::Oracle => SqlError::new(
            SqlState::FOREIGN_KEY_VIOLATION,
            format!(
                "integrity constraint ({}) violated - child record found",
                oracle_name(&key.name)
            ),
        )
        .with_oracle_code(2292),
        Dialect::PostgreSql | Dialect::SqlServer => SqlError::new(
            SqlState::FOREIGN_KEY_VIOLATION,
            format!(
                "update or delete on table \"{}\" violates foreign key constraint \"{}\" on table \"{}\"",
                parent.name, key.name, child.name
            ),
        )
        .with_detail(format!(
            "Key ({})=({}) is still referenced from table \"{}\".",
            key.parent_columns.join(", "),
            shown_values(parent_row, parent_columns),
            child.name
        ))
        .with_table(&child.name),
    }
}

#[cold]
#[inline(never)]
fn check_failed(table: &TableDefinition, check: &CheckConstraint, row: &[Value]) -> SqlError {
    match context().dialect {
        Dialect::MySql => SqlError::new(
            SqlState::CHECK_VIOLATION,
            format!("Check constraint '{}' is violated.", check.name),
        )
        .with_vendor_code(3819),
        Dialect::Sqlite => SqlError::new(
            SqlState::CHECK_VIOLATION,
            format!("CHECK constraint failed: {}", check.name),
        )
        .with_sqlite_code(275),
        Dialect::Oracle => SqlError::new(
            SqlState::CHECK_VIOLATION,
            format!("check constraint ({}) violated", oracle_name(&check.name)),
        )
        .with_oracle_code(2290),
        Dialect::PostgreSql | Dialect::SqlServer => SqlError::new(
            SqlState::CHECK_VIOLATION,
            format!(
                "new row for relation \"{}\" violates check constraint \"{}\"",
                table.name, check.name
            ),
        )
        .with_detail(format!("Failing row contains ({}).", shown_row(row)))
        .with_table(&table.name),
    }
}

pub(crate) fn check_identifier(name: &crate::ast::Name) -> SqlResult<()> {
    if name.text.contains(crate::temp_tables::TEMP_TABLE_MARKER)
        || !crate::schemas::is_well_formed(&name.text)
    {
        return Err(SqlError::new(
            SqlState::INVALID_NAME,
            format!(
                "invalid name \"{}\"",
                crate::schemas::display(&name.text).escape_default()
            ),
        )
        .at(name.position));
    }
    Ok(())
}

pub(crate) fn parse_check(check: &CheckConstraint) -> SqlResult<Expr> {
    let damaged = || crate::encoding::corrupt("check constraint");
    let tokens = tokenize_in(&check.expression, check.dialect).map_err(|_| damaged())?;
    let mut parser = Parser::new(&check.expression, &tokens).in_dialect(check.dialect);
    let expr = parser.expression().map_err(|_| damaged())?;
    if !parser.at_end() {
        return Err(damaged());
    }
    Ok(expr)
}

fn table_scope(table: &TableDefinition) -> Scope<'_> {
    Scope::One(Source {
        table,
        label: &table.name,
        offset: 0,
    })
}

pub(crate) struct PreparedChecks {
    checks: Vec<(usize, Expr)>,
}

impl PreparedChecks {
    pub(crate) fn of(table: &TableDefinition) -> SqlResult<Self> {
        let mut checks = Vec::with_capacity(table.checks.len());
        for (position, check) in table.checks.iter().enumerate() {
            let expr = parse_check(check)?;
            check_columns(&expr, table_scope(table))
                .map_err(|_| crate::encoding::corrupt("check constraint"))?;
            checks.push((position, expr));
        }
        Ok(Self { checks })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.checks.is_empty()
    }

    pub(crate) fn check(&self, table: &TableDefinition, row: &[Value]) -> SqlResult<()> {
        for (position, expr) in &self.checks {
            let evaluator = Evaluator {
                scope: table_scope(table),
                row,
                correlation: None,
            };
            if evaluator.boolean(expr, "CHECK")? == Some(false) {
                return Err(check_failed(table, &table.checks[*position], row));
            }
        }
        Ok(())
    }
}

pub(crate) fn referencing_tables(
    reader: &dyn KeyValueRead,
    parent: &str,
) -> SqlResult<Vec<String>> {
    let prefix = references_prefix(parent);
    let mut children = Vec::new();
    let mut memory = WorkMemory::new();
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |key, _| {
            memory.charge(key.len())?;
            children.push(String::from_utf8_lossy(&key[prefix.len()..]).into_owned());
            Ok(true)
        },
    )?;
    Ok(children)
}

pub(crate) fn register_references(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    child: &TableDefinition,
) -> SqlResult<()> {
    for key in &child.foreign_keys {
        writer.put(&reference_key(&key.parent, &child.name), &[])?;
    }
    refresh_parents(writer, tables, child)
}

pub(crate) fn unregister_references(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    child: &TableDefinition,
) -> SqlResult<()> {
    for key in &child.foreign_keys {
        writer.delete(&reference_key(&key.parent, &child.name))?;
    }
    refresh_parents(writer, tables, child)
}

fn refresh_parents(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    child: &TableDefinition,
) -> SqlResult<()> {
    let mut done: Vec<&str> = Vec::new();
    for key in &child.foreign_keys {
        if !done.contains(&key.parent.as_str()) {
            done.push(&key.parent);
            refresh_flags(writer, tables, &key.parent)?;
        }
    }
    Ok(())
}

pub(crate) fn flags_of(reader: &dyn KeyValueRead, name: &str) -> SqlResult<(bool, bool)> {
    let referenced = !referencing_tables(reader, name)?.is_empty();
    let owns_sequence = crate::sequences::every_sequence(reader)?
        .iter()
        .any(|(_, sequence)| {
            sequence
                .owned_by
                .as_ref()
                .is_some_and(|(table, _)| table == name)
        });
    Ok((referenced, owns_sequence))
}

pub(crate) fn refresh_flags(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
) -> SqlResult<()> {
    let Some(table) = tables.table(writer, name)? else {
        return Ok(());
    };
    let (referenced, owns_sequence) = flags_of(writer, name)?;
    if table.referenced != referenced || table.owns_sequence != owns_sequence {
        let mut table = (*table).clone();
        table.referenced = referenced;
        table.owns_sequence = owns_sequence;
        writer.put(&crate::encoding::table_key(name), &table.encode())?;
    }
    Ok(())
}

struct ChildLink {
    child: Arc<TableDefinition>,
    key: usize,
    parent_columns: Vec<usize>,
}

fn child_links(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    parent: &TableDefinition,
) -> SqlResult<Vec<ChildLink>> {
    let mut links = Vec::new();
    for name in referencing_tables(reader, &parent.name)? {
        let child = if name == parent.name {
            Some(Arc::new(parent.clone()))
        } else {
            tables.table(reader, &name)?
        };
        let Some(child) = child else {
            continue;
        };
        for (position, key) in child.foreign_keys.iter().enumerate() {
            if key.parent != parent.name {
                continue;
            }
            let Some(parent_columns) = key
                .parent_columns
                .iter()
                .map(|name| parent.column_index(name))
                .collect::<Option<Vec<usize>>>()
            else {
                return Err(crate::encoding::corrupt("foreign key"));
            };
            links.push(ChildLink {
                child: Arc::clone(&child),
                key: position,
                parent_columns,
            });
        }
    }
    Ok(links)
}

fn key_values(row: &[Value], columns: &[usize]) -> Option<Vec<Value>> {
    let values: Vec<Value> = columns
        .iter()
        .map(|&index| row.get(index).cloned().unwrap_or(Value::Null))
        .collect();
    if values.iter().any(Value::is_null) {
        None
    } else {
        Some(values)
    }
}

fn parent_has_key(
    reader: &dyn KeyValueRead,
    parent: &TableDefinition,
    parent_columns: &[usize],
    values: &[Value],
) -> SqlResult<bool> {
    let pairs: Vec<(usize, Value)> = parent_columns
        .iter()
        .copied()
        .zip(values.iter().cloned())
        .collect();
    let mut found = false;
    execute::for_each_row_with(reader, parent, &pairs, &mut |_, _| {
        found = true;
        Ok(false)
    })?;
    Ok(found)
}

fn check_parent(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    child: &TableDefinition,
    key: &ForeignKey,
    row: &[Value],
) -> SqlResult<()> {
    let Some(values) = key_values(row, &key.columns) else {
        return Ok(());
    };
    let parent = if key.parent == child.name {
        Some(Arc::new(child.clone()))
    } else {
        tables.table(reader, &key.parent)?
    };
    let Some(parent) = parent else {
        return Err(missing_parent(child, key, row));
    };
    let Some(parent_columns) = key
        .parent_columns
        .iter()
        .map(|name| parent.column_index(name))
        .collect::<Option<Vec<usize>>>()
    else {
        return Err(crate::encoding::corrupt("foreign key"));
    };
    if parent_has_key(reader, &parent, &parent_columns, &values)? {
        Ok(())
    } else {
        Err(missing_parent(child, key, row))
    }
}

pub(crate) fn check_parents(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    child: &TableDefinition,
    row: &[Value],
    only_changed_from: Option<&[Value]>,
) -> SqlResult<()> {
    if child.foreign_keys.is_empty() || !foreign_keys_enforced() {
        return Ok(());
    }
    for key in &child.foreign_keys {
        if let Some(old) = only_changed_from
            && key
                .columns
                .iter()
                .all(|&index| same_value(old.get(index), row.get(index)))
        {
            continue;
        }
        check_parent(reader, tables, child, key, row)?;
    }
    Ok(())
}

fn same_value(left: Option<&Value>, right: Option<&Value>) -> bool {
    match (left, right) {
        (Some(Value::Null), Some(Value::Null)) => true,
        (Some(Value::Null), _) | (_, Some(Value::Null)) => false,
        (Some(left), Some(right)) => {
            left == right || compare_values(left, right) == std::cmp::Ordering::Equal
        }
        _ => false,
    }
}

pub(crate) struct RowChange {
    pub(crate) table: Arc<TableDefinition>,
    pub(crate) old: Vec<Value>,
    pub(crate) new: Option<Vec<Value>>,
}

pub(crate) fn is_referenced(table: &TableDefinition) -> bool {
    foreign_keys_enforced() && table.referenced
}

pub(crate) fn propagate(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    changes: Vec<RowChange>,
) -> SqlResult<()> {
    if !foreign_keys_enforced() || changes.is_empty() {
        return Ok(());
    }
    let mut memory = WorkMemory::new();
    let mut queue: VecDeque<(RowChange, usize)> = VecDeque::new();
    for change in changes {
        memory.charge_row(&change.old)?;
        queue.push_back((change, 0));
    }
    let mut links_of: Vec<(String, Arc<Vec<ChildLink>>)> = Vec::new();
    while let Some((change, depth)) = queue.pop_front() {
        if depth > MAX_CASCADE_DEPTH {
            return Err(SqlError::new(
                SqlState::STATEMENT_TOO_COMPLEX,
                format!("foreign key actions cascade more than {MAX_CASCADE_DEPTH} levels deep"),
            ));
        }
        let links = match links_of.iter().find(|(name, _)| *name == change.table.name) {
            Some((_, links)) => Arc::clone(links),
            None => {
                let links = Arc::new(child_links(writer, tables, &change.table)?);
                links_of.push((change.table.name.clone(), Arc::clone(&links)));
                links
            }
        };
        for link in links.iter() {
            apply_action(
                writer,
                tables,
                &change,
                link,
                depth,
                &mut queue,
                &mut memory,
            )?;
        }
        memory.release(WorkMemory::row_bytes(&change.old));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_action(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    change: &RowChange,
    link: &ChildLink,
    depth: usize,
    queue: &mut VecDeque<(RowChange, usize)>,
    memory: &mut WorkMemory,
) -> SqlResult<()> {
    let key = &link.child.foreign_keys[link.key];
    let Some(old_key) = key_values(&change.old, &link.parent_columns) else {
        return Ok(());
    };
    let new_key = match &change.new {
        Some(new) => {
            if link
                .parent_columns
                .iter()
                .all(|&index| same_value(change.old.get(index), new.get(index)))
            {
                return Ok(());
            }
            Some(
                link.parent_columns
                    .iter()
                    .map(|&index| new.get(index).cloned().unwrap_or(Value::Null))
                    .collect::<Vec<Value>>(),
            )
        }
        None => None,
    };
    if parent_has_key(writer, &change.table, &link.parent_columns, &old_key)? {
        return Ok(());
    }
    let action = if change.new.is_some() {
        key.on_update
    } else {
        key.on_delete
    };
    let child = &link.child;
    if action == ReferentialAction::SetDefault && crate::sequences::mentions_nextval(child) {
        crate::sequences::load_all(writer)?;
    }
    let pairs: Vec<(usize, Value)> = key.columns.iter().copied().zip(old_key).collect();
    let checks = if matches!(
        action,
        ReferentialAction::Cascade | ReferentialAction::SetNull | ReferentialAction::SetDefault
    ) && change.new.is_some()
        || matches!(
            action,
            ReferentialAction::SetNull | ReferentialAction::SetDefault
        ) {
        Some(PreparedChecks::of(child)?)
    } else {
        None
    };
    loop {
        let mut batch: Vec<(Vec<u8>, Vec<Value>)> = Vec::new();
        let mut charged = 0;
        execute::for_each_row_with(writer, child, &pairs, &mut |row_key, row| {
            let size = WorkMemory::row_bytes(&row) + row_key.len();
            memory.charge(size)?;
            charged += size;
            batch.push((row_key.to_vec(), row));
            Ok(batch.len() < CHILD_BATCH)
        })?;
        if batch.is_empty() {
            return Ok(());
        }
        for (row_key, row) in batch {
            match action {
                ReferentialAction::NoAction | ReferentialAction::Restrict => {
                    return Err(still_referenced(
                        &change.table,
                        child,
                        key,
                        &change.old,
                        &link.parent_columns,
                    ));
                }
                ReferentialAction::Cascade if change.new.is_none() => {
                    execute::delete_stored_row(writer, child, &row_key, &row)?;
                    memory.charge_row(&row)?;
                    queue.push_back((
                        RowChange {
                            table: Arc::clone(child),
                            old: row,
                            new: None,
                        },
                        depth + 1,
                    ));
                }
                _ => {
                    let mut updated = row.clone();
                    for (position, &column) in key.columns.iter().enumerate() {
                        updated[column] = match (action, &new_key) {
                            (ReferentialAction::Cascade, Some(values)) => execute::assign_to(
                                values[position].clone(),
                                &child.columns[column],
                            )?,
                            (ReferentialAction::SetDefault, _) => {
                                execute::default_of(&child.columns[column])?
                            }
                            _ => Value::Null,
                        };
                    }
                    execute::check_not_null_of(child, &updated)?;
                    if let Some(checks) = &checks {
                        checks.check(child, &updated)?;
                    }
                    execute::replace_stored_row(writer, child, &row_key, &row, &updated)?;
                    if action == ReferentialAction::SetDefault {
                        check_parent(writer, tables, child, key, &updated)?;
                    }
                    memory.charge_row(&row)?;
                    queue.push_back((
                        RowChange {
                            table: Arc::clone(child),
                            old: row,
                            new: Some(updated),
                        },
                        depth + 1,
                    ));
                }
            }
        }
        memory.release(charged);
    }
}

fn generated_name(table: &TableDefinition, base: String) -> String {
    if !table.constraint_name_taken(&base) {
        return base;
    }
    let mut number = 1u32;
    loop {
        let candidate = format!("{base}{number}");
        if !table.constraint_name_taken(&candidate) {
            return candidate;
        }
        number = number.saturating_add(1);
    }
}

#[cold]
#[inline(never)]
fn duplicate_constraint(table: &TableDefinition, name: &str, dialect: Dialect) -> SqlError {
    match dialect {
        Dialect::MySql => SqlError::new(
            SqlState::DUPLICATE_OBJECT,
            format!("Duplicate check constraint name '{name}'."),
        )
        .with_vendor_code(3822),
        Dialect::Oracle => SqlError::new(
            SqlState::DUPLICATE_OBJECT,
            "name already used by an existing constraint",
        )
        .with_oracle_code(2264),
        _ => SqlError::new(
            SqlState::DUPLICATE_OBJECT,
            format!(
                "constraint \"{name}\" for relation \"{}\" already exists",
                table.name
            ),
        ),
    }
}

#[cold]
#[inline(never)]
fn too_many_constraints(table: &TableDefinition) -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!(
            "table \"{}\" can have at most {MAX_CONSTRAINTS} constraints of each kind",
            table.name
        ),
    )
}

pub(crate) fn define_check(
    table: &mut TableDefinition,
    spec: &ConstraintSpec,
    expr: &Expr,
    text: &str,
    dialect: Dialect,
) -> SqlResult<usize> {
    if let Some(name) = &spec.name {
        check_identifier(name)?;
    }
    if table.checks.len() >= MAX_CONSTRAINTS {
        return Err(too_many_constraints(table));
    }
    reject_aggregates(expr, "check constraints").map_err(|error| {
        if error.state() == SqlState::GROUPING_ERROR {
            SqlError::new(
                SqlState::GROUPING_ERROR,
                "aggregate functions are not allowed in check constraints",
            )
            .at(expr.position)
        } else {
            error
        }
    })?;
    if crate::subquery::mentions_subquery(expr) {
        return Err(
            SqlError::not_supported("cannot use subquery in check constraint").at(expr.position),
        );
    }
    check_columns(expr, table_scope(table))?;
    let name = match &spec.name {
        Some(name) => {
            if table.constraint_name_taken(&name.text) {
                return Err(duplicate_constraint(table, &name.text, dialect).at(name.position));
            }
            name.text.clone()
        }
        None if dialect == Dialect::Sqlite => generated_name(table, text.trim().to_owned()),
        None => {
            let column = spec
                .column
                .as_ref()
                .map(|column| column.text.clone())
                .or_else(|| first_column_named(expr, table));
            let table_name = crate::schemas::base_name(&table.name);
            let base = match (column, dialect) {
                (Some(column), Dialect::SqlServer) => format!("ck__{table_name}__{column}"),
                (None, Dialect::SqlServer) => format!("ck__{table_name}"),
                (Some(column), _) => format!("{table_name}_{column}_check"),
                (None, _) => format!("{table_name}_check"),
            };
            generated_name(table, base)
        }
    };
    table.checks.push(CheckConstraint {
        name,
        dialect,
        expression: text.trim().to_owned(),
    });
    Ok(table.checks.len() - 1)
}

fn first_column_named(expr: &Expr, table: &TableDefinition) -> Option<String> {
    if let crate::ast::ExprKind::Column { name, .. } = &expr.kind
        && table.column_index(name).is_some()
    {
        return Some(name.clone());
    }
    crate::eval::children(expr)
        .into_iter()
        .find_map(|child| first_column_named(child, table))
}

#[cold]
#[inline(never)]
fn invalid_key(message: String) -> SqlError {
    SqlError::new(SqlState::INVALID_FOREIGN_KEY, message)
}

pub(crate) fn compatible(child: SqlType, parent: SqlType) -> bool {
    child == parent
        || (child.is_numeric() && parent.is_numeric())
        || (child.is_character() && parent.is_character())
        || std::mem::discriminant(&child) == std::mem::discriminant(&parent)
}

#[cold]
#[inline(never)]
pub(crate) fn incompatible_key(
    name: &str,
    (child, child_type): (&str, SqlType),
    (parent, parent_type): (&str, SqlType),
    dialect: Dialect,
) -> SqlError {
    match dialect {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_GENERAL,
            format!(
                "Referencing column '{child}' and referenced column '{parent}' in foreign key constraint '{name}' are incompatible."
            ),
        )
        .with_vendor_code(3780),
        Dialect::Oracle => SqlError::new(
            SqlState::DATATYPE_MISMATCH,
            "column type incompatible with referenced column type",
        )
        .with_oracle_code(2267),
        _ => SqlError::new(
            SqlState::DATATYPE_MISMATCH,
            format!("foreign key constraint \"{name}\" cannot be implemented"),
        )
        .with_detail(format!(
            "Key columns \"{child}\" and \"{parent}\" are of incompatible types: {} and {}.",
            child_type.name(),
            parent_type.name()
        )),
    }
}

pub(crate) fn define_foreign_key(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    table: &mut TableDefinition,
    spec: &ConstraintSpec,
    key: &ForeignKeySpec,
    dialect: Dialect,
) -> SqlResult<usize> {
    if let Some(name) = &spec.name {
        check_identifier(name)?;
    }
    if table.foreign_keys.len() >= MAX_CONSTRAINTS {
        return Err(too_many_constraints(table));
    }
    let parent: Arc<TableDefinition> = if key.parent.text == table.name {
        Arc::new(table.clone())
    } else {
        match tables.table(reader, &key.parent.text)? {
            Some(parent) => parent,
            None => {
                return Err(SqlError::new(
                    SqlState::UNDEFINED_TABLE,
                    format!("relation \"{}\" does not exist", key.parent.text),
                )
                .at(key.parent.position));
            }
        }
    };
    if key.columns.len() > crate::encoding::MAX_INDEX_COLUMNS {
        return Err(SqlError::new(
            SqlState::TOO_MANY_COLUMNS,
            format!(
                "cannot have more than {} keys in a foreign key",
                crate::encoding::MAX_INDEX_COLUMNS
            ),
        )
        .at(spec.position));
    }
    let mut columns = Vec::with_capacity(key.columns.len());
    for name in &key.columns {
        let index = table.column_index(&name.text).ok_or_else(|| {
            SqlError::new(
                SqlState::UNDEFINED_COLUMN,
                format!(
                    "column \"{}\" referenced in foreign key constraint does not exist",
                    name.text
                ),
            )
            .at(name.position)
        })?;
        if columns.contains(&index) {
            return Err(SqlError::new(
                SqlState::DUPLICATE_COLUMN,
                "foreign key referenced-columns list must not contain duplicates".to_owned(),
            )
            .at(name.position));
        }
        columns.push(index);
    }
    let parent_columns: Vec<usize> = match &key.parent_columns {
        Some(names) => {
            let mut found = Vec::with_capacity(names.len());
            for name in names {
                let index = parent.column_index(&name.text).ok_or_else(|| {
                    SqlError::new(
                        SqlState::UNDEFINED_COLUMN,
                        format!(
                            "column \"{}\" referenced in foreign key constraint does not exist",
                            name.text
                        ),
                    )
                    .at(name.position)
                })?;
                if found.contains(&index) {
                    return Err(invalid_key(
                        "foreign key referenced-columns list must not contain duplicates"
                            .to_owned(),
                    )
                    .at(name.position));
                }
                found.push(index);
            }
            found
        }
        None => {
            if parent.primary_key.is_empty() {
                return Err(invalid_key(format!(
                    "there is no primary key for referenced table \"{}\"",
                    parent.name
                ))
                .at(key.parent.position));
            }
            parent.primary_key.clone()
        }
    };
    if parent_columns.len() != columns.len() {
        return Err(invalid_key(
            "number of referencing and referenced columns for foreign key disagree".to_owned(),
        )
        .at(spec.position));
    }
    let mut sorted = parent_columns.clone();
    sorted.sort_unstable();
    let matches_key = |key: &[usize]| {
        let mut key = key.to_vec();
        key.sort_unstable();
        key == sorted
    };
    let unique = (!parent.primary_key.is_empty() && matches_key(&parent.primary_key))
        || parent
            .indexes
            .iter()
            .any(|index| index.unique && matches_key(&index.columns));
    if !unique {
        return Err(invalid_key(format!(
            "there is no unique constraint matching given keys for referenced table \"{}\"",
            parent.name
        ))
        .at(key.parent.position));
    }
    let name = match &spec.name {
        Some(name) => {
            if table.constraint_name_taken(&name.text) {
                return Err(duplicate_constraint(table, &name.text, dialect).at(name.position));
            }
            name.text.clone()
        }
        None => {
            let names = column_names(table, &columns).join("_");
            generated_name(
                table,
                format!("{}_{names}_fkey", crate::schemas::base_name(&table.name)),
            )
        }
    };
    for (&child_column, &parent_column) in columns.iter().zip(&parent_columns) {
        let child_type = table.columns[child_column].sql_type;
        let parent_type = parent.columns[parent_column].sql_type;
        if !compatible(child_type, parent_type) {
            return Err(incompatible_key(
                &name,
                (&table.columns[child_column].name, child_type),
                (&parent.columns[parent_column].name, parent_type),
                dialect,
            ));
        }
    }
    for action in [key.on_delete, key.on_update] {
        if action == ReferentialAction::SetNull
            && columns
                .iter()
                .any(|&column| table.primary_key.contains(&column))
            && dialect == Dialect::MySql
        {
            return Err(SqlError::new(
                SqlState::MYSQL_GENERAL,
                format!(
                    "Column '{}' cannot be NOT NULL: needed in a foreign key constraint '{name}' SET NULL",
                    table.columns[columns[0]].name
                ),
            )
            .with_vendor_code(1830));
        }
    }
    table.foreign_keys.push(ForeignKey {
        name,
        columns,
        parent: parent.name.clone(),
        parent_columns: parent_columns
            .iter()
            .map(|&index| parent.columns[index].name.clone())
            .collect(),
        on_delete: key.on_delete,
        on_update: key.on_update,
        deferrable: key.deferrable,
    });
    Ok(table.foreign_keys.len() - 1)
}

pub(crate) fn validate_existing(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    table: &TableDefinition,
    check: Option<usize>,
    foreign_key: Option<usize>,
) -> SqlResult<()> {
    let checks = match check {
        Some(position) => {
            let expr = parse_check(&table.checks[position])?;
            Some((position, expr))
        }
        None => None,
    };
    let key = foreign_key.map(|position| &table.foreign_keys[position]);
    if checks.is_none() && (key.is_none() || !foreign_keys_enforced()) {
        return Ok(());
    }
    let prefix = rows_prefix(table.id);
    let end = prefix_end(&prefix);
    let mut start = prefix;
    loop {
        let mut batch: Vec<Vec<Value>> = Vec::new();
        let mut last = None;
        writer.scan(
            &start,
            &end,
            ScanDirection::Forward,
            &mut |row_key, bytes| {
                batch.push(decode_row(bytes, &table.columns)?);
                last = Some(row_key.to_vec());
                Ok(batch.len() < CHILD_BATCH)
            },
        )?;
        let Some(mut next) = last else {
            return Ok(());
        };
        for row in &batch {
            if let Some((position, expr)) = &checks {
                let evaluator = Evaluator {
                    scope: table_scope(table),
                    row,
                    correlation: None,
                };
                if evaluator.boolean(expr, "CHECK")? == Some(false) {
                    let check = &table.checks[*position];
                    return Err(match context().dialect {
                        Dialect::PostgreSql => SqlError::new(
                            SqlState::CHECK_VIOLATION,
                            format!(
                                "check constraint \"{}\" of relation \"{}\" is violated by some row",
                                check.name, table.name
                            ),
                        )
                        .with_table(&table.name),
                        Dialect::Oracle => SqlError::new(
                            SqlState::CHECK_VIOLATION,
                            format!(
                                "cannot validate ({}) - check constraint violated",
                                oracle_name(&check.name)
                            ),
                        )
                        .with_oracle_code(2293),
                        _ => check_failed(table, check, row),
                    });
                }
            }
            if let Some(key) = key
                && foreign_keys_enforced()
            {
                check_parent(writer, tables, table, key, row)?;
            }
        }
        if batch.len() < CHILD_BATCH {
            return Ok(());
        }
        next.push(0);
        start = next;
    }
}

pub(crate) fn renamed_in_text(text: &str, dialect: Dialect, from: &str, to: &str) -> String {
    let Ok(tokens) = tokenize_in(text, dialect) else {
        return text.to_owned();
    };
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (index, token) in tokens.iter().enumerate() {
        let names_it = match &token.kind {
            TokenKind::Word(word) => word.as_ref() == from,
            TokenKind::QuotedIdentifier(name) => name == from,
            _ => false,
        };
        let called = tokens
            .get(index + 1)
            .is_some_and(|next| next.kind == TokenKind::LeftParen);
        if names_it && !called && token.start >= at && token.end <= text.len() {
            out.push_str(&text[at..token.start]);
            out.push_str(&quoted_in(to, dialect));
            at = token.end;
        }
    }
    out.push_str(&text[at..]);
    out
}

fn quoted_in(name: &str, dialect: Dialect) -> String {
    match dialect {
        Dialect::MySql => format!("`{}`", name.replace('`', "``")),
        Dialect::SqlServer => format!("[{}]", name.replace(']', "]]")),
        Dialect::Oracle => {
            let shown = swap_uniform_case(name);
            format!("\"{}\"", shown.replace('"', "\"\""))
        }
        _ => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}
