use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{SqlType, Value};
use std::sync::OnceLock;

/// One call of `eggdb_foreign(source, query)`: the source's URL, what to read there, and the
/// role asking, `None` for a superuser.
#[derive(Debug, Clone, Copy)]
pub struct ForeignRequest<'a> {
    /// The source, a URL.
    pub source: &'a str,
    /// What to read there.
    pub query: &'a str,
    /// The role that called, or `None` when a superuser did.
    pub role: Option<&'a str>,
}

/// Why a foreign read failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForeignFailure {
    /// The calling role may not read this source (SQLSTATE `42501`).
    Denied(String),
    /// The source could not be read (SQLSTATE `HV000`).
    Failed(String),
}

/// Reads a foreign source for `eggdb_foreign(source, query)`; each row goes to the sink as the
/// text of one JSON object, and the sink's error stops the read.
pub type ForeignSource = Box<
    dyn Fn(&ForeignRequest<'_>, &mut dyn FnMut(String) -> Result<(), String>) -> Result<(), ForeignFailure>
        + Send
        + Sync,
>;

static SOURCE: OnceLock<ForeignSource> = OnceLock::new();

/// Sets the process's reader of foreign sources. The first registration wins for the life of the
/// process. Until one is set, `eggdb_foreign` answers SQLSTATE `0A000`, so SQL reaches nothing
/// outside the database unless the program that runs it chose to allow it.
pub fn set_foreign_source(source: ForeignSource) {
    let _ = SOURCE.set(source);
}

/// Every row `query` returns at `source`, as JSON objects' text, read through the process's
/// foreign source reader as `eggdb_foreign` reads it, for `role` (`None` for a superuser).
/// Callable from several threads at once.
pub(crate) fn read_rows(source: &str, query: &str, role: Option<&str>) -> SqlResult<Vec<String>> {
    let Some(reader) = SOURCE.get() else {
        return Err(SqlError::not_supported(
            "foreign sources are off: the program running this SQL allows none (eggdb reads EGGDB_FOREIGN_SOURCES)",
        ));
    };
    let mut rows = Vec::new();
    let request = ForeignRequest { source, query, role };
    reader(&request, &mut |row| {
        rows.push(row);
        Ok(())
    })
    .map_err(|failure| match failure {
        ForeignFailure::Denied(message) => SqlError::new(SqlState::INSUFFICIENT_PRIVILEGE, format!("permission denied to read {message}")),
        ForeignFailure::Failed(message) => SqlError::new(SqlState::FDW_ERROR, format!("eggdb_foreign({source}): {message}")),
    })?;
    Ok(rows)
}

pub(crate) fn foreign_type(types: &[Option<SqlType>]) -> SqlResult<SqlType> {
    match types {
        [source, query]
            if source.as_ref().is_none_or(|kind| kind.is_character())
                && query.as_ref().is_none_or(|kind| kind.is_character()) =>
        {
            Ok(SqlType::Json)
        }
        _ => Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            "function eggdb_foreign takes a source and a query, both text",
        )),
    }
}

pub(crate) fn foreign_rows(
    values: &[Value],
    _: SqlType,
    sink: &mut crate::set_returning::RowSink<'_>,
) -> SqlResult<()> {
    let (Some(source), Some(query)) = (
        values.first().and_then(Value::to_text),
        values.get(1).and_then(Value::to_text),
    ) else {
        return Ok(());
    };
    let role = crate::constraints::checked_role();
    let Some(reader) = SOURCE.get() else {
        return Err(SqlError::not_supported(
            "foreign sources are off: the program running this SQL allows none (eggdb reads EGGDB_FOREIGN_SOURCES)",
        ));
    };
    let mut failure = None;
    let request = ForeignRequest { source: &source, query: &query, role: role.as_deref() };
    let outcome = reader(&request, &mut |row| {
        sink(Value::Json(row)).map_err(|error| {
            let message = error.message().to_owned();
            failure = Some(error);
            message
        })
    });
    if let Some(error) = failure {
        return Err(error);
    }
    outcome.map_err(|failure| match failure {
        ForeignFailure::Denied(message) => SqlError::new(SqlState::INSUFFICIENT_PRIVILEGE, format!("permission denied to read {message}"))
            .with_hint("A superuser may read every allowed source; other roles only those whose EGGDB_FOREIGN_SOURCES entry names them."),
        ForeignFailure::Failed(message) => SqlError::new(SqlState::FDW_ERROR, format!("eggdb_foreign({source}): {message}")),
    })
}
