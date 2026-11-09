use crate::ast::{
    ColumnDefinition, CreateTable, CreateTableAs, CreateTableAsSpelling, Expr, ExprKind, Insert,
    InsertSource, InsertValue, Literal, Name, OnCommit, Select, SelectItem, SetExpr, Statement,
    TableReference,
};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{self, TableCache};
use crate::name_resolution::Binding;
use crate::session::{OutputColumn, QueryResult};
use crate::store::{KeyValueRead, KeyValueWrite};
use crate::type_affinity::TypeAffinity;
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
    if create.if_not_exists && exists_already(writer, tables, binding, create)? {
        return Ok(skipped(create, dialect));
    }
    if !create.with_data {
        without_rows(&mut create.query);
    }
    let selected = selected_rows(writer, tables, binding, dialect, &mut create.query)?;
    let columns = selected.columns.unwrap_or_default();
    let kept = if matches!(
        dialect,
        Dialect::MySql | Dialect::Oracle | Dialect::SqlServer
    ) {
        kept_not_null(&*writer, tables, &create.query, columns.len())
    } else {
        vec![false; columns.len()]
    };
    let shape = TableShape {
        columns: &columns,
        kept_not_null: &kept,
        query: &create.query,
    };
    let mut definition = table_definition(create, &shape, dialect)?;
    let inserted_columns = create
        .declared
        .as_ref()
        .map(|_| definition_names(create, &columns));
    binding.create_table(&*writer, tables, &mut definition)?;
    crate::sqlite_schema_text::create_table(writer, tables, &definition, dialect)?;
    let count = selected.rows.len();
    if count > 0 {
        let mut insert = Insert {
            table: TableReference::named(definition.name.clone(), None),
            columns: inserted_columns,
            source: InsertSource::Values(selected.rows.into_iter().map(constant_row).collect()),
            returning: None,
            on_duplicate: create.on_duplicate.clone(),
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
    Ok(QueryResult::command(completion_tag(create, dialect, count)))
}

fn completion_tag(create: &CreateTableAs, dialect: Dialect, count: usize) -> String {
    if create.spelling == CreateTableAsSpelling::SelectInto {
        return format!("SELECT {count}");
    }
    if !create.with_data {
        return "CREATE TABLE AS".to_owned();
    }
    if dialect == Dialect::MySql {
        return format!("INSERT 0 {count}");
    }
    format!("SELECT {count}")
}

fn exists_already(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    binding: &Binding<'_>,
    create: &CreateTableAs,
) -> SqlResult<bool> {
    let mut probe = empty_table(create);
    binding.create_table(&*writer, tables, &mut probe)?;
    crate::sequences::relation_exists(&*writer, &probe.name.text)
}

fn skipped(create: &CreateTableAs, dialect: Dialect) -> QueryResult {
    let tag = if dialect == Dialect::PostgreSql {
        "CREATE TABLE AS"
    } else {
        "CREATE TABLE"
    };
    QueryResult::command(tag).with_notice(format!(
        "NOTICE:  relation \"{}\" already exists, skipping",
        create.name.text
    ))
}

fn without_rows(query: &mut Statement) {
    let zero = Expr {
        kind: ExprKind::Literal(Literal::Integer(0)),
        position: 0,
    };
    match query {
        Statement::Select(select) => select.limit = Some(zero),
        Statement::Query(query) => query.limit = Some(zero),
        _ => {}
    }
}

fn first_select(query: &Statement) -> Option<&Select> {
    match query {
        Statement::Select(select) => Some(select),
        Statement::Query(query) if query.with.is_empty() => match &query.body {
            SetExpr::Select(select) => Some(select),
            SetExpr::Operation(_) | SetExpr::Query(_) => None,
        },
        _ => None,
    }
}

pub(crate) fn unnamed_oracle_expression(
    query: &Statement,
    names_given: usize,
    unaliased: &[usize],
) -> Option<usize> {
    let select = first_select(query)?;
    select
        .items
        .iter()
        .skip(names_given)
        .find_map(|item| match item {
            SelectItem::Expr { expr, .. }
                if !matches!(expr.kind, ExprKind::Column { .. })
                    && unaliased.contains(&expr.position) =>
            {
                Some(expr.position)
            }
            _ => None,
        })
}

fn kept_not_null(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    query: &Statement,
    width: usize,
) -> Vec<bool> {
    let mut kept = vec![false; width];
    let Some(select) = first_select(query) else {
        return kept;
    };
    let Some(from) = &select.from else {
        return kept;
    };
    if !from.joins.is_empty()
        || from.first.derived.is_some()
        || from.first.generated.is_some()
        || select.grouping_sets.is_some()
    {
        return kept;
    }
    let Ok(definition) = tables.require(reader, &from.first.name) else {
        return kept;
    };
    let visible: Vec<&crate::encoding::Column> = definition
        .columns
        .iter()
        .filter(|column| !crate::temp_tables::is_hidden_column(&column.name))
        .collect();
    let mut index = 0;
    for item in &select.items {
        match item {
            SelectItem::Wildcard { .. } => {
                for column in &visible {
                    if let Some(slot) = kept.get_mut(index) {
                        *slot = column.not_null;
                    }
                    index += 1;
                }
            }
            SelectItem::Expr { expr, .. } => {
                if let ExprKind::Column { name, .. } = &expr.kind
                    && let Some(column) = visible
                        .iter()
                        .find(|column| column.name.eq_ignore_ascii_case(name))
                    && let Some(slot) = kept.get_mut(index)
                {
                    *slot = column.not_null;
                }
                index += 1;
            }
        }
    }
    kept
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

struct TableShape<'a> {
    columns: &'a [OutputColumn],
    kept_not_null: &'a [bool],
    query: &'a Statement,
}

fn empty_table(create: &CreateTableAs) -> CreateTable {
    CreateTable {
        name: create.name.clone(),
        temporary: create
            .temporary
            .then_some(create.on_commit.unwrap_or(OnCommit::PreserveRows)),
        if_not_exists: false,
        columns: Vec::new(),
        primary_key: None,
        primary_key_name: None,
        indexes: Vec::new(),
        constraints: Vec::new(),
        comment: None,
        partition_by: None,
        partition_of: None,
        sqlite_text: None,
    }
}

fn definition_names(create: &CreateTableAs, columns: &[OutputColumn]) -> Vec<Name> {
    columns
        .iter()
        .enumerate()
        .map(|(index, column)| match create.column_names.get(index) {
            Some(name) => name.clone(),
            None => Name {
                text: column.name.clone(),
                position: create.name.position,
            },
        })
        .collect()
}

fn checked_column_names(create: &CreateTableAs, width: usize, dialect: Dialect) -> SqlResult<()> {
    let given = create.column_names.len();
    if given == 0 {
        return Ok(());
    }
    let position = create
        .column_names
        .first()
        .map_or(create.name.position, |name| name.position);
    if dialect == Dialect::Oracle && given != width {
        return Err(SqlError::new(
            SqlState::SYNTAX_ERROR,
            "invalid number of column names specified",
        )
        .with_oracle_code(1730)
        .at(position));
    }
    if given > width {
        return Err(SqlError::syntax(
            "too many column names were specified",
            position,
        ));
    }
    Ok(())
}

fn table_definition(
    create: &CreateTableAs,
    shape: &TableShape<'_>,
    dialect: Dialect,
) -> SqlResult<CreateTable> {
    let position = create.name.position;
    checked_column_names(create, shape.columns.len(), dialect)?;
    let names = definition_names(create, shape.columns);
    let expressions = item_expressions(shape.query, shape.columns.len());
    let mut definitions = Vec::with_capacity(shape.columns.len());
    let mut sqlite_columns = Vec::with_capacity(shape.columns.len());
    for (index, (column, name)) in shape.columns.iter().zip(names).enumerate() {
        if create.spelling == CreateTableAsSpelling::SelectInto
            && dialect == Dialect::SqlServer
            && (name.text.is_empty() || name.text == "?column?")
        {
            return Err(missing_column_name(position));
        }
        let sql_type = if dialect == Dialect::Sqlite {
            let (declared, sql_type) =
                sqlite_declared_type(expressions.get(index).copied().flatten(), column.sql_type);
            sqlite_columns.push((name.text.clone(), declared));
            sql_type
        } else {
            column.sql_type.unwrap_or(SqlType::Text)
        };
        definitions.push(ColumnDefinition {
            name: Name {
                text: name.text,
                position,
            },
            sql_type,
            not_null: shape.kept_not_null.get(index).copied().unwrap_or(false),
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
    let mut definition = empty_table(create);
    if dialect == Dialect::Sqlite {
        definition.sqlite_text = Some(sqlite_create_text(
            crate::schemas::base_name(&create.name.text),
            &sqlite_columns,
        ));
    }
    match &create.declared {
        Some(declared) => {
            definition.columns = merged_columns(declared, definitions);
            definition.primary_key = declared.primary_key.clone();
            definition.primary_key_name = declared.primary_key_name.clone();
            definition.indexes = declared.indexes.clone();
            definition.constraints = declared.constraints.clone();
            definition.comment = declared.comment.clone();
        }
        None => definition.columns = definitions,
    }
    Ok(definition)
}

fn merged_columns(
    declared: &CreateTable,
    selected: Vec<ColumnDefinition>,
) -> Vec<ColumnDefinition> {
    let same = |left: &ColumnDefinition, right: &ColumnDefinition| {
        left.name.text.eq_ignore_ascii_case(&right.name.text)
    };
    let mut merged: Vec<ColumnDefinition> = declared
        .columns
        .iter()
        .filter(|column| !selected.iter().any(|chosen| same(column, chosen)))
        .cloned()
        .collect();
    for column in selected {
        match declared
            .columns
            .iter()
            .find(|written| same(written, &column))
        {
            Some(written) => merged.push(written.clone()),
            None => merged.push(column),
        }
    }
    merged
}

fn item_expressions(query: &Statement, width: usize) -> Vec<Option<&Expr>> {
    let Some(select) = first_select(query) else {
        return vec![None; width];
    };
    let mut expressions = Vec::with_capacity(width);
    for item in &select.items {
        match item {
            SelectItem::Wildcard { .. } => return vec![None; width],
            SelectItem::Expr { expr, .. } => expressions.push(Some(expr)),
        }
    }
    expressions
}

fn is_sqlite_real_literal(expr: &Expr) -> bool {
    matches!(
        &expr.kind,
        ExprKind::Cast { expr: inner, sql_type: SqlType::Double }
            if inner.position == expr.position
                && matches!(inner.kind, ExprKind::Literal(crate::ast::Literal::Decimal(_)))
    )
}

fn sqlite_declared_type(expr: Option<&Expr>, sql_type: Option<SqlType>) -> (&'static str, SqlType) {
    let affinity_of = match expr.map(|expr| &expr.kind) {
        None | Some(ExprKind::Column { .. }) => sql_type,
        Some(_) if expr.is_some_and(is_sqlite_real_literal) => None,
        Some(ExprKind::Cast { sql_type, .. }) => Some(*sql_type),
        Some(_) => None,
    };
    match affinity_of {
        Some(SqlType::SmallInt | SqlType::Integer | SqlType::BigInt) => ("INT", SqlType::BigInt),
        Some(
            SqlType::Text
            | SqlType::Varchar(_)
            | SqlType::Bpchar(_)
            | SqlType::Dynamic(TypeAffinity::Text),
        ) => ("TEXT", SqlType::Dynamic(TypeAffinity::Text)),
        Some(SqlType::Real | SqlType::Double | SqlType::Dynamic(TypeAffinity::Real)) => {
            ("REAL", SqlType::Dynamic(TypeAffinity::Real))
        }
        Some(
            SqlType::Numeric(_)
            | SqlType::Boolean
            | SqlType::Date
            | SqlType::Timestamp(_)
            | SqlType::TimestampTz(_)
            | SqlType::Dynamic(TypeAffinity::Numeric),
        ) => ("NUM", SqlType::Dynamic(TypeAffinity::Numeric)),
        _ => ("", SqlType::Dynamic(TypeAffinity::Blob)),
    }
}

fn sqlite_identifier_length(name: &str) -> usize {
    name.len() + name.matches('"').count() + 2
}

fn sqlite_identifier(name: &str) -> String {
    let plain = !name.is_empty()
        && !name.starts_with(|first: char| first.is_ascii_digit())
        && name
            .chars()
            .all(|letter| letter.is_ascii_alphanumeric() || letter == '_' || !letter.is_ascii());
    if plain {
        return name.to_owned();
    }
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The text SQLite keeps in `sqlite_schema` for a table made by `CREATE TABLE ... AS`: each
/// column with the declared type its expression's affinity gives, on one line when the names
/// are short and one column per line otherwise.
fn sqlite_create_text(table: &str, columns: &[(String, &'static str)]) -> String {
    let length = columns
        .iter()
        .map(|(name, _)| sqlite_identifier_length(name) + 5)
        .sum::<usize>()
        + sqlite_identifier_length(table);
    let (start, separator, end) = if length < 50 {
        ("", ",", ")")
    } else {
        ("\n  ", ",\n  ", "\n)")
    };
    let listed: Vec<String> = columns
        .iter()
        .map(|(name, declared)| {
            if declared.is_empty() {
                sqlite_identifier(name)
            } else {
                format!("{} {declared}", sqlite_identifier(name))
            }
        })
        .collect();
    format!(
        "CREATE TABLE {}({start}{}{end}",
        sqlite_identifier(table),
        listed.join(separator)
    )
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
