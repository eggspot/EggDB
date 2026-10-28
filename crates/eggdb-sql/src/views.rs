use crate::ast::{AlterView, AlterViewAction, CreateView, Name, Query, SelectItem, SetExpr};
use crate::dialect::Dialect;
use crate::encoding::{
    ViewColumn, ViewDefinition, comments_prefix, dependencies_prefix, dependency_key, prefix_end,
    table_key,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{Relation, TableCache};
use crate::name_resolution::{self, Resolution};
use crate::schemas::{self, base_name, display};
use crate::session::{OutputColumn, QueryResult};
use crate::settings::Settings;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
use std::collections::HashSet;

const DEPENDENCY_VALUE: &[u8] = &[1];
const CASCADE_BATCH: usize = 256;

pub(crate) fn not_a_table(name: &str) -> SqlError {
    SqlError::new(
        SqlState::WRONG_OBJECT_TYPE,
        format!("\"{}\" is not a table", display(name)),
    )
    .with_hint("Use DROP VIEW or ALTER VIEW for a view.")
}

fn not_a_view(name: &str) -> SqlError {
    SqlError::new(
        SqlState::WRONG_OBJECT_TYPE,
        format!("\"{}\" is not a view", display(name)),
    )
    .with_hint("Use DROP TABLE to remove a table.")
}

pub(crate) fn not_updatable(name: &str, verb: &str) -> SqlError {
    SqlError::not_supported(format!("cannot {verb} view \"{}\"", display(name)))
        .with_hint("Views are read-only in this release: change the view's tables directly.")
}

fn duplicate_relation(name: &Name, internal: &str, dialect: Dialect) -> SqlError {
    let shown = base_name(internal);
    match dialect {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_TABLE_EXISTS,
            format!("Table '{shown}' already exists"),
        )
        .with_vendor_code(1050),
        Dialect::Oracle => SqlError::new(
            SqlState::DUPLICATE_TABLE,
            "name is already used by an existing object",
        )
        .with_oracle_code(955),
        Dialect::Sqlite => SqlError::new(
            SqlState::DUPLICATE_TABLE,
            format!("view {shown} already exists"),
        )
        .with_sqlite_code(1),
        Dialect::PostgreSql | Dialect::SqlServer => SqlError::new(
            SqlState::DUPLICATE_TABLE,
            format!("relation \"{shown}\" already exists"),
        ),
    }
    .at(name.position)
}

fn missing_view(name: &Name, dialect: Dialect) -> SqlError {
    match dialect {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_TABLE_NOT_FOUND,
            format!("Unknown table '{}'", display(&name.text)),
        )
        .with_vendor_code(1051),
        Dialect::Oracle => SqlError::new(SqlState::UNDEFINED_TABLE, "table or view does not exist")
            .with_oracle_code(942),
        Dialect::Sqlite => SqlError::new(
            SqlState::UNDEFINED_TABLE,
            format!("no such view: {}", display(&name.text)),
        )
        .with_sqlite_code(1),
        Dialect::PostgreSql | Dialect::SqlServer => SqlError::new(
            SqlState::UNDEFINED_TABLE,
            format!("view \"{}\" does not exist", display(&name.text)),
        ),
    }
    .at(name.position)
}

fn selects_wildcard(query: &Query) -> bool {
    let mut pending = vec![&query.body];
    while let Some(body) = pending.pop() {
        match body {
            SetExpr::Select(select) => {
                if select
                    .items
                    .iter()
                    .any(|item| matches!(item, SelectItem::Wildcard { .. }))
                {
                    return true;
                }
            }
            SetExpr::Operation(operation) => {
                pending.push(&operation.left);
                pending.push(&operation.right);
            }
            SetExpr::Query(inner) => pending.push(&inner.body),
        }
    }
    false
}

pub(crate) struct ViewContext<'a> {
    pub(crate) settings: Option<&'a Settings>,
    pub(crate) dialect: Dialect,
}

pub(crate) fn describe(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    view: &str,
    query: &mut Query,
    path: Vec<String>,
    context: &ViewContext<'_>,
) -> SqlResult<(Vec<OutputColumn>, Resolution)> {
    let resolution =
        name_resolution::resolve_view_body(reader, tables, view, query, path, context.settings)?;
    let result = crate::execute::query(reader, tables, query, true)?;
    Ok((result.columns.unwrap_or_default(), resolution))
}

pub(crate) fn create_view(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    create: &CreateView,
    context: &ViewContext<'_>,
) -> SqlResult<QueryResult> {
    let dialect = context.dialect;
    if dialect == Dialect::SqlServer {
        return Err(SqlError::not_supported(
            "Views in the SQL Server dialect are not supported in this version of EggDB.",
        ));
    }
    let result = QueryResult::command("CREATE VIEW");
    if create.text.len() > crate::encoding::MAX_VIEW_TEXT {
        return Err(SqlError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!(
                "a view's query can be at most {} bytes",
                crate::encoding::MAX_VIEW_TEXT
            ),
        )
        .at(create.name.position));
    }
    let internal = schemas::creation_name(writer, &create.name)?;
    crate::constraints::check_identifier(&Name {
        text: internal.clone(),
        position: create.name.position,
    })?;
    let existing = match tables.relation(writer, &internal)? {
        Some(Relation::View(view)) => Some(view),
        Some(Relation::Table(_)) => {
            if create.if_not_exists {
                return Ok(result.with_notice(format!(
                    "NOTICE:  relation \"{}\" already exists, skipping",
                    base_name(&internal)
                )));
            }
            return Err(duplicate_relation(&create.name, &internal, dialect));
        }
        None => {
            if crate::sequences::read(writer, &internal)?.is_some() {
                return Err(duplicate_relation(&create.name, &internal, dialect));
            }
            None
        }
    };
    if let Some(existing) = &existing {
        if create.if_not_exists {
            return Ok(result.with_notice(format!(
                "NOTICE:  relation \"{}\" already exists, skipping",
                base_name(&internal)
            )));
        }
        if !create.or_replace {
            return Err(duplicate_relation(&create.name, &internal, dialect));
        }
        if existing.dialect != dialect {
            return Err(SqlError::not_supported(format!(
                "view \"{}\" was created in another SQL dialect: drop it and create it again",
                display(&internal)
            ))
            .at(create.name.position));
        }
    }
    let defaults;
    let settings = match context.settings {
        Some(settings) => settings,
        None => {
            defaults = Settings::default();
            &defaults
        }
    };
    name_resolution::check_view_text(&create.text, dialect, settings)
        .map_err(|error| error.at_if_unplaced(create.name.position))?;
    let path = schemas::relation_path(writer)?;
    let mut query = create.query.clone();
    let (output, resolution) =
        describe(writer, tables, &internal, &mut query, path.clone(), context)?;
    if resolution.expanded.contains(&internal) || resolution.references.contains(&internal) {
        return Err(SqlError::new(
            SqlState::INVALID_OBJECT_DEFINITION,
            format!(
                "infinite recursion detected in rules for relation \"{}\"",
                display(&internal)
            ),
        )
        .at(create.name.position));
    }
    let columns = view_columns(&create.columns, &output, dialect)?;
    if let Some(existing) = &existing {
        check_replacement(existing, &columns)?;
        forget_dependencies(writer, existing)?;
    } else {
        crate::store::delete_prefix(writer, &comments_prefix(&internal))?;
    }
    let view = ViewDefinition {
        name: internal.clone(),
        dialect,
        query: create.text.clone(),
        columns,
        search_path: path,
        references: resolution.references,
        expands_wildcard: selects_wildcard(&create.query),
        has_column_list: !create.columns.is_empty(),
    };
    let bytes = view.encode();
    if ViewDefinition::decode(&internal, &bytes).is_err() {
        return Err(SqlError::not_supported(format!(
            "view \"{}\" has a definition EggDB cannot store",
            display(&internal)
        )));
    }
    writer.put(&table_key(&internal), &bytes)?;
    for reference in &view.references {
        writer.put(&dependency_key(reference, &internal), DEPENDENCY_VALUE)?;
    }
    Ok(result)
}

fn view_columns(
    names: &[Name],
    output: &[OutputColumn],
    dialect: Dialect,
) -> SqlResult<Vec<ViewColumn>> {
    if names.len() > output.len() {
        let position = names.get(output.len()).map_or(0, |name| name.position);
        return Err(SqlError::syntax(
            "CREATE VIEW specifies more column names than columns",
            position,
        ));
    }
    let mut columns: Vec<ViewColumn> = Vec::with_capacity(output.len());
    for (index, column) in output.iter().enumerate() {
        let name = match names.get(index) {
            Some(name) => {
                crate::constraints::check_identifier(name)?;
                name.text.clone()
            }
            None => column.name.clone(),
        };
        if columns.iter().any(|known| known.name == name) {
            return Err(match dialect {
                Dialect::MySql => SqlError::new(
                    SqlState::MYSQL_COLUMN_EXISTS,
                    format!("Duplicate column name '{name}'"),
                )
                .with_vendor_code(1060),
                Dialect::Oracle => {
                    SqlError::new(SqlState::DUPLICATE_COLUMN, "duplicate column name")
                        .with_oracle_code(957)
                }
                _ => SqlError::new(
                    SqlState::DUPLICATE_COLUMN,
                    format!("column \"{name}\" specified more than once"),
                ),
            });
        }
        if name.is_empty() || name.contains(crate::temp_tables::TEMP_TABLE_MARKER) {
            return Err(SqlError::new(
                SqlState::INVALID_NAME,
                format!("invalid view column name \"{}\"", name.escape_default()),
            ));
        }
        columns.push(ViewColumn {
            name,
            sql_type: column.sql_type,
            source: column.name.clone(),
        });
    }
    if columns.is_empty() {
        return Err(SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            "a view needs at least one column",
        ));
    }
    Ok(columns)
}

fn type_name(sql_type: Option<crate::types::SqlType>) -> String {
    sql_type.map_or_else(|| "unknown".to_owned(), |sql_type| sql_type.name())
}

fn check_replacement(old: &ViewDefinition, new: &[ViewColumn]) -> SqlResult<()> {
    if new.len() < old.columns.len() {
        return Err(SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            "cannot drop columns from view",
        ));
    }
    for (before, after) in old.columns.iter().zip(new) {
        if before.name != after.name {
            return Err(SqlError::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!(
                    "cannot change name of view column \"{}\" to \"{}\"",
                    before.name, after.name
                ),
            )
            .with_hint(
                "Use ALTER VIEW ... RENAME COLUMN ... to change name of view column instead.",
            ));
        }
        if before.sql_type.is_some()
            && after.sql_type.is_some()
            && before.sql_type != after.sql_type
        {
            return Err(SqlError::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!(
                    "cannot change data type of view column \"{}\" from {} to {}",
                    before.name,
                    type_name(before.sql_type),
                    type_name(after.sql_type)
                ),
            ));
        }
    }
    Ok(())
}

fn forget_dependencies(writer: &mut dyn KeyValueWrite, view: &ViewDefinition) -> SqlResult<()> {
    for reference in &view.references {
        writer.delete(&dependency_key(reference, &view.name))?;
    }
    Ok(())
}

pub(crate) fn dependents(
    reader: &dyn KeyValueRead,
    referenced: &str,
    most: usize,
) -> SqlResult<Vec<String>> {
    let prefix = dependencies_prefix(referenced);
    let mut found = Vec::new();
    let mut failure = None;
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |key, _| {
            match std::str::from_utf8(&key[prefix.len()..]) {
                Ok(name) if !name.is_empty() && schemas::is_well_formed(name) => {
                    found.push(name.to_owned());
                }
                _ => {
                    failure = Some(crate::encoding::corrupt("view dependency"));
                    return Ok(false);
                }
            }
            Ok(found.len() < most)
        },
    )?;
    match failure {
        Some(error) => Err(error),
        None => Ok(found),
    }
}

pub(crate) fn first_dependent(
    reader: &dyn KeyValueRead,
    referenced: &str,
) -> SqlResult<Option<String>> {
    Ok(dependents(reader, referenced, 1)?.into_iter().next())
}

pub(crate) fn dependent_objects_error(kind: &str, name: &str, view: &str) -> SqlError {
    SqlError::new(
        SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
        format!(
            "cannot drop {kind} {} because other objects depend on it",
            display(name)
        ),
    )
    .with_detail(format!(
        "view {} depends on {kind} {}",
        display(view),
        display(name)
    ))
    .with_hint("Use DROP ... CASCADE to drop the dependent objects too.")
}

pub(crate) fn drop_dependent_views(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    referenced: &str,
    kind: &str,
    cascade: bool,
    result: &mut QueryResult,
) -> SqlResult<()> {
    let mut queue = dependents(writer, referenced, CASCADE_BATCH)?;
    if queue.is_empty() {
        return Ok(());
    }
    if !cascade {
        return Err(dependent_objects_error(kind, referenced, &queue[0]));
    }
    let mut seen: HashSet<String> = HashSet::new();
    while let Some(view) = queue.pop() {
        if !seen.insert(view.clone()) {
            continue;
        }
        let Some(Relation::View(definition)) = tables.relation(writer, &view)? else {
            writer.delete(&dependency_key(referenced, &view))?;
            continue;
        };
        for dependent in dependents(writer, &view, usize::MAX)? {
            queue.push(dependent);
        }
        remove_view(writer, &definition, true)?;
        result
            .notices
            .push(format!("NOTICE:  drop cascades to view {}", display(&view)));
        if queue.is_empty() {
            queue = dependents(writer, referenced, usize::MAX)?;
            queue.retain(|name| !seen.contains(name));
        }
    }
    Ok(())
}

fn remove_view(
    writer: &mut dyn KeyValueWrite,
    view: &ViewDefinition,
    with_dependents: bool,
) -> SqlResult<()> {
    forget_dependencies(writer, view)?;
    if with_dependents {
        crate::store::delete_prefix(writer, &dependencies_prefix(&view.name))?;
    }
    crate::store::delete_prefix(writer, &comments_prefix(&view.name))?;
    writer.delete(&table_key(&view.name))?;
    Ok(())
}

pub(crate) fn drop_views(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    names: &[Name],
    if_exists: bool,
    cascade: bool,
    dialect: Dialect,
) -> SqlResult<QueryResult> {
    let mut result = QueryResult::command("DROP VIEW");
    for name in names {
        let view = match tables.relation(writer, &name.text)? {
            Some(Relation::View(view)) => view,
            Some(Relation::Table(_)) => {
                return Err(not_a_view(&name.text).at(name.position));
            }
            None => {
                if if_exists {
                    if dialect == Dialect::PostgreSql {
                        result = result.with_notice(format!(
                            "NOTICE:  view \"{}\" does not exist, skipping",
                            display(&name.text)
                        ));
                    }
                    continue;
                }
                return Err(missing_view(name, dialect));
            }
        };
        if dialect == Dialect::PostgreSql {
            drop_dependent_views(writer, tables, &view.name, "view", cascade, &mut result)?;
        }
        remove_view(writer, &view, dialect == Dialect::PostgreSql)?;
    }
    Ok(result)
}

pub(crate) fn move_view(writer: &mut dyn KeyValueWrite, from: &str, to: &str) -> SqlResult<()> {
    let Some(bytes) = writer.get(&table_key(from))? else {
        return Ok(());
    };
    let mut view = ViewDefinition::decode(from, &bytes)?;
    forget_dependencies(writer, &view)?;
    writer.delete(&table_key(from))?;
    view.name = to.to_owned();
    writer.put(&table_key(to), &view.encode())?;
    for reference in &view.references {
        writer.put(&dependency_key(reference, to), DEPENDENCY_VALUE)?;
    }
    crate::comments::move_owner(writer, from, to)
}

pub(crate) fn alter_view(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    alter: &AlterView,
    dialect: Dialect,
) -> SqlResult<QueryResult> {
    let result = QueryResult::command("ALTER VIEW");
    let view = match tables.relation(writer, &alter.name.text)? {
        Some(Relation::View(view)) => view,
        Some(Relation::Table(_)) => {
            return Err(not_a_view(&alter.name.text).at(alter.name.position));
        }
        None if alter.if_exists => {
            return Ok(result.with_notice(format!(
                "NOTICE:  relation \"{}\" does not exist, skipping",
                display(&alter.name.text)
            )));
        }
        None => return Err(missing_view(&alter.name, dialect)),
    };
    let target = match &alter.action {
        AlterViewAction::Accepted => return Ok(result),
        AlterViewAction::RenameTo(to) => {
            crate::constraints::check_identifier(to)?;
            Name {
                text: schemas::qualified(schemas::schema_of(&view.name), &to.text),
                position: to.position,
            }
        }
        AlterViewAction::SetSchema(schema) => {
            if !schemas::schema_exists(writer, &schema.text)? {
                return Err(schemas::missing_schema(&schema.text, dialect).at(schema.position));
            }
            Name {
                text: schemas::qualified(&schema.text, base_name(&view.name)),
                position: schema.position,
            }
        }
    };
    if target.text == view.name {
        return Ok(result);
    }
    if crate::sequences::relation_exists(writer, &target.text)? {
        return Err(duplicate_relation(&target, &target.text, dialect));
    }
    if dialect == Dialect::PostgreSql
        && let Some(dependent) = first_dependent(writer, &view.name)?
    {
        return Err(rename_refused("view", &view.name, &dependent));
    }
    move_view(writer, &view.name, &target.text)?;
    Ok(result)
}

pub(crate) fn rename_refused(kind: &str, name: &str, view: &str) -> SqlError {
    SqlError::not_supported(format!(
        "cannot rename {kind} {} because view {} depends on it",
        display(name),
        display(view)
    ))
    .with_hint("Drop the view, rename, then create the view again; views keep the names they were written with in this release.")
}

pub(crate) fn check_dependents(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    table: &str,
    change: &str,
    settings: Option<&Settings>,
) -> SqlResult<()> {
    for view_name in dependents(writer, table, usize::MAX)? {
        let Some(Relation::View(view)) = tables.relation(writer, &view_name)? else {
            continue;
        };
        if let Err(reason) = still_valid(writer, tables, &view, settings) {
            return Err(SqlError::new(
                SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                format!(
                    "cannot {change} because view {} depends on it",
                    display(&view.name)
                ),
            )
            .with_detail(reason)
            .with_hint("Drop the view first, make the change, then create the view again."));
        }
    }
    Ok(())
}

fn still_valid(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    view: &ViewDefinition,
    settings: Option<&Settings>,
) -> Result<(), String> {
    let context = ViewContext {
        settings,
        dialect: view.dialect,
    };
    let (mut query, _) =
        name_resolution::parse_view(view, settings, crate::parser::Nesting::default())
            .map_err(|error| error.message().to_owned())?;
    let path = view.search_path.clone();
    let (output, _) = describe(reader, tables, &view.name, &mut query, path, &context)
        .map_err(|error| error.with_names_displayed().message().to_owned())?;
    if output.len() < view.columns.len() {
        return Err("the view would lose columns".to_owned());
    }
    for (stored, now) in view.columns.iter().zip(&output) {
        if stored.sql_type.is_some() && now.sql_type.is_some() && stored.sql_type != now.sql_type {
            return Err(format!(
                "column \"{}\" of the view would change type from {} to {}",
                stored.name,
                type_name(stored.sql_type),
                type_name(now.sql_type)
            ));
        }
    }
    Ok(())
}

pub(crate) fn every_view(reader: &dyn KeyValueRead) -> SqlResult<Vec<ViewDefinition>> {
    let prefix = table_key("");
    let mut views = Vec::new();
    let mut failure = None;
    let mut memory = crate::memory::WorkMemory::new();
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |key, value| {
            if !crate::encoding::is_view_record(value) {
                return Ok(true);
            }
            memory.charge(key.len() + value.len() + 64)?;
            let decoded = std::str::from_utf8(&key[prefix.len()..])
                .map_err(|_| crate::encoding::corrupt("view definition"))
                .and_then(|name| ViewDefinition::decode(name, value));
            match decoded {
                Ok(view) => views.push(view),
                Err(error) => {
                    failure = Some(error);
                    return Ok(false);
                }
            }
            Ok(true)
        },
    )?;
    match failure {
        Some(error) => Err(error),
        None => Ok(views),
    }
}
