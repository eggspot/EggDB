use super::StatementTriggerCall;
use crate::ast::Statement;
use crate::error::SqlResult;
use crate::table_variables::{self, EnteredVariables, TableVariable, TableVariableRules};
use std::cell::RefCell;
use std::sync::Arc;

struct TransitionFrame {
    depth: usize,
    variables: Vec<(String, String)>,
}

thread_local! {
    static FRAMES: RefCell<Vec<TransitionFrame>> = const { RefCell::new(Vec::new()) };
}

pub(super) struct EnteredTransitionTables {
    _variables: EnteredVariables,
    height: usize,
}

impl Drop for EnteredTransitionTables {
    fn drop(&mut self) {
        FRAMES.with(|frames| {
            if let Ok(mut frames) = frames.try_borrow_mut() {
                frames.truncate(self.height);
            }
        });
    }
}

fn variable_name(depth: usize, name: &str) -> String {
    format!(
        "{}transition {depth} {name}",
        crate::temp_tables::HIDDEN_COLUMN_MARKER
    )
}

pub(super) fn enter(call: &StatementTriggerCall) -> Option<EnteredTransitionTables> {
    let trigger = &call.trigger;
    if trigger.old_table.is_none() && trigger.new_table.is_none() {
        return None;
    }
    let depth = crate::plpgsql::routine_depth() + 1;
    let mut known = (*table_variables::current()).clone();
    let mut variables = Vec::with_capacity(2);
    let sources = [
        (&trigger.old_table, &call.old_rows),
        (&trigger.new_table, &call.new_rows),
    ];
    for (name, rows) in sources {
        let Some(name) = name else {
            continue;
        };
        let variable = variable_name(depth, name);
        known.insert(
            variable.clone(),
            TableVariable {
                columns: call.columns.as_ref().clone(),
                rows: rows.clone(),
                rules: TableVariableRules::default(),
            },
        );
        variables.push((name.clone(), variable));
    }
    let entered = table_variables::enter(&Arc::new(known));
    let height = FRAMES.with(|frames| match frames.try_borrow_mut() {
        Ok(mut frames) => {
            let height = frames.len();
            frames.push(TransitionFrame { depth, variables });
            height
        }
        Err(_) => usize::MAX,
    });
    Some(EnteredTransitionTables {
        _variables: entered,
        height,
    })
}

fn visible_variables() -> Vec<(String, String)> {
    let depth = crate::plpgsql::routine_depth();
    FRAMES.with(|frames| {
        frames
            .try_borrow()
            .ok()
            .and_then(|frames| {
                frames
                    .last()
                    .filter(|frame| frame.depth == depth)
                    .map(|frame| frame.variables.clone())
            })
            .unwrap_or_default()
    })
}

pub(crate) fn replace_transition_tables(statement: &mut Statement) -> SqlResult<()> {
    let variables = visible_variables();
    if variables.is_empty() {
        return Ok(());
    }
    crate::with_statement::replace_named_tables(statement, &variables)
}
