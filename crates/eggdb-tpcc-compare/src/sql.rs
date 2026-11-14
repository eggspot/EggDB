use eggdb_mysqlwire::client::{MySqlClient, MySqlConnectOptions, parse_mysql_url};
use eggdb_pgwire::client::{PgClient, SslMode, parse_postgres_url_with_password};
use std::time::Duration;

pub(crate) type Rows = Vec<Vec<Option<String>>>;

#[derive(Debug)]
pub(crate) enum Connection {
    Postgres(PgClient),
    MySql(MySqlClient),
}

impl Connection {
    pub(crate) fn postgres(url: &str) -> Result<Self, String> {
        let mut options = parse_postgres_url_with_password(url)?;
        options.ssl_mode = SslMode::Disable;
        PgClient::connect(&options).map(Self::Postgres).map_err(|error| error.to_string())
    }

    pub(crate) fn mysql(url: &str, password: Option<String>) -> Result<Self, String> {
        let mut options: MySqlConnectOptions = parse_mysql_url(url)?;
        options.password = password;
        MySqlClient::connect(&options, Duration::from_secs(10))
            .map(Self::MySql)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn run(&mut self, sql: &str) -> Result<Rows, String> {
        match self {
            Self::Postgres(client) => client
                .simple_query(sql)
                .map(|results| results.into_iter().last().map(|last| last.rows).unwrap_or_default())
                .map_err(|error| format!("{error}: {sql}")),
            Self::MySql(client) => client
                .query(sql)
                .map(|results| results.into_iter().last().map(|last| last.rows).unwrap_or_default())
                .map_err(|error| format!("{error}: {sql}")),
        }
    }

    pub(crate) fn number(&mut self, sql: &str) -> Result<i64, String> {
        let rows = self.run(sql)?;
        rows.first()
            .and_then(|row| row.first())
            .and_then(|value| value.as_deref())
            .and_then(|text| text.split('.').next())
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| format!("no number from: {sql}"))
    }
}
