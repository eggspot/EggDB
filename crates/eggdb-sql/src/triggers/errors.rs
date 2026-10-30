use super::TriggerTiming;
use crate::ast::Name;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlState};
use crate::mysql_errors::Warning;
use crate::schemas::{base_name, display};

const IGNORED_ROW: &str = "RAISE(IGNORE)";

fn mysql(code: u16, message: String) -> SqlError {
    SqlError::new(SqlState::MYSQL_GENERAL, message).with_vendor_code(code)
}

fn sqlite(message: String) -> SqlError {
    SqlError::new(SqlState::INVALID_OBJECT_DEFINITION, message).with_sqlite_code(1)
}

fn oracle_trigger_name(name: &str) -> String {
    crate::plsql::shown_object(base_name(name))
}

pub(super) fn missing_table(table: &Name, dialect: Dialect) -> SqlError {
    let shown = base_name(&table.text);
    match dialect {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_TABLE_NOT_FOUND,
            format!("Table '{shown}' doesn't exist"),
        )
        .with_vendor_code(1146),
        Dialect::Sqlite => SqlError::new(
            SqlState::UNDEFINED_TABLE,
            format!("no such table: main.{shown}"),
        )
        .with_sqlite_code(1),
        Dialect::SqlServer => SqlError::new(
            SqlState::UNDEFINED_TABLE,
            format!("The object '{shown}' does not exist or is invalid for this operation."),
        )
        .with_sqlserver_code(8197),
        Dialect::Oracle => SqlError::new(SqlState::UNDEFINED_TABLE, "table or view does not exist")
            .with_oracle_code(942),
        Dialect::PostgreSql => SqlError::new(
            SqlState::UNDEFINED_TABLE,
            format!("relation \"{}\" does not exist", display(&table.text)),
        ),
    }
    .at(table.position)
}

pub(super) fn missing_function(name: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("function {name}() does not exist"),
    )
    .with_hint(
        "A PostgreSQL trigger runs a function without arguments that returns trigger: create it first with CREATE FUNCTION name() RETURNS trigger LANGUAGE plpgsql AS $$ ... $$.",
    )
}

pub(super) fn not_a_base_table(table: &str, dialect: Dialect) -> SqlError {
    let shown = base_name(table);
    match dialect {
        Dialect::MySql => mysql(1347, format!("'{shown}' is not BASE TABLE")),
        _ => SqlError::new(
            SqlState::WRONG_OBJECT_TYPE,
            format!("The object '{shown}' does not exist or is invalid for this operation."),
        )
        .with_sqlserver_code(8197),
    }
}

pub(super) fn view_needs_instead_of(table: &str, timing: TriggerTiming, dialect: Dialect) -> SqlError {
    let shown = base_name(table);
    match dialect {
        Dialect::Sqlite => sqlite(format!(
            "cannot create {} trigger on view: main.{shown}",
            if timing == TriggerTiming::Before {
                "BEFORE"
            } else {
                "AFTER"
            }
        )),
        Dialect::Oracle => SqlError::new(
            SqlState::WRONG_OBJECT_TYPE,
            "cannot create this trigger type on this type of view",
        )
        .with_oracle_code(25001),
        _ => SqlError::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{shown}\" is a view"))
            .with_detail("Views cannot have row-level BEFORE or AFTER triggers."),
    }
}

pub(super) fn table_cannot_be_instead_of(table: &str, dialect: Dialect) -> SqlError {
    let shown = base_name(table);
    match dialect {
        Dialect::Sqlite => sqlite(format!("cannot create INSTEAD OF trigger on table: {shown}")),
        Dialect::Oracle => SqlError::new(
            SqlState::WRONG_OBJECT_TYPE,
            "cannot create INSTEAD OF triggers on tables",
        )
        .with_oracle_code(25002),
        _ => SqlError::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{shown}\" is a table"))
            .with_detail("Tables cannot have INSTEAD OF triggers."),
    }
}

pub(super) fn missing_referenced_trigger(name: &str) -> SqlError {
    mysql(
        3011,
        format!("Referenced trigger '{name}' for the given action time and event type does not exist."),
    )
}

pub(super) fn duplicate(name: &str, table: &str, dialect: Dialect) -> SqlError {
    match dialect {
        Dialect::MySql => mysql(1359, "Trigger already exists".to_owned()),
        Dialect::Sqlite => sqlite(format!("trigger {name} already exists")),
        Dialect::SqlServer => SqlError::new(
            SqlState::DUPLICATE_OBJECT,
            format!("There is already an object named '{name}' in the database."),
        )
        .with_sqlserver_code(2714),
        Dialect::Oracle => SqlError::new(
            SqlState::DUPLICATE_OBJECT,
            format!("trigger '{}' already exists", oracle_trigger_name(name)),
        )
        .with_oracle_code(4081),
        Dialect::PostgreSql => SqlError::new(
            SqlState::DUPLICATE_OBJECT,
            format!(
                "trigger \"{name}\" for relation \"{}\" already exists",
                base_name(table)
            ),
        ),
    }
}

pub(super) fn exists_note() -> Warning {
    Warning {
        level: "Note",
        code: 1359,
        message: "Trigger already exists".to_owned(),
    }
}

pub(super) fn missing_note() -> Warning {
    Warning {
        level: "Note",
        code: 1360,
        message: "Trigger does not exist".to_owned(),
    }
}

pub(super) fn missing_trigger(name: &Name, table: Option<&Name>, dialect: Dialect) -> SqlError {
    let shown = base_name(&name.text);
    match dialect {
        Dialect::MySql => mysql(1360, "Trigger does not exist".to_owned()),
        Dialect::Sqlite => sqlite(format!("no such trigger: {shown}")),
        Dialect::SqlServer => SqlError::new(
            SqlState::UNDEFINED_OBJECT,
            format!(
                "Cannot drop the trigger '{shown}', because it does not exist or you do not have permission."
            ),
        )
        .with_sqlserver_code(3701),
        Dialect::Oracle => SqlError::new(
            SqlState::UNDEFINED_OBJECT,
            format!("trigger '{}' does not exist", oracle_trigger_name(shown)),
        )
        .with_oracle_code(4080),
        Dialect::PostgreSql => SqlError::new(
            SqlState::UNDEFINED_OBJECT,
            format!(
                "trigger \"{shown}\" for table \"{}\" does not exist",
                table.map_or_else(String::new, |table| base_name(&table.text).to_owned())
            ),
        ),
    }
    .at(name.position)
}

pub(super) fn too_deep(dialect: Dialect) -> SqlError {
    match dialect {
        Dialect::SqlServer => crate::tsql::nesting_too_deep(),
        Dialect::Sqlite => sqlite("too many levels of trigger recursion".to_owned()),
        Dialect::MySql => mysql(
            1456,
            format!(
                "Recursive limit {} (as set by the max_sp_recursion_depth variable) was exceeded for routine trigger",
                super::MAX_TRIGGER_DEPTH
            ),
        ),
        Dialect::PostgreSql | Dialect::Oracle => {
            SqlError::new(SqlState::STATEMENT_TOO_COMPLEX, "stack depth limit exceeded").with_hint(
                format!(
                    "Triggers fired by triggers nest at most {} deep in EggDB.",
                    super::MAX_TRIGGER_DEPTH
                ),
            )
        }
    }
}

pub(super) fn table_used_by_invoking_statement(table: &str) -> SqlError {
    mysql(
        1442,
        format!(
            "Can't update table '{}' in stored function/trigger because it is already used by statement which invoked this stored function/trigger.",
            base_name(table)
        ),
    )
}

pub(super) fn ignored_row() -> SqlError {
    SqlError::new(SqlState::TRIGGERED_ACTION_EXCEPTION, IGNORED_ROW).with_sqlite_code(19)
}

pub(super) fn is_ignored_row(error: &SqlError) -> bool {
    error.state() == SqlState::TRIGGERED_ACTION_EXCEPTION && error.message() == IGNORED_ROW
}

pub(super) fn raised(message: &str) -> SqlError {
    SqlError::new(SqlState::RAISE_EXCEPTION, message).with_sqlite_code(19)
}

pub(super) fn raise_outside_trigger() -> SqlError {
    sqlite("RAISE() may only be used within a trigger-program".to_owned())
}

pub(super) fn not_in_trigger(what: &str, dialect: Dialect) -> SqlError {
    match dialect {
        Dialect::MySql => mysql(
            1422,
            format!("Explicit or implicit commit is not allowed in stored function or trigger: {what}."),
        ),
        _ => SqlError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("{what} cannot run in a trigger's body"),
        )
        .with_hint(
            "A trigger's body runs INSERT, UPDATE, DELETE and SELECT statements (and MySQL's SET, IF, WHILE, DECLARE and SIGNAL).",
        ),
    }
}

pub(super) fn missing_row(qualifier: &str, column: &str, dialect: Dialect, event: &str) -> SqlError {
    match dialect {
        Dialect::MySql => mysql(
            1363,
            format!(
                "There is no {} row in on {event} trigger",
                qualifier.to_ascii_uppercase()
            ),
        ),
        _ => sqlite(format!("no such column: {qualifier}.{column}")),
    }
}

pub(super) fn missing_column(qualifier: &str, column: &str, dialect: Dialect) -> SqlError {
    match dialect {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_COLUMN_NOT_FOUND,
            format!("Unknown column '{column}' in '{}'", qualifier.to_ascii_uppercase()),
        )
        .with_vendor_code(1054),
        _ => sqlite(format!("no such column: {qualifier}.{column}")),
    }
}

pub(super) fn old_row_read_only(event: &str) -> SqlError {
    mysql(1362, format!("Updating of OLD row is not allowed in {event} trigger"))
}

pub(super) fn new_row_after() -> SqlError {
    mysql(
        1362,
        "Updating of NEW row is not allowed in after trigger".to_owned(),
    )
}

pub(super) fn signal(state: SqlState, message: Option<String>, code: Option<u16>) -> SqlError {
    let message = message.unwrap_or_else(|| {
        if state == SqlState::USER_DEFINED_CONDITION {
            "Unhandled user-defined exception condition".to_owned()
        } else {
            "Unhandled user-defined condition".to_owned()
        }
    });
    SqlError::new(state, message).with_vendor_code(code.unwrap_or(1644))
}

pub(super) fn bad_signal_state(state: &str) -> SqlError {
    mysql(1407, format!("Bad SQLSTATE: '{state}'"))
}
