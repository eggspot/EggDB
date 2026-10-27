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
}

impl Dialect {
    /// Every dialect, in the order the roadmap brought them.
    pub const ALL: [Self; 2] = [Self::PostgreSql, Self::MySql];

    /// The dialect's name as `--dialect` takes it: `postgresql` or `mysql`.
    pub fn name(self) -> &'static str {
        match self {
            Self::PostgreSql => "postgresql",
            Self::MySql => "mysql",
        }
    }

    /// The dialect named `name` (case-insensitive), accepting `postgres`, `pg` and `mariadb`
    /// as other names.
    pub fn named(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "postgresql" | "postgres" | "pg" => Some(Self::PostgreSql),
            "mysql" | "mariadb" => Some(Self::MySql),
            _ => None,
        }
    }

    /// `value` as this dialect's clients print it in text form: PostgreSQL's `t` and `f` for
    /// booleans, MySQL's `1` and `0`, and `None` for NULL.
    pub fn value_text(self, value: &crate::Value) -> Option<String> {
        match (self, value) {
            (Self::MySql, crate::Value::Boolean(flag)) => {
                Some(if *flag { "1" } else { "0" }.into())
            }
            _ => value.to_text(),
        }
    }
}
