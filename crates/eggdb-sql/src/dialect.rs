/// A SQL dialect: the language a [`Session`](crate::Session) reads, with its own lexical rules,
/// statements, functions, error codes and messages. Every dialect works on the same tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dialect {
    /// PostgreSQL's SQL, as its manual documents it: the default.
    #[default]
    PostgreSql,
    /// MySQL's SQL, as its reference manual documents it, with MySQL's error numbers and
    /// messages.
    MySql,
    /// SQLite's SQL, as sqlite.org documents it: type affinity, `INTEGER PRIMARY KEY` row ids,
    /// `PRAGMA`s, SQLite's functions and SQLite's error messages and result codes.
    Sqlite,
    /// Oracle Database's SQL, as Oracle's SQL Language Reference documents it: `DUAL`,
    /// `ROWNUM`, `FETCH FIRST`, Oracle's types and functions, the empty string as NULL, upper-case
    /// identifiers, implicit transactions and `ORA-` errors. EggDB does not speak Oracle's wire
    /// protocol.
    Oracle,
}

impl Dialect {
    /// Every dialect, in the order the roadmap brought them.
    pub const ALL: [Self; 4] = [Self::PostgreSql, Self::MySql, Self::Sqlite, Self::Oracle];

    /// The dialect's name as `--dialect` takes it: `postgresql`, `mysql`, `sqlite` or `oracle`.
    pub fn name(self) -> &'static str {
        match self {
            Self::PostgreSql => "postgresql",
            Self::MySql => "mysql",
            Self::Sqlite => "sqlite",
            Self::Oracle => "oracle",
        }
    }

    /// The dialect named `name` (case-insensitive), accepting `postgres`, `pg`, `mariadb` and
    /// `sqlite3` as other names.
    pub fn named(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "postgresql" | "postgres" | "pg" => Some(Self::PostgreSql),
            "mysql" | "mariadb" => Some(Self::MySql),
            "sqlite" | "sqlite3" => Some(Self::Sqlite),
            "oracle" => Some(Self::Oracle),
            _ => None,
        }
    }

    /// `value` as this dialect's clients print it in text form: PostgreSQL's `t` and `f` for
    /// booleans, MySQL's and SQLite's `1` and `0`, SQLite's floating point with 15 significant
    /// digits and a decimal point (`2.0`), Oracle's `TRUE` and `FALSE` and its numbers without a
    /// leading zero (`.5`), and `None` for NULL.
    pub fn value_text(self, value: &crate::Value) -> Option<String> {
        match (self, value) {
            (Self::MySql | Self::Sqlite, crate::Value::Boolean(flag)) => {
                Some(if *flag { "1" } else { "0" }.into())
            }
            (Self::Sqlite, crate::Value::Real(number)) => {
                crate::sqlite_functions::real_text(f64::from(*number))
            }
            (Self::Sqlite, crate::Value::Double(number)) => {
                crate::sqlite_functions::real_text(*number)
            }
            (Self::Oracle, crate::Value::Boolean(flag)) => {
                Some(if *flag { "TRUE" } else { "FALSE" }.into())
            }
            (Self::Oracle, crate::Value::Real(number)) => {
                Some(crate::oracle_functions::number_text(f64::from(*number)))
            }
            (Self::Oracle, crate::Value::Double(number)) => {
                Some(crate::oracle_functions::number_text(*number))
            }
            (Self::Oracle, crate::Value::Numeric(number)) => Some(oracle_numeric_text(number)),
            (Self::Sqlite, crate::Value::Numeric(number)) => Some(match number.integral_value() {
                Some(whole) => whole.to_string(),
                None => crate::sqlite_functions::real_text(number.to_f64())?,
            }),
            (Self::MySql | Self::Sqlite | Self::Oracle, crate::Value::TimestampTz(micros)) => {
                Some(match crate::datetime::utc_to_local(*micros) {
                    Ok(local) => crate::datetime::format_timestamp(local),
                    Err(_) => crate::datetime::format_timestamp(*micros),
                })
            }
            _ => value.to_text(),
        }
    }
}

fn oracle_numeric_text(number: &crate::Numeric) -> String {
    let text = number.to_string();
    if number.is_nan() || !text.contains('.') {
        return text;
    }
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if let Some(rest) = trimmed.strip_prefix("0.") {
        return format!(".{rest}");
    }
    if let Some(rest) = trimmed.strip_prefix("-0.") {
        return format!("-.{rest}");
    }
    trimmed.to_owned()
}
