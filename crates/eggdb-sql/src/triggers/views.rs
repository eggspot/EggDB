use super::{Firing, TriggerEvent};
use crate::ast::{
    Delete, Expr, ExprKind, FromClause, Insert, InsertSource, InsertValue, Literal, Select,
    SelectItem, TableReference, Update,
};
use crate::encoding::ViewDefinition;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{self, Relation, TableCache};
use crate::session::QueryResult;
use crate::store::KeyValueWrite;
use crate::types::Value;
use std::sync::Arc;

fn view_of(
    writer: &dyn crate::store::KeyValueRead,
    tables: &mut TableCache,
    target: &TableReference,
) -> SqlResult<Option<Arc<ViewDefinition>>> {
    if target.derived.is_some() || target.generated.is_some() {
        return Ok(None);
    }
    Ok(match tables.relation(writer, &target.name.text)? {
        Some(Relation::View(view)) => Some(view),
        _ => None,
    })
}

fn null_expr(position: usize) -> Expr {
    Expr {
        kind: ExprKind::Literal(Literal::Null),
        position,
    }
}

fn value_expr(value: &InsertValue, position: usize) -> Expr {
    match value {
        InsertValue::Expr(expr) => expr.clone(),
        InsertValue::Default => null_expr(position),
    }
}

fn select_over(target: &TableReference, items: Vec<SelectItem>, filter: Option<Expr>) -> Select {
    Select {
        distinct: false,
        items,
        from: Some(FromClause {
            first: target.clone(),
            joins: Vec::new(),
        }),
        filter,
        group_by: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        source_row_limit: None,
    }
}

fn constant_select(items: Vec<Expr>) -> Select {
    Select {
        distinct: false,
        items: items
            .into_iter()
            .map(|expr| SelectItem::Expr { expr, alias: None })
            .collect(),
        from: None,
        filter: None,
        group_by: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        source_row_limit: None,
    }
}

fn wildcard(position: usize) -> SelectItem {
    SelectItem::Wildcard {
        qualifier: None,
        position,
    }
}

fn missing_view_column(name: &str, view: &ViewDefinition) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_COLUMN,
        format!(
            "column \"{name}\" of relation \"{}\" does not exist",
            crate::schemas::display(&view.name)
        ),
    )
}

fn column_positions(view: &ViewDefinition, names: &[crate::ast::Name]) -> SqlResult<Vec<usize>> {
    names
        .iter()
        .map(|name| {
            view.columns
                .iter()
                .position(|column| column.name.eq_ignore_ascii_case(&name.text))
                .ok_or_else(|| missing_view_column(&name.text, view).at(name.position))
        })
        .collect()
}

pub(crate) fn insert_instead_of_view(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    insert: &Insert,
) -> SqlResult<Option<QueryResult>> {
    let Some(view) = view_of(&*writer, tables, &insert.table)? else {
        return Ok(None);
    };
    let Some(mut firing) = Firing::for_view(&*writer, tables, &view, TriggerEvent::Insert, &[])?
    else {
        return Ok(None);
    };
    let targets = match &insert.columns {
        Some(names) => column_positions(&view, names)?,
        None => (0..view.columns.len()).collect(),
    };
    let position = insert.table.name.position;
    let given: Vec<Vec<Value>> = match &insert.source {
        InsertSource::Values(rows) => {
            let mut values = Vec::with_capacity(rows.len());
            for row in rows {
                let select =
                    constant_select(row.iter().map(|value| value_expr(value, position)).collect());
                let result = execute::select(&*writer, tables, &select, false)?;
                values.push(result.rows.into_iter().next().unwrap_or_default());
            }
            values
        }
        InsertSource::Query(query) => execute::query(&*writer, tables, query, false)?.rows,
        InsertSource::DefaultValues => vec![Vec::new()],
    };
    let mut count = 0usize;
    for values in given {
        let mut row = vec![Value::Null; view.columns.len()];
        for (value, &target) in values.into_iter().zip(&targets) {
            if let Some(slot) = row.get_mut(target) {
                *slot = value;
            }
        }
        firing.instead_row(writer, tables, None, Some(row.as_slice()))?;
        count += 1;
    }
    firing.finish();
    Ok(Some(QueryResult::command(format!("INSERT 0 {count}"))))
}

pub(crate) fn update_instead_of_view(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    update: &Update,
) -> SqlResult<Option<QueryResult>> {
    let Some(view) = view_of(&*writer, tables, &update.table)? else {
        return Ok(None);
    };
    let names: Vec<crate::ast::Name> = update
        .assignments
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    let targets = column_positions(&view, &names)?;
    let updated: Vec<&str> = names.iter().map(|name| name.text.as_str()).collect();
    let Some(mut firing) =
        Firing::for_view(&*writer, tables, &view, TriggerEvent::Update, &updated)?
    else {
        return Ok(None);
    };
    let position = update.table.name.position;
    let mut items = vec![wildcard(position)];
    items.extend(update.assignments.iter().map(|(_, value)| SelectItem::Expr {
        expr: value_expr(value, position),
        alias: None,
    }));
    let select = select_over(&update.table, items, update.filter.clone());
    let rows = execute::select(&*writer, tables, &select, false)?.rows;
    let width = view.columns.len();
    let mut count = 0usize;
    for row in rows {
        let old: Vec<Value> = row.iter().take(width).cloned().collect();
        let mut new = old.clone();
        for (value, &target) in row.into_iter().skip(width).zip(&targets) {
            if let Some(slot) = new.get_mut(target) {
                *slot = value;
            }
        }
        firing.instead_row(writer, tables, Some(old.as_slice()), Some(new.as_slice()))?;
        count += 1;
    }
    firing.finish();
    Ok(Some(QueryResult::command(format!("UPDATE {count}"))))
}

pub(crate) fn delete_instead_of_view(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    delete: &Delete,
) -> SqlResult<Option<QueryResult>> {
    let Some(view) = view_of(&*writer, tables, &delete.table)? else {
        return Ok(None);
    };
    let Some(mut firing) = Firing::for_view(&*writer, tables, &view, TriggerEvent::Delete, &[])?
    else {
        return Ok(None);
    };
    let position = delete.table.name.position;
    let select = select_over(&delete.table, vec![wildcard(position)], delete.filter.clone());
    let rows = execute::select(&*writer, tables, &select, false)?.rows;
    let mut count = 0usize;
    for row in rows {
        firing.instead_row(writer, tables, Some(row.as_slice()), None)?;
        count += 1;
    }
    firing.finish();
    Ok(Some(QueryResult::command(format!("DELETE {count}"))))
}
