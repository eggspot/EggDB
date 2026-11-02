mod control;
mod cursors;
mod errors;
mod functions;
mod routines;
mod row_assignment;
mod statements;
mod variables;

pub(crate) use control::{
    Declaration, Instruction, Program, ProgramEnd, TryBlock, chooses_dialect, uses_batch_logic,
};
pub(crate) use cursors::{
    Cursor, CursorMove, CursorState, MAX_CURSORS, OpenCursor, cursor_already_open, cursor_exists,
    cursor_not_open, cursor_status, forward_only, into_count_mismatch, invalid_cursor_scope,
    no_such_cursor, too_many_cursors, FETCH_BEYOND, FETCH_SUCCEEDED,
};
pub(crate) use errors::{CaughtError, line_of, nesting_too_deep, raised_message, thrown};
pub(crate) use functions::{
    enter_nesting, function_arguments, inline_table_columns, invoke_function, refused_in_function,
    side_effecting,
};
pub(crate) use row_assignment::{
    ASSIGNED_VARIABLE, RowAssignment, reads_assigned_variables, refuses_per_row,
};
pub(crate) use routines::{ProcedureRun, TsqlReturns, TsqlRoutine};
pub(crate) use statements::{
    MatchingRow, apply_changes, changed_rows, columns_mismatch, combined, declare, every_row,
    filled_rows, matching_of, matching_select, remove_rows, scalar_select, undeclared, value_items,
    variable_column, variable_columns,
};
pub(crate) use variables::{BatchContext, DynamicBatch, ScalarVariable, procedure_object_name};

#[cfg(test)]
mod cursors_tests;
#[cfg(test)]
mod functions_tests;
#[cfg(test)]
mod reference_tests;
#[cfg(test)]
mod tests;
