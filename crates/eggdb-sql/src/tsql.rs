mod control;
mod errors;
mod functions;
mod routines;
mod statements;
mod variables;

pub(crate) use control::{
    Declaration, Instruction, MAX_DYNAMIC_NESTING, Program, ProgramEnd, TryBlock, chooses_dialect,
    uses_batch_logic,
};
pub(crate) use errors::{CaughtError, line_of, nesting_too_deep, raised_message, thrown};
pub(crate) use functions::{
    function_arguments, inline_table_columns, invoke_function, refused_in_function, side_effecting,
};
pub(crate) use routines::{ProcedureRun, TsqlReturns, TsqlRoutine};
pub(crate) use statements::{
    MatchingRow, apply_changes, changed_rows, columns_mismatch, combined, declare, every_row,
    filled_rows, matching_of, matching_select, remove_rows, scalar_select, undeclared,
    value_items, variable_column, variable_columns,
};
pub(crate) use variables::{BatchContext, DynamicBatch, ScalarVariable};

#[cfg(test)]
mod functions_tests;
#[cfg(test)]
mod tests;
