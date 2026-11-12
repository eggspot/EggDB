//! PostgreSQL's foreign data statements over `eggdb_foreign` (D229, D916): `CREATE SERVER`
//! names a source, `CREATE FOREIGN TABLE` declares columns over a query on it and becomes a
//! view that reads the source on every use, and `DROP` removes them.

use crate::ast::Name;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::sqlserver_security::{literal, qualified};

pub(crate) const SERVERS_TABLE: &str = "eggdb_internal.foreign_servers";
pub(crate) const TABLES_TABLE: &str = "eggdb_internal.foreign_tables";
pub(crate) const CREATE_REGISTRY: &str = "CREATE SCHEMA IF NOT EXISTS eggdb_internal; \
     CREATE TABLE IF NOT EXISTS eggdb_internal.foreign_servers (name text PRIMARY KEY, wrapper text NOT NULL, source text NOT NULL); \
     CREATE TABLE IF NOT EXISTS eggdb_internal.foreign_tables (name text PRIMARY KEY, server text NOT NULL)";
const ROW_ALIAS: &str = "eggdb_foreign_row";

/// A foreign data statement.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ForeignDataCommand {
    CreateWrapper,
    DropWrapper,
    CreateServer {
        name: Name,
        wrapper: String,
        options: Vec<(String, String)>,
        if_not_exists: bool,
    },
    DropServer {
        names: Vec<Name>,
        if_exists: bool,
        cascade: bool,
    },
    CreateTable {
        name: Name,
        columns: Vec<(String, String)>,
        server: String,
        options: Vec<(String, String)>,
        if_not_exists: bool,
    },
    DropTable {
        names: Vec<Name>,
        if_exists: bool,
    },
    ImportSchema {
        remote: String,
        limit: Option<(bool, Vec<String>)>,
        server: String,
        local: String,
        options: Vec<(String, String)>,
    },
}

impl ForeignDataCommand {
    pub(crate) fn tag(&self) -> &'static str {
        match self {
            Self::CreateWrapper => "CREATE FOREIGN DATA WRAPPER",
            Self::DropWrapper => "DROP FOREIGN DATA WRAPPER",
            Self::CreateServer { .. } => "CREATE SERVER",
            Self::DropServer { .. } => "DROP SERVER",
            Self::CreateTable { .. } => "CREATE FOREIGN TABLE",
            Self::DropTable { .. } => "DROP FOREIGN TABLE",
            Self::ImportSchema { .. } => "IMPORT FOREIGN SCHEMA",
        }
    }
}

fn option<'o>(options: &'o [(String, String)], name: &str) -> Option<&'o str> {
    options.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
}

fn needs(options: &[(String, String)], name: &str, wrapper: &str) -> SqlResult<String> {
    option(options, name).map(str::to_owned).ok_or_else(|| {
        SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("a {wrapper} server needs the option {name}"),
        )
    })
}

/// The `eggdb_foreign` source a server's wrapper and options name: `source` as given, or a
/// URL built from `host`, `port`, `dbname` and `user` for the PostgreSQL, MySQL and SQL Server
/// wrappers; `file_fdw` servers read the files their tables name.
pub(crate) fn server_source(wrapper: &str, options: &[(String, String)]) -> SqlResult<String> {
    if option(options, "password").is_some() {
        return Err(SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "keep passwords out of the catalog: the eggdb program reads them from EGGDB_FOREIGN_PASSFILE or the source's own variable",
        ));
    }
    if let Some(source) = option(options, "source") {
        return Ok(source.to_owned());
    }
    let host = option(options, "host").unwrap_or("localhost");
    Ok(match wrapper {
        "postgres_fdw" | "eggdb" | "eggdb_fdw" => format!(
            "postgresql://{}@{host}:{}/{}",
            needs(options, "user", wrapper)?,
            option(options, "port").unwrap_or("5432"),
            option(options, "dbname").unwrap_or_default()
        ),
        "mysql_fdw" => format!(
            "mysql://{}@{host}:{}/{}",
            needs(options, "user", wrapper)?,
            option(options, "port").unwrap_or("3306"),
            option(options, "dbname").unwrap_or_default()
        ),
        "tds_fdw" => format!(
            "sqlserver://{}@{}:{}/{}",
            needs(options, "user", wrapper)?,
            option(options, "servername").unwrap_or(host),
            option(options, "port").unwrap_or("1433"),
            option(options, "database").or_else(|| option(options, "dbname")).unwrap_or_default()
        ),
        "file_fdw" => "file:".to_owned(),
        other => {
            return Err(SqlError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("the foreign data wrapper {other} is not supported: give the server a source option, or use postgres_fdw, mysql_fdw, tds_fdw or file_fdw"),
            ));
        }
    })
}

fn source_quoted(source: &str, part: &str) -> String {
    if source.starts_with("mysql://") {
        format!("`{}`", part.replace('`', "``"))
    } else if source.starts_with("sqlserver://") {
        format!("[{}]", part.replace(']', "]]"))
    } else {
        format!("\"{}\"", part.replace('"', "\"\""))
    }
}

/// The source and query a foreign table reads: its `query` option, its `table_name` (with
/// `schema_name`) on the server, or for a `file:` server its `filename`.
pub(crate) fn table_source(server_source: &str, table: &str, options: &[(String, String)]) -> SqlResult<(String, String)> {
    if server_source == "file:" {
        let file = option(options, "filename").ok_or_else(|| {
            SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "a file_fdw foreign table needs the option filename")
        })?;
        return Ok((format!("file:{file}"), "*".to_owned()));
    }
    if let Some(query) = option(options, "query") {
        return Ok((server_source.to_owned(), query.to_owned()));
    }
    let remote = option(options, "table_name").unwrap_or(table);
    let query = match option(options, "schema_name") {
        Some(schema) => format!("SELECT * FROM {}.{}", source_quoted(server_source, schema), source_quoted(server_source, remote)),
        None => format!("SELECT * FROM {}", source_quoted(server_source, remote)),
    };
    Ok((server_source.to_owned(), query))
}

/// The query that lists a remote schema's columns in the source's `information_schema`,
/// aliased so every dialect names the fields alike.
pub(crate) fn columns_query(source: &str, schema: &str) -> String {
    format!(
        "SELECT table_name AS table_name, column_name AS column_name, data_type AS data_type FROM information_schema.columns WHERE table_schema = {} ORDER BY table_name, ordinal_position",
        if source.starts_with("sqlserver://") { format!("N{}", literal(schema)) } else { literal(schema) }
    )
}

/// The local type for a remote column's `information_schema` data type; text for one EggDB
/// does not know.
pub(crate) fn local_type(remote: &str) -> &'static str {
    match remote.trim().to_ascii_lowercase().as_str() {
        "smallint" | "tinyint" | "int2" | "year" => "smallint",
        "integer" | "int" | "int4" | "mediumint" => "integer",
        "bigint" | "int8" => "bigint",
        "real" | "float4" | "float" => "real",
        "double precision" | "double" | "float8" => "double precision",
        "numeric" | "decimal" | "money" | "smallmoney" => "numeric",
        "boolean" | "bool" | "bit" => "boolean",
        "date" => "date",
        "time" | "time without time zone" => "time",
        "timestamp" | "timestamp without time zone" | "datetime" | "datetime2" | "smalldatetime" => "timestamp",
        "timestamp with time zone" | "datetimeoffset" => "timestamptz",
        "uuid" | "uniqueidentifier" => "uuid",
        "json" | "jsonb" => "jsonb",
        "bytea" | "blob" | "longblob" | "mediumblob" | "tinyblob" | "binary" | "varbinary" | "image" => "bytea",
        "interval" => "interval",
        _ => "text",
    }
}

/// The view a foreign table becomes: each declared column read from the source's JSON rows
/// and cast to its declared type.
pub(crate) fn view_sql(table: &str, columns: &[(String, String)], source: &str, query: &str) -> String {
    let projected: Vec<String> = columns
        .iter()
        .map(|(column, type_text)| {
            let operator = if type_text.trim().to_ascii_lowercase().starts_with("json") { "->" } else { "->>" };
            format!(
                "({ROW_ALIAS}.o {operator} {})::{type_text} AS {}",
                literal(column),
                crate::schema::quoted(column)
            )
        })
        .collect();
    format!(
        "CREATE VIEW {} AS SELECT {} FROM eggdb_foreign({}, {}) AS {ROW_ALIAS}(o)",
        qualified(table),
        projected.join(", "),
        literal(source),
        literal(query)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(key, value)| ((*key).to_owned(), (*value).to_owned())).collect()
    }

    #[test]
    fn foreign_tables_become_views_over_their_server() {
        let store = crate::memory_store::MemoryStore::default();
        let mut session = crate::Session::new(&store);
        let mut run = |sql: &str| session.execute(sql, &mut |_| {});
        run("CREATE SERVER shop FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host 'pg.internal', dbname 'shop', user 'reader')").unwrap();
        run("CREATE FOREIGN TABLE orders (id integer NOT NULL, total numeric(10, 2), doc jsonb) SERVER shop OPTIONS (schema_name 'sales', table_name 'orders')").unwrap();
        assert_eq!(run("CREATE SERVER shop FOREIGN DATA WRAPPER postgres_fdw OPTIONS (user 'x')").unwrap_err().state(), SqlState::DUPLICATE_OBJECT);
        let read = run("SELECT id FROM orders").unwrap_err();
        assert_eq!(read.state().code(), "HV000");
        assert_eq!(run("DROP SERVER shop").unwrap_err().state(), SqlState::DEPENDENT_OBJECTS_STILL_EXIST);
        run("DROP SERVER shop CASCADE").unwrap();
        assert_eq!(run("SELECT id FROM orders").unwrap_err().state(), SqlState::UNDEFINED_TABLE);
        run("DROP FOREIGN TABLE IF EXISTS orders").unwrap();
        run("CREATE FOREIGN DATA WRAPPER anything HANDLER h").unwrap();
    }

    #[test]
    fn remote_types_map_to_local_ones() {
        assert_eq!(local_type("character varying"), "text");
        assert_eq!(local_type("datetime2"), "timestamp");
        assert_eq!(local_type("TINYINT"), "smallint");
        assert_eq!(local_type("uniqueidentifier"), "uuid");
        assert!(columns_query("sqlserver://r@h:1433/d", "dbo").contains("table_schema = N'dbo'"));
    }

    #[test]
    fn servers_name_sources_and_tables_name_queries() {
        assert_eq!(
            server_source("postgres_fdw", &options(&[("host", "pg.internal"), ("dbname", "shop"), ("user", "reader")])).unwrap(),
            "postgresql://reader@pg.internal:5432/shop"
        );
        assert_eq!(
            server_source("mysql_fdw", &options(&[("host", "my.internal"), ("port", "3307"), ("user", "reader")])).unwrap(),
            "mysql://reader@my.internal:3307/"
        );
        assert!(server_source("postgres_fdw", &options(&[("user", "reader"), ("password", "x")])).is_err());
        assert!(server_source("oracle_fdw", &options(&[("user", "reader")])).is_err());
        assert_eq!(
            table_source("mysql://reader@my.internal:3307/shop", "orders", &options(&[("table_name", "Orders")])).unwrap().1,
            "SELECT * FROM `Orders`"
        );
        assert_eq!(
            table_source("file:", "events", &options(&[("filename", "/data/events.csv")])).unwrap(),
            ("file:/data/events.csv".to_owned(), "*".to_owned())
        );
        let view = view_sql("orders", &[("id".to_owned(), "integer".to_owned()), ("doc".to_owned(), "jsonb".to_owned())], "postgresql://r@h:5432/d", "SELECT 1");
        assert_eq!(
            view,
            "CREATE VIEW \"public\".\"orders\" AS SELECT (eggdb_foreign_row.o ->> 'id')::integer AS id, (eggdb_foreign_row.o -> 'doc')::jsonb AS doc FROM eggdb_foreign('postgresql://r@h:5432/d', 'SELECT 1') AS eggdb_foreign_row(o)"
        );
    }
}
