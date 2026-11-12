use crate::status::{EGGDB_ERR_IO, Failure};
use eggdb::sql::Parameter;
use eggdb_mysqlwire::client::{MySqlClient, MySqlClientError, parse_mysql_url};
use eggdb_pgwire::client::QueryRows;
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const PASSWORD_VARIABLE: &str = "MYSQL_PWD";

const BROKEN_CONNECTION: &str = "the connection to the server broke during an earlier call; close this session with eggdb_sql_close and connect again";

pub(crate) struct MySqlRemote {
    client: MySqlClient,
    broken: bool,
}

pub(crate) fn is_mysql_url(url: &str) -> bool {
    url.starts_with("mysql://")
}

fn failure(error: MySqlClientError) -> (Failure, bool) {
    match error {
        MySqlClientError::Server { state, message, .. } => {
            (Failure::remote_sql(&state, &message), false)
        }
        other => (Failure::new(EGGDB_ERR_IO, other.to_string()), true),
    }
}

fn literal(value: Option<&str>) -> String {
    match value {
        None => "NULL".to_owned(),
        Some(text) => format!("'{}'", text.replace('\\', "\\\\").replace('\'', "''")),
    }
}

fn inlined(text: &str, parameters: &[Parameter]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut quote = None;
    let mut next = 0;
    for character in text.chars() {
        match quote {
            Some(open) => {
                if character == open {
                    quote = None;
                }
                out.push(character);
            }
            None if matches!(character, '\'' | '"' | '`') => {
                quote = Some(character);
                out.push(character);
            }
            None if character == '?' && next < parameters.len() => {
                out.push_str(&literal(parameters[next].value.as_deref()));
                next += 1;
            }
            None => out.push(character),
        }
    }
    out
}

fn rows_of(columns: Vec<String>, rows: Vec<Vec<Option<String>>>) -> QueryRows {
    let tag = if columns.is_empty() {
        "OK".to_owned()
    } else {
        format!("SELECT {}", rows.len())
    };
    QueryRows {
        column_types: vec![0; columns.len()],
        columns,
        rows,
        tag,
    }
}

impl MySqlRemote {
    pub(crate) fn connect(url: &str) -> Result<Self, Failure> {
        let mut options = parse_mysql_url(url).map_err(Failure::invalid_argument)?;
        options.password = std::env::var(PASSWORD_VARIABLE).ok();
        let client =
            MySqlClient::connect(&options, CONNECT_TIMEOUT).map_err(|error| failure(error).0)?;
        Ok(Self {
            client,
            broken: false,
        })
    }

    pub(crate) fn run(
        &mut self,
        text: &str,
        parameters: Option<&[Parameter]>,
    ) -> Result<Vec<QueryRows>, Failure> {
        if self.broken {
            return Err(Failure::new(EGGDB_ERR_IO, BROKEN_CONNECTION));
        }
        let sent = match parameters {
            Some(values) if !values.is_empty() => inlined(text, values),
            _ => text.to_owned(),
        };
        match self.client.query(&sent) {
            Ok(results) => Ok(results
                .into_iter()
                .map(|result| rows_of(result.columns, result.rows))
                .collect()),
            Err(error) => {
                let (failed, broke) = failure(error);
                self.broken |= broke;
                Err(failed)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{inlined, literal};
    use eggdb::sql::Parameter;

    fn parameter(value: Option<&str>) -> Parameter {
        Parameter {
            declared_type: None,
            value: value.map(str::to_owned),
        }
    }

    #[test]
    fn parameters_become_escaped_literals_outside_quoted_text() {
        assert_eq!(literal(Some("it's a \\ path")), "'it''s a \\\\ path'");
        let sent = inlined(
            "SELECT '?', `?`, ? , ?",
            &[parameter(Some("a")), parameter(None)],
        );
        assert_eq!(sent, "SELECT '?', `?`, 'a' , NULL");
    }
}
