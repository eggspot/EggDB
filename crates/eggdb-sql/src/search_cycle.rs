use crate::arrays::{ArrayElement, ArrayValue};
use crate::ast::{
    CommonTable, CycleClause, CycleMarkValues, Expr, ExprKind, JoinCondition, Literal, Name,
    SearchClause, SearchOrder, Select, SelectItem, SetExpr, SetOperation, TableReference,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope};
use crate::session::OutputColumn;
use crate::types::{
    Conversion, DefinedType, DefinedValue, SqlType, Value, compare_values, convert,
};
use std::cmp::Ordering;

pub(crate) struct SearchCycle {
    width: usize,
    search: Option<SearchSequence>,
    cycle: Option<CycleMark>,
    recursive_term: SetExpr,
}

struct SearchSequence {
    order: SearchOrder,
    columns: Vec<usize>,
    name: String,
}

struct CycleMark {
    columns: Vec<usize>,
    mark_name: String,
    path_name: String,
    cycle: Value,
    default: Value,
    sql_type: SqlType,
}

pub(crate) fn has_clauses(table: &CommonTable) -> bool {
    table.search.is_some() || table.cycle.is_some()
}

pub(crate) fn not_recursive(table: &CommonTable) -> SqlError {
    SqlError::new(SqlState::SYNTAX_ERROR, "WITH query is not recursive").at(table.name.position)
}

impl SearchCycle {
    pub(crate) fn new(
        table: &CommonTable,
        operation: &SetOperation,
        columns: &[OutputColumn],
    ) -> SqlResult<Option<Self>> {
        if !has_clauses(table) {
            return Ok(None);
        }
        let name = &table.name;
        if simple_select(&operation.left).is_none() {
            return Err(syntax(
                "with a SEARCH or CYCLE clause, the left side of the UNION must be a SELECT".into(),
                name.position,
            ));
        }
        let Some(right) = simple_select(&operation.right) else {
            return Err(syntax(
                "with a SEARCH or CYCLE clause, the right side of the UNION must be a SELECT"
                    .into(),
                name.position,
            ));
        };
        let shown = shown_names(table, columns)?;
        let search = match &table.search {
            Some(clause) => Some(search_sequence(clause, &shown)?),
            None => None,
        };
        let cycle = match &table.cycle {
            Some(clause) => Some(cycle_mark(clause, &shown)?),
            None => None,
        };
        if let (Some(search), Some(cycle), Some(clause)) = (&search, &cycle, &table.search) {
            if search.name == cycle.mark_name {
                return Err(syntax(
                    "search sequence column name and cycle mark column name are the same".into(),
                    clause.position,
                ));
            }
            if search.name == cycle.path_name {
                return Err(syntax(
                    "search sequence column name and cycle path column name are the same".into(),
                    clause.position,
                ));
            }
        }
        let mut parent_columns = Vec::with_capacity(2);
        if let Some(search) = &search {
            parent_columns.push(search.name.as_str());
        }
        if let Some(cycle) = &cycle {
            parent_columns.push(cycle.path_name.as_str());
        }
        let recursive_term = with_parent_columns(right.clone(), name, &shown, &parent_columns)?;
        Ok(Some(Self {
            width: columns.len(),
            search,
            cycle,
            recursive_term: SetExpr::Select(Box::new(recursive_term)),
        }))
    }

    pub(crate) fn recursive_term(&self) -> &SetExpr {
        &self.recursive_term
    }

    pub(crate) fn added_columns(&self) -> Vec<OutputColumn> {
        let mut added = Vec::with_capacity(3);
        if let Some(search) = &self.search {
            added.push(OutputColumn {
                name: search.name.clone(),
                sql_type: Some(match search.order {
                    SearchOrder::DepthFirst => SqlType::Array(ArrayElement::Record),
                    SearchOrder::BreadthFirst => SqlType::Defined(DefinedType::RECORD),
                }),
            });
        }
        if let Some(cycle) = &self.cycle {
            added.push(OutputColumn {
                name: cycle.mark_name.clone(),
                sql_type: Some(cycle.sql_type),
            });
            added.push(OutputColumn {
                name: cycle.path_name.clone(),
                sql_type: Some(SqlType::Array(ArrayElement::Record)),
            });
        }
        added
    }

    pub(crate) fn parent_width(&self) -> usize {
        usize::from(self.search.is_some()) + usize::from(self.cycle.is_some())
    }

    pub(crate) fn seed_row(&self, mut row: Vec<Value>) -> Vec<Value> {
        if let Some(search) = &self.search {
            let fields = fields_of(&row, &search.columns);
            row.push(match search.order {
                SearchOrder::DepthFirst => record_array(vec![crate::records::anonymous(fields)]),
                SearchOrder::BreadthFirst => breadth_record(0, fields),
            });
        }
        if let Some(cycle) = &self.cycle {
            let fields = fields_of(&row, &cycle.columns);
            row.push(cycle.default.clone());
            row.push(record_array(vec![crate::records::anonymous(fields)]));
        }
        row
    }

    pub(crate) fn step_row(&self, mut row: Vec<Value>, parent: Vec<Value>) -> Vec<Value> {
        let mut parent = parent.into_iter();
        if let Some(search) = &self.search {
            let fields = fields_of(&row, &search.columns);
            let parent_sequence = parent.next().unwrap_or(Value::Null);
            row.push(match search.order {
                SearchOrder::DepthFirst => {
                    let mut path = record_items(parent_sequence);
                    path.push(crate::records::anonymous(fields));
                    record_array(path)
                }
                SearchOrder::BreadthFirst => {
                    breadth_record(depth_of(&parent_sequence).saturating_add(1), fields)
                }
            });
        }
        if let Some(cycle) = &self.cycle {
            let visited = crate::records::anonymous(fields_of(&row, &cycle.columns));
            let mut path = record_items(parent.next().unwrap_or(Value::Null));
            let repeated = path
                .iter()
                .any(|seen| compare_values(seen, &visited) == Ordering::Equal);
            row.push(if repeated {
                cycle.cycle.clone()
            } else {
                cycle.default.clone()
            });
            path.push(visited);
            row.push(record_array(path));
        }
        row
    }

    pub(crate) fn expands(&self, row: &[Value]) -> bool {
        let Some(cycle) = &self.cycle else {
            return true;
        };
        let at = self.width + usize::from(self.search.is_some());
        match row.get(at) {
            Some(mark) if !mark.is_null() && !cycle.cycle.is_null() => {
                compare_values(mark, &cycle.cycle) != Ordering::Equal
            }
            _ => false,
        }
    }
}

fn with_parent_columns(
    mut select: Select,
    table: &Name,
    shown: &[String],
    parent_columns: &[&str],
) -> SqlResult<Select> {
    let qualifier = recursive_reference(&select, table)?;
    let mut items = Vec::with_capacity(select.items.len() + shown.len() + parent_columns.len());
    for item in std::mem::take(&mut select.items) {
        match item {
            SelectItem::Wildcard {
                qualifier: Some(named),
                position,
            } if named == qualifier => {
                items.extend(column_items(&qualifier, shown, position));
            }
            SelectItem::Wildcard {
                qualifier: None,
                position,
            } => items.extend(expanded_wildcard(
                &select, table, &qualifier, shown, position,
            )?),
            other => items.push(other),
        }
    }
    for name in parent_columns {
        items.push(SelectItem::Expr {
            expr: column_reference(&qualifier, name, table.position),
            alias: Some((*name).to_owned()),
        });
    }
    select.items = items;
    Ok(select)
}

fn syntax(message: String, position: usize) -> SqlError {
    SqlError::new(SqlState::SYNTAX_ERROR, message).at(position)
}

fn simple_select(body: &SetExpr) -> Option<&Select> {
    match body {
        SetExpr::Select(select) => Some(select),
        SetExpr::Query(query)
            if query.with.is_empty()
                && query.order_by.is_empty()
                && query.limit.is_none()
                && query.offset.is_none() =>
        {
            simple_select(&query.body)
        }
        _ => None,
    }
}

fn shown_names(table: &CommonTable, columns: &[OutputColumn]) -> SqlResult<Vec<String>> {
    if let Some(extra) = table.column_names.get(columns.len()) {
        return Err(SqlError::new(
            SqlState::INVALID_COLUMN_REFERENCE,
            format!(
                "table \"{}\" has {} columns available but {} columns specified",
                table.name.text,
                columns.len(),
                table.column_names.len()
            ),
        )
        .at(extra.position));
    }
    Ok(columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            table
                .column_names
                .get(index)
                .map_or_else(|| column.name.clone(), |name| name.text.clone())
        })
        .collect())
}

fn listed_columns(
    names: &[Name],
    shown: &[String],
    clause: &str,
    position: usize,
) -> SqlResult<Vec<usize>> {
    let mut indexes = Vec::with_capacity(names.len());
    for name in names {
        let Some(index) = shown.iter().position(|column| *column == name.text) else {
            return Err(syntax(
                format!(
                    "{clause} column \"{}\" not in WITH query column list",
                    name.text
                ),
                position,
            ));
        };
        if indexes.contains(&index) {
            return Err(SqlError::new(
                SqlState::DUPLICATE_COLUMN,
                format!("{clause} column \"{}\" specified more than once", name.text),
            )
            .at(position));
        }
        indexes.push(index);
    }
    Ok(indexes)
}

fn search_sequence(clause: &SearchClause, shown: &[String]) -> SqlResult<SearchSequence> {
    let columns = listed_columns(&clause.columns, shown, "search", clause.position)?;
    let name = clause.sequence_column.text.clone();
    if shown.contains(&name) {
        return Err(syntax(
            format!(
                "search sequence column name \"{name}\" already used in WITH query column list"
            ),
            clause.position,
        ));
    }
    Ok(SearchSequence {
        order: clause.order,
        columns,
        name,
    })
}

fn cycle_mark(clause: &CycleClause, shown: &[String]) -> SqlResult<CycleMark> {
    let columns = listed_columns(&clause.columns, shown, "cycle", clause.position)?;
    let mark_name = clause.mark_column.text.clone();
    let path_name = clause.path_column.text.clone();
    if shown.contains(&mark_name) {
        return Err(syntax(
            format!(
                "cycle mark column name \"{mark_name}\" already used in WITH query column list"
            ),
            clause.position,
        ));
    }
    if shown.contains(&path_name) {
        return Err(syntax(
            format!(
                "cycle path column name \"{path_name}\" already used in WITH query column list"
            ),
            clause.position,
        ));
    }
    if mark_name == path_name {
        return Err(syntax(
            "cycle mark column name and cycle path column name are the same".into(),
            clause.position,
        ));
    }
    let (cycle, default, sql_type) = match &clause.mark_values {
        Some(values) => mark_values(values)?,
        None => (
            Value::Boolean(true),
            Value::Boolean(false),
            SqlType::Boolean,
        ),
    };
    Ok(CycleMark {
        columns,
        mark_name,
        path_name,
        cycle,
        default,
        sql_type,
    })
}

fn is_untyped(expr: &Expr) -> bool {
    matches!(
        expr.kind,
        ExprKind::Literal(Literal::String(_) | Literal::Null)
    )
}

fn constant(expr: &Expr, hint: Option<SqlType>) -> SqlResult<Value> {
    Evaluator {
        scope: Scope::empty(),
        row: &[],
        correlation: None,
    }
    .eval(expr, hint)
}

fn typed_constant(expr: &Expr) -> SqlResult<Option<(Value, SqlType)>> {
    if is_untyped(expr) {
        return Ok(None);
    }
    let value = constant(expr, None)?;
    Ok(value.sql_type().map(|sql_type| (value, sql_type)))
}

fn mark_values(values: &CycleMarkValues) -> SqlResult<(Value, Value, SqlType)> {
    let cycle = typed_constant(&values.cycle)?;
    let default = typed_constant(&values.default)?;
    let sql_type = match (&cycle, &default) {
        (Some((_, left)), Some((_, right))) => crate::set_operations::common_type(*left, *right)
            .ok_or_else(|| {
                SqlError::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!(
                        "CYCLE types {} and {} cannot be matched",
                        left.name(),
                        right.name()
                    ),
                )
                .at(values.default.position)
            })?,
        (Some((_, found)), None) | (None, Some((_, found))) => *found,
        (None, None) => SqlType::Text,
    };
    let resolve = |typed: Option<(Value, SqlType)>, expr: &Expr| -> SqlResult<Value> {
        match typed {
            Some((value, found)) if found == sql_type => Ok(value),
            Some((value, _)) => convert(value, sql_type, Conversion::Explicit),
            None => constant(expr, Some(sql_type)),
        }
    };
    Ok((
        resolve(cycle, &values.cycle)?,
        resolve(default, &values.default)?,
        sql_type,
    ))
}

fn from_items(select: &Select) -> Vec<(&TableReference, Option<&JoinCondition>)> {
    let Some(from) = &select.from else {
        return Vec::new();
    };
    std::iter::once((&from.first, None))
        .chain(
            from.joins
                .iter()
                .map(|join| (&join.table, Some(&join.condition))),
        )
        .collect()
}

fn names_table(reference: &TableReference, table: &Name) -> bool {
    reference.derived.is_none()
        && reference.generated.is_none()
        && reference.name.text == table.text
}

fn recursive_reference(select: &Select, table: &Name) -> SqlResult<String> {
    let mut found = from_items(select)
        .into_iter()
        .filter(|(reference, _)| names_table(reference, table));
    let Some((reference, _)) = found.next() else {
        return Err(syntax(
            format!(
                "with a SEARCH or CYCLE clause, the recursive reference to WITH query \"{}\" must be at the top level of its right-hand SELECT",
                table.text
            ),
            table.position,
        ));
    };
    if found.next().is_some() {
        return Err(SqlError::new(
            SqlState::INVALID_RECURSION,
            format!(
                "recursive reference to query \"{}\" must not appear more than once",
                table.text
            ),
        )
        .at(table.position));
    }
    Ok(reference
        .alias
        .clone()
        .unwrap_or_else(|| reference.name.text.clone()))
}

fn column_reference(qualifier: &str, name: &str, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Column {
            qualifier: Some(qualifier.to_owned()),
            name: name.to_owned(),
        },
        position,
    }
}

fn column_items(qualifier: &str, shown: &[String], position: usize) -> Vec<SelectItem> {
    shown
        .iter()
        .map(|name| SelectItem::Expr {
            expr: column_reference(qualifier, name, position),
            alias: Some(name.clone()),
        })
        .collect()
}

fn expanded_wildcard(
    select: &Select,
    table: &Name,
    qualifier: &str,
    shown: &[String],
    position: usize,
) -> SqlResult<Vec<SelectItem>> {
    let mut items = Vec::new();
    for (reference, condition) in from_items(select) {
        if matches!(
            condition,
            Some(JoinCondition::Using(_) | JoinCondition::Natural)
        ) {
            return Err(SqlError::not_supported(
                "SELECT * over a join with USING or NATURAL in the recursive term of a query with SEARCH or CYCLE",
            )
            .at(position));
        }
        if names_table(reference, table) {
            items.extend(column_items(qualifier, shown, position));
            continue;
        }
        let named = match &reference.alias {
            Some(alias) => alias.clone(),
            None if reference.derived.is_none() && reference.generated.is_none() => reference
                .name
                .text
                .rsplit(crate::schemas::QUALIFIER)
                .next()
                .unwrap_or_default()
                .to_owned(),
            None => {
                return Err(SqlError::not_supported(
                    "SELECT * over an unnamed FROM item in the recursive term of a query with SEARCH or CYCLE",
                )
                .at(position));
            }
        };
        items.push(SelectItem::Wildcard {
            qualifier: Some(named),
            position,
        });
    }
    Ok(items)
}

fn fields_of(row: &[Value], columns: &[usize]) -> Vec<Value> {
    columns
        .iter()
        .map(|index| row.get(*index).cloned().unwrap_or(Value::Null))
        .collect()
}

fn record_array(items: Vec<Value>) -> Value {
    Value::Array(Box::new(ArrayValue::new(ArrayElement::Record, items)))
}

fn record_items(value: Value) -> Vec<Value> {
    match value {
        Value::Array(array) => array.items,
        _ => Vec::new(),
    }
}

fn breadth_record(depth: i64, fields: Vec<Value>) -> Value {
    let mut all = Vec::with_capacity(fields.len() + 1);
    all.push(Value::BigInt(depth));
    all.extend(fields);
    crate::records::anonymous(all)
}

fn depth_of(sequence: &Value) -> i64 {
    match sequence {
        Value::Defined(defined) => match defined.as_ref() {
            DefinedValue::Record(record) => record
                .fields
                .first()
                .and_then(Value::as_integer)
                .unwrap_or(0),
            _ => 0,
        },
        _ => 0,
    }
}
