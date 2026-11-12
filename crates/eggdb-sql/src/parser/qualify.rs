use crate::ast::{DerivedTable, Expr, ExprKind, FromClause, Name, Query, Select, SelectItem, TableReference};
use crate::dialect::Dialect;

const QUALIFY_COLUMN: &str = "\u{2}qualify";
const QUALIFIED_TABLE: &str = "\u{2}qualified";

/// `select` with a `QUALIFY condition` clause, as a query that keeps the rows meeting the
/// condition after window functions: the select becomes a derived table whose hidden column
/// holds the condition (when it calls a window function; otherwise the condition reads the
/// select's output columns and runs outside), and `DISTINCT` moves outside, since `QUALIFY`
/// filters before duplicates are removed. `ORDER BY` and `LIMIT`, attached afterwards, apply
/// to the filtered rows and name output columns.
pub(super) fn wrap(mut inner: Select, condition: Expr, dialect: Dialect) -> Select {
    let position = condition.position;
    let distinct = std::mem::take(&mut inner.distinct);
    let filter = if crate::window::first_call(&condition).is_some() {
        inner.items.push(SelectItem::Expr {
            expr: condition,
            alias: Some(QUALIFY_COLUMN.to_owned()),
        });
        Expr {
            kind: ExprKind::Column {
                qualifier: None,
                name: QUALIFY_COLUMN.to_owned(),
            },
            position,
        }
    } else {
        condition
    };
    Select {
        distinct,
        items: vec![SelectItem::Wildcard {
            qualifier: None,
            position,
        }],
        from: Some(FromClause {
            first: TableReference {
                name: Name {
                    text: QUALIFIED_TABLE.to_owned(),
                    position,
                },
                alias: Some(QUALIFIED_TABLE.to_owned()),
                derived: Some(Box::new(DerivedTable {
                    query: Query::of_select(inner, position),
                    column_names: Vec::new(),
                    dialect,
                    lateral: false,
                    hierarchy: None,
                })),
                generated: None,
                partition_selection: None,
                system_time: None,
            },
            joins: Vec::new(),
        }),
        filter: Some(filter),
        group_by: Vec::new(),
        grouping_sets: None,
        distinct_on: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        source_row_limit: None,
    }
}
