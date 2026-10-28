use crate::error::SqlError;

const GENERIC_ERROR: u16 = 50000;

const SEVERITIES: [(u16, u8); 46] = [
    (102, 15),
    (103, 15),
    (105, 15),
    (137, 15),
    (156, 15),
    (195, 15),
    (1038, 15),
    (131, 16),
    (192, 16),
    (207, 16),
    (208, 16),
    (209, 16),
    (213, 16),
    (241, 16),
    (245, 16),
    (402, 16),
    (512, 16),
    (515, 16),
    (544, 16),
    (547, 16),
    (911, 16),
    (1002, 16),
    (1222, 16),
    (2601, 14),
    (2627, 14),
    (2628, 16),
    (2705, 16),
    (2714, 16),
    (2715, 16),
    (2750, 16),
    (3701, 11),
    (3729, 16),
    (3902, 16),
    (3903, 16),
    (3906, 16),
    (4060, 11),
    (4104, 16),
    (8110, 16),
    (8115, 16),
    (8120, 16),
    (8134, 16),
    (8152, 16),
    (17809, 20),
    (18456, 14),
    (40517, 16),
    (GENERIC_ERROR, 16),
];

/// The severity SQL Server reports for error `code`: 16 for most errors a statement can
/// cause, 15 for syntax errors, 14 for duplicate keys and failed logins, 11 for a missing
/// object or database.
pub(crate) fn severity_of(code: u16) -> u8 {
    SEVERITIES
        .iter()
        .find(|(known, _)| *known == code)
        .map_or(16, |&(_, severity)| severity)
}

fn quoted_names(message: &str) -> Vec<String> {
    message
        .split('"')
        .skip(1)
        .step_by(2)
        .map(shown_name)
        .collect()
}

fn shown_name(name: &str) -> String {
    match name.strip_prefix("public.") {
        Some(rest) => format!("dbo.{rest}"),
        None => name.to_owned(),
    }
}

fn first_word(text: &str) -> String {
    text.split_whitespace()
        .find(|word| !word.starts_with("--"))
        .unwrap_or("")
        .trim_start_matches(';')
        .to_ascii_uppercase()
}

fn object_name(table: &str) -> String {
    if table.contains('.') {
        table.to_owned()
    } else {
        format!("dbo.{table}")
    }
}

fn type_name(postgresql: &str) -> &str {
    match postgresql {
        "integer" => "int",
        "bigint" => "bigint",
        "smallint" => "smallint",
        "text" | "character varying" | "unknown" => "nvarchar",
        "character" | "bpchar" => "nchar",
        "numeric" => "numeric",
        "double precision" => "float",
        "real" => "real",
        "boolean" => "bit",
        "date" => "date",
        "timestamp without time zone" | "timestamp" => "datetime2",
        "timestamp with time zone" => "datetimeoffset",
        "time without time zone" | "time" => "time",
        "bytea" => "varbinary",
        "uuid" => "uniqueidentifier",
        other => other,
    }
}

fn operator_name(symbol: &str) -> Option<&'static str> {
    Some(match symbol {
        "+" => "add",
        "-" => "subtract",
        "*" => "multiply",
        "/" => "divide",
        "%" => "modulo",
        "=" => "equal to",
        "<>" => "not equal to",
        "<" => "less than",
        "<=" => "less than or equal to",
        ">" => "greater than",
        ">=" => "greater than or equal to",
        _ => return None,
    })
}

fn is_text_type(name: &str) -> bool {
    matches!(
        name,
        "text" | "character varying" | "character" | "bpchar" | "unknown" | "varchar"
    )
}

fn concatenates_text(message: &str) -> bool {
    let Some(rest) = message.strip_prefix("operator does not exist: ") else {
        return false;
    };
    match rest.split_once(" + ") {
        Some((left, right)) => is_text_type(left) || is_text_type(right),
        None => rest.strip_prefix("+ ").is_some_and(is_text_type),
    }
}

fn incompatible_operands(message: &str) -> Option<String> {
    let rest = message.strip_prefix("operator does not exist: ")?;
    let words: Vec<&str> = rest.split(' ').collect();
    let at = words
        .iter()
        .position(|word| operator_name(word).is_some())?;
    let operator = operator_name(words[at])?;
    let left = words[..at].join(" ");
    let right = words[at + 1..].join(" ");
    if left.is_empty() || right.is_empty() {
        return None;
    }
    Some(format!(
        "The data types {} and {} are incompatible in the {operator} operator.",
        type_name(&left),
        type_name(&right)
    ))
}

fn near(text: &str, error: &SqlError) -> String {
    if let Some(found) = error
        .message()
        .strip_prefix("syntax error at or near \"")
        .and_then(|rest| rest.strip_suffix('"'))
    {
        return found.to_owned();
    }
    let end = error.position().unwrap_or(text.len()).min(text.len());
    text.get(..end)
        .unwrap_or("")
        .split_whitespace()
        .last()
        .unwrap_or("")
        .to_owned()
}

fn syntax(error: &SqlError, text: &str) -> (u16, String) {
    let message = error.message();
    if message.starts_with("Incorrect syntax near") || message.starts_with("An object or column") {
        let code = if message.starts_with("An object") {
            1038
        } else {
            102
        };
        return (code, message.to_owned());
    }
    if message.starts_with("INSERT has more") {
        return (
            213,
            "Column name or number of supplied values does not match table definition.".to_owned(),
        );
    }
    if message == "VALUES lists must all be the same length" {
        return (
            10709,
            "The number of columns for each row in a table value constructor must be the same."
                .to_owned(),
        );
    }
    if let Some(rest) = message.strip_prefix("unterminated quoted string at or near \"") {
        let string = rest.trim_end_matches('"').trim_start_matches(['\'', 'N']);
        return (
            105,
            format!("Unclosed quotation mark after the character string '{string}'."),
        );
    }
    if message.starts_with("unterminated") {
        return (
            102,
            "Incorrect syntax: the text ends inside a quoted name or a comment.".to_owned(),
        );
    }
    let found = near(text, error);
    if crate::parser::is_sqlserver_reserved(&found.to_ascii_lowercase()) {
        return (
            156,
            format!(
                "Incorrect syntax near the keyword '{}'.",
                found.to_ascii_uppercase()
            ),
        );
    }
    (102, format!("Incorrect syntax near '{found}'."))
}

fn duplicate_key(error: &SqlError) -> (u16, String) {
    let names = quoted_names(error.message());
    let constraint = names.first().cloned().unwrap_or_default();
    let value = error
        .detail()
        .and_then(|detail| detail.split_once(")=("))
        .and_then(|(_, rest)| rest.split_once(") already exists"))
        .map_or(String::new(), |(value, _)| {
            format!(" The duplicate key value is ({value}).")
        });
    let object = error.table().map(object_name).unwrap_or_default();
    let kind = if constraint.ends_with("_pkey") || constraint.starts_with("pk_") {
        "PRIMARY KEY"
    } else {
        "UNIQUE KEY"
    };
    (
        2627,
        format!(
            "Violation of {kind} constraint '{constraint}'. Cannot insert duplicate key in object '{object}'.{value}"
        ),
    )
}

fn not_null(error: &SqlError, text: &str, database: &str) -> (u16, String) {
    let names = quoted_names(error.message());
    let column = names.first().cloned().unwrap_or_default();
    let table = names
        .get(1)
        .map(|table| object_name(table))
        .unwrap_or_default();
    let statement = if first_word(text) == "UPDATE" {
        "UPDATE"
    } else {
        "INSERT"
    };
    (
        515,
        format!(
            "Cannot insert the value NULL into column '{column}', table '{database}.{table}'; column does not allow nulls. {statement} fails."
        ),
    )
}

fn conflict(error: &SqlError, text: &str, kind: &str) -> (u16, String) {
    let names = quoted_names(error.message());
    let constraint = names.get(1).cloned().unwrap_or_default();
    let statement = match first_word(text).as_str() {
        "UPDATE" => "UPDATE",
        "DELETE" => "DELETE",
        _ => "INSERT",
    };
    let kind = if statement == "DELETE" && kind == "FOREIGN KEY" {
        "REFERENCE"
    } else {
        kind
    };
    (
        547,
        format!(
            "The {statement} statement conflicted with the {kind} constraint \"{constraint}\"."
        ),
    )
}

fn conversion(message: &str) -> (u16, String) {
    let Some(rest) = message.strip_prefix("invalid input syntax for type ") else {
        return (
            245,
            "Conversion failed when converting a value to another data type.".to_owned(),
        );
    };
    let (target, value) = match rest.split_once(": \"") {
        Some((target, value)) => (target, value.trim_end_matches('"')),
        None => (rest, ""),
    };
    (
        245,
        format!(
            "Conversion failed when converting the nvarchar value '{value}' to data type {}.",
            type_name(target)
        ),
    )
}

fn not_supported(message: &str) -> String {
    if message.ends_with("in this version of EggDB.") {
        return message.to_owned();
    }
    let trimmed = message.trim_end_matches('.').trim_end_matches(" yet");
    if let Some(what) = trimmed.strip_suffix(" are not supported") {
        return format!("{what} are not supported in this version of EggDB.");
    }
    let what = trimmed.trim_end_matches(" is not supported");
    format!("{what} is not supported in this version of EggDB.")
}

fn generic(error: &SqlError) -> String {
    let names = quoted_names(error.message());
    if names.is_empty() {
        format!("The statement failed with SQLSTATE {}.", error.state())
    } else {
        format!(
            "The statement failed with SQLSTATE {} on '{}'.",
            error.state(),
            names.join("', '")
        )
    }
}

/// `error`, raised by the engine in PostgreSQL's terms while running `text` (at `statement`, the
/// failing statement and the text after it), with SQL Server's
/// number and wording, as Microsoft's "Database engine events and errors" lists them, in the
/// database named `database`. An error without a SQL Server counterpart becomes `Msg 50000`
/// (the number SQL Server gives user messages of `RAISERROR`), with a neutral message rebuilt
/// from its SQLSTATE and the names it carries, so no PostgreSQL wording reaches a SQL Server
/// client.
pub(crate) fn translate(error: SqlError, text: &str, statement: &str, database: &str) -> SqlError {
    if error.vendor() == Some(crate::Dialect::SqlServer) {
        return error;
    }
    let message = error.message().to_owned();
    let names = quoted_names(&message);
    let first = names.first().cloned().unwrap_or_default();
    let (code, wording) = match error.state().code() {
        "42601" => syntax(&error, text),
        "42622" if message.starts_with("The identifier") => (103, message),
        "42622" => (
            103,
            format!("The identifier that starts with '{first}' is too long."),
        ),
        "42P01" if first_word(statement) == "DROP" => (
            3701,
            format!(
                "Cannot drop the table '{first}', because it does not exist or you do not have permission."
            ),
        ),
        "42P01" => (208, format!("Invalid object name '{first}'.")),
        "42703" => (207, format!("Invalid column name '{first}'.")),
        "42702" => (209, format!("Ambiguous column name '{first}'.")),
        "42P07" | "42S01" | "42710" => (
            2714,
            format!("There is already an object named '{first}' in the database."),
        ),
        "42701" => (
            2705,
            format!("Column names in each table must be unique. Column name '{first}' is specified more than once."),
        ),
        "42883" if message.starts_with("function ") => {
            let (function, arguments) = message
                .strip_prefix("function ")
                .and_then(|rest| rest.split_once('('))
                .map_or((String::new(), String::new()), |(function, rest)| {
                    let arguments = rest.rsplit_once(')').map_or(rest, |(inside, _)| inside);
                    (function.to_owned(), arguments.to_owned())
                });
            if ["sum", "avg"].contains(&function.as_str()) && !arguments.is_empty() {
                (
                    8117,
                    format!(
                        "Operand data type {} is invalid for {function} operator.",
                        type_name(arguments.split('(').next().unwrap_or_default().trim())
                    ),
                )
            } else {
                (
                    195,
                    format!("'{function}' is not a recognized built-in function name."),
                )
            }
        }
        "42883" if concatenates_text(&message) => (
            40517,
            "String concatenation with + is not supported in this version of EggDB.".to_owned(),
        ),
        "42883" => match incompatible_operands(&message) {
            Some(wording) => (402, wording),
            None => (402, "The data types are incompatible in this operator.".to_owned()),
        },
        "2201W" | "2201X" => (
            1060,
            "The number of rows provided for a TOP or FETCH clauses row count parameter must be an integer.".to_owned(),
        ),
        "42804" | "42846" => (
            402,
            "The data types of the operands are incompatible.".to_owned(),
        ),
        "42803" => {
            let column = first.rsplit('.').next().unwrap_or("").to_owned();
            (
                8120,
                format!(
                    "Column '{column}' is invalid in the select list because it is not contained in either an aggregate function or the GROUP BY clause."
                ),
            )
        }
        "42P16" if message.starts_with("Incorrect table definition") => (
            40517,
            "An IDENTITY column must be the first column of the table's primary key in this version of EggDB.".to_owned(),
        ),
        "42P16" if message.starts_with("multiple primary keys") => (
            8110,
            "Cannot add multiple PRIMARY KEY constraints to the table.".to_owned(),
        ),
        "23505" => duplicate_key(&error),
        "23502" => not_null(&error, statement, database),
        "23503" => conflict(&error, statement, "FOREIGN KEY"),
        "23514" => conflict(&error, statement, "CHECK"),
        "22001" => (8152, "String or binary data would be truncated.".to_owned()),
        "22012" => (8134, "Divide by zero error encountered.".to_owned()),
        "22003" => (
            8115,
            "Arithmetic overflow error converting expression to its data type.".to_owned(),
        ),
        "22P02" => conversion(&message),
        "22007" | "22008" => (
            241,
            "Conversion failed when converting date and/or time from character string."
                .to_owned(),
        ),
        "21000" => (
            512,
            "Subquery returned more than 1 value. This is not permitted when the subquery follows =, !=, <, <= , >, >= or when the subquery is used as an expression.".to_owned(),
        ),
        "0A000" => (40517, not_supported(&message)),
        "55P03" => (1222, "Lock request time out period exceeded.".to_owned()),
        "25006" => (
            3906,
            format!("Failed to update database \"{database}\" because the database is read-only."),
        ),
        "3D000" => (
            911,
            format!("Database '{first}' does not exist. Make sure that the name is entered correctly."),
        ),
        _ => (GENERIC_ERROR, generic(&error)),
    };
    let translated = SqlError::new(error.state(), wording).with_sqlserver_code(code);
    match error.position() {
        Some(position) => translated.at_if_unplaced(position),
        None => translated,
    }
}

#[cfg(test)]
mod tests {
    use super::{severity_of, translate};
    use crate::error::{SqlError, SqlState};

    fn shown(error: SqlError, text: &str) -> String {
        translate(error, text, text, "shop").to_string()
    }

    #[test]
    fn errors_take_sql_server_numbers_severities_and_wording() {
        assert_eq!(
            shown(
                SqlError::new(
                    SqlState::UNDEFINED_TABLE,
                    "relation \"missing\" does not exist"
                ),
                "SELECT * FROM missing"
            ),
            "Msg 208, Level 16, State 1: Invalid object name 'missing'."
        );
        assert_eq!(
            shown(
                SqlError::new(SqlState::UNDEFINED_TABLE, "table \"t\" does not exist"),
                "DROP TABLE t"
            ),
            "Msg 3701, Level 11, State 1: Cannot drop the table 't', because it does not exist or you do not have permission."
        );
        assert_eq!(
            shown(
                SqlError::new(SqlState::UNDEFINED_COLUMN, "column \"x\" does not exist"),
                "SELECT x FROM t"
            ),
            "Msg 207, Level 16, State 1: Invalid column name 'x'."
        );
        let duplicate = SqlError::new(
            SqlState::UNIQUE_VIOLATION,
            "duplicate key value violates unique constraint \"t_pkey\"",
        )
        .with_detail("Key (id)=(1) already exists.")
        .with_table("t");
        assert_eq!(
            shown(duplicate, "INSERT INTO t VALUES (1)"),
            "Msg 2627, Level 14, State 1: Violation of PRIMARY KEY constraint 't_pkey'. Cannot insert duplicate key in object 'dbo.t'. The duplicate key value is (1)."
        );
        assert_eq!(
            shown(
                SqlError::new(
                    SqlState::NOT_NULL_VIOLATION,
                    "null value in column \"name\" of relation \"t\" violates not-null constraint"
                ),
                "INSERT INTO t (id) VALUES (1)"
            ),
            "Msg 515, Level 16, State 1: Cannot insert the value NULL into column 'name', table 'shop.dbo.t'; column does not allow nulls. INSERT fails."
        );
        assert_eq!(
            shown(
                SqlError::new(SqlState::DIVISION_BY_ZERO, "division by zero"),
                "SELECT 1/0"
            ),
            "Msg 8134, Level 16, State 1: Divide by zero error encountered."
        );
        assert_eq!(
            shown(
                SqlError::new(
                    SqlState::UNDEFINED_FUNCTION,
                    "operator does not exist: integer - text"
                ),
                "SELECT 1 - name FROM t"
            ),
            "Msg 402, Level 16, State 1: The data types int and nvarchar are incompatible in the subtract operator."
        );
        for message in [
            "operator does not exist: text + text",
            "operator does not exist: character varying + integer",
            "operator does not exist: integer + character varying",
        ] {
            let error = translate(
                SqlError::new(SqlState::UNDEFINED_FUNCTION, message),
                "SELECT a + b FROM t",
                "SELECT a + b FROM t",
                "shop",
            );
            assert_eq!(error.vendor_code(), Some(40517), "{message}");
        }
        assert_eq!(
            shown(
                SqlError::new(
                    SqlState::UNDEFINED_FUNCTION,
                    "function sum(character varying) does not exist"
                ),
                "SELECT sum(name) FROM t"
            ),
            "Msg 8117, Level 16, State 1: Operand data type nvarchar is invalid for sum operator."
        );
        assert_eq!(
            shown(
                SqlError::new(
                    SqlState::UNDEFINED_FUNCTION,
                    "function sum(character varying(20)) does not exist"
                ),
                "SELECT sum(name) FROM t"
            ),
            "Msg 8117, Level 16, State 1: Operand data type nvarchar is invalid for sum operator."
        );
    }

    #[test]
    fn syntax_errors_name_the_token_or_keyword_near_them() {
        let error = translate(
            SqlError::syntax("syntax error at or near \"FORM\"", 9),
            "SELECT * FORM t",
            "SELECT * FORM t",
            "shop",
        );
        assert_eq!(error.vendor_code(), Some(102));
        assert_eq!(error.message(), "Incorrect syntax near 'FORM'.");
        let keyword = translate(
            SqlError::syntax("syntax error at or near \"from\"", 7),
            "SELECT from t",
            "SELECT from t",
            "shop",
        );
        assert_eq!(keyword.vendor_code(), Some(156));
        assert_eq!(
            keyword.message(),
            "Incorrect syntax near the keyword 'FROM'."
        );
        assert_eq!(severity_of(102), 15);
        assert_eq!(severity_of(18456), 14);
    }

    #[test]
    fn errors_without_a_counterpart_carry_no_postgresql_wording() {
        let error = translate(
            SqlError::new(
                SqlState::INTERNAL_ERROR,
                "could not open relation with OID 5",
            )
            .with_hint("PostgreSQL hint"),
            "SELECT 1",
            "SELECT 1",
            "shop",
        );
        assert_eq!(error.vendor_code(), Some(50000));
        assert_eq!(error.message(), "The statement failed with SQLSTATE XX000.");
        assert_eq!(error.hint(), None);
        let unsupported = translate(
            SqlError::not_supported("SELECT INTO is not supported yet"),
            "SELECT * INTO u FROM t",
            "SELECT * INTO u FROM t",
            "shop",
        );
        assert_eq!(unsupported.vendor_code(), Some(40517));
        assert_eq!(
            unsupported.message(),
            "SELECT INTO is not supported in this version of EggDB."
        );
    }
}
