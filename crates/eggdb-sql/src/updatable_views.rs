use crate::ast::{
    Delete, Expr, ExprKind, Insert, InsertSource, InsertValue, JoinCondition, Name, OnDuplicate,
    Query, SelectItem, SetExpr, TableReference, Update,
};
use crate::encoding::{CheckOption, ViewDefinition};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::children_mut;
use crate::execute::{self, Relation, TableCache};
use crate::schemas::{base_name, display};
use crate::session::{OutputColumn, QueryResult};
use crate::store::{KeyValueRead, KeyValueWrite};
use crate::types::Value;

const MOST_VIEW_LEVELS: usize = 32;
const CHECK_LABEL: &str = "\u{2}check option";

struct Level {
    view: String,
    label: String,
    base: TableReference,
    base_label: String,
    columns: Vec<(String, Expr)>,
    condition: Option<Expr>,
    check: Option<CheckOption>,
}

pub(crate) fn is_view(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    target: &TableReference,
) -> SqlResult<bool> {
    if target.derived.is_some() || target.generated.is_some() {
        return Ok(false);
    }
    Ok(matches!(
        tables
            .relation(reader, &target.name.text)
            .map_err(|error| error.at_if_unplaced(target.name.position))?,
        Some(Relation::View(_))
    ))
}

fn not_automatically_updatable(view: &str, verb: &str, detail: &str) -> SqlError {
    SqlError::new(
        SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
        format!("cannot {verb} view \"{}\"", display(view)),
    )
    .with_detail(detail)
    .with_hint("Change the view's table directly, or make the view select straight from one table.")
}

fn not_updatable_column(verb: &str, column: &str, view: &str, position: usize) -> SqlError {
    SqlError::not_supported(format!(
        "cannot {verb} column \"{column}\" of view \"{}\"",
        display(view)
    ))
    .with_detail("View columns that are not columns of their base relation are not updatable.")
    .at(position)
}

fn missing_column(column: &str, view: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_COLUMN,
        format!(
            "column \"{column}\" of relation \"{}\" does not exist",
            display(view)
        ),
    )
    .at(position)
}

fn column_expr(label: &str, name: &str, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Column {
            qualifier: Some(label.to_owned()),
            name: name.to_owned(),
        },
        position,
    }
}

fn view_level(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    view: &ViewDefinition,
    label: String,
    verb: &str,
) -> SqlResult<Level> {
    let (mut query, _) =
        crate::name_resolution::parse_view(view, None, crate::parser::Nesting::default())?;
    crate::name_resolution::resolve_view_body(
        reader,
        tables,
        &view.name,
        &mut query,
        view.search_path.clone(),
        None,
    )?;
    let refused = |detail: &str| not_automatically_updatable(&view.name, verb, detail);
    if !query.with.is_empty() {
        return Err(refused("Views containing WITH are not automatically updatable."));
    }
    if query.limit.is_some() || query.offset.is_some() {
        return Err(refused(
            "Views containing LIMIT or OFFSET are not automatically updatable.",
        ));
    }
    let SetExpr::Select(select) = &query.body else {
        return Err(refused(
            "Views containing UNION, INTERSECT, or EXCEPT are not automatically updatable.",
        ));
    };
    if select.distinct {
        return Err(refused("Views containing DISTINCT are not automatically updatable."));
    }
    if !select.group_by.is_empty() {
        return Err(refused("Views containing GROUP BY are not automatically updatable."));
    }
    if select.having.is_some() {
        return Err(refused("Views containing HAVING are not automatically updatable."));
    }
    if select.limit.is_some() || select.offset.is_some() || select.source_row_limit.is_some() {
        return Err(refused(
            "Views containing LIMIT or OFFSET are not automatically updatable.",
        ));
    }
    if crate::aggregate::is_aggregate_query(select) {
        return Err(refused(
            "Views that return aggregate functions are not automatically updatable.",
        ));
    }
    if crate::set_returning::select_needs_expansion(select) {
        return Err(refused(
            "Views that return window functions or set-returning functions are not automatically updatable.",
        ));
    }
    let single = "Views that do not select from a single table or view are not automatically updatable.";
    let Some(from) = &select.from else {
        return Err(refused(single));
    };
    if !from.joins.is_empty() || from.first.generated.is_some() {
        return Err(refused(single));
    }
    let first = &from.first;
    let base_name_text = first.name.text.clone();
    let relation = tables.relation(reader, &base_name_text)?;
    let base_columns: Vec<String> = match &relation {
        Some(Relation::Table(table)) if first.derived.is_none() => table
            .columns
            .iter()
            .filter(|column| !crate::temp_tables::is_hidden_column(&column.name))
            .map(|column| column.name.clone())
            .collect(),
        Some(Relation::View(inner)) => inner
            .columns
            .iter()
            .map(|column| column.name.clone())
            .collect(),
        _ => return Err(refused(single)),
    };
    let base_label = first
        .alias
        .clone()
        .unwrap_or_else(|| base_name_text.clone());
    let position = first.name.position;
    let mut outputs = Vec::with_capacity(view.columns.len());
    for item in &select.items {
        match item {
            SelectItem::Wildcard { .. } => outputs.extend(
                base_columns
                    .iter()
                    .map(|name| column_expr(&base_label, name, position)),
            ),
            SelectItem::Expr { expr, .. } => outputs.push(expr.clone()),
        }
    }
    let columns = view
        .columns
        .iter()
        .zip(outputs)
        .map(|(column, expr)| (column.name.clone(), expr))
        .collect();
    Ok(Level {
        view: view.name.clone(),
        label,
        base: TableReference::named(first.name.clone(), Some(base_label.clone())),
        base_label,
        columns,
        condition: select.filter.clone(),
        check: view.check_option,
    })
}

fn chain(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    target: &TableReference,
    verb: &str,
) -> SqlResult<Vec<Level>> {
    let mut levels: Vec<Level> = Vec::new();
    let mut name = target.name.text.clone();
    let mut label = target
        .alias
        .clone()
        .unwrap_or_else(|| base_name(&target.name.text).to_owned());
    loop {
        let relation = tables
            .relation(reader, &name)
            .map_err(|error| error.at_if_unplaced(target.name.position))?;
        let Some(Relation::View(view)) = relation else {
            return Ok(levels);
        };
        if levels.len() >= MOST_VIEW_LEVELS {
            return Err(SqlError::new(
                SqlState::STATEMENT_TOO_COMPLEX,
                format!(
                    "a statement can change a table through at most {MOST_VIEW_LEVELS} views"
                ),
            )
            .at(target.name.position));
        }
        let level = view_level(reader, tables, &view, label, verb)
            .map_err(|error| error.at_if_unplaced(target.name.position))?;
        name = level.base.name.text.clone();
        label = level.base_label.clone();
        levels.push(level);
    }
}

fn updatable_column(expr: &Expr, base_label: &str) -> Option<String> {
    match &expr.kind {
        ExprKind::Column { qualifier, name }
            if qualifier.as_deref().is_none_or(|qualifier| qualifier == base_label) =>
        {
            Some(name.clone())
        }
        _ => None,
    }
}

fn names_level_column<'l>(expr: &Expr, level: &'l Level, qualified_only: bool) -> Option<&'l Expr> {
    let ExprKind::Column { qualifier, name } = &expr.kind else {
        return None;
    };
    let named = match qualifier.as_deref() {
        Some(qualifier) => qualifier == level.label,
        None => !qualified_only,
    };
    if !named {
        return None;
    }
    level
        .columns
        .iter()
        .find(|(column, _)| column == name)
        .map(|(_, base)| base)
}

fn map_qualified(expr: &mut Expr, level: &Level) -> SqlResult<()> {
    if let Some(base) = names_level_column(expr, level, true) {
        let position = expr.position;
        *expr = base.clone();
        expr.position = position;
        return Ok(());
    }
    if let ExprKind::Subquery(subquery) = &mut expr.kind {
        map_in_query(&mut subquery.query, level)?;
    }
    for child in children_mut(expr) {
        map_qualified(child, level)?;
    }
    Ok(())
}

fn map_in_body(body: &mut SetExpr, level: &Level) -> SqlResult<()> {
    match body {
        SetExpr::Select(select) => {
            if let Some(from) = &mut select.from {
                for reference in std::iter::once(&mut from.first)
                    .chain(from.joins.iter_mut().map(|join| &mut join.table))
                {
                    if let Some(derived) = &mut reference.derived {
                        map_in_query(&mut derived.query, level)?;
                    }
                }
                for join in &mut from.joins {
                    if let JoinCondition::On(on) = &mut join.condition {
                        map_qualified(on, level)?;
                    }
                }
            }
            for item in &mut select.items {
                if let SelectItem::Expr { expr, .. } = item {
                    map_qualified(expr, level)?;
                }
            }
            let rest = select
                .filter
                .iter_mut()
                .chain(select.group_by.iter_mut())
                .chain(select.having.iter_mut())
                .chain(select.order_by.iter_mut().map(|item| &mut item.expr));
            for expr in rest {
                map_qualified(expr, level)?;
            }
            Ok(())
        }
        SetExpr::Operation(operation) => {
            map_in_body(&mut operation.left, level)?;
            map_in_body(&mut operation.right, level)
        }
        SetExpr::Query(query) => map_in_query(query, level),
    }
}

fn map_in_query(query: &mut Query, level: &Level) -> SqlResult<()> {
    for table in &mut query.with {
        map_in_query(&mut table.query, level)?;
    }
    map_in_body(&mut query.body, level)
}

fn mapped(expr: &Expr, level: &Level) -> SqlResult<Expr> {
    if let Some(base) = names_level_column(expr, level, false) {
        let mut replaced = base.clone();
        replaced.position = expr.position;
        return Ok(replaced);
    }
    let rebuilt = crate::aggregate::rebuild(expr, &mut |child| mapped(child, level))?;
    let position = rebuilt.position;
    match rebuilt.kind {
        ExprKind::Subquery(mut subquery) => {
            map_in_query(&mut subquery.query, level)?;
            Ok(Expr {
                kind: ExprKind::Subquery(subquery),
                position,
            })
        }
        kind => Ok(Expr { kind, position }),
    }
}

fn conjoined(left: Option<Expr>, right: Option<Expr>) -> Option<Expr> {
    match (left, right) {
        (Some(left), Some(right)) => {
            let position = left.position;
            Some(Expr {
                kind: ExprKind::And(vec![left, right]),
                position,
            })
        }
        (left, right) => left.or(right),
    }
}

fn mapped_returning(
    returning: Option<Vec<SelectItem>>,
    level: &Level,
) -> SqlResult<Option<Vec<SelectItem>>> {
    let Some(items) = returning else {
        return Ok(None);
    };
    let mut mapped_items = Vec::with_capacity(items.len());
    for item in items {
        match item {
            SelectItem::Wildcard {
                qualifier,
                position,
            } if qualifier.as_deref().is_none_or(|qualifier| qualifier == level.label) => {
                for (name, base) in &level.columns {
                    let mut expr = base.clone();
                    expr.position = position;
                    mapped_items.push(SelectItem::Expr {
                        expr,
                        alias: Some(name.clone()),
                    });
                }
            }
            SelectItem::Expr { expr, alias } => {
                let alias = alias.or_else(|| Some(crate::eval::output_name(&expr)));
                mapped_items.push(SelectItem::Expr {
                    expr: mapped(&expr, level)?,
                    alias,
                });
            }
            other => mapped_items.push(other),
        }
    }
    Ok(Some(mapped_items))
}

struct Checks {
    conditions: Vec<Expr>,
    cascading: bool,
}

impl Checks {
    fn new() -> Self {
        Self {
            conditions: Vec::new(),
            cascading: false,
        }
    }

    fn descend(&mut self, level: &Level) -> SqlResult<()> {
        self.conditions = self
            .conditions
            .iter()
            .map(|condition| mapped(condition, level))
            .collect::<SqlResult<_>>()?;
        if (level.check.is_some() || self.cascading)
            && let Some(condition) = &level.condition
        {
            self.conditions.push(condition.clone());
        }
        self.cascading |= level.check == Some(CheckOption::Cascaded);
        Ok(())
    }

    fn condition(self) -> Option<Expr> {
        self.conditions
            .into_iter()
            .fold(None, |all, condition| conjoined(all, Some(condition)))
    }
}

fn with_check(returning: &mut Option<Vec<SelectItem>>, check: Option<&Expr>) {
    if let Some(check) = check {
        returning.get_or_insert_with(Vec::new).push(SelectItem::Expr {
            expr: check.clone(),
            alias: Some(CHECK_LABEL.to_owned()),
        });
    }
}

fn checked(
    mut result: QueryResult,
    check: bool,
    user_returning: bool,
    view: &str,
) -> SqlResult<QueryResult> {
    if !check {
        return Ok(result);
    }
    for row in &mut result.rows {
        if row.pop() != Some(Value::Boolean(true)) {
            return Err(SqlError::new(
                SqlState::WITH_CHECK_OPTION_VIOLATION,
                format!("new row violates check option for view \"{}\"", display(view)),
            ));
        }
    }
    if let Some(columns) = &mut result.columns {
        columns.pop();
    }
    if !user_returning {
        result.columns = None;
        result.rows.clear();
    }
    Ok(result)
}

fn first_view(levels: &[Level]) -> String {
    levels
        .first()
        .map_or_else(String::new, |level| level.view.clone())
}

pub(crate) fn update(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    update: &Update,
) -> SqlResult<QueryResult> {
    let levels = chain(writer, tables, &update.table, "update")?;
    let mut statement = update.clone();
    let user_returning = statement.returning.is_some();
    let mut checks = Checks::new();
    for level in &levels {
        let mut assignments = Vec::with_capacity(statement.assignments.len());
        for (name, value) in std::mem::take(&mut statement.assignments) {
            let (_, column) = level
                .columns
                .iter()
                .find(|(column, _)| *column == name.text)
                .ok_or_else(|| missing_column(&name.text, &level.view, name.position))?;
            let base = updatable_column(column, &level.base_label).ok_or_else(|| {
                not_updatable_column("update", &name.text, &level.view, name.position)
            })?;
            let value = match value {
                InsertValue::Expr(expr) => InsertValue::Expr(mapped(&expr, level)?),
                InsertValue::Default => InsertValue::Default,
            };
            assignments.push((
                Name {
                    text: base,
                    position: name.position,
                },
                value,
            ));
        }
        statement.assignments = assignments;
        let filter = statement
            .filter
            .take()
            .map(|filter| mapped(&filter, level))
            .transpose()?;
        statement.filter = conjoined(filter, level.condition.clone());
        statement.returning = mapped_returning(statement.returning.take(), level)?;
        checks.descend(level)?;
        statement.table = level.base.clone();
    }
    let check = checks.condition();
    with_check(&mut statement.returning, check.as_ref());
    let result = execute::update(writer, tables, &statement)?;
    checked(result, check.is_some(), user_returning, &first_view(&levels))
}

fn listed_width(source: &InsertSource) -> Option<usize> {
    match source {
        InsertSource::Values(rows) => rows.first().map(Vec::len),
        InsertSource::Query(_) | InsertSource::DefaultValues => None,
    }
}

pub(crate) fn insert(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    insert: &Insert,
) -> SqlResult<QueryResult> {
    let levels = chain(writer, tables, &insert.table, "insert into")?;
    if !matches!(insert.on_duplicate, OnDuplicate::Fail) {
        return Err(SqlError::not_supported(
            "an INSERT into a view that resolves conflicts is not supported yet",
        )
        .at(insert.table.name.position));
    }
    let mut statement = insert.clone();
    let user_returning = statement.returning.is_some();
    let defaults_only = matches!(statement.source, InsertSource::DefaultValues);
    let mut checks = Checks::new();
    for level in &levels {
        if !defaults_only {
            let position = statement.table.name.position;
            let listed: Vec<Name> = match statement.columns.take() {
                Some(columns) => columns,
                None => {
                    let width = listed_width(&statement.source).unwrap_or(level.columns.len());
                    level
                        .columns
                        .iter()
                        .take(width)
                        .map(|(name, _)| Name {
                            text: name.clone(),
                            position,
                        })
                        .collect()
                }
            };
            let mut base_columns = Vec::with_capacity(listed.len());
            for name in listed {
                let (_, column) = level
                    .columns
                    .iter()
                    .find(|(column, _)| *column == name.text)
                    .ok_or_else(|| missing_column(&name.text, &level.view, name.position))?;
                let base = updatable_column(column, &level.base_label).ok_or_else(|| {
                    not_updatable_column("insert into", &name.text, &level.view, name.position)
                })?;
                base_columns.push(Name {
                    text: base,
                    position: name.position,
                });
            }
            statement.columns = Some(base_columns);
        }
        statement.returning = mapped_returning(statement.returning.take(), level)?;
        checks.descend(level)?;
        statement.table = level.base.clone();
    }
    let check = checks.condition();
    with_check(&mut statement.returning, check.as_ref());
    let result = execute::insert(writer, tables, &statement)?;
    checked(result, check.is_some(), user_returning, &first_view(&levels))
}

pub(crate) fn delete(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    delete: &Delete,
) -> SqlResult<QueryResult> {
    let levels = chain(writer, tables, &delete.table, "delete from")?;
    let mut statement = delete.clone();
    for level in &levels {
        let filter = statement
            .filter
            .take()
            .map(|filter| mapped(&filter, level))
            .transpose()?;
        statement.filter = conjoined(filter, level.condition.clone());
        statement.returning = mapped_returning(statement.returning.take(), level)?;
        statement.table = level.base.clone();
    }
    execute::delete(writer, tables, &statement)
}

pub(crate) fn returning_columns(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    target: &TableReference,
    items: &Option<Vec<SelectItem>>,
) -> SqlResult<Option<Vec<OutputColumn>>> {
    let levels = chain(reader, tables, target, "change")?;
    let mut returning = items.clone();
    let mut table = target.clone();
    for level in &levels {
        returning = mapped_returning(returning, level)?;
        table = level.base.clone();
    }
    execute::returning_columns(reader, tables, &table, &returning)
}
