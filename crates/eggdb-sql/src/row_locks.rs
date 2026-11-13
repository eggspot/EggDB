use crate::advisory_locks::{RowWait, lock_row};
use crate::ast::{BinaryOperator, Expr, ExprKind, Literal, RowLockWait, Select, SelectItem};
use crate::encoding::TableDefinition;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{self, TableCache};
use crate::store::KeyValueRead;
use crate::types::Value;

fn refused(clause: &str) -> SqlError {
    SqlError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("FOR UPDATE is not allowed with {clause}"),
    )
}

fn column(qualifier: &str, name: &str) -> Expr {
    Expr {
        kind: ExprKind::Column {
            qualifier: Some(qualifier.to_owned()),
            name: name.to_owned(),
        },
        position: 0,
    }
}

fn constant(value: Value) -> Expr {
    Expr {
        kind: ExprKind::Constant {
            sql_type: value.sql_type(),
            value,
        },
        position: 0,
    }
}

fn literal_count(expr: Option<&Expr>) -> Option<usize> {
    match &expr?.kind {
        ExprKind::Literal(Literal::Integer(count)) => usize::try_from(*count).ok(),
        _ => None,
    }
}

fn key_query(select: &Select, table: &TableDefinition, qualifier: &str) -> Select {
    let mut keys = select.clone();
    keys.items = table
        .primary_key
        .iter()
        .map(|&index| SelectItem::Expr {
            expr: column(qualifier, &table.columns[index].name),
            alias: None,
        })
        .collect();
    keys.limit = None;
    keys.offset = None;
    keys.locking = None;
    keys
}

fn is_locked_row(table: &TableDefinition, qualifier: &str, keys: &[Vec<Value>]) -> Expr {
    if keys.is_empty() {
        return Expr {
            kind: ExprKind::Literal(Literal::Boolean(false)),
            position: 0,
        };
    }
    if let [only] = table.primary_key.as_slice() {
        return Expr {
            kind: ExprKind::InList {
                expr: Box::new(column(qualifier, &table.columns[*only].name)),
                list: keys
                    .iter()
                    .filter_map(|key| key.first().cloned().map(constant))
                    .collect(),
                negated: false,
            },
            position: 0,
        };
    }
    let matches = keys
        .iter()
        .map(|key| Expr {
            kind: ExprKind::And(
                table
                    .primary_key
                    .iter()
                    .zip(key)
                    .map(|(&index, value)| Expr {
                        kind: ExprKind::Binary(
                            BinaryOperator::Equal,
                            Box::new(column(qualifier, &table.columns[index].name)),
                            Box::new(constant(value.clone())),
                        ),
                        position: 0,
                    })
                    .collect(),
            ),
            position: 0,
        })
        .collect();
    Expr {
        kind: ExprKind::Or(matches),
        position: 0,
    }
}

/// Locks the rows a `SELECT ... FOR UPDATE | FOR SHARE` over one table returns, for the
/// session's transaction (D1126): it finds their primary keys with the same `FROM` and
/// `WHERE`, locks each, and with `SKIP LOCKED` narrows the query to the rows it could lock, in
/// the query's order, up to its `OFFSET` and `LIMIT`. A table without a primary key, or a
/// query over a join, a view or a function, locks nothing; one with `GROUP BY`, `DISTINCT` or
/// `HAVING` is refused, as PostgreSQL refuses it.
pub(crate) fn lock_selected_rows(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    select: &mut Select,
) -> SqlResult<()> {
    let Some(locking) = select.locking.take() else {
        return Ok(());
    };
    if !select.group_by.is_empty() || select.grouping_sets.is_some() {
        return Err(refused("GROUP BY clause"));
    }
    if select.distinct || !select.distinct_on.is_empty() {
        return Err(refused("DISTINCT clause"));
    }
    if select.having.is_some() {
        return Err(refused("HAVING clause"));
    }
    let Some(from) = &select.from else {
        return Ok(());
    };
    if !from.joins.is_empty()
        || from.first.derived.is_some()
        || from.first.generated.is_some()
    {
        return Ok(());
    }
    let table = tables.require(reader, &from.first.name)?;
    if table.primary_key.is_empty() {
        return Ok(());
    }
    let qualifier = from
        .first
        .alias
        .clone()
        .unwrap_or_else(|| crate::schemas::base_name(&from.first.name.text).to_owned());
    let found = execute::select(reader, tables, &key_query(select, &table, &qualifier), false)?;
    let wanted = match (literal_count(select.limit.as_ref()), literal_count(select.offset.as_ref())) {
        (Some(limit), offset) => Some(limit.saturating_add(offset.unwrap_or(0))),
        (None, _) => None,
    };
    let wait = match locking.wait {
        RowLockWait::Wait => RowWait::Wait,
        RowLockWait::NoWait => RowWait::NoWait,
        RowLockWait::SkipLocked => RowWait::SkipLocked,
    };
    let mut locked = Vec::new();
    for key_values in found.rows {
        if wanted.is_some_and(|wanted| locked.len() >= wanted) {
            break;
        }
        let mut row = vec![Value::Null; table.columns.len()];
        for (&index, value) in table.primary_key.iter().zip(&key_values) {
            if let Some(slot) = row.get_mut(index) {
                *slot = value.clone();
            }
        }
        if lock_row(&execute::row_key(&table, &row), locking.exclusive, wait)? {
            locked.push(key_values);
        }
    }
    if locking.wait == RowLockWait::SkipLocked {
        let narrowed = is_locked_row(&table, &qualifier, &locked);
        select.filter = Some(match select.filter.take() {
            Some(filter) => Expr {
                position: filter.position,
                kind: ExprKind::And(vec![filter, narrowed]),
            },
            None => narrowed,
        });
    }
    Ok(())
}
