use crate::error::SqlError;

const SQLITE_ERROR: u16 = 1;
const SQLITE_BUSY: u16 = 5;
const SQLITE_NOMEM: u16 = 7;
const SQLITE_READONLY: u16 = 8;
const SQLITE_IOERR: u16 = 10;
const SQLITE_CORRUPT: u16 = 11;
const SQLITE_MISMATCH: u16 = 20;
const SQLITE_CONSTRAINT_NOTNULL: u16 = 1299;
const SQLITE_CONSTRAINT_PRIMARYKEY: u16 = 1555;

const RESULT_CODES: &[(u16, &str)] = &[
    (1, "SQLITE_ERROR"),
    (2, "SQLITE_INTERNAL"),
    (5, "SQLITE_BUSY"),
    (7, "SQLITE_NOMEM"),
    (8, "SQLITE_READONLY"),
    (10, "SQLITE_IOERR"),
    (11, "SQLITE_CORRUPT"),
    (18, "SQLITE_TOOBIG"),
    (19, "SQLITE_CONSTRAINT"),
    (20, "SQLITE_MISMATCH"),
    (25, "SQLITE_RANGE"),
    (275, "SQLITE_CONSTRAINT_CHECK"),
    (787, "SQLITE_CONSTRAINT_FOREIGNKEY"),
    (1299, "SQLITE_CONSTRAINT_NOTNULL"),
    (1555, "SQLITE_CONSTRAINT_PRIMARYKEY"),
    (2067, "SQLITE_CONSTRAINT_UNIQUE"),
];

/// The name of a SQLite result code or extended result code, as sqlite.org's "Result and
/// Error Codes" lists it: `SQLITE_CONSTRAINT_PRIMARYKEY` for `1555`, `None` for codes EggDB
/// does not give.
pub fn sqlite_result_code_name(code: u16) -> Option<&'static str> {
    RESULT_CODES
        .iter()
        .find(|(known, _)| *known == code)
        .map(|(_, name)| *name)
}

fn quoted_names(message: &str) -> Vec<&str> {
    message.split('"').skip(1).step_by(2).collect()
}

fn token_at(text: &str, position: usize) -> Option<&str> {
    let mut start = position.min(text.len());
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    let rest = &text[start..];
    let first = rest.chars().next()?;
    let length = if first.is_alphanumeric() || first == '_' {
        rest.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
            .unwrap_or(rest.len())
    } else if first == '\'' || first == '"' || first == '`' {
        rest[1..].find(first).map_or(rest.len(), |end| end + 2)
    } else {
        let two = rest.get(..2);
        if two.is_some_and(|pair| {
            ["||", "<=", ">=", "==", "!=", "<>", "<<", ">>", "->"].contains(&pair)
        }) {
            2
        } else {
            first.len_utf8()
        }
    };
    Some(&rest[..length])
}

fn syntax(text: &str, position: Option<usize>) -> String {
    match position.and_then(|position| token_at(text, position)) {
        Some(token) => format!("near \"{token}\": syntax error"),
        None => "incomplete input".to_owned(),
    }
}

fn unique_failed(error: &SqlError, names: &[&str]) -> String {
    let table = names
        .first()
        .map(|name| name.strip_suffix("_pkey").unwrap_or(name))
        .unwrap_or("");
    let columns: Vec<String> = error
        .detail()
        .and_then(|detail| detail.strip_prefix("Key ("))
        .and_then(|rest| rest.split_once(")="))
        .map(|(columns, _)| {
            columns
                .split(", ")
                .map(|column| format!("{table}.{column}"))
                .collect()
        })
        .unwrap_or_default();
    format!("UNIQUE constraint failed: {}", columns.join(", "))
}

pub(crate) fn translate(error: SqlError, text: &str) -> SqlError {
    if error.vendor_code().is_some() {
        return error;
    }
    let message = error.message().to_owned();
    let position = error.position();
    let names = quoted_names(&message);
    let first = names.first().copied().unwrap_or("").to_owned();
    let (code, wording) = match error.state().code() {
        "42601" if message.starts_with("unrecognized token") => (SQLITE_ERROR, message.clone()),
        "42601" if message.starts_with("unterminated") => {
            let start = position.unwrap_or(0).min(text.len());
            let rest = text.get(start..).unwrap_or("");
            (SQLITE_ERROR, format!("unrecognized token: \"{rest}\""))
        }
        "42601" if message.starts_with("INSERT has more expressions") => (
            SQLITE_ERROR,
            format!(
                "table {} has fewer columns than values supplied",
                token_at(text, position.unwrap_or(0)).unwrap_or("")
            ),
        ),
        "42601" if message.starts_with("INSERT has more target") => (
            SQLITE_ERROR,
            "values supplied do not match the columns given".to_owned(),
        ),
        "42601" if message.starts_with("VALUES lists must") => (
            SQLITE_ERROR,
            "all VALUES must have the same number of terms".to_owned(),
        ),
        "42601" if message.starts_with("syntax error") || message.starts_with("trailing junk") => {
            (SQLITE_ERROR, syntax(text, position))
        }
        "42P01" => (SQLITE_ERROR, format!("no such table: {first}")),
        "42P07" => (SQLITE_ERROR, format!("table {first} already exists")),
        "42703" if names.len() >= 2 && message.contains(" of relation ") => (
            SQLITE_ERROR,
            format!("table {} has no column named {first}", names[1]),
        ),
        "42703" => (SQLITE_ERROR, format!("no such column: {first}")),
        "42702" => (SQLITE_ERROR, format!("ambiguous column name: {first}")),
        "42701" => (SQLITE_ERROR, format!("duplicate column name: {first}")),
        "42P16" if message.starts_with("multiple primary keys") => (
            SQLITE_ERROR,
            format!("table \"{first}\" has more than one primary key"),
        ),
        "23505" => (SQLITE_CONSTRAINT_PRIMARYKEY, unique_failed(&error, &names)),
        "23502" => {
            let table = names.get(1).copied().unwrap_or("");
            (
                SQLITE_CONSTRAINT_NOTNULL,
                format!("NOT NULL constraint failed: {table}.{first}"),
            )
        }
        "42883" if message.starts_with("function ") => {
            let function = message
                .strip_prefix("function ")
                .and_then(|rest| rest.split_once('('))
                .map_or("", |(function, _)| function);
            (SQLITE_ERROR, format!("no such function: {function}"))
        }
        "22P02" | "42846" | "42804" => (SQLITE_MISMATCH, "datatype mismatch".to_owned()),
        "22003" => (SQLITE_ERROR, "integer overflow".to_owned()),
        "25006" => (
            SQLITE_READONLY,
            "attempt to write a readonly database".to_owned(),
        ),
        "55P03" => (SQLITE_BUSY, "database is locked".to_owned()),
        "XX001" => (
            SQLITE_CORRUPT,
            "database disk image is malformed".to_owned(),
        ),
        "58030" => (SQLITE_IOERR, "disk I/O error".to_owned()),
        "53200" => (SQLITE_NOMEM, "out of memory".to_owned()),
        _ => (SQLITE_ERROR, message.clone()),
    };
    let mut translated = SqlError::new(error.state(), wording).with_sqlite_code(code);
    if let Some(hint) = error.hint() {
        translated = translated.with_hint(hint);
    }
    match position {
        Some(position) => translated.at_if_unplaced(position),
        None => translated,
    }
}

#[cfg(test)]
mod tests {
    use super::{sqlite_result_code_name, translate};
    use crate::error::{SqlError, SqlState};

    #[test]
    fn errors_take_sqlite_wording_and_result_codes() {
        let syntax = SqlError::syntax("syntax error at or near \"FORM\"", 9);
        let shown = translate(syntax, "SELECT 1 FORM t");
        assert_eq!(shown.to_string(), "Error: near \"FORM\": syntax error");
        assert_eq!(shown.vendor_code(), Some(1));
        let missing = SqlError::new(SqlState::UNDEFINED_TABLE, "relation \"t\" does not exist");
        assert_eq!(translate(missing, "").message(), "no such table: t");
        let duplicate = SqlError::new(
            SqlState::UNIQUE_VIOLATION,
            "duplicate key value violates unique constraint \"t_pkey\"",
        )
        .with_detail("Key (a, b)=(1, x) already exists.");
        let shown = translate(duplicate, "");
        assert_eq!(shown.message(), "UNIQUE constraint failed: t.a, t.b");
        assert_eq!(
            shown.vendor_code().and_then(sqlite_result_code_name),
            Some("SQLITE_CONSTRAINT_PRIMARYKEY")
        );
        let end = SqlError::syntax("syntax error at end of input", 6);
        assert_eq!(translate(end, "SELECT").message(), "incomplete input");
    }
}
