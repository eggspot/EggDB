use crate::ast::{DerivedTable, Expr, ExprKind, JoinKind, Name, Query, Select, SelectItem, SetExpr};
use crate::eval::{children, children_mut, output_name};

/// The visible columns, in order, of the stored table a derived table's `SELECT *` reads, or
/// `None` when the name is not a stored table of this statement.
pub(crate) type StoredColumns<'c> = dyn FnMut(&Name) -> Option<Vec<String>> + 'c;

fn keeps_every_row_of_its_tables(select: &Select) -> bool {
    select.from.as_ref().is_some_and(|from| {
        from.joins
            .iter()
            .all(|join| matches!(join.kind, JoinKind::Inner | JoinKind::Cross | JoinKind::Comma))
    })
}

fn filters_rows_only(query: &Query) -> Option<&Select> {
    let SetExpr::Select(select) = &query.body else {
        return None;
    };
    let plain = query.with.is_empty()
        && query.order_by.is_empty()
        && query.limit.is_none()
        && query.offset.is_none()
        && !select.distinct
        && select.group_by.is_empty()
        && select.having.is_none()
        && select.limit.is_none()
        && select.offset.is_none()
        && select.source_row_limit.is_none()
        && !crate::aggregate::is_aggregate_query(select)
        && !crate::set_returning::select_needs_expansion(select)
        && select.from.is_some();
    plain.then_some(select)
}

fn depends_only_on_its_columns(expr: &Expr) -> bool {
    let kind_allowed = matches!(
        expr.kind,
        ExprKind::Literal(_)
            | ExprKind::Column { .. }
            | ExprKind::Constant { .. }
            | ExprKind::Unary(..)
            | ExprKind::And(_)
            | ExprKind::Or(_)
            | ExprKind::Binary(..)
            | ExprKind::IsNull { .. }
            | ExprKind::IsTruth { .. }
            | ExprKind::IsDistinctFrom { .. }
            | ExprKind::Between { .. }
            | ExprKind::InList { .. }
            | ExprKind::Like { .. }
            | ExprKind::Cast { .. }
            | ExprKind::Case { .. }
    );
    kind_allowed && children(expr).into_iter().all(depends_only_on_its_columns)
}

struct Outputs {
    names: Vec<String>,
    sources: Vec<Expr>,
}

impl Outputs {
    fn source_of(&self, name: &str) -> Option<&Expr> {
        let mut found = self
            .names
            .iter()
            .zip(&self.sources)
            .filter(|(output, _)| *output == name);
        let (_, source) = found.next()?;
        found.next().is_none().then_some(source)
    }
}

fn plain_column(name: String, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Column {
            qualifier: None,
            name,
        },
        position,
    }
}

fn outputs_of(derived: &DerivedTable, stored_columns: &mut StoredColumns<'_>) -> Option<Outputs> {
    let select = filters_rows_only(&derived.query)?;
    let renamed = |index: usize| derived.column_names.get(index).map(|name| name.text.clone());
    if let [SelectItem::Wildcard {
        qualifier: None,
        position,
    }] = select.items.as_slice()
    {
        let from = select.from.as_ref()?;
        if !from.joins.is_empty() || from.first.derived.is_some() || from.first.generated.is_some()
        {
            return None;
        }
        let columns = stored_columns(&from.first.name)?;
        if !derived.column_names.is_empty() && derived.column_names.len() != columns.len() {
            return None;
        }
        let names = (0..columns.len())
            .map(|index| renamed(index).or_else(|| columns.get(index).cloned()))
            .collect::<Option<Vec<String>>>()?;
        let sources = columns
            .into_iter()
            .map(|column| plain_column(column, *position))
            .collect();
        return Some(Outputs { names, sources });
    }
    let mut names = Vec::with_capacity(select.items.len());
    let mut sources = Vec::with_capacity(select.items.len());
    for (index, item) in select.items.iter().enumerate() {
        let SelectItem::Expr { expr, alias } = item else {
            return None;
        };
        if !matches!(expr.kind, ExprKind::Column { .. }) {
            return None;
        }
        names.push(
            renamed(index)
                .or_else(|| alias.clone())
                .unwrap_or_else(|| output_name(expr)),
        );
        sources.push(expr.clone());
    }
    Some(Outputs { names, sources })
}

fn names_only(expr: &Expr, label: &str, alone: bool, outputs: &Outputs) -> bool {
    if let ExprKind::Column { qualifier, name } = &expr.kind {
        let ours = match qualifier {
            Some(qualifier) => qualifier == label,
            None => alone,
        };
        return ours && outputs.source_of(name).is_some();
    }
    children(expr)
        .into_iter()
        .all(|child| names_only(child, label, alone, outputs))
}

fn moved_inside(expr: &mut Expr, outputs: &Outputs) {
    if let ExprKind::Column { name, .. } = &expr.kind {
        if let Some(source) = outputs.source_of(name) {
            let position = expr.position;
            *expr = source.clone();
            expr.position = position;
        }
        return;
    }
    for child in children_mut(expr) {
        moved_inside(child, outputs);
    }
}

fn conjuncts(filter: &Expr) -> Vec<&Expr> {
    match &filter.kind {
        ExprKind::And(parts) => parts.iter().collect(),
        _ => vec![filter],
    }
}

fn with_condition(select: &mut Select, condition: Expr) {
    select.filter = Some(match select.filter.take() {
        None => condition,
        Some(Expr {
            kind: ExprKind::And(mut parts),
            position,
        }) => {
            parts.push(condition);
            Expr {
                kind: ExprKind::And(parts),
                position,
            }
        }
        Some(existing) => {
            let position = existing.position;
            Expr {
                kind: ExprKind::And(vec![existing, condition]),
                position,
            }
        }
    });
}

/// Copies each `WHERE` condition of `select` that reads only the columns of one derived table
/// (a view or a subquery in `FROM`) into that table's own `WHERE`, rewritten over the columns
/// it reads, so a lookup through a view reads by key instead of reading the whole view. The
/// condition stays in `select` too. Only conditions built from columns, constants, operators,
/// casts and `CASE` move, only into a derived table that filters the plain columns of its
/// tables (no grouping, `DISTINCT`, limits, windows or set-returning functions), and only when
/// no outer join could turn a row it removes into a row extended with NULLs.
pub(crate) fn push_conditions_into_derived(
    select: &mut Select,
    stored_columns: &mut StoredColumns<'_>,
) {
    if !keeps_every_row_of_its_tables(select) {
        return;
    }
    let Some(filter) = select.filter.clone() else {
        return;
    };
    let Some(from) = select.from.as_mut() else {
        return;
    };
    let alone = from.joins.is_empty();
    let references = std::iter::once(&mut from.first).chain(from.joins.iter_mut().map(|join| &mut join.table));
    for reference in references {
        let label = reference
            .alias
            .clone()
            .unwrap_or_else(|| reference.name.text.clone());
        let Some(derived) = reference.derived.as_mut() else {
            continue;
        };
        if derived.lateral {
            continue;
        }
        let Some(outputs) = outputs_of(derived, stored_columns) else {
            continue;
        };
        let moved: Vec<Expr> = conjuncts(&filter)
            .into_iter()
            .filter(|conjunct| {
                depends_only_on_its_columns(conjunct) && names_only(conjunct, &label, alone, &outputs)
            })
            .map(|conjunct| {
                let mut inside = conjunct.clone();
                moved_inside(&mut inside, &outputs);
                inside
            })
            .collect();
        if let SetExpr::Select(inner) = &mut derived.query.body {
            for condition in moved {
                with_condition(inner, condition);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::push_conditions_into_derived;
    use crate::ast::{Select, Statement};
    use crate::lexer::tokenize;
    use crate::parser::{Parameters, Parser};
    use crate::settings::Settings;

    fn select(text: &str) -> Select {
        let settings = Settings::default();
        let tokens = tokenize(text).expect("tokens");
        let mut parser = Parser::new(text, &tokens).with_context(Parameters::Absent, &settings);
        match parser.statement().expect("statement") {
            Statement::Select(select) => select,
            other => panic!("not a select: {other:?}"),
        }
    }

    fn pushed(text: &str) -> Select {
        let mut select = select(text);
        push_conditions_into_derived(&mut select, &mut |_| {
            Some(vec!["id".to_owned(), "name".to_owned(), "score".to_owned()])
        });
        select
    }

    fn derived_filter(select: &Select) -> Option<String> {
        let from = select.from.as_ref()?;
        std::iter::once(&from.first)
            .chain(from.joins.iter().map(|join| &join.table))
            .filter_map(|reference| reference.derived.as_ref())
            .find_map(|derived| match &derived.query.body {
                crate::ast::SetExpr::Select(inner) => {
                    inner.filter.as_ref().map(|filter| format!("{filter:?}"))
                }
                _ => None,
            })
    }

    #[test]
    fn a_condition_on_a_plain_column_moves_into_the_derived_table() {
        let select = pushed("SELECT v.name FROM (SELECT id AS k, name FROM people) v WHERE v.k = 5 AND name <> 'x'");
        let filter = derived_filter(&select).expect("a moved condition");
        assert!(filter.contains("\"id\""), "{filter}");
        assert!(filter.contains("String(\"x\")"), "{filter}");
        assert!(select.filter.is_some());
    }

    #[test]
    fn a_star_reads_the_stored_columns_through_the_new_names() {
        let select = pushed("SELECT a FROM (SELECT * FROM people) AS v (a, b, c) WHERE b = 'p'");
        let filter = derived_filter(&select).expect("a moved condition");
        assert!(filter.contains("\"name\""), "{filter}");
    }

    #[test]
    fn conditions_that_could_change_the_result_stay_outside() {
        for text in [
            "SELECT * FROM (SELECT DISTINCT id FROM people) v WHERE id = 1",
            "SELECT * FROM (SELECT id FROM people LIMIT 3) v WHERE id = 1",
            "SELECT * FROM (SELECT count(*) AS n FROM people) v WHERE n = 1",
            "SELECT * FROM (SELECT id + 1 AS id FROM people) v WHERE id = 1",
            "SELECT * FROM (SELECT id FROM people) v WHERE random() < id",
            "SELECT * FROM (SELECT id FROM people) v WHERE id IN (SELECT 1)",
            "SELECT * FROM t LEFT JOIN (SELECT id FROM people) v ON true WHERE v.id IS NULL",
            "SELECT * FROM t, (SELECT id FROM people) v WHERE id = 1",
            "SELECT * FROM (SELECT id FROM people) v WHERE w.id = 1",
        ] {
            assert_eq!(derived_filter(&pushed(text)), None, "{text}");
        }
    }
}
