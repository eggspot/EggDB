use crate::ast::{
    ColumnDefinition, CreateTable, CreateTableAs, Expr, ExprKind, Insert, InsertSource,
    InsertValue, Name, OnCommit, OnDuplicate, Statement, TableReference,
};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{self, TableCache};
use crate::name_resolution::Binding;
use crate::session::{OutputColumn, QueryResult};
use crate::store::KeyValueWrite;
use crate::types::{SqlType, Value};

fn missing_column_name(position: usize) -> SqlError {
    SqlError::new(
        SqlState::INVALID_NAME,
        "An object or column name is missing or empty. For SELECT INTO statements, verify each column has a name. For other statements, look for empty alias names. Aliases defined as \"\" or [] are not allowed. Change the alias to a valid name.",
    )
    .with_sqlserver_code(1038)
    .at(position)
}

pub(crate) fn run(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    binding: &Binding<'_>,
    dialect: Dialect,
    create: &mut CreateTableAs,
) -> SqlResult<QueryResult> {
    let selected = selected_rows(writer, tables, binding, dialect, &mut create.query)?;
    let columns = selected.columns.unwrap_or_default();
    let mut definition = table_definition(create, &columns, dialect)?;
    binding.create_table(&*writer, tables, &mut definition)?;
    execute::create_table(writer, tables, &definition, dialect)?;
    let count = selected.rows.len();
    if count > 0 {
        let mut insert = Insert {
            table: TableReference::named(definition.name.clone(), None),
            columns: None,
            source: InsertSource::Values(selected.rows.into_iter().map(constant_row).collect()),
            returning: None,
            on_duplicate: OnDuplicate::Fail,
            conflict_target: None,
            rollback_on_conflict: false,
            keeps_changes_on_conflict: false,
            overriding: None,
        };
        let binding = Binding {
            settings: binding.settings,
            walk_expressions: binding.walk_expressions,
            target_privilege: None,
        };
        binding.insert(&*writer, tables, &mut insert)?;
        execute::insert(writer, tables, &insert)?;
    }
    Ok(QueryResult::command(format!("SELECT {count}")))
}

fn selected_rows(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    binding: &Binding<'_>,
    dialect: Dialect,
    query: &mut Statement,
) -> SqlResult<QueryResult> {
    let settings = binding.settings;
    match query {
        Statement::Select(select) => {
            binding.select(&*writer, tables, select)?;
            if dialect == Dialect::PostgreSql
                || crate::routines::select_calls_marked_routine(select)
            {
                crate::routines::prepare_select(
                    writer,
                    tables,
                    settings,
                    select,
                    crate::routines::Prefetch::Everything,
                )?;
            }
            execute::select(&*writer, tables, select, false)
        }
        Statement::Query(query) => {
            binding.query(&*writer, tables, query)?;
            if dialect == Dialect::PostgreSql || crate::routines::query_calls_marked_routine(query)
            {
                crate::routines::prepare_query(
                    writer,
                    tables,
                    settings,
                    query,
                    crate::routines::Prefetch::Everything,
                )?;
            }
            execute::query(&*writer, tables, query, false)
        }
        _ => Err(SqlError::new(
            SqlState::INTERNAL_ERROR,
            "a table was to be made from a statement that is not a query",
        )),
    }
}

fn table_definition(
    create: &CreateTableAs,
    columns: &[OutputColumn],
    dialect: Dialect,
) -> SqlResult<CreateTable> {
    let position = create.name.position;
    let mut definitions = Vec::with_capacity(columns.len());
    for column in columns {
        if dialect == Dialect::SqlServer && (column.name.is_empty() || column.name == "?column?") {
            return Err(missing_column_name(position));
        }
        definitions.push(ColumnDefinition {
            name: Name {
                text: column.name.clone(),
                position,
            },
            sql_type: column.sql_type.unwrap_or(SqlType::Text),
            not_null: false,
            primary_key: false,
            default: None,
            default_dialect: dialect,
            auto_increment: false,
            identity_seed: None,
            generated_always: false,
            unique: None,
            constraints: Vec::new(),
            comment: None,
            generated: None,
        });
    }
    Ok(CreateTable {
        name: create.name.clone(),
        temporary: create.temporary.then_some(OnCommit::PreserveRows),
        if_not_exists: false,
        columns: definitions,
        primary_key: None,
        primary_key_name: None,
        indexes: Vec::new(),
        constraints: Vec::new(),
        comment: None,
        partition_by: None,
        partition_of: None,
        sqlite_text: None,
    })
}

fn constant_row(row: Vec<Value>) -> Vec<InsertValue> {
    row.into_iter()
        .map(|value| {
            InsertValue::Expr(Expr {
                kind: ExprKind::Constant {
                    sql_type: value.sql_type(),
                    value,
                },
                position: 0,
            })
        })
        .collect()
}
