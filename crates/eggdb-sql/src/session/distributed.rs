use super::{OutputColumn, QueryResult, Session};
use crate::dialect::Dialect;
use crate::distributed::{PARTIALS, ShardedTable, fixed_value, plan_distributed, shard_of};
use crate::dump::sql_string;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::{Token, TokenKind, tokenize_in};
use crate::types::{SqlType, Value};

const CREATE_REGISTRY: &str = "CREATE SCHEMA IF NOT EXISTS eggdb_internal; CREATE TABLE IF NOT EXISTS eggdb_internal.sharded_tables (name text PRIMARY KEY, shard_key text NOT NULL, sources text NOT NULL)";
const CREATE_INDEX_REGISTRY: &str = "CREATE SCHEMA IF NOT EXISTS eggdb_internal; CREATE TABLE IF NOT EXISTS eggdb_internal.global_indexes (name text PRIMARY KEY, table_name text NOT NULL, column_name text NOT NULL, sources text NOT NULL)";
const INDEX_TABLE: &str = "eggdb_global_index";
const SOURCE_SEPARATOR: char = '\n';
const SHARD_ROW: &str = "eggdb_shard_row";
const MOST_ROWS_GATHERED: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ShardStatement {
    Shard {
        table: String,
        key: String,
        sources: Vec<String>,
    },
    Unshard {
        table: String,
    },
    ShowShard {
        table: String,
        key: String,
    },
    CreateIndex {
        name: String,
        table: String,
        column: String,
        sources: Vec<String>,
    },
    DropIndex {
        name: String,
    },
    ShowIndexShard {
        name: String,
        value: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GlobalIndexEntry {
    name: String,
    table: String,
    column: String,
    sources: Vec<String>,
}

fn word_is(token: &Token<'_>, word: &str) -> bool {
    matches!(&token.kind, TokenKind::Word(found) if found.eq_ignore_ascii_case(word))
}

fn identifier(token: &Token<'_>) -> Option<String> {
    match &token.kind {
        TokenKind::Word(word) => Some(word.to_ascii_lowercase()),
        TokenKind::QuotedIdentifier(name) => Some(name.clone()),
        _ => None,
    }
}

fn table_name<'a, 't>(tokens: &'a [Token<'t>]) -> Option<(String, &'a [Token<'t>])> {
    match tokens {
        [schema, dot, table, rest @ ..] if matches!(dot.kind, TokenKind::Dot) => {
            Some((format!("{}.{}", identifier(schema)?, identifier(table)?), rest))
        }
        [table, rest @ ..] => Some((identifier(table)?, rest)),
        [] => None,
    }
}

fn sources_of(sources: &[Token<'_>]) -> Option<Vec<String>> {
    let mut found = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        match (&source.kind, index % 2) {
            (TokenKind::String(source), 0) => found.push(source.to_string()),
            (TokenKind::Comma, 1) => {}
            _ => return None,
        }
    }
    (!found.is_empty() && sources.len() % 2 == 1).then_some(found)
}

fn literal_of(token: &Token<'_>) -> Option<String> {
    match &token.kind {
        TokenKind::String(text) => Some(text.to_string()),
        TokenKind::Integer(digits) => Some((*digits).to_owned()),
        _ => None,
    }
}

fn index_statement(tokens: &[Token<'_>]) -> SqlResult<Option<ShardStatement>> {
    let malformed = || {
        SqlError::new(
            SqlState::SYNTAX_ERROR,
            "the global index statements are CREATE GLOBAL INDEX <name> ON <table> (<column>) ON '<source>' [, ...], DROP GLOBAL INDEX <name> and SHOW SHARD FOR INDEX <name> VALUE '<value>'",
        )
    };
    match tokens {
        [create, global, index, name, on, rest @ ..]
            if word_is(create, "create") && word_is(global, "global") && word_is(index, "index") && word_is(on, "on") =>
        {
            let name = identifier(name).ok_or_else(malformed)?;
            let (table, rest) = table_name(rest).ok_or_else(malformed)?;
            let [open, column, close, on_sources, sources @ ..] = rest else {
                return Err(malformed());
            };
            if !matches!(open.kind, TokenKind::LeftParen) || !matches!(close.kind, TokenKind::RightParen) || !word_is(on_sources, "on") {
                return Err(malformed());
            }
            let column = identifier(column).ok_or_else(malformed)?;
            let sources = sources_of(sources).ok_or_else(malformed)?;
            Ok(Some(ShardStatement::CreateIndex { name, table, column, sources }))
        }
        [drop, global, index, name] if word_is(drop, "drop") && word_is(global, "global") && word_is(index, "index") => {
            Ok(Some(ShardStatement::DropIndex { name: identifier(name).ok_or_else(malformed)? }))
        }
        [show, shard, for_word, index, name, value_word, value]
            if word_is(show, "show") && word_is(shard, "shard") && word_is(for_word, "for") && word_is(index, "index") && word_is(value_word, "value") =>
        {
            Ok(Some(ShardStatement::ShowIndexShard {
                name: identifier(name).ok_or_else(malformed)?,
                value: literal_of(value).ok_or_else(malformed)?,
            }))
        }
        _ => Ok(None),
    }
}

fn statement(text: &str) -> SqlResult<Option<ShardStatement>> {
    let lowered = text.trim_start().to_ascii_lowercase();
    let global = lowered.starts_with("create global index ")
        || lowered.starts_with("drop global index ")
        || lowered.starts_with("show shard for index ");
    if !global && !["shard ", "unshard ", "show shard "].iter().any(|start| lowered.starts_with(start)) {
        return Ok(None);
    }
    if global {
        let tokens = tokenize_in(text, Dialect::PostgreSql)?;
        let tokens: Vec<Token<'_>> = tokens
            .into_iter()
            .filter(|token| !matches!(token.kind, TokenKind::Semicolon | TokenKind::End))
            .collect();
        return index_statement(&tokens);
    }
    let tokens = tokenize_in(text, Dialect::PostgreSql)?;
    let tokens: Vec<Token<'_>> = tokens
        .into_iter()
        .filter(|token| !matches!(token.kind, TokenKind::Semicolon | TokenKind::End))
        .collect();
    let malformed = || {
        SqlError::new(
            SqlState::SYNTAX_ERROR,
            "the shard statements are SHARD TABLE <table> BY <column> ON '<source>' [, ...], UNSHARD TABLE <table> and SHOW SHARD FOR <table> KEY '<value>'",
        )
    };
    match tokens.as_slice() {
        [shard, table, rest @ ..] if word_is(shard, "shard") && word_is(table, "table") => {
            let (table, rest) = table_name(rest).ok_or_else(malformed)?;
            let [by, key, on, sources @ ..] = rest else {
                return Err(malformed());
            };
            if !word_is(by, "by") || !word_is(on, "on") {
                return Err(malformed());
            }
            let key = identifier(key).ok_or_else(malformed)?;
            let found = sources_of(sources).ok_or_else(malformed)?;
            Ok(Some(ShardStatement::Shard { table, key, sources: found }))
        }
        [unshard, table, rest @ ..] if word_is(unshard, "unshard") && word_is(table, "table") => {
            match table_name(rest) {
                Some((table, [])) => Ok(Some(ShardStatement::Unshard { table })),
                _ => Err(malformed()),
            }
        }
        [show, shard, rest @ ..] if word_is(show, "show") && word_is(shard, "shard") => {
            let [for_word, rest @ ..] = rest else {
                return Err(malformed());
            };
            if !word_is(for_word, "for") {
                return Err(malformed());
            }
            let (table, rest) = table_name(rest).ok_or_else(malformed)?;
            match rest {
                [key_word, value] if word_is(key_word, "key") => {
                    let key = literal_of(value).ok_or_else(malformed)?;
                    Ok(Some(ShardStatement::ShowShard { table, key }))
                }
                _ => Err(malformed()),
            }
        }
        _ => Ok(None),
    }
}

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

impl Session<'_> {
    fn distributed_sql(&mut self, text: &str) -> SqlResult<Vec<QueryResult>> {
        let dialect = self.dialect;
        self.dialect = Dialect::PostgreSql;
        let outcome = self.run_quietly(text);
        self.dialect = dialect;
        outcome
    }

    fn sharded_registry_exists(&mut self) -> SqlResult<bool> {
        let internal = crate::schemas::qualified("eggdb_internal", "sharded_tables");
        self.read(|reader, _| Ok(reader.get(&crate::encoding::table_key(&internal))?.is_some()))
    }

    fn sharded_tables(&mut self) -> SqlResult<Vec<ShardedTable>> {
        let results = self.distributed_sql("SELECT name, shard_key, sources FROM eggdb_internal.sharded_tables")?;
        Ok(results
            .iter()
            .flat_map(|result| result.rows.iter())
            .filter_map(|row| {
                let text = |index: usize| row.get(index).and_then(Value::to_text);
                Some(ShardedTable {
                    name: text(0)?,
                    shard_key: text(1)?,
                    sources: text(2)?.split(SOURCE_SEPARATOR).map(str::to_owned).collect(),
                })
            })
            .collect())
    }

    fn global_indexes(&mut self) -> SqlResult<Vec<GlobalIndexEntry>> {
        let internal = crate::schemas::qualified("eggdb_internal", "global_indexes");
        let registered = self.read(|reader, _| Ok(reader.get(&crate::encoding::table_key(&internal))?.is_some()))?;
        if !registered {
            return Ok(Vec::new());
        }
        let results = self.distributed_sql("SELECT name, table_name, column_name, sources FROM eggdb_internal.global_indexes")?;
        Ok(results
            .iter()
            .flat_map(|result| result.rows.iter())
            .filter_map(|row| {
                let text = |index: usize| row.get(index).and_then(Value::to_text);
                Some(GlobalIndexEntry {
                    name: text(0)?,
                    table: text(1)?,
                    column: text(2)?,
                    sources: text(3)?.split(SOURCE_SEPARATOR).map(str::to_owned).collect(),
                })
            })
            .collect())
    }

    fn shards_by_index(&mut self, text: &str, table: &ShardedTable) -> SqlResult<Option<Vec<usize>>> {
        for index in self.global_indexes()?.into_iter().filter(|index| index.table == table.name) {
            let Some(value) = fixed_value(text, &index.column)? else {
                continue;
            };
            let Some(source) = index.sources.get(shard_of(&value, index.sources.len())) else {
                continue;
            };
            let lookup = format!(
                "SELECT shard_key FROM {INDEX_TABLE} WHERE index_name = {} AND value = {}",
                sql_string(&index.name),
                sql_string(&value)
            );
            let results = self.distributed_sql(&format!(
                "SELECT ({SHARD_ROW}.o ->> 'shard_key') FROM eggdb_foreign({}, {}) AS {SHARD_ROW}(o)",
                sql_string(source),
                sql_string(&lookup)
            ))?;
            let mut shards: Vec<usize> = results
                .iter()
                .flat_map(|result| result.rows.iter())
                .filter_map(|row| row.first().and_then(Value::to_text))
                .map(|key| shard_of(&key, table.sources.len()))
                .collect();
            shards.sort_unstable();
            shards.dedup();
            return Ok(Some(shards));
        }
        Ok(None)
    }

    fn local_columns(&mut self, table: &str) -> SqlResult<Vec<(String, String)>> {
        let internal = match table.split_once('.') {
            Some((schema, name)) => crate::schemas::qualified(schema, name),
            None => table.to_owned(),
        };
        self.read(|reader, _| {
            let Some(stored) = reader.get(&crate::encoding::table_key(&internal))? else {
                return Err(SqlError::new(
                    SqlState::UNDEFINED_TABLE,
                    format!("the sharded table \"{table}\" is not declared here: create it with its columns on the coordinator too"),
                ));
            };
            let definition = crate::encoding::TableDefinition::decode(&internal, &stored)?;
            Ok(definition
                .columns
                .iter()
                .map(|column| (column.name.to_ascii_lowercase(), column.sql_type.name()))
                .collect())
        })
    }

    fn run_shard_statement(
        &mut self,
        statement: ShardStatement,
        on_result: &mut dyn FnMut(QueryResult) -> bool,
    ) -> SqlResult<()> {
        match statement {
            ShardStatement::Shard { table, key, sources } => {
                if self.settings.checked_role.is_some() || !self.settings.is_superuser() {
                    return Err(SqlError::new(SqlState::INSUFFICIENT_PRIVILEGE, "permission denied: SHARD TABLE needs a superuser"));
                }
                let columns = self.local_columns(&table)?;
                if !columns.iter().any(|(name, _)| *name == key) {
                    return Err(SqlError::new(
                        SqlState::UNDEFINED_COLUMN,
                        format!("column \"{key}\" of relation \"{table}\" does not exist"),
                    ));
                }
                let joined = sources.join(&SOURCE_SEPARATOR.to_string());
                self.atomically(|session| {
                    session.distributed_sql(CREATE_REGISTRY)?;
                    session.distributed_sql(&format!(
                        "DELETE FROM eggdb_internal.sharded_tables WHERE name = {}",
                        sql_string(&table)
                    ))?;
                    session.distributed_sql(&format!(
                        "INSERT INTO eggdb_internal.sharded_tables (name, shard_key, sources) VALUES ({}, {}, {})",
                        sql_string(&table),
                        sql_string(&key),
                        sql_string(&joined)
                    ))?;
                    Ok(())
                })?;
                on_result(QueryResult::command("SHARD TABLE"));
            }
            ShardStatement::Unshard { table } => {
                if self.settings.checked_role.is_some() || !self.settings.is_superuser() {
                    return Err(SqlError::new(SqlState::INSUFFICIENT_PRIVILEGE, "permission denied: UNSHARD TABLE needs a superuser"));
                }
                if self.sharded_registry_exists()? {
                    self.distributed_sql(&format!(
                        "DELETE FROM eggdb_internal.sharded_tables WHERE name = {}",
                        sql_string(&table)
                    ))?;
                }
                on_result(QueryResult::command("UNSHARD TABLE"));
            }
            ShardStatement::CreateIndex { name, table, column, sources } => {
                if self.settings.checked_role.is_some() || !self.settings.is_superuser() {
                    return Err(SqlError::new(SqlState::INSUFFICIENT_PRIVILEGE, "permission denied: CREATE GLOBAL INDEX needs a superuser"));
                }
                let sharded = if self.sharded_registry_exists()? { self.sharded_tables()? } else { Vec::new() };
                if !sharded.iter().any(|sharded| sharded.name == table) {
                    return Err(SqlError::new(SqlState::UNDEFINED_TABLE, format!("\"{table}\" is not a sharded table")));
                }
                if !self.local_columns(&table)?.iter().any(|(name, _)| *name == column) {
                    return Err(SqlError::new(
                        SqlState::UNDEFINED_COLUMN,
                        format!("column \"{column}\" of relation \"{table}\" does not exist"),
                    ));
                }
                let joined = sources.join(&SOURCE_SEPARATOR.to_string());
                self.atomically(|session| {
                    session.distributed_sql(CREATE_INDEX_REGISTRY)?;
                    session.distributed_sql(&format!("DELETE FROM eggdb_internal.global_indexes WHERE name = {}", sql_string(&name)))?;
                    session.distributed_sql(&format!(
                        "INSERT INTO eggdb_internal.global_indexes (name, table_name, column_name, sources) VALUES ({}, {}, {}, {})",
                        sql_string(&name),
                        sql_string(&table),
                        sql_string(&column),
                        sql_string(&joined)
                    ))?;
                    Ok(())
                })?;
                on_result(QueryResult::command("CREATE GLOBAL INDEX"));
            }
            ShardStatement::DropIndex { name } => {
                if self.settings.checked_role.is_some() || !self.settings.is_superuser() {
                    return Err(SqlError::new(SqlState::INSUFFICIENT_PRIVILEGE, "permission denied: DROP GLOBAL INDEX needs a superuser"));
                }
                if !self.global_indexes()?.is_empty() {
                    self.distributed_sql(&format!("DELETE FROM eggdb_internal.global_indexes WHERE name = {}", sql_string(&name)))?;
                }
                on_result(QueryResult::command("DROP GLOBAL INDEX"));
            }
            ShardStatement::ShowIndexShard { name, value } => {
                let Some(index) = self.global_indexes()?.into_iter().find(|index| index.name == name) else {
                    return Err(SqlError::new(SqlState::UNDEFINED_OBJECT, format!("global index \"{name}\" does not exist")));
                };
                let shard = shard_of(&value, index.sources.len());
                let columns = vec![
                    OutputColumn { name: "shard".to_owned(), sql_type: Some(SqlType::Integer) },
                    OutputColumn { name: "source".to_owned(), sql_type: Some(SqlType::Text) },
                ];
                let row = vec![
                    Value::Integer(shard as i32),
                    index.sources.get(shard).map_or(Value::Null, |source| Value::Text(source.clone())),
                ];
                on_result(QueryResult::rows("SHOW".to_owned(), columns, vec![row]));
            }
            ShardStatement::ShowShard { table, key } => {
                let found = if self.sharded_registry_exists()? {
                    self.sharded_tables()?.into_iter().find(|sharded| sharded.name == table)
                } else {
                    None
                };
                let Some(sharded) = found else {
                    return Err(SqlError::new(SqlState::UNDEFINED_TABLE, format!("\"{table}\" is not a sharded table")));
                };
                let shard = shard_of(&key, sharded.sources.len());
                let columns = vec![
                    OutputColumn { name: "shard".to_owned(), sql_type: Some(SqlType::Integer) },
                    OutputColumn { name: "source".to_owned(), sql_type: Some(SqlType::Text) },
                ];
                let row = vec![
                    Value::Integer(shard as i32),
                    sharded.sources.get(shard).map_or(Value::Null, |source| Value::Text(source.clone())),
                ];
                on_result(QueryResult::rows("SHOW".to_owned(), columns, vec![row]));
            }
        }
        Ok(())
    }

    /// Runs the shard statements, and a `SELECT` on a sharded table through the distributed
    /// planner (D1041): every chosen shard runs the pushed-down query through
    /// `eggdb_foreign`, and this server runs the final query over their answers.
    pub(super) fn prepare_distributed(
        &mut self,
        text: &str,
        on_result: &mut dyn FnMut(QueryResult) -> bool,
    ) -> SqlResult<bool> {
        if let Some(statement) = statement(text)? {
            self.run_shard_statement(statement, on_result)?;
            return Ok(true);
        }
        let lowered = text.trim_start().to_ascii_lowercase();
        if !lowered.starts_with("select") || lowered.contains(&format!(" as {PARTIALS}")) {
            return Ok(false);
        }
        if !self.sharded_registry_exists()? {
            return Ok(false);
        }
        for table in self.sharded_tables()? {
            let bare = table.name.rsplit('.').next().unwrap_or(&table.name).to_owned();
            if !lowered.contains(&bare) {
                continue;
            }
            let columns = self.local_columns(&table.name)?;
            let Some(mut plan) = plan_distributed(text, &table, &columns)? else {
                continue;
            };
            if plan.shards.len() > 1
                && let Some(shards) = self.shards_by_index(text, &table)?
            {
                plan.shards = shards;
            }
            let casts: Vec<String> = plan
                .columns
                .iter()
                .map(|column| {
                    format!(
                        "({SHARD_ROW}.o ->> {})::{} AS {}",
                        sql_string(&column.key),
                        column.type_text,
                        quoted(&column.name)
                    )
                })
                .collect();
            let sources: Vec<&String> = plan.shards.iter().filter_map(|shard| table.sources.get(*shard)).collect();
            let gathered = gather_in_parallel(&sources, &plan.shard_sql)?;
            let partials = match gathered {
                Some(rows) if rows.is_empty() => format!(
                    "SELECT {} FROM (SELECT NULL::jsonb AS o) AS {SHARD_ROW} WHERE false",
                    casts.join(", ")
                ),
                Some(rows) => format!(
                    "SELECT {} FROM (VALUES {}) AS {SHARD_ROW}(o)",
                    casts.join(", "),
                    rows.iter().map(|row| format!("({}::jsonb)", sql_string(row))).collect::<Vec<_>>().join(", ")
                ),
                None => sources
                    .iter()
                    .map(|source| {
                        format!(
                            "SELECT {} FROM eggdb_foreign({}, {}) AS {SHARD_ROW}(o)",
                            casts.join(", "),
                            sql_string(source),
                            sql_string(&plan.shard_sql)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" UNION ALL "),
            };
            let final_sql = plan.final_sql.replacen(
                &format!(" FROM {PARTIALS}"),
                &format!(" FROM ({partials}) AS {PARTIALS}"),
                1,
            );
            let dialect = self.dialect;
            self.dialect = Dialect::PostgreSql;
            let outcome = self.execute(&final_sql, &mut |result| {
                on_result(result);
            });
            self.dialect = dialect;
            outcome?;
            return Ok(true);
        }
        Ok(false)
    }
}

/// Every shard's answer to `shard_sql`, read from all of them at once, one thread each (D1050):
/// the shards work in parallel and the coordinator waits for the slowest instead of the sum.
/// `None` when the answers hold more rows than are worth gathering here, so the final query
/// streams them through `eggdb_foreign` instead.
fn gather_in_parallel(sources: &[&String], shard_sql: &str) -> SqlResult<Option<Vec<String>>> {
    let role = crate::constraints::checked_role();
    let answers: Vec<SqlResult<Vec<String>>> = std::thread::scope(|scope| {
        let readers: Vec<_> = sources
            .iter()
            .map(|source| {
                let role = role.clone();
                scope.spawn(move || crate::foreign::read_rows(source, shard_sql, role.as_deref()))
            })
            .collect();
        readers
            .into_iter()
            .map(|reader| {
                reader
                    .join()
                    .unwrap_or_else(|_| Err(SqlError::new(SqlState::FDW_ERROR, "a shard read stopped unexpectedly")))
            })
            .collect()
    });
    let mut rows = Vec::new();
    for answer in answers {
        rows.extend(answer?);
        if rows.len() > MOST_ROWS_GATHERED {
            return Ok(None);
        }
    }
    Ok(Some(rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_statements_read_their_parts() {
        assert_eq!(
            statement("SHARD TABLE app.orders BY customer ON 'postgresql://a@s1/db', 'postgresql://a@s2/db';").unwrap(),
            Some(ShardStatement::Shard {
                table: "app.orders".to_owned(),
                key: "customer".to_owned(),
                sources: vec!["postgresql://a@s1/db".to_owned(), "postgresql://a@s2/db".to_owned()],
            })
        );
        assert_eq!(
            statement("unshard table orders").unwrap(),
            Some(ShardStatement::Unshard { table: "orders".to_owned() })
        );
        assert_eq!(
            statement("SHOW SHARD FOR orders KEY 42").unwrap(),
            Some(ShardStatement::ShowShard { table: "orders".to_owned(), key: "42".to_owned() })
        );
        assert!(statement("SHARD TABLE orders BY customer ON").is_err());
        assert!(statement("SHARD TABLE orders BY customer ON 'a',").is_err());
        assert!(statement("SHOW SHARD orders").is_err());
        assert_eq!(
            statement("CREATE GLOBAL INDEX by_email ON users (email) ON 'postgresql://a@i1/db', 'postgresql://a@i2/db'").unwrap(),
            Some(ShardStatement::CreateIndex {
                name: "by_email".to_owned(),
                table: "users".to_owned(),
                column: "email".to_owned(),
                sources: vec!["postgresql://a@i1/db".to_owned(), "postgresql://a@i2/db".to_owned()],
            })
        );
        assert_eq!(
            statement("DROP GLOBAL INDEX by_email").unwrap(),
            Some(ShardStatement::DropIndex { name: "by_email".to_owned() })
        );
        assert_eq!(
            statement("SHOW SHARD FOR INDEX by_email VALUE 'ada@example.com'").unwrap(),
            Some(ShardStatement::ShowIndexShard { name: "by_email".to_owned(), value: "ada@example.com".to_owned() })
        );
        assert!(statement("CREATE GLOBAL INDEX by_email ON users email ON 'a'").is_err());
        assert_eq!(statement("SHOW search_path").unwrap(), None);
        assert_eq!(statement("SELECT 1").unwrap(), None);
    }
}
