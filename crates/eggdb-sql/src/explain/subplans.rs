use crate::ast::{
    BinaryOperator, Expr, ExprKind, JoinCondition, Select, SelectItem, Subquery, SubqueryState,
    SubqueryTest,
};
use crate::error::{SqlError, SqlResult};
use crate::execute::{self, ExplainedQuery, TableCache};
use crate::store::KeyValueRead;
use crate::subquery::{self, PreparedForExplain};
use std::borrow::Cow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubplanRole {
    Init { parameter: usize },
    Sub { hashed: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubplanPlace {
    Projection,
    Condition,
}

pub(crate) struct Subplan {
    pub(crate) number: usize,
    pub(crate) role: SubplanRole,
    pub(crate) place: SubplanPlace,
    pub(crate) correlated: bool,
    pub(crate) position: usize,
    pub(crate) plan: ExplainedQuery,
}

#[derive(Default)]
struct Numbering {
    plans: usize,
    parameters: usize,
}

pub(crate) fn explain_subplans(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    select: &Select,
) -> SqlResult<ExplainedQuery> {
    let prepared = subquery::prepare_for_explain(reader, tables, select)?;
    explain_prepared(
        reader,
        tables,
        &prepared,
        Cow::Borrowed(&prepared.select),
        &mut Numbering::default(),
    )
}

fn placed_expressions(select: &Select) -> Vec<(&Expr, SubplanPlace)> {
    let mut placed: Vec<(&Expr, SubplanPlace)> = select
        .items
        .iter()
        .filter_map(|item| match item {
            SelectItem::Expr { expr, .. } => Some((expr, SubplanPlace::Projection)),
            _ => None,
        })
        .collect();
    let condition = |expr| (expr, SubplanPlace::Condition);
    placed.extend(select.filter.iter().map(condition));
    placed.extend(select.having.iter().map(condition));
    if let Some(from) = &select.from {
        placed.extend(from.joins.iter().filter_map(|join| match &join.condition {
            JoinCondition::On(expr) => Some(condition(expr)),
            _ => None,
        }));
    }
    placed.extend(select.group_by.iter().map(condition));
    placed.extend(select.distinct_on.iter().map(condition));
    placed.extend(select.order_by.iter().map(|item| condition(&item.expr)));
    placed.extend(select.limit.iter().map(condition));
    placed.extend(select.offset.iter().map(condition));
    placed
}

fn explain_prepared(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    prepared: &PreparedForExplain,
    select: Cow<'_, Select>,
    numbering: &mut Numbering,
) -> SqlResult<ExplainedQuery> {
    let mut subplans = Vec::new();
    for (expr, place) in placed_expressions(&select) {
        collect(
            reader,
            tables,
            prepared,
            expr,
            place,
            numbering,
            &mut subplans,
        )?;
    }
    let mut out = execute::explain_select(reader, tables, &select)?;
    out.subplans = subplans;
    Ok(out)
}

fn collect(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    prepared: &PreparedForExplain,
    expr: &Expr,
    place: SubplanPlace,
    numbering: &mut Numbering,
    out: &mut Vec<Subplan>,
) -> SqlResult<()> {
    if let ExprKind::Subquery(found) = &expr.kind {
        if let Some(operand) = found.test.operand() {
            collect(reader, tables, prepared, operand, place, numbering, out)?;
        }
        let planned = subplan(reader, tables, prepared, found, place, numbering)?;
        out.push(planned);
        return Ok(());
    }
    for child in crate::eval::children(expr) {
        collect(reader, tables, prepared, child, place, numbering, out)?;
    }
    Ok(())
}

fn missing_subplan() -> SqlError {
    SqlError::not_supported("EXPLAIN of this subquery")
}

fn subplan(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    prepared: &PreparedForExplain,
    found: &Subquery,
    place: SubplanPlace,
    numbering: &mut Numbering,
) -> SqlResult<Subplan> {
    let original = subquery::lone_select(&found.query).ok_or_else(missing_subplan)?;
    let (plan, correlated) = match &found.state {
        SubqueryState::Correlated { template, .. } => {
            let inner = prepared
                .correlated
                .get(*template)
                .ok_or_else(missing_subplan)?;
            let select = if original.limit.is_none() && inner.select.limit.is_some() {
                let mut uncapped = inner.select.clone();
                uncapped.limit = None;
                Cow::Owned(uncapped)
            } else {
                Cow::Borrowed(&inner.select)
            };
            (
                explain_prepared(reader, tables, inner, select, numbering)?,
                true,
            )
        }
        _ => {
            let inner = subquery::prepare_for_explain(reader, tables, original)?;
            (
                explain_prepared(
                    reader,
                    tables,
                    &inner,
                    Cow::Borrowed(&inner.select),
                    numbering,
                )?,
                false,
            )
        }
    };
    numbering.plans += 1;
    let role = match (&found.test, correlated) {
        (_, true) => SubplanRole::Sub { hashed: false },
        (SubqueryTest::Scalar | SubqueryTest::Exists | SubqueryTest::Array, false) => {
            let parameter = numbering.parameters;
            numbering.parameters += 1;
            SubplanRole::Init { parameter }
        }
        (SubqueryTest::In { .. }, false) => SubplanRole::Sub { hashed: true },
        (
            SubqueryTest::Compare {
                operator: BinaryOperator::Equal,
                all: false,
                ..
            }
            | SubqueryTest::Compare {
                operator: BinaryOperator::NotEqual,
                all: true,
                ..
            },
            false,
        ) => SubplanRole::Sub { hashed: true },
        _ => SubplanRole::Sub { hashed: false },
    };
    Ok(Subplan {
        number: numbering.plans,
        role,
        place,
        correlated,
        position: found.query.position,
        plan,
    })
}

pub(crate) fn subplan_reference(subplan: &Subplan, test: &SubqueryTest) -> String {
    let negated = matches!(test, SubqueryTest::In { negated: true, .. })
        || matches!(
            test,
            SubqueryTest::Compare {
                operator: BinaryOperator::NotEqual,
                all: true,
                ..
            }
        ) && matches!(subplan.role, SubplanRole::Sub { hashed: true });
    let reference = match subplan.role {
        SubplanRole::Init { parameter } => return format!("${parameter}"),
        SubplanRole::Sub { hashed: true } => format!("(hashed SubPlan {})", subplan.number),
        SubplanRole::Sub { hashed: false } => format!("(SubPlan {})", subplan.number),
    };
    if negated {
        format!("(NOT {reference})")
    } else {
        reference
    }
}

pub(crate) fn subplan_label(subplan: &Subplan) -> String {
    match subplan.role {
        SubplanRole::Init { parameter } => {
            format!("InitPlan {} (returns ${parameter})", subplan.number)
        }
        SubplanRole::Sub { .. } => format!("SubPlan {}", subplan.number),
    }
}

pub(crate) fn mentions_subplan(expr: &Expr) -> bool {
    matches!(expr.kind, ExprKind::Subquery(_))
        || crate::eval::children(expr)
            .into_iter()
            .any(mentions_subplan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planned(role: SubplanRole) -> Subplan {
        Subplan {
            number: 2,
            role,
            place: SubplanPlace::Condition,
            correlated: false,
            position: 0,
            plan: ExplainedQuery::default(),
        }
    }

    fn column() -> Expr {
        Expr {
            kind: ExprKind::Column {
                qualifier: None,
                name: "a".to_owned(),
            },
            position: 0,
        }
    }

    #[test]
    fn references_follow_postgresqls_spelling_for_each_role() {
        let init = planned(SubplanRole::Init { parameter: 0 });
        assert_eq!(subplan_reference(&init, &SubqueryTest::Scalar), "$0");
        assert_eq!(subplan_label(&init), "InitPlan 2 (returns $0)");
        let hashed = planned(SubplanRole::Sub { hashed: true });
        let not_in = SubqueryTest::In {
            expr: column(),
            negated: true,
        };
        assert_eq!(
            subplan_reference(&hashed, &not_in),
            "(NOT (hashed SubPlan 2))"
        );
        assert_eq!(subplan_label(&hashed), "SubPlan 2");
        let plain = planned(SubplanRole::Sub { hashed: false });
        assert_eq!(
            subplan_reference(&plain, &SubqueryTest::Exists),
            "(SubPlan 2)"
        );
    }
}
