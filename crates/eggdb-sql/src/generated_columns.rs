use crate::ast::{Expr, ExprKind, GenerationSpec};
use crate::dialect::Dialect;
use crate::encoding::{GeneratedColumn, TableDefinition};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope, Source};
use crate::lexer::tokenize_in;
use crate::parser::Parser;
use crate::types::{SqlType, Value};

impl TableDefinition {
    pub(crate) fn generation_of(&self, column: usize) -> Option<&GeneratedColumn> {
        self.generated.iter().find(|entry| entry.column == column)
    }

    pub(crate) fn is_generated(&self, column: usize) -> bool {
        self.generation_of(column).is_some()
    }
}

fn table_scope(table: &TableDefinition) -> Scope<'_> {
    Scope::One(Source {
        table,
        label: &table.name,
        offset: 0,
    })
}

pub(crate) fn parse(entry: &GeneratedColumn) -> SqlResult<Expr> {
    let damaged = || crate::encoding::corrupt("generated column");
    let tokens = tokenize_in(&entry.expression, entry.dialect).map_err(|_| damaged())?;
    let mut parser = Parser::new(&entry.expression, &tokens).in_dialect(entry.dialect);
    let expr = parser.expression().map_err(|_| damaged())?;
    if !parser.at_end() {
        return Err(damaged());
    }
    Ok(expr)
}

fn referenced_columns(expr: &Expr, table: &TableDefinition, found: &mut Vec<usize>) {
    if let ExprKind::Column { name, .. } = &expr.kind
        && let Some(position) = table.column_index(name)
        && !found.contains(&position)
    {
        found.push(position);
    }
    for child in crate::eval::children(expr) {
        referenced_columns(child, table, found);
    }
}

pub(crate) fn columns_of(table: &TableDefinition, entry: &GeneratedColumn) -> Vec<usize> {
    let mut found = Vec::new();
    if let Ok(expr) = parse(entry) {
        referenced_columns(&expr, table, &mut found);
    }
    found
}

pub(crate) fn dependents_of(table: &TableDefinition, column: usize) -> Vec<usize> {
    table
        .generated
        .iter()
        .filter(|entry| entry.column != column && columns_of(table, entry).contains(&column))
        .map(|entry| entry.column)
        .collect()
}

pub(crate) struct Generator {
    steps: Vec<(usize, Expr, Dialect)>,
}

impl Generator {
    pub(crate) fn of(table: &TableDefinition) -> SqlResult<Option<Self>> {
        if table.generated.is_empty() {
            return Ok(None);
        }
        let mut pending = Vec::with_capacity(table.generated.len());
        for entry in &table.generated {
            let expr = parse(entry)?;
            let mut columns = Vec::new();
            referenced_columns(&expr, table, &mut columns);
            columns.retain(|&column| table.is_generated(column));
            pending.push((entry.column, expr, entry.dialect, columns));
        }
        let steps = ordered(pending).ok_or_else(|| crate::encoding::corrupt("generated column"))?;
        Ok(Some(Self { steps }))
    }

    pub(crate) fn fill(&self, table: &TableDefinition, row: &mut [Value]) -> SqlResult<()> {
        let rule = crate::execute::assignment_rule;
        for (column, expr, dialect) in &self.steps {
            let Some(definition) = table.columns.get(*column) else {
                continue;
            };
            let evaluated = Evaluator {
                scope: table_scope(table),
                row: &*row,
                correlation: None,
            }
            .eval(expr, Some(definition.sql_type))?;
            let value = rule(*dialect)(evaluated, definition)?;
            if let Some(slot) = row.get_mut(*column) {
                *slot = value;
            }
        }
        Ok(())
    }
}

type PendingStep = (usize, Expr, Dialect, Vec<usize>);

fn ordered(mut pending: Vec<PendingStep>) -> Option<Vec<(usize, Expr, Dialect)>> {
    let mut steps: Vec<(usize, Expr, Dialect)> = Vec::with_capacity(pending.len());
    while !pending.is_empty() {
        let ready = pending.iter().position(|(column, _, _, needs)| {
            needs
                .iter()
                .all(|need| need != column && steps.iter().any(|(done, _, _)| done == need))
        })?;
        let (column, expr, dialect, _) = pending.remove(ready);
        steps.push((column, expr, dialect));
    }
    Some(steps)
}

pub(crate) fn define(
    table: &mut TableDefinition,
    specs: &[(usize, &GenerationSpec)],
    dialect: Dialect,
) -> SqlResult<()> {
    for &(column, spec) in specs {
        if table
            .columns
            .get(column)
            .is_some_and(|found| found.default.is_some())
        {
            return Err(both_default_and_generation(table, column, dialect).at(spec.position));
        }
        table.generated.push(GeneratedColumn {
            column,
            dialect,
            expression: spec.text.clone(),
            stored: spec.stored,
        });
    }
    table.generated.sort_by_key(|entry| entry.column);
    for &(column, spec) in specs {
        check_expression(table, column, spec, dialect)?;
        if !spec.type_given {
            let inferred = crate::typing::static_type(&spec.expr, table_scope(table))
                .or_else(|| (dialect == Dialect::Oracle).then_some(SqlType::Numeric(None)))
                .ok_or_else(|| {
                    SqlError::not_supported(
                        "a computed column whose type EggDB cannot work out from its expression",
                    )
                    .with_hint(
                        "Cast the expression to the type it gives, as in CAST(a + b AS int).",
                    )
                    .at(spec.position)
                })?;
            if let Some(found) = table.columns.get_mut(column) {
                found.sql_type = inferred;
            }
        }
    }
    if Generator::of(table).is_err() {
        let column = specs.first().map_or(0, |&(column, _)| column);
        let position = specs.first().map_or(0, |&(_, spec)| spec.position);
        return Err(loop_error(table, column, dialect).at(position));
    }
    Ok(())
}

fn column_name(table: &TableDefinition, column: usize) -> &str {
    table
        .columns
        .get(column)
        .map_or("", |found| found.name.as_str())
}

fn table_name(table: &TableDefinition) -> String {
    crate::schemas::base_name(&table.name).to_owned()
}

fn check_expression(
    table: &TableDefinition,
    column: usize,
    spec: &GenerationSpec,
    dialect: Dialect,
) -> SqlResult<()> {
    let expr = &spec.expr;
    let position = spec.position;
    if crate::subquery::mentions_subquery(expr) {
        return Err(disallowed(table, column, dialect, Disallowed::Subquery).at(position));
    }
    if crate::eval::reject_aggregates(expr, "column generation expressions").is_err() {
        return Err(disallowed(table, column, dialect, Disallowed::Aggregate).at(position));
    }
    let changing = crate::partitions::any_part(expr, &crate::partitions::changes_between_calls);
    if changing && (dialect != Dialect::SqlServer || spec.stored) {
        return Err(disallowed(table, column, dialect, Disallowed::Volatile).at(position));
    }
    crate::eval::check_columns(expr, table_scope(table))?;
    let mut referenced = Vec::new();
    referenced_columns(expr, table, &mut referenced);
    for &other in &referenced {
        if table
            .columns
            .get(other)
            .is_some_and(|found| found.auto_increment)
            && dialect == Dialect::MySql
        {
            return Err(SqlError::new(
                SqlState::MYSQL_GENERAL,
                format!(
                    "Generated column '{}' cannot refer to auto-increment column.",
                    column_name(table, column)
                ),
            )
            .with_vendor_code(3109)
            .at(position));
        }
        if !table.is_generated(other) {
            continue;
        }
        let refused = match dialect {
            Dialect::MySql => other >= column,
            Dialect::Sqlite => other == column,
            _ => true,
        };
        if refused {
            return Err(generated_reference(table, column, other, dialect).at(position));
        }
    }
    Ok(())
}

enum Disallowed {
    Subquery,
    Aggregate,
    Volatile,
}

fn disallowed(
    table: &TableDefinition,
    column: usize,
    dialect: Dialect,
    what: Disallowed,
) -> SqlError {
    let name = column_name(table, column);
    match dialect {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_GENERAL,
            format!("Expression of generated column '{name}' contains a disallowed function."),
        )
        .with_vendor_code(3763),
        Dialect::Sqlite => SqlError::new(
            SqlState::INVALID_OBJECT_DEFINITION,
            match what {
                Disallowed::Subquery => "subqueries prohibited in generated columns",
                Disallowed::Aggregate => "misuse of aggregate function in a generated column",
                Disallowed::Volatile => "non-deterministic functions prohibited in generated columns",
            },
        )
        .with_sqlite_code(1),
        Dialect::SqlServer => match what {
            Disallowed::Subquery => SqlError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "Subqueries are not allowed in this context. Only scalar expressions are allowed.",
            )
            .with_sqlserver_code(1046),
            Disallowed::Aggregate => SqlError::new(
                SqlState::GROUPING_ERROR,
                "An aggregate may not appear in a computed column expression or check constraint.",
            )
            .with_sqlserver_code(175),
            Disallowed::Volatile => SqlError::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                format!(
                    "Computed column '{name}' in table '{}' cannot be persisted because the column is non-deterministic.",
                    table_name(table)
                ),
            )
            .with_sqlserver_code(4936),
        },
        Dialect::Oracle => SqlError::new(
            SqlState::INVALID_OBJECT_DEFINITION,
            "only pure functions can be specified in a virtual column expression",
        )
        .with_oracle_code(54002),
        Dialect::PostgreSql => match what {
            Disallowed::Subquery => {
                SqlError::not_supported("cannot use subquery in column generation expression")
            }
            Disallowed::Aggregate => SqlError::new(
                SqlState::GROUPING_ERROR,
                "aggregate functions are not allowed in column generation expressions",
            ),
            Disallowed::Volatile => SqlError::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                "generation expression is not immutable",
            ),
        },
    }
}

fn generated_reference(
    table: &TableDefinition,
    column: usize,
    other: usize,
    dialect: Dialect,
) -> SqlError {
    let name = column_name(table, column);
    let other_name = column_name(table, other);
    match dialect {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_GENERAL,
            "Generated column can refer only to generated columns defined prior to it.",
        )
        .with_vendor_code(3107),
        Dialect::Sqlite => loop_error(table, column, dialect),
        Dialect::SqlServer => SqlError::new(
            SqlState::INVALID_OBJECT_DEFINITION,
            format!(
                "Computed column '{other_name}' in table '{}' is not allowed to be used in another computed-column definition.",
                table_name(table)
            ),
        )
        .with_sqlserver_code(1759),
        Dialect::Oracle => SqlError::new(
            SqlState::INVALID_OBJECT_DEFINITION,
            "virtual column is referenced in a column expression",
        )
        .with_oracle_code(54012),
        Dialect::PostgreSql => SqlError::new(
            SqlState::INVALID_OBJECT_DEFINITION,
            format!("cannot use generated column \"{other_name}\" in column generation expression"),
        )
        .with_detail(if other == column {
            format!("Column \"{name}\" cannot reference itself.")
        } else {
            "A generated column cannot reference another generated column.".to_owned()
        }),
    }
}

fn loop_error(table: &TableDefinition, column: usize, dialect: Dialect) -> SqlError {
    let error = SqlError::new(
        SqlState::INVALID_OBJECT_DEFINITION,
        format!(
            "generated column loop on \"{}\"",
            column_name(table, column)
        ),
    );
    if dialect == Dialect::Sqlite {
        error.with_sqlite_code(1)
    } else {
        error
    }
}

fn both_default_and_generation(
    table: &TableDefinition,
    column: usize,
    dialect: Dialect,
) -> SqlError {
    let name = column_name(table, column);
    match dialect {
        Dialect::Sqlite => SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            "cannot use DEFAULT on a generated column",
        )
        .with_sqlite_code(1),
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_GENERAL,
            format!("Incorrect usage of DEFAULT and generated column '{name}'"),
        )
        .with_vendor_code(1221),
        _ => SqlError::new(
            SqlState::SYNTAX_ERROR,
            format!("both default and generation expression specified for column \"{name}\""),
        ),
    }
}

pub(crate) fn written(table: &TableDefinition, column: usize, updating: bool) -> SqlError {
    let name = column_name(table, column);
    match crate::constraints::current_dialect() {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_GENERAL,
            format!(
                "The value specified for generated column '{name}' in table '{}' is not allowed.",
                table_name(table)
            ),
        )
        .with_vendor_code(3105),
        Dialect::Sqlite => SqlError::new(
            SqlState::GENERATED_ALWAYS,
            if updating {
                format!("cannot UPDATE generated column \"{name}\"")
            } else {
                format!("cannot INSERT into generated column \"{name}\"")
            },
        )
        .with_sqlite_code(1),
        Dialect::SqlServer => SqlError::new(
            SqlState::GENERATED_ALWAYS,
            format!(
                "The column \"{name}\" cannot be modified because it is either a computed column or is the result of a UNION operator."
            ),
        )
        .with_sqlserver_code(271),
        Dialect::Oracle if updating => SqlError::new(
            SqlState::GENERATED_ALWAYS,
            "UPDATE operation disallowed on virtual columns",
        )
        .with_oracle_code(54017),
        Dialect::Oracle => SqlError::new(
            SqlState::GENERATED_ALWAYS,
            "INSERT operation disallowed on virtual columns",
        )
        .with_oracle_code(54013),
        Dialect::PostgreSql => SqlError::new(
            SqlState::GENERATED_ALWAYS,
            if updating {
                format!("column \"{name}\" can only be updated to DEFAULT")
            } else {
                format!("cannot insert a non-DEFAULT value into column \"{name}\"")
            },
        )
        .with_detail(format!("Column \"{name}\" is a generated column.")),
    }
}

pub(crate) fn refuses_default() -> bool {
    crate::constraints::current_dialect() == Dialect::SqlServer
}

pub(crate) fn hidden_from_column_list(dialect: Dialect) -> bool {
    matches!(dialect, Dialect::SqlServer | Dialect::Sqlite)
}

pub(crate) fn check_insert(
    table: &TableDefinition,
    targets: &[usize],
    given: impl Fn(usize) -> Option<bool>,
) -> SqlResult<()> {
    if table.generated.is_empty() || crate::constraints::keeps_identity_values() {
        return Ok(());
    }
    for (slot, &target) in targets.iter().enumerate() {
        if !table.is_generated(target) {
            continue;
        }
        let refused = match given(slot) {
            Some(true) => true,
            Some(false) => refuses_default(),
            None => false,
        };
        if refused {
            return Err(written(table, target, false));
        }
    }
    Ok(())
}

pub(crate) fn dropped_dependency(
    table: &TableDefinition,
    column: usize,
    dependent: usize,
    dialect: Dialect,
) -> SqlError {
    let name = column_name(table, column);
    let dependent_name = column_name(table, dependent);
    let table_name = table_name(table);
    match dialect {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_GENERAL,
            format!("Column '{name}' has a generated column dependency."),
        )
        .with_vendor_code(3108),
        Dialect::Sqlite => SqlError::new(
            SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
            format!("error in table {table_name} after drop column: no such column: {name}"),
        )
        .with_sqlite_code(1),
        Dialect::SqlServer => SqlError::new(
            SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
            format!("The column '{dependent_name}' is dependent on column '{name}'."),
        )
        .with_sqlserver_code(5074),
        Dialect::Oracle => SqlError::new(
            SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
            "column to be dropped is used in a virtual column expression",
        )
        .with_oracle_code(54031),
        Dialect::PostgreSql => SqlError::new(
            SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
            format!(
                "cannot drop column {name} of table {table_name} because other objects depend on it"
            ),
        )
        .with_detail(format!(
            "column {dependent_name} of table {table_name} depends on column {name}"
        ))
        .with_hint("Use DROP ... CASCADE to drop the dependent objects too."),
    }
}

pub(crate) fn virtual_index_refused(table: &TableDefinition, columns: &[usize]) -> SqlResult<()> {
    let refused = columns.iter().any(|&column| {
        table
            .generation_of(column)
            .is_some_and(|entry| !entry.stored && entry.dialect == Dialect::PostgreSql)
    });
    if refused {
        return Err(SqlError::not_supported(
            "indexes on virtual generated columns are not supported",
        ));
    }
    Ok(())
}

pub(crate) fn sqlite_primary_key_refused(table: &TableDefinition) -> SqlResult<()> {
    if table
        .primary_key
        .iter()
        .any(|&column| table.is_generated(column))
    {
        return Err(SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            "generated columns cannot be part of the PRIMARY KEY",
        )
        .with_sqlite_code(1));
    }
    Ok(())
}
