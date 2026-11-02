use super::variables::BatchContext;
use crate::ast::{Expr, ExprKind, Literal, Select, SelectItem, SetExpr, Statement};
use crate::eval::{children, children_mut};
use crate::session::OutputColumn;
use crate::types::Value;

pub(crate) const ASSIGNED_VARIABLE: &str = "@";

type ColumnReference = (Option<String>, String);

pub(crate) struct RowAssignment {
    pub(crate) inputs: Statement,
    pub(crate) values: Vec<Expr>,
    columns: Vec<ColumnReference>,
}

fn select_of(query: &Statement) -> Option<&Select> {
    match query {
        Statement::Select(select) => Some(select),
        Statement::Query(query) => match &query.body {
            SetExpr::Select(select) => Some(select.as_ref()),
            _ => None,
        },
        _ => None,
    }
}

fn select_of_mut(query: &mut Statement) -> Option<&mut Select> {
    match query {
        Statement::Select(select) => Some(select),
        Statement::Query(query) => match &mut query.body {
            SetExpr::Select(select) => Some(select.as_mut()),
            _ => None,
        },
        _ => None,
    }
}

fn item_values(select: &Select) -> impl Iterator<Item = &Expr> {
    select.items.iter().filter_map(|item| match item {
        SelectItem::Expr { expr, .. } => Some(expr),
        SelectItem::Wildcard { .. } => None,
    })
}

fn any_node(expr: &Expr, test: &dyn Fn(&Expr) -> bool) -> bool {
    test(expr) || children(expr).into_iter().any(|child| any_node(child, test))
}

fn is_assigned_variable(expr: &Expr) -> bool {
    matches!(&expr.kind, ExprKind::Column { qualifier: Some(qualifier), .. } if qualifier == ASSIGNED_VARIABLE)
}

pub(crate) fn reads_assigned_variables(query: &Statement) -> bool {
    select_of(query).is_some_and(|select| {
        item_values(select).any(|value| any_node(value, &is_assigned_variable))
    })
}

pub(crate) fn refuses_per_row(query: &Statement) -> Option<&'static str> {
    let select = select_of(query)?;
    let aggregates = item_values(select).any(|value| {
        any_node(value, &|node: &Expr| {
            matches!(node.kind, ExprKind::Aggregate { .. } | ExprKind::Subquery(_))
        })
    });
    if aggregates || select.distinct {
        return Some(
            "A SELECT that assigns a variable from itself with aggregates, subqueries or DISTINCT (use SET @v = (SELECT ...))",
        );
    }
    None
}

fn collect_columns(expr: &Expr, columns: &mut Vec<ColumnReference>) {
    if let ExprKind::Column { qualifier, name } = &expr.kind {
        if qualifier.as_deref() != Some(ASSIGNED_VARIABLE) {
            let reference = (qualifier.clone(), name.clone());
            if !columns.contains(&reference) {
                columns.push(reference);
            }
        }
        return;
    }
    for child in children(expr) {
        collect_columns(child, columns);
    }
}

fn column_item(reference: &ColumnReference, position: usize) -> SelectItem {
    SelectItem::Expr {
        expr: Expr {
            kind: ExprKind::Column {
                qualifier: reference.0.clone(),
                name: reference.1.clone(),
            },
            position,
        },
        alias: None,
    }
}

impl RowAssignment {
    pub(crate) fn plan(query: &Statement) -> Option<Self> {
        let mut inputs = query.clone();
        let select = select_of_mut(&mut inputs)?;
        let values: Vec<Expr> = item_values(select).cloned().collect();
        let mut columns = Vec::new();
        for value in &values {
            collect_columns(value, &mut columns);
        }
        let position = values.first().map_or(0, |value| value.position);
        let mut items = vec![SelectItem::Expr {
            expr: Expr {
                kind: ExprKind::Literal(Literal::Integer(1)),
                position,
            },
            alias: None,
        }];
        items.extend(columns.iter().map(|reference| column_item(reference, position)));
        select.items = items;
        Some(Self {
            inputs,
            values,
            columns,
        })
    }

    pub(crate) fn bound(
        &self,
        value: &Expr,
        output: &[OutputColumn],
        row: &[Value],
        context: &BatchContext,
    ) -> Expr {
        let mut bound = value.clone();
        self.bind(&mut bound, output, row, context);
        bound
    }

    fn bind(&self, expr: &mut Expr, output: &[OutputColumn], row: &[Value], context: &BatchContext) {
        if let ExprKind::Column { qualifier, name } = &expr.kind {
            let (qualifier, name) = (qualifier.clone(), name.clone());
            let position = expr.position;
            if qualifier.as_deref() == Some(ASSIGNED_VARIABLE) {
                if let Some(variable) = context.variable(&name) {
                    *expr = variable.constant(position);
                }
                return;
            }
            let reference = (qualifier, name);
            if let Some(index) = self.columns.iter().position(|known| *known == reference) {
                let value = row.get(index + 1).cloned().unwrap_or(Value::Null);
                let sql_type = output
                    .get(index + 1)
                    .and_then(|column| column.sql_type)
                    .or_else(|| value.sql_type());
                *expr = Expr {
                    kind: ExprKind::Constant { value, sql_type },
                    position,
                };
            }
            return;
        }
        for child in children_mut(expr) {
            self.bind(child, output, row, context);
        }
    }
}
