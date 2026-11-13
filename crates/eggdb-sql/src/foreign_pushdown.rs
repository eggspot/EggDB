use crate::ast::{BinaryOperator, Expr, ExprKind, GeneratedTable, Literal, Select};

const FOREIGN_READ: &str = "eggdb_foreign";
const PUSHED_ALIAS: &str = "eggdb_pushed";

#[derive(Clone, Copy, PartialEq, Eq)]
enum RemoteDialect {
    PostgreSql,
    MySql,
    SqlServer,
}

fn dialect_of(source: &str) -> Option<RemoteDialect> {
    if source.starts_with("postgresql://") || source.starts_with("postgres://") || source.starts_with("eggdb://") {
        Some(RemoteDialect::PostgreSql)
    } else if source.starts_with("mysql://") {
        Some(RemoteDialect::MySql)
    } else if source.starts_with("sqlserver://") {
        Some(RemoteDialect::SqlServer)
    } else {
        None
    }
}

fn quoted(dialect: RemoteDialect, name: &str) -> String {
    match dialect {
        RemoteDialect::MySql => format!("`{}`", name.replace('`', "``")),
        RemoteDialect::SqlServer => format!("[{}]", name.replace(']', "]]")),
        RemoteDialect::PostgreSql => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}

fn remote_literal(dialect: RemoteDialect, literal: &Literal) -> Option<String> {
    match literal {
        Literal::Integer(number) => Some(number.to_string()),
        Literal::Decimal(text) if text.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'e' | b'E' | b'+')) => Some(text.clone()),
        Literal::String(text) => Some(format!("'{}'", text.replace('\'', "''"))),
        Literal::Boolean(value) => Some(match (dialect, value) {
            (RemoteDialect::SqlServer, true) => "1".to_owned(),
            (RemoteDialect::SqlServer, false) => "0".to_owned(),
            (_, true) => "TRUE".to_owned(),
            (_, false) => "FALSE".to_owned(),
        }),
        _ => None,
    }
}

fn is_number(literal: &Literal) -> bool {
    matches!(literal, Literal::Integer(_) | Literal::Decimal(_))
}

fn row_column<'e>(expr: &'e Expr, row: &str) -> Option<&'e str> {
    let inner = match &expr.kind {
        ExprKind::Cast { expr, .. } => expr,
        _ => expr,
    };
    let ExprKind::Binary(BinaryOperator::JsonFieldText, object, field) = &inner.kind else {
        return None;
    };
    let ExprKind::Column { name, .. } = &object.kind else {
        return None;
    };
    let ExprKind::Literal(Literal::String(column)) = &field.kind else {
        return None;
    };
    (name == row).then_some(column.as_str())
}

fn literal_of(expr: &Expr) -> Option<&Literal> {
    match &expr.kind {
        ExprKind::Literal(literal) => Some(literal),
        ExprKind::Cast { expr, .. } => literal_of(expr),
        _ => None,
    }
}

fn mirrored(operator: &BinaryOperator) -> Option<BinaryOperator> {
    Some(match operator {
        BinaryOperator::Equal => BinaryOperator::Equal,
        BinaryOperator::Less => BinaryOperator::Greater,
        BinaryOperator::LessOrEqual => BinaryOperator::GreaterOrEqual,
        BinaryOperator::Greater => BinaryOperator::Less,
        BinaryOperator::GreaterOrEqual => BinaryOperator::LessOrEqual,
        _ => return None,
    })
}

fn operator_text(operator: &BinaryOperator) -> Option<&'static str> {
    Some(match operator {
        BinaryOperator::Equal => "=",
        BinaryOperator::Less => "<",
        BinaryOperator::LessOrEqual => "<=",
        BinaryOperator::Greater => ">",
        BinaryOperator::GreaterOrEqual => ">=",
        _ => return None,
    })
}

fn remote_condition(dialect: RemoteDialect, condition: &Expr, row: &str) -> Option<String> {
    match &condition.kind {
        ExprKind::Binary(operator, left, right) => {
            let (column, literal, operator) = match (row_column(left, row), literal_of(right)) {
                (Some(column), Some(literal)) => (column, literal, *operator),
                _ => (row_column(right, row)?, literal_of(left)?, mirrored(operator)?),
            };
            let text = operator_text(&operator)?;
            if operator != BinaryOperator::Equal && !is_number(literal) {
                return None;
            }
            Some(format!("{} {text} {}", quoted(dialect, column), remote_literal(dialect, literal)?))
        }
        ExprKind::IsNull { expr, negated } => {
            let column = row_column(expr, row)?;
            Some(format!("{} IS {}NULL", quoted(dialect, column), if *negated { "NOT " } else { "" }))
        }
        ExprKind::InList { expr, list, negated: false } => {
            let column = row_column(expr, row)?;
            let values = list
                .iter()
                .map(|item| literal_of(item).and_then(|literal| remote_literal(dialect, literal)))
                .collect::<Option<Vec<_>>>()?;
            (!values.is_empty() && values.len() <= 1_000).then(|| format!("{} IN ({})", quoted(dialect, column), values.join(", ")))
        }
        _ => None,
    }
}

fn conjuncts(filter: &Expr) -> Vec<&Expr> {
    match &filter.kind {
        ExprKind::And(parts) => parts.iter().flat_map(conjuncts).collect(),
        _ => vec![filter],
    }
}

/// Moves the filters of a `SELECT` over a lone `eggdb_foreign(source, query)` call into the
/// query sent to the source, as postgres_fdw does (D1110): equality, `IN` and `IS [NOT] NULL`
/// on any declared column, and ranges against numbers, each comparing one column the row
/// object names (`(o ->> 'col')::type`) with a literal. The local filter stays and checks the
/// rows again, so a remote comparison that is looser (a case-insensitive collation) only lets
/// through rows the local one then removes. Returns how many conditions were pushed.
pub(crate) fn push_filters_into_foreign_read(select: &mut Select) -> usize {
    let Some(filter) = &select.filter else {
        return 0;
    };
    let Some(from) = &select.from else {
        return 0;
    };
    if !from.joins.is_empty() {
        return 0;
    }
    let Some(GeneratedTable::Function(function)) = from.first.generated.as_deref() else {
        return 0;
    };
    if function.name != FOREIGN_READ || function.ordinality || function.arguments.len() != 2 {
        return 0;
    }
    let (ExprKind::Literal(Literal::String(source)), ExprKind::Literal(Literal::String(query))) =
        (&function.arguments[0].kind, &function.arguments[1].kind)
    else {
        return 0;
    };
    let Some(dialect) = dialect_of(source) else {
        return 0;
    };
    let Some(row) = function.column_names.first().map(|name| name.text.clone()) else {
        return 0;
    };
    let pushed: Vec<String> = conjuncts(filter)
        .into_iter()
        .filter_map(|condition| remote_condition(dialect, condition, &row))
        .collect();
    if pushed.is_empty() {
        return 0;
    }
    let tail = format!(") AS {PUSHED_ALIAS} WHERE {}", pushed.join(" AND "));
    if query.ends_with(&tail) {
        return 0;
    }
    let rewritten = format!("SELECT * FROM ({query}{tail}");
    let count = pushed.len();
    if let Some(from) = &mut select.from
        && let Some(GeneratedTable::Function(function)) = from.first.generated.as_deref_mut()
        && let Some(argument) = function.arguments.get_mut(1)
    {
        argument.kind = ExprKind::Literal(Literal::String(rewritten));
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{FromClause, Name, TableFunction, TableReference};

    fn at(kind: ExprKind) -> Expr {
        Expr { kind, position: 0 }
    }

    fn field(column: &str) -> Expr {
        at(ExprKind::Binary(
            BinaryOperator::JsonFieldText,
            Box::new(at(ExprKind::Column { qualifier: Some("r".to_owned()), name: "o".to_owned() })),
            Box::new(at(ExprKind::Literal(Literal::String(column.to_owned())))),
        ))
    }

    fn select(source: &str, filter: Expr) -> Select {
        let mut reference = TableReference::named(Name { text: FOREIGN_READ.to_owned(), position: 0 }, Some("r".to_owned()));
        reference.generated = Some(Box::new(GeneratedTable::Function(TableFunction {
            name: FOREIGN_READ.to_owned(),
            arguments: vec![at(ExprKind::Literal(Literal::String(source.to_owned()))), at(ExprKind::Literal(Literal::String("SELECT * FROM \"people\"".to_owned())))],
            ordinality: false,
            column_names: vec![Name { text: "o".to_owned(), position: 0 }],
            position: 0,
        })));
        Select {
            distinct: false,
            items: Vec::new(),
            from: Some(FromClause { first: reference, joins: Vec::new() }),
            filter: Some(filter),
            group_by: Vec::new(),
            grouping_sets: None,
            distinct_on: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
            source_row_limit: None,
            locking: None,
        }
    }

    fn remote_query(select: &Select) -> String {
        let Some(GeneratedTable::Function(function)) = select.from.as_ref().and_then(|from| from.first.generated.as_deref()) else {
            return String::new();
        };
        match &function.arguments[1].kind {
            ExprKind::Literal(Literal::String(query)) => query.clone(),
            _ => String::new(),
        }
    }

    #[test]
    fn equalities_lists_nulls_and_numeric_ranges_go_to_the_source() {
        let filter = at(ExprKind::And(vec![
            at(ExprKind::Binary(BinaryOperator::Equal, Box::new(field("name")), Box::new(at(ExprKind::Literal(Literal::String("O'Hara".to_owned())))))),
            at(ExprKind::Binary(BinaryOperator::Less, Box::new(at(ExprKind::Literal(Literal::Integer(30))))), Box::new(field("age")))),
            at(ExprKind::IsNull { expr: Box::new(field("left_at")), negated: false }),
            at(ExprKind::Binary(BinaryOperator::Greater, Box::new(field("city")), Box::new(at(ExprKind::Literal(Literal::String("M".to_owned())))))),
        ]));
        let mut postgres = select("postgresql://h/db", filter.clone());
        assert_eq!(push_filters_into_foreign_read(&mut postgres), 3);
        assert_eq!(
            remote_query(&postgres),
            "SELECT * FROM (SELECT * FROM \"people\") AS eggdb_pushed WHERE \"name\" = 'O''Hara' AND \"age\" > 30 AND \"left_at\" IS NULL"
        );
        assert_eq!(push_filters_into_foreign_read(&mut postgres), 0);
        let mut sqlserver = select("sqlserver://h/db", filter.clone());
        push_filters_into_foreign_read(&mut sqlserver);
        assert!(remote_query(&sqlserver).contains("[name] = 'O''Hara'"));
        let mut file = select("file:/tmp/people.csv", filter);
        assert_eq!(push_filters_into_foreign_read(&mut file), 0);
    }
}
