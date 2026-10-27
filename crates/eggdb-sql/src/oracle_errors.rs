use crate::error::SqlError;
use crate::lexer::swap_uniform_case;

fn quoted_names(message: &str) -> Vec<&str> {
    message.split('"').skip(1).step_by(2).collect()
}

fn shown(name: &str) -> String {
    swap_uniform_case(name)
}

fn first_word(text: &str) -> String {
    text.split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn syntax_code(error: &SqlError, text: &str) -> (u16, String) {
    let message = error.message();
    if message.starts_with("FROM keyword not found") {
        return (923, message.to_owned());
    }
    if message == "SQL command not properly ended" {
        return (933, message.to_owned());
    }
    if message.starts_with("missing left parenthesis") {
        return (906, "missing left parenthesis".to_owned());
    }
    if message.starts_with("not enough arguments") {
        return (938, "not enough arguments for function".to_owned());
    }
    if message.starts_with("too many arguments") {
        return (939, "too many arguments for function".to_owned());
    }
    if message.starts_with("zero-length delimited identifier") {
        return (1741, "illegal zero-length identifier".to_owned());
    }
    if message.starts_with("unterminated quoted string") {
        return (1756, "quoted string not properly terminated".to_owned());
    }
    if message.starts_with("invalid character") {
        return (911, "invalid character".to_owned());
    }
    if message.starts_with("syntax error at end of input") {
        return (921, "unexpected end of SQL command".to_owned());
    }
    let position = error.position().unwrap_or(0).min(text.len());
    let before = text.get(..position).unwrap_or("").trim_end();
    if before.is_empty() {
        return (900, "invalid SQL statement".to_owned());
    }
    if before.ends_with([',', '(', '+', '-', '*', '/', '=', '<', '>', '|'])
        || before.to_ascii_lowercase().ends_with("select")
    {
        return (936, "missing expression".to_owned());
    }
    (933, "SQL command not properly ended".to_owned())
}

fn not_null_target(names: &[&str], user: &str) -> String {
    let column = names.first().map_or(String::new(), |name| shown(name));
    let table = names.get(1).map_or(String::new(), |name| shown(name));
    format!("(\"{user}\".\"{table}\".\"{column}\")")
}

fn too_large(error: &SqlError) -> String {
    let maximum = error
        .message()
        .rsplit_once('(')
        .and_then(|(_, rest)| rest.strip_suffix(')'))
        .unwrap_or("?");
    format!("value too large for column (maximum: {maximum})")
}

pub(crate) fn translate(error: SqlError, text: &str, user: &str) -> SqlError {
    if error.vendor_code().is_some() {
        return error;
    }
    let message = error.message().to_owned();
    let names = quoted_names(&message);
    let first = names.first().map_or(String::new(), |name| shown(name));
    let translated = match error.state().code() {
        "42601" => Some(syntax_code(&error, text)),
        "42622" => Some((972, "identifier is too long".to_owned())),
        "42P01" => Some((942, "table or view does not exist".to_owned())),
        "42P07" => Some((955, "name is already used by an existing object".to_owned())),
        "42703" => Some((904, format!("\"{first}\": invalid identifier"))),
        "42883" if message.starts_with("function ") => {
            let function = message
                .strip_prefix("function ")
                .and_then(|rest| rest.split_once('('))
                .map_or(String::new(), |(function, _)| function.to_ascii_uppercase());
            Some((904, format!("\"{function}\": invalid identifier")))
        }
        "42702" => Some((918, "column ambiguously defined".to_owned())),
        "42701" => Some((957, "duplicate column name".to_owned())),
        "42P16" if message.starts_with("multiple primary keys") => {
            Some((2260, "table can have only one primary key".to_owned()))
        }
        "42P16" => Some((3001, format!("unimplemented feature: {message}"))),
        "23505" if message.starts_with("could not create unique index") => Some((
            1452,
            "cannot CREATE UNIQUE INDEX; duplicate keys found".to_owned(),
        )),
        "23505" => Some((1, format!("unique constraint ({user}.{first}) violated"))),
        "23502" if first_word(text) == "update" => Some((
            1407,
            format!("cannot update {} to NULL", not_null_target(&names, user)),
        )),
        "23502" => Some((
            1400,
            format!("cannot insert NULL into {}", not_null_target(&names, user)),
        )),
        "22001" => Some((12899, too_large(&error))),
        "22012" => Some((1476, "divisor is equal to zero".to_owned())),
        "22P02" => Some((1722, "invalid number".to_owned())),
        "22003" if message == "numeric field overflow" => Some((
            1438,
            "value larger than specified precision allowed for this column".to_owned(),
        )),
        "22003" => Some((1426, "numeric overflow".to_owned())),
        "22007" => Some((1861, "literal does not match format string".to_owned())),
        "22008" if message.starts_with("date/time field value out of range") => Some((
            1847,
            "day of month must be between 1 and last day of month".to_owned(),
        )),
        "22008" => Some((
            1841,
            "(full) year must be between -4713 and +9999, and not be 0".to_owned(),
        )),
        "42804" | "42846" => Some((932, "inconsistent datatypes".to_owned())),
        "42803" => Some((979, "not a GROUP BY expression".to_owned())),
        "0A000" => Some((3001, format!("unimplemented feature: {message}"))),
        "25006" => Some((
            16000,
            "database or pluggable database open for read-only access".to_owned(),
        )),
        "55P03" => Some((
            54,
            "resource busy and acquire with NOWAIT specified or timeout expired".to_owned(),
        )),
        "42P02" => Some((1008, "not all variables bound".to_owned())),
        "54000" => Some((
            1489,
            "result of string concatenation is too long".to_owned(),
        )),
        _ => None,
    };
    let Some((code, wording)) = translated else {
        return error;
    };
    let mut result = SqlError::new(error.state(), wording).with_oracle_code(code);
    if let Some(hint) = error.hint() {
        result = result.with_hint(hint);
    }
    match error.position() {
        Some(position) => result.at_if_unplaced(position),
        None => result,
    }
}

#[cfg(test)]
mod tests {
    use super::translate;
    use crate::error::{SqlError, SqlState};

    #[test]
    fn errors_take_oracle_numbers_and_wording() {
        let missing = translate(
            SqlError::new(SqlState::UNDEFINED_TABLE, "relation \"t\" does not exist"),
            "SELECT * FROM t",
            "EGGDB",
        );
        assert_eq!(missing.vendor_code(), Some(942));
        assert_eq!(
            missing.to_string(),
            "ORA-00942: table or view does not exist"
        );
        let column = translate(
            SqlError::new(SqlState::UNDEFINED_COLUMN, "column \"x\" does not exist"),
            "SELECT x FROM t",
            "EGGDB",
        );
        assert_eq!(column.to_string(), "ORA-00904: \"X\": invalid identifier");
        let duplicate = translate(
            SqlError::new(
                SqlState::UNIQUE_VIOLATION,
                "duplicate key value violates unique constraint \"t_pkey\"",
            ),
            "INSERT INTO t VALUES (1)",
            "EGGDB",
        );
        assert_eq!(
            duplicate.to_string(),
            "ORA-00001: unique constraint (EGGDB.T_PKEY) violated"
        );
        let null = translate(
            SqlError::new(
                SqlState::NOT_NULL_VIOLATION,
                "null value in column \"name\" of relation \"t\" violates not-null constraint",
            ),
            "INSERT INTO t VALUES (1, NULL)",
            "EGGDB",
        );
        assert_eq!(
            null.to_string(),
            "ORA-01400: cannot insert NULL into (\"EGGDB\".\"T\".\"NAME\")"
        );
        let syntax = translate(
            SqlError::syntax("syntax error at or near \"c\"", 11),
            "SELECT a b c FROM t",
            "EGGDB",
        );
        assert_eq!(syntax.vendor_code(), Some(933));
        let missing = translate(
            SqlError::syntax("syntax error at or near \"FROM\"", 10),
            "SELECT a, FROM t",
            "EGGDB",
        );
        assert_eq!(missing.vendor_code(), Some(936));
        let unknown = translate(
            SqlError::syntax("syntax error at or near \"FOO\"", 0),
            "FOO BAR",
            "EGGDB",
        );
        assert_eq!(unknown.to_string(), "ORA-00900: invalid SQL statement");
    }
}
