use std::cell::Cell;

thread_local! {
    static UNDO_LENGTH: Cell<Option<usize>> = const { Cell::new(None) };
    static PROCEDURE_STATEMENT_START: Cell<Option<usize>> = const { Cell::new(None) };
}

pub(crate) fn note_undo_length(length: usize) {
    UNDO_LENGTH.with(|found| found.set(Some(length)));
}

pub(crate) fn begin_statement() {
    UNDO_LENGTH.with(|found| found.set(None));
    PROCEDURE_STATEMENT_START.with(|found| found.set(None));
}

pub(crate) fn mark_procedure_statement() {
    let length = UNDO_LENGTH.with(Cell::get);
    if length.is_some() {
        PROCEDURE_STATEMENT_START.with(|found| found.set(length));
    }
}

pub(crate) fn take_procedure_statement_start() -> Option<usize> {
    PROCEDURE_STATEMENT_START.with(Cell::take)
}
