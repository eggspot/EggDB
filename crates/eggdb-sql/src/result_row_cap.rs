use crate::ast::{Expr, ExprKind, Literal, Query, SetExpr, Statement};

pub(crate) fn cap_result_rows(statement: &mut Statement, most_rows: u64) {
    let limit = match statement {
        Statement::Select(select) if !crate::routines::select_may_write(select) => {
            &mut select.limit
        }
        Statement::Query(query) if query_only_reads(query) => query_limit(query),
        _ => return,
    };
    lower_limit(limit, most_rows);
}

fn query_limit(query: &mut Query) -> &mut Option<Expr> {
    let tail_is_empty =
        query.order_by.is_empty() && query.limit.is_none() && query.offset.is_none();
    match &mut query.body {
        SetExpr::Select(select) if tail_is_empty => &mut select.limit,
        _ => &mut query.limit,
    }
}

fn query_only_reads(query: &Query) -> bool {
    query
        .with
        .iter()
        .all(|common| query_only_reads(&common.query))
        && body_only_reads(&query.body)
}

fn body_only_reads(body: &SetExpr) -> bool {
    match body {
        SetExpr::Select(select) => !crate::routines::select_may_write(select),
        SetExpr::Operation(operation) => {
            body_only_reads(&operation.left) && body_only_reads(&operation.right)
        }
        SetExpr::Query(query) => query_only_reads(query),
    }
}

fn lower_limit(limit: &mut Option<Expr>, most_rows: u64) {
    let current = match limit.as_ref() {
        None => None,
        Some(expr) if matches!(expr.kind, ExprKind::OracleFunction { .. }) => return,
        Some(expr) => match crate::execute::row_count(Some(expr), "LIMIT") {
            Ok(count) => count,
            Err(_) => return,
        },
    };
    if current.is_some_and(|count| count <= most_rows) {
        return;
    }
    let position = limit.as_ref().map_or(0, |expr| expr.position);
    *limit = Some(Expr {
        kind: ExprKind::Literal(Literal::Integer(
            i64::try_from(most_rows).unwrap_or(i64::MAX),
        )),
        position,
    });
}
