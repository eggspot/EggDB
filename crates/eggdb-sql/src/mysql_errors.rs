use crate::error::{SqlError, SqlState};

const NEAR_TEXT_CHARACTERS: usize = 80;

/// A MySQL warning or note, as `SHOW WARNINGS` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// `Note`, `Warning` or `Error`.
    pub level: &'static str,
    /// MySQL's error number, such as `1062`.
    pub code: u16,
    /// The message.
    pub message: String,
}

fn quoted_names(message: &str) -> Vec<&str> {
    message.split('"').skip(1).step_by(2).collect()
}

fn first_quoted(message: &str) -> &str {
    quoted_names(message).first().copied().unwrap_or("")
}

fn statement_word_before(text: &str, position: Option<usize>) -> String {
    let end = position.unwrap_or(0).min(text.len());
    let mut start = end;
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    let head = &text[..start];
    let statement_start = head.rfind(';').map_or(0, |at| at + 1);
    head[statement_start..]
        .trim_start()
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect::<String>()
        .to_ascii_lowercase()
}

fn near(text: &str, position: Option<usize>) -> String {
    let Some(mut start) = position.filter(|&position| position <= text.len()) else {
        return "near '' at line 1".to_owned();
    };
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    let rest: String = text[start..].chars().take(NEAR_TEXT_CHARACTERS).collect();
    let line = text[..start].matches('\n').count() + 1;
    format!("near '{rest}' at line {line}")
}

fn translated(code: u16, state: SqlState, message: String) -> SqlError {
    SqlError::new(state, message).with_vendor_code(code)
}

fn keep_position(error: SqlError, position: Option<usize>) -> SqlError {
    match position {
        Some(position) => error.at_if_unplaced(position),
        None => error,
    }
}

pub(crate) fn translate(error: SqlError, text: &str, database: &str) -> SqlError {
    if error.vendor_code().is_some() {
        return error;
    }
    let message = error.message();
    let position = error.position();
    let name = first_quoted(message).to_owned();
    let syntax = || {
        translated(
            1064,
            SqlState::MYSQL_SYNTAX_OR_ACCESS,
            format!(
                "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use {}",
                near(text, position)
            ),
        )
    };
    let general = |code: u16| translated(code, SqlState::MYSQL_GENERAL, message.to_owned());
    let result = match error.state().code() {
        "42P01" if statement_word_before(text, position) == "drop" => translated(
            1051,
            SqlState::MYSQL_TABLE_NOT_FOUND,
            format!("Unknown table '{database}.{name}'"),
        ),
        "42P01" => translated(
            1146,
            SqlState::MYSQL_TABLE_NOT_FOUND,
            format!("Table '{database}.{name}' doesn't exist"),
        ),
        "42P07" => translated(
            1050,
            SqlState::MYSQL_TABLE_EXISTS,
            format!("Table '{name}' already exists"),
        ),
        "42703" => {
            let column = name.rsplit('.').next().unwrap_or(&name).to_owned();
            translated(
                1054,
                SqlState::MYSQL_COLUMN_NOT_FOUND,
                format!("Unknown column '{column}' in 'field list'"),
            )
        }
        "42702" => translated(
            1052,
            SqlState::MYSQL_INTEGRITY,
            format!("Column '{name}' in field list is ambiguous"),
        ),
        "42701" => translated(
            1060,
            SqlState::MYSQL_COLUMN_EXISTS,
            format!("Duplicate column name '{name}'"),
        ),
        "42712" => translated(
            1066,
            SqlState::MYSQL_SYNTAX_OR_ACCESS,
            format!("Not unique table/alias: '{name}'"),
        ),
        "42601"
            if message.starts_with("INSERT has more")
                || message.starts_with("VALUES lists must") =>
        {
            translated(
                1136,
                SqlState::MYSQL_VALUE_COUNT,
                "Column count doesn't match value count at row 1".to_owned(),
            )
        }
        "42P02"
            if message
                .rsplit('$')
                .next()
                .and_then(|number| number.parse::<u64>().ok())
                .is_none_or(|number| number > 65_535) =>
        {
            translated(
                1390,
                SqlState::MYSQL_GENERAL,
                "Prepared statement contains too many placeholders".to_owned(),
            )
        }
        "42601" if message.starts_with("subquery must return only one column") => translated(
            1241,
            SqlState::CARDINALITY_VIOLATION,
            "Operand should contain 1 column(s)".to_owned(),
        ),
        "42601" if message.ends_with("query must have the same number of columns") => translated(
            1222,
            SqlState::CARDINALITY_VIOLATION,
            "The used SELECT statements have a different number of columns".to_owned(),
        ),
        "21000" => translated(
            1242,
            SqlState::CARDINALITY_VIOLATION,
            "Subquery returns more than 1 row".to_owned(),
        ),
        "54001" if message.starts_with("Recursive query aborted") => translated(
            3636,
            SqlState::MYSQL_GENERAL,
            format!("{message} Try increasing @@cte_max_recursion_depth to a larger value."),
        ),
        "42601" | "2201W" | "2201X" | "42P02" => syntax(),
        "42704" if message.starts_with("type ") => syntax(),
        "42704" if message.starts_with("unrecognized configuration parameter") => translated(
            1193,
            SqlState::MYSQL_GENERAL,
            format!("Unknown system variable '{name}'"),
        ),
        "23505" => {
            let values = error
                .detail()
                .and_then(|detail| detail.split_once(")=("))
                .and_then(|(_, rest)| {
                    rest.rsplit_once(") already exists")
                        .or_else(|| rest.rsplit_once(") is duplicated"))
                })
                .map(|(values, _)| values.replace(", ", "-"))
                .unwrap_or_default();
            let key = match error.table() {
                Some(table) if name != format!("{table}_pkey") => format!("{table}.{name}"),
                Some(table) => format!("{table}.PRIMARY"),
                None => format!("{}.PRIMARY", name.strip_suffix("_pkey").unwrap_or(&name)),
            };
            translated(
                1062,
                SqlState::MYSQL_INTEGRITY,
                format!("Duplicate entry '{values}' for key '{key}'"),
            )
        }
        "23502" => translated(
            1048,
            SqlState::MYSQL_INTEGRITY,
            format!("Column '{name}' cannot be null"),
        ),
        "22021" => translated(
            1366,
            SqlState::MYSQL_GENERAL,
            "Incorrect string value: the bytes are not UTF-8".to_owned(),
        ),
        "22001" => translated(
            1406,
            SqlState::STRING_DATA_RIGHT_TRUNCATION,
            format!("Data too long for column: {message}"),
        ),
        "22003" => translated(
            1264,
            SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
            format!("Out of range value: {message}"),
        ),
        "22012" => translated(1365, SqlState::DIVISION_BY_ZERO, "Division by 0".to_owned()),
        "22007" | "22008" => translated(
            1292,
            SqlState::INVALID_DATETIME_FORMAT,
            format!("Incorrect datetime value: {message}"),
        ),
        "22P02" => {
            let type_name = message
                .strip_prefix("invalid input syntax for type ")
                .and_then(|rest| rest.split_once(':'))
                .map_or("value", |(type_name, _)| type_name);
            translated(
                1366,
                SqlState::MYSQL_GENERAL,
                format!("Incorrect {type_name} value: '{name}'"),
            )
        }
        "42883" if message.starts_with("function ") => {
            let function = message
                .strip_prefix("function ")
                .and_then(|rest| rest.split_once('('))
                .map_or("", |(function, _)| function);
            translated(
                1305,
                SqlState::MYSQL_SYNTAX_OR_ACCESS,
                format!("FUNCTION {database}.{function} does not exist"),
            )
        }
        "42883" if message.starts_with("Incorrect parameter count") => {
            translated(1582, SqlState::MYSQL_SYNTAX_OR_ACCESS, message.to_owned())
        }
        "0A000" => translated(
            1235,
            SqlState::MYSQL_SYNTAX_OR_ACCESS,
            format!("This version of EggDB doesn't yet support '{message}'"),
        ),
        "3D000" => translated(
            1049,
            SqlState::MYSQL_SYNTAX_OR_ACCESS,
            format!("Unknown database '{name}'"),
        ),
        "55P03" => translated(
            1205,
            SqlState::MYSQL_GENERAL,
            "Lock wait timeout exceeded; try restarting transaction".to_owned(),
        ),
        "25006" => translated(
            1792,
            SqlState::READ_ONLY_SQL_TRANSACTION,
            "Cannot execute statement in a READ ONLY transaction.".to_owned(),
        ),
        "42P16" if message.starts_with("multiple primary keys") => translated(
            1068,
            SqlState::MYSQL_SYNTAX_OR_ACCESS,
            "Multiple primary key defined".to_owned(),
        ),
        "42P16" if message.contains("auto column") => {
            translated(1075, SqlState::MYSQL_SYNTAX_OR_ACCESS, message.to_owned())
        }
        "42803" => translated(1055, SqlState::MYSQL_SYNTAX_OR_ACCESS, message.to_owned()),
        "53200" => general(1041),
        "28000" | "28P01" => translated(
            1045,
            SqlState::INVALID_AUTHORIZATION_SPECIFICATION,
            message.to_owned(),
        ),
        _ => general(1105),
    };
    keep_position(result, position)
}

pub(crate) fn note_for(notice: &str, database: &str) -> Option<Warning> {
    let name = first_quoted(notice);
    if notice.contains("already exists, skipping") {
        return Some(Warning {
            level: "Note",
            code: 1050,
            message: format!("Table '{name}' already exists"),
        });
    }
    if notice.contains("does not exist, skipping") {
        return Some(Warning {
            level: "Note",
            code: 1051,
            message: format!("Unknown table '{database}.{name}'"),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{near, translate};
    use crate::error::{SqlError, SqlState};

    #[test]
    fn errors_take_mysql_numbers_states_and_wording() {
        let missing = SqlError::new(
            SqlState::UNDEFINED_TABLE,
            "relation \"items\" does not exist",
        );
        let shown = translate(missing.clone(), "SELECT * FROM items", "shop");
        assert_eq!(shown.vendor_code(), Some(1146));
        assert_eq!(
            shown.to_string(),
            "ERROR 1146 (42S02): Table 'shop.items' doesn't exist"
        );
        let dropped = translate(missing.at(11), "DROP TABLE items", "shop");
        assert_eq!(dropped.message(), "Unknown table 'shop.items'");
        let duplicate = SqlError::new(
            SqlState::UNIQUE_VIOLATION,
            "duplicate key value violates unique constraint \"t_pkey\"",
        )
        .with_detail("Key (a, b)=(1, x) already exists.");
        assert_eq!(
            translate(duplicate, "", "db").message(),
            "Duplicate entry '1-x' for key 't.PRIMARY'"
        );
    }

    #[test]
    fn syntax_errors_quote_the_text_from_the_error_on() {
        assert_eq!(near("SELECT\nFROM x", Some(7)), "near 'FROM x' at line 2");
        assert_eq!(near("SELECT", Some(6)), "near '' at line 1");
        assert_eq!(near("é", Some(1)), "near 'é' at line 1");
    }
}
