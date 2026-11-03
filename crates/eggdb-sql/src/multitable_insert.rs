use crate::ast::{
    Expr, ExprKind, Insert, InsertSource, InsertTarget, InsertValue, MultitableInsert,
    MultitableInsertKind, OnDuplicate, TargetValue,
};
use crate::error::SqlResult;
use crate::execute::{self, TableCache};
use crate::name_resolution::Binding;
use crate::roles::Privilege;
use crate::session::QueryResult;
use crate::store::KeyValueWrite;
use crate::types::Value;

fn constant(value: Value) -> InsertValue {
    InsertValue::Expr(Expr {
        kind: ExprKind::Constant {
            sql_type: value.sql_type(),
            value,
        },
        position: 0,
    })
}

fn target_row(target: &InsertTarget, row: &[Value], computed: usize) -> Vec<InsertValue> {
    match &target.values {
        Some(values) => values
            .iter()
            .map(|value| match value {
                TargetValue::Computed(at) => {
                    constant(row.get(*at).cloned().unwrap_or(Value::Null))
                }
                TargetValue::Default => InsertValue::Default,
            })
            .collect(),
        None => row
            .get(computed..)
            .unwrap_or(&[])
            .iter()
            .cloned()
            .map(constant)
            .collect(),
    }
}

fn taken(condition: Option<usize>, row: &[Value]) -> bool {
    condition.is_none_or(|at| matches!(row.get(at), Some(Value::Boolean(true))))
}

fn chosen_targets(insert: &MultitableInsert, row: &[Value]) -> Vec<usize> {
    let mut chosen = Vec::new();
    let mut first_target = 0;
    let mut matched = false;
    for branch in &insert.branches {
        if taken(branch.condition, row) {
            matched = true;
            chosen.extend(first_target..first_target + branch.targets.len());
            if insert.kind == MultitableInsertKind::First {
                return chosen;
            }
        }
        first_target += branch.targets.len();
    }
    if !matched {
        chosen.extend(first_target..first_target + insert.otherwise.len());
    }
    chosen
}

/// Runs an Oracle `INSERT ALL` or `INSERT FIRST`: reads the subquery once, picks the `INTO`
/// clauses each of its rows goes to (every `WHEN` that holds, the first one, or `ELSE`), and
/// inserts each clause's rows into its table, all as one statement.
pub(crate) fn run(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    binding: &Binding<'_>,
    insert: &mut MultitableInsert,
) -> SqlResult<QueryResult> {
    let reading = Binding {
        settings: binding.settings,
        walk_expressions: binding.walk_expressions,
        target_privilege: Some(Privilege::Select),
    };
    reading.query(&*writer, tables, &mut insert.source)?;
    if crate::routines::query_calls_marked_routine(&insert.source) {
        crate::routines::prepare_query(
            writer,
            tables,
            binding.settings,
            &mut insert.source,
            crate::routines::Prefetch::Everything,
        )?;
    }
    let selected = execute::query(&*writer, tables, &insert.source, false)?;
    let targets: Vec<&InsertTarget> = insert.targets().collect();
    let mut rows_of: Vec<Vec<Vec<InsertValue>>> = vec![Vec::new(); targets.len()];
    for row in &selected.rows {
        for at in chosen_targets(insert, row) {
            if let (Some(target), Some(rows)) = (targets.get(at), rows_of.get_mut(at)) {
                rows.push(target_row(target, row, insert.computed));
            }
        }
    }
    let writing = Binding {
        settings: binding.settings,
        walk_expressions: binding.walk_expressions,
        target_privilege: Some(Privilege::Insert),
    };
    let mut inserted = 0;
    for (target, rows) in targets.into_iter().zip(rows_of) {
        if rows.is_empty() {
            continue;
        }
        inserted += rows.len();
        let mut statement = Insert {
            table: target.table.clone(),
            columns: target.columns.clone(),
            source: InsertSource::Values(rows),
            returning: None,
            on_duplicate: OnDuplicate::Fail,
            conflict_target: None,
            rollback_on_conflict: false,
            overriding: None,
        };
        writing.insert(&*writer, tables, &mut statement)?;
        execute::insert(writer, tables, &statement)?;
    }
    Ok(QueryResult::command(format!("INSERT 0 {inserted}")))
}

#[cfg(test)]
mod tests {
    use super::chosen_targets;
    use crate::ast::{
        InsertBranch, InsertTarget, MultitableInsert, MultitableInsertKind, Name, Query, Select,
        TableReference,
    };
    use crate::types::Value;

    fn target(name: &str) -> InsertTarget {
        InsertTarget {
            table: TableReference::named(
                Name {
                    text: name.to_owned(),
                    position: 0,
                },
                None,
            ),
            columns: None,
            values: None,
        }
    }

    fn insert(kind: MultitableInsertKind) -> MultitableInsert {
        MultitableInsert {
            kind,
            source: Query::of_select(
                Select {
                    distinct: false,
                    items: Vec::new(),
                    from: None,
                    filter: None,
                    group_by: Vec::new(),
                    grouping_sets: None,
                    distinct_on: Vec::new(),
                    having: None,
                    order_by: Vec::new(),
                    limit: None,
                    offset: None,
                    source_row_limit: None,
                },
                0,
            ),
            computed: 2,
            branches: vec![
                InsertBranch {
                    condition: Some(0),
                    targets: vec![target("small")],
                },
                InsertBranch {
                    condition: Some(1),
                    targets: vec![target("medium"), target("audit")],
                },
            ],
            otherwise: vec![target("large")],
        }
    }

    #[test]
    fn insert_all_takes_every_branch_that_holds_and_else_only_when_none_does() {
        let all = insert(MultitableInsertKind::All);
        let both = [Value::Boolean(true), Value::Boolean(true)];
        let neither = [Value::Boolean(false), Value::Null];
        assert_eq!(chosen_targets(&all, &both), vec![0, 1, 2]);
        assert_eq!(chosen_targets(&all, &neither), vec![3]);
    }

    #[test]
    fn insert_first_stops_at_the_first_branch_that_holds() {
        let first = insert(MultitableInsertKind::First);
        let both = [Value::Boolean(true), Value::Boolean(true)];
        let second = [Value::Null, Value::Boolean(true)];
        assert_eq!(chosen_targets(&first, &both), vec![0]);
        assert_eq!(chosen_targets(&first, &second), vec![1, 2]);
    }
}
