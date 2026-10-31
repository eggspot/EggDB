use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{SqlType, Value};
use std::sync::OnceLock;

/// Reads a foreign source for `eggdb_foreign(source, query)`: `source` names it (a URL) and
/// `query` says what to read there; each row goes to the sink as the text of one JSON object,
/// and the sink's error stops the read.
pub type ForeignSource = Box<dyn Fn(&str, &str, &mut dyn FnMut(String) -> Result<(), String>) -> Result<(), String> + Send + Sync>;

static SOURCE: OnceLock<ForeignSource> = OnceLock::new();

/// Sets the process's reader of foreign sources. The first registration wins for the life of the
/// process. Until one is set, `eggdb_foreign` answers SQLSTATE `0A000`, so SQL reaches nothing
/// outside the database unless the program that runs it chose to allow it.
pub fn set_foreign_source(source: ForeignSource) {
    let _ = SOURCE.set(source);
}

pub(crate) fn foreign_type(types: &[Option<SqlType>]) -> SqlResult<SqlType> {
    match types {
        [source, query] if source.as_ref().is_none_or(SqlType::is_character) && query.as_ref().is_none_or(SqlType::is_character) => Ok(SqlType::Json),
        _ => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "function eggdb_foreign takes a source and a query, both text")),
    }
}

pub(crate) fn foreign_rows(values: &[Value], _: SqlType, sink: &mut crate::set_returning::RowSink<'_>) -> SqlResult<()> {
    let (Some(source), Some(query)) = (values.first().and_then(Value::to_text), values.get(1).and_then(Value::to_text)) else {
        return Ok(());
    };
    let Some(reader) = SOURCE.get() else {
        return Err(SqlError::not_supported("foreign sources are off: the program running this SQL allows none (eggdb reads EGGDB_FOREIGN_SOURCES)"));
    };
    let mut failure = None;
    let outcome = reader(&source, &query, &mut |row| {
        sink(Value::Json(row)).map_err(|error| {
            let message = error.message().to_owned();
            failure = Some(error);
            message
        })
    });
    if let Some(error) = failure {
        return Err(error);
    }
    outcome.map_err(|message| SqlError::new(SqlState::FDW_ERROR, format!("eggdb_foreign({source}): {message}")))
}
