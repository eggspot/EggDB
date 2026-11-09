use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::Value;

pub(crate) const POSTGRESQL: &str = "sqrt";
pub(crate) const MYSQL: &str = "mysql_sqrt";
pub(crate) const SQLSERVER: &str = "sqlserver_sqrt";
pub(crate) const INTERNAL_NAMES: [&str; 2] = [MYSQL, SQLSERVER];

fn dialect_of(name: &str) -> Option<Dialect> {
    match name {
        POSTGRESQL => Some(Dialect::PostgreSql),
        MYSQL => Some(Dialect::MySql),
        SQLSERVER => Some(Dialect::SqlServer),
        _ => None,
    }
}

pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    let dialect = dialect_of(name)?;
    let [value] = values else {
        return None;
    };
    square_root(value, dialect)
}

pub(crate) fn square_root(value: &Value, dialect: Dialect) -> Option<SqlResult<Value>> {
    if value.is_null() {
        return Some(Ok(Value::Null));
    }
    match dialect {
        Dialect::PostgreSql => postgresql(value),
        Dialect::MySql => Some(Ok(mysql(value))),
        Dialect::Sqlite => Some(Ok(sqlite(value))),
        Dialect::SqlServer => Some(sqlserver(value)),
        Dialect::Oracle => Some(crate::oracle_functions::square_root(value)),
    }
}

fn postgresql(value: &Value) -> Option<SqlResult<Value>> {
    Some(match value {
        Value::Numeric(number) => number
            .square_root_to_significant_digits()
            .map(Value::Numeric),
        Value::Text(text) => match text.trim().parse::<f64>() {
            Ok(number) => postgresql_float(number),
            Err(_) => Err(SqlError::new(
                SqlState::INVALID_TEXT_REPRESENTATION,
                format!("invalid input syntax for type double precision: \"{text}\""),
            )),
        },
        Value::Boolean(_) => return None,
        other => postgresql_float(other.as_float()?),
    })
}

fn postgresql_float(number: f64) -> SqlResult<Value> {
    if number < 0.0 {
        return Err(SqlError::new(
            SqlState::INVALID_ARGUMENT_FOR_POWER,
            "cannot take square root of a negative number",
        ));
    }
    Ok(Value::Double(number.sqrt()))
}

fn root_or_null(number: f64) -> Value {
    if number < 0.0 || number.is_nan() {
        Value::Null
    } else {
        Value::Double(number.sqrt())
    }
}

fn mysql(value: &Value) -> Value {
    let number = match value {
        Value::Text(text) => crate::sqlite_functions::numeric_prefix(text),
        Value::Boolean(flag) => f64::from(u8::from(*flag)),
        other => match other.as_float() {
            Some(number) => number,
            None => return Value::Null,
        },
    };
    root_or_null(number)
}

fn sqlite(value: &Value) -> Value {
    let number = match value {
        Value::Text(text) => match text.trim().parse::<f64>() {
            Ok(number) if number.is_finite() => number,
            _ => return Value::Null,
        },
        Value::Boolean(flag) => f64::from(u8::from(*flag)),
        other => match other.as_float() {
            Some(number) => number,
            None => return Value::Null,
        },
    };
    root_or_null(number)
}

fn sqlserver(value: &Value) -> SqlResult<Value> {
    let number = match value {
        Value::Text(text) | Value::Bpchar(text) => text
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite())
            .ok_or_else(|| {
                SqlError::new(
                    SqlState::INVALID_TEXT_REPRESENTATION,
                    format!(
                        "Error converting data type {} to float.",
                        crate::sqlserver_types::value_type_name(value)
                    ),
                )
                .with_sqlserver_code(8114)
            })?,
        Value::Boolean(flag) => f64::from(u8::from(*flag)),
        other => other.as_float().ok_or_else(|| {
            SqlError::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "Operand type clash: {} is incompatible with float",
                    crate::sqlserver_types::value_type_name(other)
                ),
            )
            .with_sqlserver_code(206)
        })?,
    };
    if number < 0.0 || number.is_nan() {
        return Err(SqlError::new(
            SqlState::INVALID_ARGUMENT_FOR_POWER,
            "An invalid floating point operation occurred.",
        )
        .with_sqlserver_code(3623));
    }
    Ok(Value::Double(number.sqrt()))
}

#[cfg(test)]
mod tests {
    use super::{MYSQL, POSTGRESQL, SQLSERVER, function, square_root};
    use crate::dialect::Dialect;
    use crate::error::SqlState;
    use crate::numeric::Numeric;
    use crate::types::Value;

    fn numeric(text: &str) -> Value {
        Value::Numeric(Numeric::parse(text).expect("a number"))
    }

    fn shown(result: Option<crate::error::SqlResult<Value>>) -> String {
        match result {
            Some(Ok(value)) => value.to_text().unwrap_or_else(|| "NULL".to_owned()),
            Some(Err(error)) => format!("error {}", error.state().code()),
            None => "no such function".to_owned(),
        }
    }

    #[test]
    fn postgresql_keeps_numeric_at_sixteen_significant_digits_and_refuses_negatives() {
        assert_eq!(
            shown(function(POSTGRESQL, &[numeric("2")])),
            "1.414213562373095"
        );
        assert_eq!(
            shown(function(POSTGRESQL, &[numeric("12345")])),
            "111.1080555135405"
        );
        assert_eq!(
            shown(function(POSTGRESQL, &[numeric("2.00000000000000000000")])),
            "1.41421356237309504880"
        );
        assert_eq!(shown(function(POSTGRESQL, &[Value::Integer(16)])), "4");
        assert_eq!(
            shown(function(POSTGRESQL, &[Value::Double(2.0)])),
            "1.4142135623730951"
        );
        let refused = function(POSTGRESQL, &[Value::Integer(-1)]);
        let Some(Err(error)) = refused else {
            panic!("a negative argument must fail");
        };
        assert_eq!(error.state(), SqlState::INVALID_ARGUMENT_FOR_POWER);
        assert_eq!(
            error.message(),
            "cannot take square root of a negative number"
        );
        assert_eq!(shown(function(POSTGRESQL, &[numeric("-4")])), "error 2201F");
        assert_eq!(
            shown(function(POSTGRESQL, &[Value::Boolean(true)])),
            "no such function"
        );
        assert_eq!(shown(function(POSTGRESQL, &[Value::Null])), "NULL");
    }

    #[test]
    fn mysql_and_sqlite_give_null_for_a_negative_or_unreadable_argument() {
        assert_eq!(shown(function(MYSQL, &[Value::Integer(-16)])), "NULL");
        assert_eq!(shown(function(MYSQL, &[Value::Integer(16)])), "4");
        assert_eq!(
            shown(function(MYSQL, &[Value::Text("9abc".to_owned())])),
            "3"
        );
        assert_eq!(shown(Some(Ok(super::sqlite(&Value::Integer(-1))))), "NULL");
        assert_eq!(
            shown(square_root(&Value::Text("abc".to_owned()), Dialect::Sqlite)),
            "NULL"
        );
        assert_eq!(
            shown(square_root(
                &Value::Text(" 6.25 ".to_owned()),
                Dialect::Sqlite
            )),
            "2.5"
        );
    }

    #[test]
    fn sql_server_answers_error_3623_for_a_negative_argument() {
        let Some(Err(error)) = function(SQLSERVER, &[Value::Integer(-1)]) else {
            panic!("a negative argument must fail");
        };
        assert_eq!(error.vendor_code(), Some(3623));
        assert_eq!(
            error.message(),
            "An invalid floating point operation occurred."
        );
        assert_eq!(shown(function(SQLSERVER, &[numeric("6.25")])), "2.5");
        let Some(Err(error)) = function(SQLSERVER, &[Value::Text("x".to_owned())]) else {
            panic!("text that is not a number must fail");
        };
        assert_eq!(error.vendor_code(), Some(8114));
    }
}
