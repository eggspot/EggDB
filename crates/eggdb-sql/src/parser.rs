mod constraints;
mod copy;
mod indexes;
mod merge;
mod mysql;
mod oracle;
mod plpgsql;
mod plsql;
mod plsql_primary;
mod plsql_routines;
mod plsql_triggers;
mod postgresql;
mod query;
mod routines;
mod schemas;
mod sqlite;
mod sqlserver;
mod sqlserver_batch;
mod sqlserver_functions;
mod sqlserver_logic;
mod sqlserver_routines;
mod triggers;
mod types;
mod windows;

pub use oracle::ORACLE_COMPATIBLE_VERSION;
pub(crate) use oracle::{is_reserved as is_oracle_reserved, oracle_table_name};
pub(crate) use plpgsql::plpgsql_program;
pub(crate) use plsql::{plsql_package_program, plsql_program};
pub use sqlserver::SQLSERVER_COMPATIBLE_VERSION;
pub(crate) use sqlserver::{
    is_reserved as is_sqlserver_reserved, kept_default as sqlserver_kept_default,
    not_supported as sqlserver_not_supported,
    parameter_declarations as sqlserver_parameter_declarations,
    procedure_routine_name as sqlserver_procedure_routine_name,
    starts_statement as sqlserver_starts_statement,
};

pub(crate) use schemas::Nesting;
pub use sqlite::SQLITE_COMPATIBLE_VERSION;
pub(crate) use sqlite::is_schema_table;

use crate::ast::{
    AggregateFunction, BinaryOperator, ColumnDefinition, CreateTable, Delete, DiscardTarget, Expr,
    ExprKind, FromClause, IndexSpec, Insert, InsertSource, InsertValue, Join, JoinCondition, JoinKind,
    Literal, Name, OnDuplicate, Overriding, Select, SelectItem, Statement, TableReference,
    TruthTest, UnaryOperator, Update,
};
use crate::dialect::Dialect;
use crate::encoding::{IndexMethod, SortOptions};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::interval::IntervalFields;
use crate::lexer::{Token, TokenKind, swap_uniform_case};
use crate::session::{NamedParameter, Parameter};
use crate::settings::Settings;
use crate::types::{NumericModifier, SqlType, Value};
use std::borrow::Cow;

const MAX_NESTING: usize = 64;
const MAX_JOINED_TABLES: usize = 64;

const AGGREGATES: &[&str] = &[
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "bool_and",
    "bool_or",
    "every",
    "string_agg",
    "array_agg",
    "json_agg",
    "jsonb_agg",
    "json_object_agg",
    "jsonb_object_agg",
    "stddev",
    "variance",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    Or,
    And,
    Not,
    Is,
    Comparison,
    Pattern,
    Other,
    Additive,
    Multiplicative,
    Power,
    Unary,
}

pub(crate) enum LiteralToken<'t> {
    Null,
    Boolean(bool),
    Integer(i64),
    Decimal(Cow<'t, str>),
    String(&'t str),
}

pub(crate) struct LiteralRows<'t> {
    pub(crate) table: TableReference,
    pub(crate) width: usize,
    pub(crate) rows: usize,
    tokens: &'t [Token<'t>],
}

impl<'t> LiteralRows<'t> {
    pub(crate) fn first_row(&self) -> usize {
        0
    }

    pub(crate) fn next_row(&self, at: &mut usize, row: &mut Vec<(usize, LiteralToken<'t>)>) {
        row.clear();
        *at += 1;
        while row.len() < self.width {
            let token = &self.tokens[*at];
            let (negative, number) = match &token.kind {
                TokenKind::Operator(_) => (true, &self.tokens[*at + 1]),
                _ => (false, token),
            };
            let value = match (&number.kind, negative) {
                (TokenKind::Integer(digits), false) => match digits.parse::<i64>() {
                    Ok(value) => LiteralToken::Integer(value),
                    Err(_) => LiteralToken::Decimal(Cow::Borrowed(digits)),
                },
                (TokenKind::Integer(digits), true) => match digits.parse::<i64>() {
                    Ok(value) => LiteralToken::Integer(-value),
                    Err(_) if *digits == "9223372036854775808" => LiteralToken::Integer(i64::MIN),
                    Err(_) => LiteralToken::Decimal(Cow::Owned(format!("-{digits}"))),
                },
                (TokenKind::Decimal(text), false) => LiteralToken::Decimal(Cow::Borrowed(text)),
                (TokenKind::Decimal(text), true) => {
                    LiteralToken::Decimal(Cow::Owned(format!("-{text}")))
                }
                (TokenKind::String(text), _) => LiteralToken::String(text),
                (TokenKind::Word(word), _) if word == "true" => LiteralToken::Boolean(true),
                (TokenKind::Word(word), _) if word == "false" => LiteralToken::Boolean(false),
                _ => LiteralToken::Null,
            };
            row.push((token.start, value));
            *at += if negative { 3 } else { 2 };
        }
        *at += 1;
    }
}

fn is_plain_literal(kind: &TokenKind<'_>) -> bool {
    matches!(
        kind,
        TokenKind::Integer(_) | TokenKind::Decimal(_) | TokenKind::String(_)
    ) || matches!(kind, TokenKind::Word(word) if word == "null" || word == "true" || word == "false")
}

const POSTGRESQL_ONLY_FUNCTIONS: [&str; 40] = [
    "set_config",
    "current_schemas",
    "nextval",
    "currval",
    "lastval",
    "setval",
    "current_time",
    "localtime",
    "justify_days",
    "justify_hours",
    "justify_interval",
    "age",
    "gen_random_uuid",
    "encode",
    "decode",
    "get_byte",
    "substring",
    "now",
    "transaction_timestamp",
    "statement_timestamp",
    "clock_timestamp",
    "localtimestamp",
    "current_date",
    "current_timestamp",
    "date_part",
    "date_trunc",
    "json_typeof",
    "jsonb_typeof",
    "json_array_length",
    "jsonb_array_length",
    "array_to_string",
    "array_length",
    "array_upper",
    "array_lower",
    "array_ndims",
    "cardinality",
    "generate_series",
    "unnest",
    "quote_ident",
    "btrim",
];

const POSTGRESQL_ONLY_SET_RETURNING: [&str; 3] = [
    "regexp_matches",
    "regexp_split_to_table",
    "generate_subscripts",
];

const POSTGRESQL_FORMAT_FUNCTIONS: [&str; 4] = ["to_char", "to_date", "to_timestamp", "to_number"];

fn is_postgresql_only(name: &str) -> bool {
    POSTGRESQL_ONLY_FUNCTIONS.contains(&name)
        || POSTGRESQL_ONLY_SET_RETURNING.contains(&name)
        || POSTGRESQL_FORMAT_FUNCTIONS.contains(&name)
        || crate::string_functions::FUNCTIONS.contains(&name)
        || crate::regexp_functions::FUNCTIONS.contains(&name)
        || crate::json_functions::FUNCTIONS.contains(&name)
        || crate::json_functions::SET_RETURNING.contains(&name)
        || crate::temporal::CONSTRUCTORS.contains(&name)
}

const NAMED_PARAMETERS: [(&str, &[&str]); 5] = [
    (
        "make_interval",
        &["years", "months", "weeks", "days", "hours", "mins", "secs"],
    ),
    ("make_date", &["year", "month", "day"]),
    ("make_time", &["hour", "min", "sec"]),
    (
        "make_timestamp",
        &["year", "month", "mday", "hour", "min", "sec"],
    ),
    (
        "make_timestamptz",
        &["year", "month", "mday", "hour", "min", "sec", "timezone"],
    ),
];

const INTERNAL_FUNCTIONS: [&str; 26] = [
    crate::triggers::RAISE_FUNCTION,
    crate::routines::ROUTINE_CALL,
    "regclass_cast",
    "oracle_nextval",
    "oracle_currval",
    "mysql_curtime",
    "mysql_greatest",
    "mysql_least",
    "mysql_now",
    "mysql_utc_now",
    "mysql_extract",
    "oracle_extract",
    "concat_or_null",
    "array_constructor",
    "array_element_cast",
    "array_subscript",
    "array_any",
    "array_all",
    "string_agg_arguments",
    "collate",
    "object_agg_arguments",
    "similar_substring_pattern",
    crate::json_functions::WHOLE_ROW,
    crate::json_functions::ROW_OBJECT,
    crate::aggregate::ORDERED_ARGUMENTS,
    crate::aggregate::FILTERED_ARGUMENTS,
    crate::aggregate::ORDERED_SET_ARGUMENTS,
    crate::window::WINDOW_CALL,
];

const NOT_IN_MYSQL: [&str; 1] = ["trunc"];

const NOT_IN_SQLITE: [&str; 6] = ["trunc", "ceil", "ceiling", "floor", "greatest", "least"];

pub(crate) struct Parser<'t> {
    text: &'t str,
    tokens: &'t [Token<'t>],
    at: usize,
    depth: usize,
    parameters: Parameters<'t>,
    settings: Option<&'t Settings>,
    dialect: Dialect,
    placeholders_seen: usize,
    query_depth: usize,
    queries: usize,
    setting_changes: Vec<(String, String)>,
    pending_top: Option<Expr>,
    view_query_depth: Option<usize>,
    ignored_view_top: bool,
    batch: Option<&'t crate::tsql::BatchContext>,
    declared_variables: Vec<crate::tsql::ScalarVariable>,
    declared_tables: Vec<String>,
    variables_read: Vec<String>,
    select_targets: Option<Vec<(crate::ast::AssignmentTarget, Vec<String>)>>,
    select_windows: Option<Vec<(String, crate::window::WindowDefinition)>>,
    trigger_frame: Option<&'t crate::triggers::TriggerFrame>,
    routine_frame: Option<&'t crate::plpgsql::RoutineFrame>,
    returns_values: bool,
    function_body: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum Parameters<'t> {
    Absent,
    Values(&'t [Parameter]),
    Types(&'t [Option<SqlType>]),
    Named(&'t [NamedParameter]),
}

const MAX_PARAMETER_NUMBER: u32 = 65535;

pub(crate) fn parameter_number(digits: &str) -> Option<usize> {
    digits
        .parse::<u32>()
        .ok()
        .filter(|number| (1..=MAX_PARAMETER_NUMBER).contains(number))
        .map(|number| number as usize)
}

pub(crate) fn is_reserved(word: &str) -> bool {
    matches!(
        word,
        "all"
            | "and"
            | "any"
            | "array"
            | "as"
            | "asc"
            | "both"
            | "case"
            | "cast"
            | "check"
            | "collate"
            | "column"
            | "constraint"
            | "create"
            | "current_date"
            | "current_time"
            | "current_timestamp"
            | "default"
            | "desc"
            | "distinct"
            | "do"
            | "else"
            | "end"
            | "except"
            | "false"
            | "fetch"
            | "for"
            | "foreign"
            | "from"
            | "grant"
            | "group"
            | "having"
            | "in"
            | "initially"
            | "intersect"
            | "into"
            | "lateral"
            | "leading"
            | "limit"
            | "localtime"
            | "localtimestamp"
            | "not"
            | "null"
            | "offset"
            | "on"
            | "only"
            | "or"
            | "order"
            | "placing"
            | "primary"
            | "references"
            | "returning"
            | "select"
            | "session_user"
            | "some"
            | "symmetric"
            | "table"
            | "then"
            | "to"
            | "trailing"
            | "true"
            | "union"
            | "unique"
            | "user"
            | "using"
            | "variadic"
            | "when"
            | "where"
            | "window"
            | "with"
    )
}

impl<'t> Parser<'t> {
    pub(crate) fn new(text: &'t str, tokens: &'t [Token]) -> Self {
        Self {
            text,
            tokens,
            at: 0,
            depth: 0,
            parameters: Parameters::Absent,
            settings: None,
            dialect: Dialect::PostgreSql,
            placeholders_seen: 0,
            query_depth: 0,
            queries: 0,
            setting_changes: Vec::new(),
            pending_top: None,
            view_query_depth: None,
            ignored_view_top: false,
            batch: None,
            declared_variables: Vec::new(),
            declared_tables: Vec::new(),
            variables_read: Vec::new(),
            select_targets: None,
            select_windows: None,
            trigger_frame: None,
            routine_frame: None,
            returns_values: false,
            function_body: false,
        }
    }

    pub(crate) fn returning_values(mut self) -> Self {
        self.returns_values = true;
        self
    }

    pub(crate) fn with_batch_context(mut self, batch: &'t crate::tsql::BatchContext) -> Self {
        self.batch = Some(batch);
        self
    }

    pub(crate) fn take_setting_changes(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.setting_changes)
    }

    pub(crate) fn in_dialect(mut self, dialect: Dialect) -> Self {
        self.dialect = dialect;
        self
    }

    fn mysql(&self) -> bool {
        self.dialect == Dialect::MySql
    }

    fn sqlite(&self) -> bool {
        self.dialect == Dialect::Sqlite
    }

    fn oracle(&self) -> bool {
        self.dialect == Dialect::Oracle
    }

    fn sqlserver(&self) -> bool {
        self.dialect == Dialect::SqlServer
    }

    fn reserved(&self, word: &str) -> bool {
        match self.dialect {
            Dialect::PostgreSql => is_reserved(word),
            Dialect::MySql => mysql::is_reserved(word),
            Dialect::Sqlite => sqlite::is_reserved(word),
            Dialect::Oracle => oracle::is_reserved(word),
            Dialect::SqlServer => sqlserver::is_reserved(word),
        }
    }

    fn clause_word(&self, word: &str) -> bool {
        is_clause_word(word)
            || self.mysql() && mysql::is_clause_word(word)
            || self.sqlite() && sqlite::is_clause_word(word)
            || self.oracle() && oracle::is_clause_word(word)
            || self.sqlserver() && word == "output"
    }

    pub(crate) fn with_context(
        mut self,
        parameters: Parameters<'t>,
        settings: &'t Settings,
    ) -> Self {
        self.parameters = parameters;
        self.settings = Some(settings);
        self
    }

    pub(crate) fn resuming_at(mut self, (at, placeholders_seen): (usize, usize)) -> Self {
        self.at = at;
        self.placeholders_seen = placeholders_seen;
        self
    }

    pub(crate) fn resume_point(&self) -> (usize, usize) {
        (self.at, self.placeholders_seen)
    }

    pub(crate) fn skip_empty_statements(&mut self) {
        while self.peek_kind() == &TokenKind::Semicolon {
            self.at += 1;
        }
    }

    pub(crate) fn at_end(&self) -> bool {
        self.peek_kind() == &TokenKind::End
    }

    pub(crate) fn position(&self) -> usize {
        self.peek().start
    }

    pub(crate) fn literal_rows(&mut self) -> Option<LiteralRows<'t>> {
        if self.oracle()
            || self.sqlserver()
            || !self.is_word("insert")
            || !self.is_word_at(1, "into")
        {
            return None;
        }
        let start = self.at;
        self.at += 2;
        let found = self.literal_rows_after_into();
        if found.is_none() {
            self.at = start;
        }
        found
    }

    fn literal_rows_after_into(&mut self) -> Option<LiteralRows<'t>> {
        let table = self.table_reference().ok()?;
        if table.alias.is_some() || !self.is_word("values") {
            return None;
        }
        let first = self.at + 1;
        let tokens = self.tokens;
        let mut at = first;
        let mut width = None;
        let mut rows = 0;
        loop {
            if tokens.get(at)?.kind != TokenKind::LeftParen {
                return None;
            }
            at += 1;
            let mut count = 0;
            loop {
                if matches!(tokens.get(at)?.kind, TokenKind::Operator("-"))
                    && matches!(
                        tokens.get(at + 1)?.kind,
                        TokenKind::Integer(_) | TokenKind::Decimal(_)
                    )
                    && !(self.sqlite() && matches!(tokens.get(at + 1)?.kind, TokenKind::Decimal(_)))
                {
                    at += 2;
                } else if is_plain_literal(&tokens.get(at)?.kind)
                    && !(self.sqlite()
                        && matches!(
                            tokens.get(at)?.kind,
                            TokenKind::Word(_) | TokenKind::Decimal(_)
                        ))
                {
                    at += 1;
                } else {
                    return None;
                }
                count += 1;
                match tokens.get(at)?.kind {
                    TokenKind::Comma => at += 1,
                    TokenKind::RightParen => {
                        at += 1;
                        break;
                    }
                    _ => return None,
                }
            }
            if width.is_some_and(|width| width != count) {
                return None;
            }
            width = Some(count);
            rows += 1;
            match tokens.get(at)?.kind {
                TokenKind::Comma => at += 1,
                TokenKind::Semicolon => {
                    self.at = at + 1;
                    break;
                }
                TokenKind::End => {
                    self.at = at;
                    break;
                }
                _ => return None,
            }
        }
        Some(LiteralRows {
            table,
            width: width?,
            rows,
            tokens: &tokens[first..],
        })
    }

    pub(crate) fn statement(&mut self) -> SqlResult<Statement> {
        let statement = self.statement_body()?;
        if self.oracle() && self.ended_plsql_unit(&statement) {
            return Ok(statement);
        }
        match self.peek_kind() {
            TokenKind::Semicolon => {
                self.at += 1;
                Ok(statement)
            }
            TokenKind::End => Ok(statement),
            TokenKind::Word(word)
                if self.sqlserver()
                    && (sqlserver::starts_statement(word) || sqlserver::ends_a_block(word)) =>
            {
                Ok(statement)
            }
            _ => Err(self.unexpected()),
        }
    }

    fn peek(&self) -> &'t Token<'t> {
        let index = self.at.min(self.tokens.len().saturating_sub(1));
        &self.tokens[index]
    }

    fn peek_kind(&self) -> &'t TokenKind<'t> {
        &self.peek().kind
    }

    fn peek_kind_at(&self, ahead: usize) -> &'t TokenKind<'t> {
        let index = (self.at + ahead).min(self.tokens.len().saturating_sub(1));
        &self.tokens[index].kind
    }

    fn is_word(&self, word: &str) -> bool {
        matches!(self.peek_kind(), TokenKind::Word(found) if found == word)
    }

    fn is_word_at(&self, ahead: usize, word: &str) -> bool {
        matches!(self.peek_kind_at(ahead), TokenKind::Word(found) if found == word)
    }

    fn take_word(&mut self, word: &str) -> bool {
        if self.is_word(word) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect_word(&mut self, word: &str) -> SqlResult<()> {
        if self.take_word(word) {
            Ok(())
        } else {
            Err(self.unexpected())
        }
    }

    fn take(&mut self, kind: &TokenKind) -> bool {
        if self.peek_kind() == kind {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: &TokenKind) -> SqlResult<()> {
        if self.take(kind) {
            Ok(())
        } else {
            Err(self.unexpected())
        }
    }

    fn is_operator(&self, symbol: &str) -> bool {
        matches!(self.peek_kind(), TokenKind::Operator(found) if *found == symbol)
    }

    fn take_operator(&mut self, symbol: &str) -> bool {
        if self.is_operator(symbol) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    pub(crate) fn unexpected(&self) -> SqlError {
        let token = self.peek();
        if token.kind == TokenKind::End {
            return SqlError::syntax("syntax error at end of input", token.start);
        }
        let text = self.text.get(token.start..token.end).unwrap_or("");
        SqlError::syntax(format!("syntax error at or near \"{text}\""), token.start)
    }

    fn not_supported(&self, what: &str) -> SqlError {
        SqlError::not_supported(format!("{what} is not supported yet")).at(self.position())
    }

    fn name(&mut self) -> SqlResult<Name> {
        let token = self.peek();
        let text = match &token.kind {
            TokenKind::Word(word) if !self.reserved(word) => word.as_ref().to_owned(),
            TokenKind::QuotedIdentifier(name) if name.contains(crate::schemas::QUALIFIER) => {
                return Err(SqlError::new(
                    SqlState::INVALID_NAME,
                    format!("invalid name \"{}\"", name.escape_default()),
                )
                .at(token.start));
            }
            TokenKind::QuotedIdentifier(name) => name.clone(),
            _ => return Err(self.unexpected()),
        };
        self.at += 1;
        Ok(Name {
            text,
            position: token.start,
        })
    }

    fn table_name(&mut self) -> SqlResult<Name> {
        let name = self.name()?;
        if self.mysql() && self.peek_kind() == &TokenKind::Dot {
            self.at += 1;
            let table = self.name()?;
            if name.text.eq_ignore_ascii_case(crate::schemas::INFORMATION_SCHEMA) {
                return Ok(Name {
                    text: crate::schemas::written_qualified(
                        crate::schemas::INFORMATION_SCHEMA,
                        &table.text.to_ascii_lowercase(),
                    ),
                    position: name.position,
                });
            }
            self.check_database(&name)?;
            return Ok(table);
        }
        if self.sqlite() && self.peek_kind() == &TokenKind::Dot {
            self.at += 1;
            let table = self.name()?;
            if name.text == "temp" {
                return Ok(Name {
                    text: crate::temporary::internal_name(&table.text),
                    position: name.position,
                });
            }
            if name.text != "main" {
                return Err(SqlError::new(
                    SqlState::INVALID_CATALOG_NAME,
                    format!("unknown database {}", name.text),
                )
                .at(name.position));
            }
            return Ok(table);
        }
        if self.oracle() && self.peek_kind() == &TokenKind::Dot {
            self.at += 1;
            let table = self.name()?;
            let own = swap_uniform_case(&name.text) == self.oracle_user();
            if oracle::is_dictionary_name(&table.text) {
                if name.text == "sys" {
                    return Ok(Name {
                        text: crate::schemas::written_qualified("sys", &table.text),
                        position: table.position,
                    });
                }
                if own {
                    return Ok(table);
                }
            }
            let schema = if own {
                self.oracle_user_schema()
            } else {
                name.text.clone()
            };
            return Ok(Name {
                text: crate::schemas::written_qualified(&schema, &table.text),
                position: table.position,
            });
        }
        if self.sqlserver() && self.peek_kind() == &TokenKind::Dot {
            self.at += 1;
            return self.sqlserver_qualified_name(name);
        }
        if self.peek_kind() == &TokenKind::Dot {
            self.at += 1;
            return self.qualified_part(&name);
        }
        Ok(name)
    }

    fn statement_body(&mut self) -> SqlResult<Statement> {
        if let Some(statement) = self.trigger_statement()? {
            return Ok(statement);
        }
        if self.postgresql()
            && let Some(statement) = self.routine_statement()?
        {
            return Ok(statement);
        }
        if self.mysql()
            && let Some(statement) = self.mysql_statement()?
        {
            return Ok(statement);
        }
        if self.sqlite()
            && let Some(statement) = self.sqlite_statement()?
        {
            return Ok(statement);
        }
        if self.oracle()
            && let Some(statement) = self.oracle_statement()?
        {
            return Ok(statement);
        }
        if self.sqlserver()
            && let Some(statement) = self.sqlserver_statement()?
        {
            return Ok(statement);
        }
        if !self.sqlite() && self.peek_kind() == &TokenKind::LeftParen {
            return self.query_statement();
        }
        let TokenKind::Word(word) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        match word.as_ref() {
            "create" => self.create(),
            "drop" => self.drop(),
            "insert" => self.insert(),
            "select" | "with" => self.query_statement(),
            "update" => self.update(),
            "delete" => self.delete(),
            "begin" | "start" => self.begin(),
            "commit" | "end" => {
                self.at += 1;
                self.transaction_noise();
                Ok(Statement::Commit)
            }
            "rollback" | "abort" => {
                self.at += 1;
                self.transaction_noise();
                if self.is_word("to") {
                    return Err(self.not_supported("ROLLBACK TO SAVEPOINT"));
                }
                Ok(Statement::Rollback)
            }
            "values" if self.postgresql() => self.query_statement(),
            "values" | "table" => Err(self.not_supported("a VALUES or TABLE statement")),
            "set" => self.set(),
            "show" => self.show(),
            "reset" => self.reset(),
            "explain" => self.explain(),
            "alter" if self.is_word_at(1, "table") => {
                self.at += 1;
                self.alter_table()
            }
            "alter" if self.postgresql() && self.is_word_at(1, "sequence") => {
                self.at += 1;
                self.alter_sequence()
            }
            "alter" if self.postgresql() && self.is_word_at(1, "schema") => {
                self.at += 1;
                self.alter_schema()
            }
            "alter" if self.postgresql() && self.is_word_at(1, "view") => {
                self.at += 1;
                self.alter_view()
            }
            "alter" if self.postgresql() && self.is_word_at(1, "type") => {
                self.at += 1;
                self.alter_type()
            }
            "alter" if self.postgresql() && self.is_word_at(1, "domain") => {
                self.at += 1;
                self.alter_domain()
            }
            "alter"
                if self.postgresql()
                    && self.is_word_at(1, "default")
                    && self.is_word_at(2, "privileges") =>
            {
                self.grant_or_revoke("ALTER DEFAULT PRIVILEGES")
            }
            "grant" if self.postgresql() => self.grant_or_revoke("GRANT"),
            "revoke" if self.postgresql() => self.grant_or_revoke("REVOKE"),
            "comment" if self.postgresql() || self.oracle() => self.comment(),
            "copy" if self.postgresql() || self.oracle() => self.copy(),
            "discard" if self.postgresql() => self.discard(),
            "alter" | "truncate" | "savepoint" | "release" | "copy" | "grant" | "revoke"
            | "prepare" | "execute" | "analyze" | "vacuum" | "discard" | "listen" | "notify"
            | "unlisten" | "deallocate" => Err(self.not_supported(&word.to_ascii_uppercase())),
            _ => Err(self.unexpected()),
        }
    }

    fn parameter_name(&mut self) -> SqlResult<String> {
        let mut name = match self.peek_kind() {
            TokenKind::Word(word) => word.as_ref().to_owned(),
            TokenKind::QuotedIdentifier(name) => name.clone(),
            _ => return Err(self.unexpected()),
        };
        self.at += 1;
        while self.take(&TokenKind::Dot) {
            let part = match self.peek_kind() {
                TokenKind::Word(word) => word.as_ref().to_owned(),
                TokenKind::QuotedIdentifier(name) => name.clone(),
                _ => return Err(self.unexpected()),
            };
            self.at += 1;
            name.push('.');
            name.push_str(&part);
        }
        Ok(name)
    }

    fn setting_value(&mut self) -> SqlResult<Option<String>> {
        if self.take_word("default") {
            return Ok(None);
        }
        let mut parts = Vec::new();
        loop {
            let negative = self.take_operator("-");
            let part = match self.peek_kind() {
                TokenKind::Word(word) => word.as_ref().to_owned(),
                TokenKind::QuotedIdentifier(name) => name.clone(),
                TokenKind::String(text) => text.as_ref().to_owned(),
                TokenKind::Integer(digits) | TokenKind::Decimal(digits) => (*digits).to_owned(),
                _ => return Err(self.unexpected()),
            };
            self.at += 1;
            parts.push(if negative { format!("-{part}") } else { part });
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        Ok(Some(parts.join(", ")))
    }

    fn set(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        if self.is_word("local") {
            return Err(self.not_supported("SET LOCAL"));
        }
        let _ = self.take_word("session");
        if self.is_word("characteristics") || self.is_word("transaction") {
            return self.set_transaction();
        }
        if self.is_word("role") {
            return self.set_role();
        }
        if self.postgresql() && self.is_word("constraints") {
            return self.set_constraints();
        }
        if self.is_word("authorization") || self.is_word("constraints") {
            return Err(self.not_supported(&format!(
                "SET {}",
                self.text
                    .get(self.peek().start..self.peek().end)
                    .unwrap_or("")
                    .to_ascii_uppercase()
            )));
        }
        let name = if self.is_word("time") && self.is_word_at(1, "zone") {
            self.at += 2;
            if self.take_word("local") {
                return Ok(Statement::Set {
                    name: "TimeZone".into(),
                    value: None,
                });
            }
            return Ok(Statement::Set {
                name: "TimeZone".into(),
                value: self.setting_value()?,
            });
        } else if self.take_word("names") {
            "client_encoding".to_owned()
        } else {
            let name = self.parameter_name()?;
            if !self.take_word("to") && !self.take_operator("=") {
                return Err(self.unexpected());
            }
            name
        };
        Ok(Statement::Set {
            name,
            value: self.setting_value()?,
        })
    }

    fn set_role(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        let position = self.position();
        let role = match self.peek_kind() {
            TokenKind::String(text) => {
                let text = text.as_ref().to_owned();
                self.at += 1;
                text
            }
            TokenKind::Word(word) if word == "none" => {
                self.at += 1;
                return Ok(Statement::Ignored {
                    tag: "SET",
                    notice: None,
                });
            }
            _ => self.name()?.text,
        };
        let user = self
            .settings
            .and_then(|settings| settings.get("session_authorization").ok());
        if user.is_some_and(|user| user != role) {
            return Err(SqlError::not_supported(
                "SET ROLE to a role other than the session's user is not supported yet",
            )
            .at(position));
        }
        Ok(Statement::Ignored {
            tag: "SET",
            notice: None,
        })
    }

    fn set_transaction(&mut self) -> SqlResult<Statement> {
        let characteristics = self.take_word("characteristics");
        if characteristics {
            self.expect_word("as")?;
        }
        self.expect_word("transaction")?;
        if self.is_word("snapshot") {
            return Err(self.not_supported("SET TRANSACTION SNAPSHOT"));
        }
        let mut read_only = None;
        loop {
            if self.take_word("isolation") {
                self.isolation_level()?;
            } else if self.take_word("read") {
                if self.take_word("only") {
                    read_only = Some(true);
                } else {
                    self.expect_word("write")?;
                    read_only = Some(false);
                }
            } else if self.take_word("not") {
                self.expect_word("deferrable")?;
            } else if !self.take_word("deferrable") {
                break;
            }
            self.take(&TokenKind::Comma);
        }
        match (characteristics, read_only) {
            (true, Some(read_only)) => Ok(Statement::Set {
                name: "default_transaction_read_only".to_owned(),
                value: Some(if read_only { "on" } else { "off" }.to_owned()),
            }),
            (false, Some(true)) => Err(self.not_supported("SET TRANSACTION READ ONLY")),
            _ => Ok(Statement::Ignored {
                tag: "SET",
                notice: None,
            }),
        }
    }

    fn isolation_level(&mut self) -> SqlResult<()> {
        self.expect_word("level")?;
        if self.take_word("serializable") {
            return Ok(());
        }
        if self.take_word("repeatable") {
            return self.expect_word("read");
        }
        if self.take_word("read") && (self.take_word("committed") || self.take_word("uncommitted"))
        {
            return Ok(());
        }
        Err(self.unexpected())
    }

    fn show(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        if self.take_word("all") {
            return Ok(Statement::Show { name: None });
        }
        let name = if self.is_word("time") && self.is_word_at(1, "zone") {
            self.at += 2;
            "TimeZone".to_owned()
        } else if self.is_word("transaction") && self.is_word_at(1, "isolation") {
            self.at += 2;
            self.expect_word("level")?;
            "transaction_isolation".to_owned()
        } else if self.is_word("session") && self.is_word_at(1, "authorization") {
            self.at += 2;
            "session_authorization".to_owned()
        } else {
            self.parameter_name()?
        };
        Ok(Statement::Show { name: Some(name) })
    }

    fn discard(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        let target = if self.take_word("all") {
            DiscardTarget::All
        } else if self.take_word("plans") {
            DiscardTarget::Plans
        } else if self.take_word("sequences") {
            DiscardTarget::Sequences
        } else if self.take_word("temp") || self.take_word("temporary") {
            DiscardTarget::Temporary
        } else {
            return Err(self.unexpected());
        };
        Ok(Statement::Discard(target))
    }

    fn reset(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        if self.take_word("all") {
            return Ok(Statement::Reset { name: None });
        }
        if self.take_word("role") {
            return Ok(Statement::Ignored {
                tag: "RESET",
                notice: None,
            });
        }
        let name = if self.is_word("time") && self.is_word_at(1, "zone") {
            self.at += 2;
            "TimeZone".to_owned()
        } else {
            self.parameter_name()?
        };
        Ok(Statement::Reset { name: Some(name) })
    }

    fn parameter(&mut self, digits: &str, position: usize) -> SqlResult<Expr> {
        self.at += 1;
        let missing = || {
            SqlError::new(
                SqlState::UNDEFINED_PARAMETER,
                format!("there is no parameter ${digits}"),
            )
            .at(position)
        };
        let number = parameter_number(digits).ok_or_else(missing)?;
        let (declared_type, value) = match self.parameters {
            Parameters::Absent | Parameters::Named(_) => return Err(missing()),
            Parameters::Values(values) => {
                let parameter = values.get(number - 1).ok_or_else(missing)?;
                (parameter.declared_type, parameter.value.clone())
            }
            Parameters::Types(types) => (types.get(number - 1).copied().flatten(), None),
        };
        let literal = Expr {
            kind: ExprKind::Literal(match value {
                Some(text) => Literal::String(text),
                None => Literal::Null,
            }),
            position,
        };
        Ok(match (declared_type, literal.kind) {
            (Some(SqlType::Bytea), ExprKind::Literal(Literal::String(text))) => Expr {
                kind: ExprKind::Constant {
                    value: Value::Bytea(
                        crate::bytea::parse(&text).map_err(|error| error.at(position))?,
                    ),
                    sql_type: Some(SqlType::Bytea),
                },
                position,
            },
            (Some(sql_type), kind) => Expr {
                kind: ExprKind::Cast {
                    expr: Box::new(Expr { kind, position }),
                    sql_type,
                },
                position,
            },
            (None, kind) => Expr { kind, position },
        })
    }

    fn session_value(&mut self, name: &'static str, position: usize) -> SqlResult<Expr> {
        let defaults;
        let settings = match self.settings {
            Some(settings) => settings,
            None => {
                defaults = Settings::default();
                &defaults
            }
        };
        let value = match name {
            "version" => format!(
                "PostgreSQL {} compatible (EggDB {})",
                crate::settings::POSTGRESQL_COMPATIBLE_VERSION,
                env!("CARGO_PKG_VERSION")
            ),
            "current_database" | "current_catalog" => settings.database.clone(),
            "current_schema" => "public".to_owned(),
            _ => settings.get("session_authorization")?,
        };
        Ok(Expr {
            kind: ExprKind::SessionValue { name, value },
            position,
        })
    }

    fn set_config_argument(&mut self) -> SqlResult<Option<String>> {
        let refused = |parser: &Self| {
            parser.not_supported("set_config with arguments other than constants and parameters")
        };
        let value = match self.peek_kind() {
            TokenKind::String(text) => Some(text.as_ref().to_owned()),
            TokenKind::Parameter(digits) if !self.mysql() && !self.sqlite() => {
                let number = parameter_number(digits).ok_or_else(|| refused(self))?;
                match self.parameters {
                    Parameters::Values(values) => Some(
                        values
                            .get(number - 1)
                            .and_then(|parameter| parameter.value.clone())
                            .ok_or_else(|| refused(self))?,
                    ),
                    Parameters::Types(_) => None,
                    Parameters::Absent | Parameters::Named(_) => return Err(refused(self)),
                }
            }
            _ => return Err(refused(self)),
        };
        self.at += 1;
        Ok(value)
    }

    fn set_config(&mut self, position: usize) -> SqlResult<Expr> {
        let setting = self.set_config_argument()?;
        self.expect(&TokenKind::Comma)?;
        let value = self.set_config_argument()?;
        self.expect(&TokenKind::Comma)?;
        let local = match self.peek_kind() {
            TokenKind::Word(word) if word == "true" => true,
            TokenKind::Word(word) if word == "false" => false,
            _ => {
                return Err(
                    self.not_supported("set_config with arguments other than constants and parameters")
                );
            }
        };
        self.at += 1;
        self.expect(&TokenKind::RightParen)?;
        if local {
            return Err(SqlError::not_supported(
                "set_config with is_local true (SET LOCAL) is not supported yet",
            )
            .at(position));
        }
        if let (Some(setting), Some(value)) = (setting, &value) {
            self.setting_changes.push((setting, value.clone()));
        }
        Ok(Expr {
            kind: ExprKind::SessionValue {
                name: "set_config",
                value: value.unwrap_or_default(),
            },
            position,
        })
    }

    fn session_function(&mut self, name: &Name) -> SqlResult<Option<Expr>> {
        let known: &'static str = match name.text.as_str() {
            "version" => "version",
            "current_database" => "current_database",
            "current_schema" => "current_schema",
            "current_user" => "current_user",
            "session_user" => "session_user",
            "current_setting" => "current_setting",
            "current_schemas" if self.postgresql() => "current_schemas",
            "set_config" if self.postgresql() => "set_config",
            _ => return Ok(None),
        };
        self.expect(&TokenKind::LeftParen)?;
        if known == "current_schemas" {
            let implicit = match self.peek_kind() {
                TokenKind::Word(word) if word == "true" => "true",
                TokenKind::Word(word) if word == "false" => "false",
                _ => {
                    return Err(self.not_supported(
                        "current_schemas with an argument other than true or false",
                    ));
                }
            };
            self.at += 1;
            self.expect(&TokenKind::RightParen)?;
            return Ok(Some(Expr {
                kind: ExprKind::SessionValue {
                    name: "current_schemas",
                    value: implicit.to_owned(),
                },
                position: name.position,
            }));
        }
        if known == "set_config" {
            return self.set_config(name.position).map(Some);
        }
        if known == "current_setting" {
            let TokenKind::String(setting) = self.peek_kind() else {
                return Err(
                    self.not_supported("current_setting with an argument other than a constant")
                );
            };
            self.at += 1;
            if self.take(&TokenKind::Comma) {
                return Err(self.not_supported("current_setting with missing_ok"));
            }
            self.expect(&TokenKind::RightParen)?;
            let defaults;
            let settings = match self.settings {
                Some(settings) => settings,
                None => {
                    defaults = Settings::default();
                    &defaults
                }
            };
            let value = settings.get(setting)?;
            return Ok(Some(Expr {
                kind: ExprKind::SessionValue {
                    name: "current_setting",
                    value,
                },
                position: name.position,
            }));
        }
        self.expect(&TokenKind::RightParen)?;
        self.session_value(known, name.position).map(Some)
    }

    fn transaction_noise(&mut self) {
        let _ = self.take_word("work") || self.take_word("transaction");
    }

    fn begin(&mut self) -> SqlResult<Statement> {
        let start = self.is_word("start");
        self.at += 1;
        if start {
            self.expect_word("transaction")?;
        } else {
            self.transaction_noise();
        }
        let mut read_only = false;
        loop {
            if self.take_word("read") {
                if self.take_word("only") {
                    read_only = true;
                } else {
                    self.expect_word("write")?;
                    read_only = false;
                }
            } else if self.take_word("isolation") {
                self.isolation_level()?;
            } else if self.is_word("deferrable") || self.is_word("not") {
                return Err(self.not_supported("DEFERRABLE"));
            } else {
                break;
            }
            self.take(&TokenKind::Comma);
        }
        Ok(Statement::Begin { read_only })
    }

    fn create(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        if self.sqlserver()
            && let Some(refused) = self.sqlserver_special_index()
        {
            return Err(refused);
        }
        if self.at_index_definition() {
            return self.create_index();
        }
        if (self.postgresql() || self.sqlserver()) && self.is_word("schema") {
            return self.create_schema();
        }
        if self.postgresql() && self.is_word("extension") {
            return self.create_extension();
        }
        if self.is_word("or") && self.sqlite() {
            return Err(self.unexpected());
        }
        if self.is_word("or") && self.is_word_at(1, "replace") {
            self.at += 2;
            return self.create_view(true);
        }
        if self.is_word("view")
            || (self.mysql()
                && (self.is_word("algorithm") || self.is_word("definer") || self.is_word("sql")))
            || (self.oracle()
                && (self.is_word("force")
                    || (self.is_word("no") && self.is_word_at(1, "force"))
                    || self.is_word("editioning")
                    || self.is_word("editionable")
                    || self.is_word("noneditionable")))
            || ((self.is_word("temp") || self.is_word("temporary") || self.is_word("recursive"))
                && self.is_word_at(1, "view"))
        {
            return self.create_view(false);
        }
        if self.postgresql() && self.is_word("sequence") {
            return self.create_sequence();
        }
        if self.postgresql() && self.is_word("type") {
            return self.create_type();
        }
        if self.postgresql() && self.is_word("domain") {
            return self.create_domain();
        }
        if self.postgresql() && self.is_word("unlogged") && self.is_word_at(1, "table") {
            self.at += 1;
        }
        if self.postgresql()
            && (self.is_word("local") || self.is_word("global"))
            && (self.is_word_at(1, "temp") || self.is_word_at(1, "temporary"))
        {
            self.at += 1;
        }
        let mut temporary = None;
        if (self.postgresql() || self.sqlite() || self.mysql())
            && (self.is_word("temp") || self.is_word("temporary"))
            && self.is_word_at(1, "table")
        {
            self.at += 1;
            temporary = Some(crate::ast::OnCommit::PreserveRows);
        }
        if self.oracle() && self.is_word("global") && self.is_word_at(1, "temporary") {
            return Err(self.not_supported("CREATE GLOBAL TEMPORARY TABLE"));
        }
        if self.is_word("temp") || self.is_word("temporary") || self.is_word("unlogged") {
            return Err(self.not_supported("CREATE TEMPORARY or UNLOGGED TABLE"));
        }
        if !self.take_word("table") {
            if matches!(self.peek_kind(), TokenKind::Word(_)) {
                let TokenKind::Word(word) = self.peek_kind() else {
                    return Err(self.unexpected());
                };
                if matches!(
                    word.as_ref(),
                    "unique"
                        | "bitmap"
                        | "fulltext"
                        | "spatial"
                        | "sequence"
                        | "schema"
                        | "domain"
                        | "function"
                        | "type"
                        | "trigger"
                        | "database"
                        | "extension"
                        | "or"
                        | "materialized"
                ) {
                    return Err(
                        self.not_supported(&format!("CREATE {}", word.to_ascii_uppercase()))
                    );
                }
            }
            return Err(self.unexpected());
        }
        let if_not_exists = if self.take_word("if") {
            self.expect_word("not")?;
            self.expect_word("exists")?;
            true
        } else {
            false
        };
        let name = self.table_name()?;
        self.expect(&TokenKind::LeftParen)?;
        let mut columns = Vec::new();
        let mut primary_key = None;
        let mut primary_key_name = None;
        let mut indexes = Vec::new();
        let mut constraints = Vec::new();
        let mut integer_columns = Vec::new();
        loop {
            if self.is_word("primary") || self.is_word("constraint") {
                let position = self.position();
                let constraint = if self.take_word("constraint") {
                    if self.mysql()
                        && (self.is_word("primary")
                            || self.at_table_constraint()
                            || self.is_word("unique"))
                    {
                        None
                    } else {
                        Some(self.name()?)
                    }
                } else {
                    None
                };
                if self.is_word("unique") {
                    indexes.push(self.unique_constraint(constraint, position)?);
                    self.oracle_enabled_constraint()?;
                } else if self.take_word("primary") {
                    self.expect_word("key")?;
                    let key = if self.sqlserver() {
                        self.sqlserver_key_columns()?
                    } else {
                        self.name_list()?
                    };
                    if primary_key.is_some() {
                        return Err(multiple_primary_keys(&name, position));
                    }
                    primary_key = Some((key, position));
                    primary_key_name = constraint;
                    if self.sqlite() {
                        self.sqlite_conflict_clause()?;
                    }
                    self.oracle_enabled_constraint()?;
                } else if self.at_table_constraint() {
                    constraints.push(self.table_constraint_body(constraint, position)?);
                } else {
                    return Err(self.table_constraint_not_supported());
                }
            } else if self.at_table_constraint() {
                let position = self.position();
                constraints.push(self.table_constraint_body(None, position)?);
            } else if self.is_word("unique") {
                let position = self.position();
                indexes.push(self.unique_constraint(None, position)?);
                self.oracle_enabled_constraint()?;
            } else if self.mysql()
                && let Some(index) = self.mysql_index_entry()?
            {
                indexes.push(index);
            } else if self.is_word("exclude") || self.is_word("like") {
                return Err(self.table_constraint_not_supported());
            } else if self.sqlserver()
                && let Some(refused) = self.sqlserver_table_element_refused()
            {
                return Err(refused);
            } else if self.sqlite() {
                let (column, exact_integer) = self.sqlite_column_definition()?;
                integer_columns.push(exact_integer);
                columns.push(column);
            } else {
                columns.push(self.column_definition()?);
            }
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen)?;
        let comment = if self.mysql() {
            self.mysql_table_options()?
        } else {
            None
        };
        if self.sqlite() {
            self.sqlite_table_finish(&name, &mut columns, &primary_key, &integer_columns)?;
        }
        if self.sqlserver() {
            self.sqlserver_table_tail()?;
        }
        if self.oracle() {
            self.skip_physical_properties()?;
        }
        if self.postgresql() && self.is_word("on") && self.is_word_at(1, "commit") {
            let position = self.position();
            self.at += 2;
            let action = if self.take_word("preserve") {
                self.expect_word("rows")?;
                crate::ast::OnCommit::PreserveRows
            } else if self.take_word("delete") {
                self.expect_word("rows")?;
                crate::ast::OnCommit::DeleteRows
            } else {
                self.expect_word("drop")?;
                crate::ast::OnCommit::Drop
            };
            if temporary.is_none() {
                return Err(SqlError::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    "ON COMMIT can only be used on temporary tables",
                )
                .at(position));
            }
            temporary = Some(action);
        }
        if self.sqlserver() && crate::temporary::is_sqlserver_temporary(&name.text) {
            temporary = Some(crate::ast::OnCommit::PreserveRows);
        }
        if self.is_word("partition") || self.is_word("inherits") || self.is_word("with") {
            return Err(self.not_supported("table options"));
        }
        Ok(Statement::CreateTable(CreateTable {
            name,
            temporary,
            if_not_exists,
            columns,
            primary_key,
            primary_key_name,
            indexes,
            constraints,
            comment,
        }))
    }

    fn oracle_enabled_constraint(&mut self) -> SqlResult<()> {
        if self.oracle() && !self.oracle_constraint_states()? {
            return Err(self.not_supported("disabled constraints"));
        }
        Ok(())
    }

    fn table_constraint_not_supported(&self) -> SqlError {
        self.not_supported(
            "a table constraint other than PRIMARY KEY, UNIQUE, CHECK and FOREIGN KEY",
        )
    }

    fn unique_constraint(
        &mut self,
        constraint: Option<Name>,
        position: usize,
    ) -> SqlResult<IndexSpec> {
        self.expect_word("unique")?;
        let mut name = constraint;
        if self.mysql() {
            let _ = self.take_word("key") || self.take_word("index");
            if self.peek_kind() != &TokenKind::LeftParen && !self.is_word("using") {
                name = Some(self.name()?);
            }
            if self.is_word("using") {
                self.at += 1;
                self.label()?;
            }
        }
        if self.is_word("nulls") {
            return Err(self.not_supported("UNIQUE NULLS [NOT] DISTINCT"));
        }
        let (columns, sort_options) = if self.sqlserver() {
            (self.sqlserver_key_columns()?, SortOptions::default())
        } else {
            let listed = self.ordered_index_column_list(true)?;
            let sort_options = if self.mysql() {
                listed.sort_options
            } else {
                SortOptions::default()
            };
            (listed.names, sort_options)
        };
        if self.sqlite() {
            self.sqlite_conflict_clause()?;
        }
        Ok(IndexSpec {
            name,
            unique: true,
            columns,
            position,
            method: IndexMethod::BTree,
            sort_options,
        })
    }

    fn name_list(&mut self) -> SqlResult<Vec<Name>> {
        self.expect(&TokenKind::LeftParen)?;
        let mut names = vec![self.name()?];
        while self.take(&TokenKind::Comma) {
            names.push(self.name()?);
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(names)
    }

    fn column_definition(&mut self) -> SqlResult<ColumnDefinition> {
        let name = self.name()?;
        if self.sqlserver() && self.is_word("as") {
            return Err(sqlserver::not_supported("Computed columns").at(self.position()));
        }
        let serial = self.serial_type();
        let sql_type = match serial {
            Some(sql_type) => sql_type,
            None => self.sql_type()?,
        };
        let mut column = ColumnDefinition {
            name,
            sql_type,
            not_null: serial.is_some(),
            primary_key: false,
            default: None,
            default_dialect: Dialect::PostgreSql,
            auto_increment: serial.is_some(),
            identity_seed: None,
            generated_always: false,
            unique: None,
            constraints: Vec::new(),
            comment: None,
        };
        loop {
            if self.mysql() && self.mysql_column_option(&mut column)? {
                continue;
            }
            if self.sqlserver() && self.sqlserver_column_option()? {
                continue;
            }
            let constraint = if self.take_word("constraint") {
                Some(self.name()?)
            } else {
                None
            };
            if self.column_constraint(&mut column, constraint.clone())? {
            } else if self.take_word("unique") {
                if self.is_word("nulls") {
                    return Err(self.not_supported("UNIQUE NULLS [NOT] DISTINCT"));
                }
                column.unique = Some(constraint);
                if self.sqlserver() {
                    self.skip_clustering();
                    self.skip_key_options()?;
                }
                self.oracle_enabled_constraint()?;
            } else if self.take_word("not") {
                self.expect_word("null")?;
                column.not_null = !self.oracle() || self.oracle_constraint_states()?;
            } else if self.take_word("null") {
                column.not_null = false;
                if self.oracle() {
                    self.oracle_constraint_states()?;
                }
            } else if self.take_word("primary") {
                self.expect_word("key")?;
                column.primary_key = true;
                if self.sqlserver() {
                    self.skip_clustering();
                    self.skip_key_options()?;
                }
                self.oracle_enabled_constraint()?;
            } else if self.oracle() && self.take_word("collate") {
                self.oracle_collation()?;
            } else if self.is_word("default") {
                self.at += 1;
                let start = self.position();
                let expr = self.binary_operand()?;
                let end = self.tokens[self.at.saturating_sub(1)].end;
                let written = self.text.get(start..end).unwrap_or("");
                let (source, dialect) = if self.sqlserver() {
                    sqlserver::kept_default(&expr, written, column.sql_type)
                        .map_err(|error| error.at(start))?
                } else {
                    (written.to_owned(), Dialect::PostgreSql)
                };
                column.default = Some((expr, source));
                column.default_dialect = dialect;
            } else if self.oracle() && self.is_word("generated") {
                self.oracle_identity(&mut column)?;
            } else if self.sqlserver() && self.is_word("identity") {
                self.sqlserver_identity(&mut column)?;
            } else if self.postgresql() && self.is_word("generated") {
                self.postgresql_identity(&mut column)?;
            } else if ["generated", "collate"]
                .iter()
                .any(|word| self.is_word(word))
            {
                return Err(self.not_supported(
                    "a column constraint other than PRIMARY KEY, UNIQUE, NOT NULL, DEFAULT, CHECK and REFERENCES",
                ));
            } else {
                return Ok(column);
            }
        }
    }

    fn postgresql(&self) -> bool {
        self.dialect == Dialect::PostgreSql
    }

    fn typed_literal_allowed(&self, word: &str) -> bool {
        match word {
            "numeric" | "decimal" | "timestamptz" | "json" | "jsonb" | "timetz" | "interval"
            | "bytea" | "uuid" | "bpchar" | "char" | "character" => self.postgresql(),
            "time" => self.postgresql() || self.mysql(),
            "date" | "timestamp" => !self.sqlite() && !self.sqlserver(),
            _ => true,
        }
    }

    fn serial_type(&mut self) -> Option<SqlType> {
        if !self.postgresql() {
            return None;
        }
        let TokenKind::Word(word) = self.peek_kind() else {
            return None;
        };
        let sql_type = match word.as_ref() {
            "smallserial" | "serial2" => SqlType::SmallInt,
            "serial" | "serial4" => SqlType::Integer,
            "bigserial" | "serial8" => SqlType::BigInt,
            _ => return None,
        };
        self.at += 1;
        Some(sql_type)
    }

    fn postgresql_identity(&mut self, column: &mut ColumnDefinition) -> SqlResult<()> {
        let position = self.position();
        self.at += 1;
        let always = self.take_word("always");
        if !always {
            self.expect_word("by")?;
            self.expect_word("default")?;
        }
        self.expect_word("as")?;
        if self.peek_kind() == &TokenKind::LeftParen {
            return Err(self.not_supported("generated columns"));
        }
        self.expect_word("identity")?;
        let options_position = self.position();
        let options = self.identity_options()?;
        if !matches!(
            column.sql_type,
            SqlType::SmallInt | SqlType::Integer | SqlType::BigInt
        ) {
            return Err(SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!(
                    "identity column type must be smallint, integer, or bigint, not {}",
                    column.sql_type.name()
                ),
            )
            .at(position));
        }
        column.identity_seed = constraints::postgresql_identity_seed(&options, column.sql_type)
            .map_err(|error| error.at(options_position))?;
        column.auto_increment = true;
        column.generated_always = always;
        column.not_null = true;
        Ok(())
    }

    fn numeric_modifier(&mut self, position: usize) -> SqlResult<Option<NumericModifier>> {
        if !self.take(&TokenKind::LeftParen) {
            return Ok(None);
        }
        let precision = self.signed_type_argument()?;
        let scale = if self.take(&TokenKind::Comma) {
            self.signed_type_argument()?
        } else {
            0
        };
        self.expect(&TokenKind::RightParen)?;
        crate::numeric::check_modifier(precision, scale).map_err(|error| error.at(position))?;
        Ok(Some(NumericModifier {
            precision: precision as u16,
            scale: scale as i16,
        }))
    }

    fn signed_type_argument(&mut self) -> SqlResult<i64> {
        let negative = self.take_operator("-");
        let TokenKind::Integer(digits) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let value: i64 = digits.parse().unwrap_or(i64::MAX);
        self.at += 1;
        Ok(if negative { -value } else { value })
    }

    fn time_precision(&mut self, position: usize, name: &str) -> SqlResult<Option<u8>> {
        match self.type_modifier()? {
            None => Ok(None),
            Some(precision @ 0..=6) => Ok(Some(precision as u8)),
            Some(precision) if precision < 0 => Err(SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("{name}({precision}) precision must not be negative"),
            )
            .at(position)),
            Some(_) => Ok(Some(6)),
        }
    }

    fn typed_literal_with_modifiers(&self, word: &str) -> bool {
        let with_precision = matches!(self.peek_kind_at(1), TokenKind::LeftParen)
            && matches!(self.peek_kind_at(2), TokenKind::Integer(_))
            && matches!(self.peek_kind_at(3), TokenKind::RightParen);
        let zone_at = if with_precision { 4 } else { 1 };
        let zoned = matches!(word, "time" | "timestamp")
            && (self.is_word_at(zone_at, "with") || self.is_word_at(zone_at, "without"))
            && self.is_word_at(zone_at + 1, "time")
            && self.is_word_at(zone_at + 2, "zone");
        let string_at = if zoned { zone_at + 3 } else { zone_at };
        matches!(
            word,
            "time" | "timestamp" | "interval" | "timetz" | "timestamptz"
        ) && self.postgresql()
            && (with_precision || zoned)
            && matches!(self.peek_kind_at(string_at), TokenKind::String(_))
    }

    fn interval_fields(&mut self) -> SqlResult<Option<IntervalFields>> {
        const FIELDS: [&str; 6] = ["year", "month", "day", "hour", "minute", "second"];
        let Some(first) = FIELDS.into_iter().find(|field| self.is_word(field)) else {
            return Ok(None);
        };
        let position = self.position();
        self.at += 1;
        let last = if self.is_word("to") {
            self.at += 1;
            let last = FIELDS.into_iter().find(|field| self.is_word(field));
            if last.is_none() {
                return Err(self.unexpected());
            }
            self.at += 1;
            last
        } else {
            None
        };
        IntervalFields::from_words(first, last)
            .map(Some)
            .ok_or_else(|| SqlError::syntax("syntax error at or near \"to\"", position))
    }

    fn interval_type(&mut self, position: usize) -> SqlResult<SqlType> {
        if self.peek_kind() == &TokenKind::LeftParen {
            let precision = self.time_precision(position, "interval")?;
            return Ok(SqlType::Interval {
                fields: None,
                precision,
            });
        }
        let fields = self.interval_fields()?;
        let precision = if fields.is_none_or(IntervalFields::ends_in_seconds) {
            self.time_precision(position, "interval")?
        } else {
            None
        };
        Ok(SqlType::Interval { fields, precision })
    }

    fn character_length(&mut self, position: usize) -> SqlResult<Option<u32>> {
        match self.type_modifier()? {
            None => Ok(None),
            Some(length @ 1..=10_485_760) => Ok(Some(length as u32)),
            Some(length) if length < 1 => Err(SqlError::new(
                SqlState::INVALID_TEXT_REPRESENTATION,
                "length for type char must be at least 1",
            )
            .at(position)),
            Some(_) => Err(SqlError::new(
                SqlState::INVALID_TEXT_REPRESENTATION,
                "length for type char cannot exceed 10485760",
            )
            .at(position)),
        }
    }

    fn timestamp_type(&mut self, word: &str, position: usize) -> SqlResult<SqlType> {
        let precision = self.time_precision(position, word)?;
        if word == "timestamptz" {
            return Ok(SqlType::TimestampTz(precision));
        }
        if word == "timetz" {
            return Ok(SqlType::TimeTz(precision));
        }
        let zoned = if self.is_word("with") && self.is_word_at(1, "time") {
            self.at += 2;
            self.expect_word("zone")?;
            true
        } else if self.is_word("without") {
            self.at += 1;
            self.expect_word("time")?;
            self.expect_word("zone")?;
            false
        } else {
            false
        };
        Ok(match (word, zoned) {
            ("time", true) => SqlType::TimeTz(precision),
            ("time", false) => SqlType::Time(precision),
            (_, true) => SqlType::TimestampTz(precision),
            (_, false) => SqlType::Timestamp(precision),
        })
    }

    pub(crate) fn sql_type(&mut self) -> SqlResult<SqlType> {
        if self.mysql() {
            return self.mysql_type();
        }
        if self.sqlite() {
            return self.sqlite_type().map(|(sql_type, _)| sql_type);
        }
        if self.oracle() {
            return self.oracle_type();
        }
        if self.sqlserver() {
            return self.sqlserver_type();
        }
        let position = self.position();
        let sql_type = self.element_type()?;
        if self.peek_kind() == &TokenKind::LeftBracket || self.is_word("array") {
            return self.array_column_type(sql_type, position);
        }
        Ok(sql_type)
    }

    fn array_column_type(&mut self, element: SqlType, position: usize) -> SqlResult<SqlType> {
        if self.take_word("array") {
            if self.peek_kind() == &TokenKind::LeftBracket {
                self.array_dimension()?;
            }
        } else {
            while self.peek_kind() == &TokenKind::LeftBracket {
                self.array_dimension()?;
            }
        }
        match crate::arrays::ArrayElement::of(element) {
            Some(found) if found.is_storable() => Ok(SqlType::Array(found)),
            _ => Err(crate::arrays::unsupported_element(element).at(position)),
        }
    }

    pub(crate) fn cast_type(&mut self) -> SqlResult<SqlType> {
        let position = self.position();
        let (sql_type, array) = self.cast_target()?;
        if !array {
            return Ok(sql_type);
        }
        crate::arrays::ArrayElement::required(sql_type)
            .map(SqlType::Array)
            .map_err(|error| error.at(position))
    }

    fn cast_target(&mut self) -> SqlResult<(SqlType, bool)> {
        if !self.postgresql() {
            return self.sql_type().map(|sql_type| (sql_type, false));
        }
        let sql_type = match self.catalog_cast_type() {
            Some(found) => found,
            None => self.element_type()?,
        };
        let array = if self.take_word("array") {
            if self.peek_kind() == &TokenKind::LeftBracket {
                self.array_dimension()?;
            }
            true
        } else {
            let bracketed = self.peek_kind() == &TokenKind::LeftBracket;
            while self.peek_kind() == &TokenKind::LeftBracket {
                self.array_dimension()?;
            }
            bracketed
        };
        Ok((sql_type, array))
    }

    fn catalog_cast_type(&mut self) -> Option<SqlType> {
        let qualified = self.is_word("pg_catalog") && self.peek_kind_at(1) == &TokenKind::Dot;
        let at = if qualified { 2 } else { 0 };
        let found = match self.peek_kind_at(at) {
            TokenKind::Word(word) => crate::pg_catalog::cast_type_named(word)?,
            TokenKind::QuotedIdentifier(word) => crate::pg_catalog::cast_type_named(word)?,
            _ => return None,
        };
        self.at += at + 1;
        Some(found)
    }

    fn array_dimension(&mut self) -> SqlResult<()> {
        self.expect(&TokenKind::LeftBracket)?;
        if matches!(self.peek_kind(), TokenKind::Integer(_)) {
            self.at += 1;
        }
        self.expect(&TokenKind::RightBracket)
    }

    fn type_schema(&mut self) -> SqlResult<()> {
        let schema = match self.peek_kind() {
            TokenKind::Word(word) => word.as_ref(),
            TokenKind::QuotedIdentifier(name) => name.as_str(),
            _ => return Ok(()),
        };
        if self.peek_kind_at(1) != &TokenKind::Dot
            || !matches!(
                self.peek_kind_at(2),
                TokenKind::Word(_) | TokenKind::QuotedIdentifier(_)
            )
        {
            return Ok(());
        }
        if schema == "pg_catalog" {
            self.at += 2;
            return Ok(());
        }
        let position = self.position();
        let name = match self.peek_kind_at(2) {
            TokenKind::Word(word) => word.as_ref().to_owned(),
            TokenKind::QuotedIdentifier(name) => name.clone(),
            _ => String::new(),
        };
        Err(SqlError::new(
            SqlState::UNDEFINED_OBJECT,
            format!("type \"{schema}.{name}\" does not exist"),
        )
        .at(position))
    }

    fn qualified_user_type(&mut self) -> Option<SqlResult<SqlType>> {
        let part = |kind: &TokenKind<'_>| match kind {
            TokenKind::Word(word) => Some(word.as_ref().to_owned()),
            TokenKind::QuotedIdentifier(name) => Some(name.clone()),
            _ => None,
        };
        let schema = part(self.peek_kind())?;
        if schema == "pg_catalog" || self.peek_kind_at(1) != &TokenKind::Dot {
            return None;
        }
        let name = part(self.peek_kind_at(2))?;
        let position = self.position();
        self.at += 3;
        Some(crate::user_types::resolve_type(Some(&schema), &name, position))
    }

    fn element_type(&mut self) -> SqlResult<SqlType> {
        if self.postgresql() {
            if let Some(found) = self.qualified_user_type() {
                return found;
            }
            self.type_schema()?;
        }
        let token = self.peek();
        let word: &str = match &token.kind {
            TokenKind::Word(word) => word,
            TokenKind::QuotedIdentifier(name) => name,
            _ => return Err(self.unexpected()),
        };
        self.at += 1;
        let sql_type = match word {
            "smallint" | "int2" => SqlType::SmallInt,
            "integer" | "int" | "int4" => SqlType::Integer,
            "bigint" | "int8" => SqlType::BigInt,
            "real" | "float4" => SqlType::Real,
            "float8" => SqlType::Double,
            "double" => {
                self.expect_word("precision")?;
                SqlType::Double
            }
            "float" => match self.type_modifier()? {
                Some(bits @ 1..=24) if bits > 0 => SqlType::Real,
                Some(25..=53) | None => SqlType::Double,
                Some(_) => {
                    return Err(SqlError::new(
                        SqlState::INVALID_TEXT_REPRESENTATION,
                        "precision for type float must be between 1 and 53 bits",
                    )
                    .at(token.start));
                }
            },
            "boolean" | "bool" => SqlType::Boolean,
            "text" => SqlType::Text,
            "varchar" => SqlType::Varchar(self.varchar_length(token.start)?),
            "character" | "char" if self.is_word("varying") => {
                self.at += 1;
                SqlType::Varchar(self.varchar_length(token.start)?)
            }
            "numeric" | "decimal" | "dec" => SqlType::Numeric(self.numeric_modifier(token.start)?),
            "date" => SqlType::Date,
            "timestamp" | "timestamptz" => self.timestamp_type(word, token.start)?,
            "json" => SqlType::Json,
            "jsonb" => SqlType::Jsonb,
            "time" | "timetz" => self.timestamp_type(word, token.start)?,
            "interval" => self.interval_type(token.start)?,
            "uuid" => SqlType::Uuid,
            "bytea" => SqlType::Bytea,
            "character" | "char" => {
                SqlType::Bpchar(Some(self.character_length(token.start)?.unwrap_or(1)))
            }
            "bpchar" => SqlType::Bpchar(self.character_length(token.start)?),
            "money" | "inet" | "cidr" | "xml" | "bit" | "point" => {
                return Err(
                    SqlError::not_supported(format!("type {word} is not supported yet"))
                        .at(token.start),
                );
            }
            _ if self.postgresql() => {
                return crate::user_types::resolve_type(None, word, token.start);
            }
            _ => {
                return Err(SqlError::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!("type \"{word}\" does not exist"),
                )
                .at(token.start));
            }
        };
        Ok(sql_type)
    }

    fn type_modifier(&mut self) -> SqlResult<Option<i64>> {
        if !self.take(&TokenKind::LeftParen) {
            return Ok(None);
        }
        let token = self.peek();
        let TokenKind::Integer(digits) = &token.kind else {
            return Err(self.unexpected());
        };
        self.at += 1;
        self.expect(&TokenKind::RightParen)?;
        Ok(Some(digits.parse().unwrap_or(i64::MAX)))
    }

    fn varchar_length(&mut self, position: usize) -> SqlResult<Option<u32>> {
        match self.type_modifier()? {
            None => Ok(None),
            Some(length @ 1..=10_485_760) => Ok(Some(length as u32)),
            Some(length) if length < 1 => Err(SqlError::new(
                SqlState::INVALID_TEXT_REPRESENTATION,
                "length for type varchar must be at least 1",
            )
            .at(position)),
            Some(_) => Err(SqlError::new(
                SqlState::INVALID_TEXT_REPRESENTATION,
                "length for type varchar cannot exceed 10485760",
            )
            .at(position)),
        }
    }

    fn drop(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        if self.is_word("index") {
            return self.drop_index();
        }
        if (self.postgresql() || self.sqlserver()) && self.is_word("schema") {
            return self.drop_schema();
        }
        if self.is_word("view") {
            return self.drop_view();
        }
        if self.is_word("materialized") && self.is_word_at(1, "view") {
            return Err(self.not_supported("DROP MATERIALIZED VIEW"));
        }
        if self.postgresql() && self.is_word("sequence") {
            return self.drop_sequence();
        }
        if self.postgresql() && (self.is_word("type") || self.is_word("domain")) {
            return self.drop_type();
        }
        let only_temporary =
            self.mysql() && self.is_word("temporary") && self.is_word_at(1, "table");
        if only_temporary {
            self.at += 1;
        }
        if !self.take_word("table") {
            return match self.peek_kind() {
                TokenKind::Word(word) => {
                    Err(self.not_supported(&format!("DROP {}", word.to_ascii_uppercase())))
                }
                _ => Err(self.unexpected()),
            };
        }
        let if_exists = if self.take_word("if") {
            self.expect_word("exists")?;
            true
        } else {
            false
        };
        let mut names = vec![self.table_name()?];
        while self.take(&TokenKind::Comma) {
            names.push(self.table_name()?);
        }
        if only_temporary {
            for name in &mut names {
                name.text = crate::temporary::internal_name(&name.text);
            }
        }
        let cascade = !self.sqlite() && self.take_word("cascade");
        if !cascade {
            self.take_word("restrict");
        }
        Ok(Statement::DropTable {
            names,
            if_exists,
            cascade,
        })
    }

    fn table_or_function(&mut self) -> SqlResult<TableReference> {
        if !self.sqlite() && self.is_word("lateral") {
            let position = self.position();
            self.at += 1;
            let mut reference = self.table_or_function()?;
            return match &mut reference.derived {
                Some(derived) => {
                    derived.lateral = true;
                    Ok(reference)
                }
                None if reference.generated.is_some() => Ok(reference),
                None => Err(SqlError::syntax(
                    "LATERAL must be followed by a subquery or a function call",
                    position,
                )),
            };
        }
        if self.postgresql() && self.at_table_function() {
            return self.table_function();
        }
        if self.sqlserver() && self.at_openjson() {
            return self.sqlserver_openjson();
        }
        if self.sqlserver() && self.at_string_split() {
            return self.sqlserver_string_split();
        }
        if self.sqlserver() && self.at_sqlserver_table_function() {
            return self.sqlserver_table_function();
        }
        self.table_reference()
    }

    fn table_reference(&mut self) -> SqlResult<TableReference> {
        if self.sqlserver()
            && let Some(reference) = self.transition_table_reference()?
        {
            return Ok(reference);
        }
        if self.sqlserver()
            && matches!(self.peek_kind(), TokenKind::Variable(name) if !name.starts_with("@@"))
        {
            return self.table_variable_reference();
        }
        if self.peek_kind() == &TokenKind::LeftParen {
            if self.at_derived_table() {
                return self.derived_table();
            }
            return Err(self.not_supported("parenthesized joins in FROM"));
        }
        if self.is_word("only") {
            self.at += 1;
        }
        let name = self.table_name()?;
        let alias = if self.take_word("as") || self.at_bare_alias() {
            Some(self.name()?.text)
        } else {
            None
        };
        if self.peek_kind() == &TokenKind::LeftParen && alias.is_some() {
            return Err(self.not_supported("column aliases in FROM"));
        }
        Ok(TableReference::named(name, alias))
    }

    fn at_bare_alias(&self) -> bool {
        matches!(self.peek_kind(), TokenKind::Word(word) if !self.reserved(word) && !self.clause_word(word))
            || matches!(self.peek_kind(), TokenKind::QuotedIdentifier(_))
    }

    fn insert(&mut self) -> SqlResult<Statement> {
        let replace = self.is_word("replace");
        self.at += 1;
        let mut on_duplicate = if replace {
            OnDuplicate::Replace
        } else {
            OnDuplicate::Fail
        };
        let mut rollback_on_conflict = false;
        if self.mysql() {
            if !replace && self.take_word("ignore") {
                on_duplicate = OnDuplicate::Ignore;
            }
            self.take_word("into");
        } else if self.sqlite() {
            if !replace && self.take_word("or") {
                (on_duplicate, rollback_on_conflict) = self.sqlite_conflict_resolution()?;
            }
            self.expect_word("into")?;
        } else if self.sqlserver() {
            self.take_word("into");
        } else {
            self.expect_word("into")?;
        }
        let start = self.at;
        let table = match self.table_name() {
            Ok(name) if self.postgresql() && self.is_word("overriding") => {
                TableReference::named(name, None)
            }
            _ => {
                self.at = start;
                self.table_reference()?
            }
        };
        let columns = if self.peek_kind() == &TokenKind::LeftParen
            && !self.is_word_at(1, "select")
            && !self.is_word_at(1, "values")
        {
            Some(self.name_list()?)
        } else {
            None
        };
        let output = if self.sqlserver() {
            self.sqlserver_output(sqlserver_functions::OutputStatement::Insert)?
        } else {
            None
        };
        let overriding = if self.take_word("overriding") {
            if !self.postgresql() {
                return Err(self.not_supported("OVERRIDING"));
            }
            let overriding = if self.take_word("system") {
                Overriding::SystemValue
            } else {
                self.expect_word("user")?;
                Overriding::UserValue
            };
            self.expect_word("value")?;
            Some(overriding)
        } else {
            None
        };
        let source = if self.take_word("default") {
            self.expect_word("values")?;
            InsertSource::DefaultValues
        } else if self.take_word("values") || self.mysql() && self.take_word("value") {
            let mut rows = Vec::new();
            loop {
                self.expect(&TokenKind::LeftParen)?;
                let mut row = Vec::new();
                loop {
                    row.push(self.insert_value()?);
                    if !self.take(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(&TokenKind::RightParen)?;
                rows.push(row);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            InsertSource::Values(rows)
        } else if self.is_word("select")
            || self.is_word("with")
            || self.peek_kind() == &TokenKind::LeftParen
        {
            InsertSource::Query(Box::new(self.insert_query()?))
        } else {
            return Err(self.unexpected());
        };
        if self.mysql() && self.is_word("on") {
            on_duplicate = self.on_duplicate_key_update(on_duplicate)?;
        }
        if self.sqlite() && self.is_word("on") {
            on_duplicate = self.sqlite_upsert()?;
        }
        if self.is_word("on") {
            return Err(self.not_supported("ON CONFLICT"));
        }
        let returning = if self.mysql() {
            None
        } else if output.is_some() {
            output
        } else {
            self.returning()?
        };
        Ok(Statement::Insert(Insert {
            table,
            columns,
            source,
            returning,
            on_duplicate,
            rollback_on_conflict,
            overriding,
        }))
    }

    fn insert_value(&mut self) -> SqlResult<InsertValue> {
        if self.take_word("default") {
            Ok(InsertValue::Default)
        } else {
            Ok(InsertValue::Expr(self.expression()?))
        }
    }

    fn returning(&mut self) -> SqlResult<Option<Vec<SelectItem>>> {
        if !self.sqlserver() && self.take_word("returning") {
            Ok(Some(self.select_items()?))
        } else {
            Ok(None)
        }
    }

    fn update(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        let table = self.table_reference()?;
        self.expect_word("set")?;
        let mut assignments = Vec::new();
        loop {
            if self.peek_kind() == &TokenKind::LeftParen {
                return Err(self.not_supported("multiple-column assignment in UPDATE"));
            }
            let column = self.name()?;
            if self.peek_kind() == &TokenKind::Dot || self.peek_kind() == &TokenKind::LeftBracket {
                return Err(self.not_supported("assigning to a field or element"));
            }
            if !self.take_operator("=") {
                return Err(self.unexpected());
            }
            assignments.push((column, self.insert_value()?));
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        let output = if self.sqlserver() {
            self.sqlserver_output(sqlserver_functions::OutputStatement::Update)?
        } else {
            None
        };
        if self.is_word("from") {
            return Err(self.not_supported("UPDATE ... FROM"));
        }
        let filter = self.filter()?;
        let returning = match output {
            Some(output) => Some(output),
            None => self.returning()?,
        };
        Ok(Statement::Update(Update {
            table,
            assignments,
            filter,
            returning,
        }))
    }

    fn delete(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        if self.oracle() {
            self.take_word("from");
        } else {
            self.expect_word("from")?;
        }
        let table = self.table_reference()?;
        if self.is_word("using") {
            return Err(self.not_supported("DELETE ... USING"));
        }
        let output = if self.sqlserver() {
            self.sqlserver_output(sqlserver_functions::OutputStatement::Delete)?
        } else {
            None
        };
        let filter = self.filter()?;
        let returning = match output {
            Some(output) => Some(output),
            None => self.returning()?,
        };
        Ok(Statement::Delete(Delete {
            table,
            filter,
            returning,
        }))
    }

    fn filter(&mut self) -> SqlResult<Option<Expr>> {
        if self.take_word("where") {
            if self.is_word("current") {
                return Err(self.not_supported("WHERE CURRENT OF"));
            }
            let condition = self.expression()?;
            Ok(Some(self.condition(condition)))
        } else {
            Ok(None)
        }
    }

    fn select_clauses(&mut self) -> SqlResult<Select> {
        self.at += 1;
        let distinct = self.take_word("distinct");
        if distinct && self.is_word("on") {
            return Err(self.not_supported("DISTINCT ON"));
        }
        if !distinct {
            self.take_word("all");
        }
        let top = if self.sqlserver() && self.is_word("top") {
            self.sqlserver_top()?
        } else {
            None
        };
        let called = matches!(self.peek_kind(), TokenKind::Word(_))
            && self.peek_kind_at(1) == &TokenKind::LeftParen;
        let items = if self.is_clause_end() && !(self.sqlite() && called) {
            if self.sqlite() || self.oracle() || self.sqlserver() {
                return Err(self.unexpected());
            }
            Vec::new()
        } else {
            self.select_items()?
        };
        if self.is_word("into") {
            return Err(self.not_supported("SELECT INTO"));
        }
        let mut items = items;
        let from = if self.take_word("from") {
            if self.oracle() && self.oracle_dual() {
                items = Self::dual_items(items);
                None
            } else if self.mysql() && self.take_word("dual") {
                None
            } else {
                Some(self.joined_tables()?)
            }
        } else if self.oracle() {
            return Err(self.oracle_from_expected());
        } else {
            None
        };
        let (filter, source_row_limit) = if self.oracle() {
            oracle::split_rownum(self.filter()?)?
        } else {
            (self.filter()?, None)
        };
        let group_by = if self.take_word("group") {
            self.expect_word("by")?;
            self.group_by()?
        } else {
            Vec::new()
        };
        let having = if self.take_word("having") {
            let condition = self.expression()?;
            Some(self.condition(condition))
        } else {
            None
        };
        let windows = if self.take_word("window") {
            self.window_clause()?
        } else {
            Vec::new()
        };
        for item in &mut items {
            if let SelectItem::Expr { expr, .. } = item {
                crate::window::resolve_references(expr, &windows)?;
            }
        }
        self.select_windows = Some(windows);
        if self.oracle() && (self.is_word("connect") || self.is_word("start")) {
            return Err(self.not_supported("hierarchical queries (CONNECT BY)"));
        }
        if self.oracle() {
            oracle::reject_rownum_outside_where(&items, &group_by, having.as_ref(), &[])?;
        }
        if top.is_some() {
            self.pending_top = top;
        }
        let items = if self.postgresql() {
            crate::json_functions::with_whole_rows(items, from.as_ref())
        } else {
            items
        };
        Ok(Select {
            distinct,
            items,
            from,
            filter,
            group_by,
            having,
            order_by: Vec::new(),
            limit: None,
            offset: None,
            source_row_limit,
        })
    }

    fn leading_join_group(&mut self) -> SqlResult<FromClause> {
        let position = self.position();
        self.expect(&TokenKind::LeftParen)?;
        self.depth += 1;
        if self.depth > MAX_NESTING {
            return Err(SqlError::new(
                SqlState::STATEMENT_TOO_COMPLEX,
                "stack depth limit exceeded",
            )
            .at(position));
        }
        let group = self.joined_tables();
        self.depth -= 1;
        let group = group?;
        self.expect(&TokenKind::RightParen)?;
        if group.joins.is_empty()
            || group.joins.iter().any(|join| join.kind == JoinKind::Comma)
            || self.is_word("as")
            || self.at_bare_alias()
        {
            return Err(SqlError::not_supported(
                "parenthesized joins in FROM other than a leading join group is not supported yet",
            )
            .at(position));
        }
        Ok(group)
    }

    fn joined_tables(&mut self) -> SqlResult<FromClause> {
        let (first, mut joins) = if self.peek_kind() == &TokenKind::LeftParen
            && !self.at_derived_table()
            && !self.sqlite()
        {
            let group = self.leading_join_group()?;
            (group.first, group.joins)
        } else {
            (self.table_or_function()?, Vec::new())
        };
        loop {
            if self.sqlserver()
                && (self.is_word("cross") || self.is_word("outer"))
                && self.is_word_at(1, "apply")
            {
                let outer = self.is_word("outer");
                let position = self.position();
                if joins.len() + 1 >= MAX_JOINED_TABLES {
                    return Err(SqlError::new(
                        SqlState::PROGRAM_LIMIT_EXCEEDED,
                        format!("a FROM clause can join at most {MAX_JOINED_TABLES} tables"),
                    )
                    .at(position));
                }
                self.at += 2;
                let mut table = self.table_or_function()?;
                if let Some(derived) = &mut table.derived {
                    derived.lateral = true;
                }
                let (kind, condition) = if outer {
                    (
                        JoinKind::Left,
                        JoinCondition::On(Expr {
                            kind: ExprKind::Literal(Literal::Boolean(true)),
                            position,
                        }),
                    )
                } else {
                    (JoinKind::Cross, JoinCondition::Always)
                };
                joins.push(Join {
                    kind,
                    table,
                    condition,
                });
                continue;
            }
            let kind = if self.take(&TokenKind::Comma) {
                JoinKind::Comma
            } else if self.take_word("cross") {
                self.expect_word("join")?;
                JoinKind::Cross
            } else if self.take_word("join") {
                JoinKind::Inner
            } else if self.take_word("inner") {
                self.expect_word("join")?;
                JoinKind::Inner
            } else if self.take_word("left") {
                self.take_word("outer");
                self.expect_word("join")?;
                JoinKind::Left
            } else if self.take_word("right") {
                self.take_word("outer");
                self.expect_word("join")?;
                JoinKind::Right
            } else if !self.mysql() && self.take_word("full") {
                self.take_word("outer");
                self.expect_word("join")?;
                JoinKind::Full
            } else if self.is_word("natural") {
                return Err(self.not_supported("NATURAL joins"));
            } else {
                break;
            };
            if joins.len() + 1 >= MAX_JOINED_TABLES {
                return Err(SqlError::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    format!("a FROM clause can join at most {MAX_JOINED_TABLES} tables"),
                )
                .at(self.position()));
            }
            let table = self.table_or_function()?;
            let condition = match kind {
                JoinKind::Comma | JoinKind::Cross => JoinCondition::Always,
                JoinKind::Inner | JoinKind::Left | JoinKind::Right | JoinKind::Full => {
                    if self.take_word("on") {
                        let condition = self.expression()?;
                        JoinCondition::On(self.condition(condition))
                    } else if self.take_word("using") {
                        let names = self.name_list()?;
                        if self.is_word("as") {
                            return Err(self.not_supported("an alias for USING"));
                        }
                        JoinCondition::Using(names)
                    } else {
                        return Err(self.unexpected());
                    }
                }
            };
            joins.push(Join {
                kind,
                table,
                condition,
            });
        }
        Ok(FromClause { first, joins })
    }

    fn group_by(&mut self) -> SqlResult<Vec<Expr>> {
        if self.is_word("all") || self.is_word("distinct") {
            return Err(self.not_supported("GROUP BY ALL and GROUP BY DISTINCT"));
        }
        let mut keys = Vec::new();
        loop {
            let grouping_set = (["rollup", "cube"].iter().any(|word| self.is_word(word))
                && self.peek_kind_at(1) == &TokenKind::LeftParen)
                || (self.is_word("grouping") && self.is_word_at(1, "sets"))
                || (self.peek_kind() == &TokenKind::LeftParen
                    && self.peek_kind_at(1) == &TokenKind::RightParen);
            if grouping_set {
                return Err(self.not_supported("ROLLUP, CUBE and GROUPING SETS"));
            }
            keys.push(self.expression()?);
            if !self.take(&TokenKind::Comma) {
                return Ok(keys);
            }
        }
    }

    fn is_clause_end(&self) -> bool {
        matches!(
            self.peek_kind(),
            TokenKind::Semicolon | TokenKind::End | TokenKind::RightParen
        ) || matches!(self.peek_kind(), TokenKind::Word(word) if self.clause_word(word) || word == "from")
    }

    fn select_items(&mut self) -> SqlResult<Vec<SelectItem>> {
        let mut items = Vec::new();
        loop {
            items.push(self.select_item()?);
            if !self.take(&TokenKind::Comma) {
                return Ok(items);
            }
        }
    }

    fn select_item(&mut self) -> SqlResult<SelectItem> {
        let position = self.position();
        if self.is_operator("*") {
            self.at += 1;
            return Ok(SelectItem::Wildcard {
                qualifier: None,
                position,
            });
        }
        if (matches!(self.peek_kind(), TokenKind::Word(word) if !self.reserved(word))
            || matches!(self.peek_kind(), TokenKind::QuotedIdentifier(_)))
            && self.peek_kind_at(1) == &TokenKind::Dot
            && matches!(self.peek_kind_at(2), TokenKind::Operator(op) if *op == "*")
        {
            let qualifier = self.name()?.text;
            self.at += 2;
            return Ok(SelectItem::Wildcard {
                qualifier: Some(qualifier),
                position,
            });
        }
        if self.sqlserver()
            && self.select_targets.is_some()
            && self.query_depth == 0
        {
            return self.sqlserver_assignment_item();
        }
        if self.sqlserver()
            && let Some(alias) = self.sqlserver_assigned_alias()?
        {
            let expr = self.expression()?;
            return Ok(SelectItem::Expr {
                expr,
                alias: Some(alias),
            });
        }
        let first_token = self.at;
        let expr = self.expression()?;
        let alias = if self.take_word("as")
            || matches!(self.peek_kind(), TokenKind::Word(word) if !self.reserved(word) && !self.clause_word(word))
            || matches!(self.peek_kind(), TokenKind::QuotedIdentifier(_))
            || (self.mysql() || self.sqlite() || self.sqlserver())
                && matches!(self.peek_kind(), TokenKind::String(_))
        {
            Some(self.label()?)
        } else if self.mysql()
            && let ExprKind::Literal(Literal::String(text)) = &expr.kind
        {
            Some(text.clone())
        } else if self.oracle() && !matches!(expr.kind, ExprKind::Column { .. }) {
            Some(oracle::label(&self.source_text(first_token)))
        } else if self.sqlserver() && !matches!(expr.kind, ExprKind::Column { .. }) {
            Some(String::new())
        } else if (self.mysql() || self.sqlite()) && !matches!(expr.kind, ExprKind::Column { .. }) {
            Some(self.source_text(first_token))
        } else {
            None
        };
        Ok(SelectItem::Expr { expr, alias })
    }

    fn source_text(&self, first_token: usize) -> String {
        let start = self.tokens[first_token].start;
        let end = self.tokens[self.at.saturating_sub(1)].end.max(start);
        self.text.get(start..end).unwrap_or("").to_owned()
    }

    fn label(&mut self) -> SqlResult<String> {
        match self.peek_kind() {
            TokenKind::String(text) if self.mysql() || self.sqlite() || self.sqlserver() => {
                let text = text.as_ref().to_owned();
                self.at += 1;
                Ok(text)
            }
            TokenKind::Word(word) => {
                let word = word.as_ref().to_owned();
                self.at += 1;
                Ok(word)
            }
            TokenKind::QuotedIdentifier(name) => {
                let name = name.clone();
                self.at += 1;
                Ok(name)
            }
            _ => Err(self.unexpected()),
        }
    }

    pub(crate) fn expression(&mut self) -> SqlResult<Expr> {
        self.expression_above(Level::Or)
    }

    fn expression_above(&mut self, least: Level) -> SqlResult<Expr> {
        self.depth += 1;
        let result = if self.depth > MAX_NESTING {
            Err(too_deep(self.position()))
        } else {
            self.expression_from(least)
        };
        self.depth -= 1;
        result
    }

    fn expression_from(&mut self, least: Level) -> SqlResult<Expr> {
        let entry_depth = self.depth;
        let result = self.operator_chain(least);
        self.depth = entry_depth;
        result
    }

    fn operator_chain(&mut self, least: Level) -> SqlResult<Expr> {
        let mut left = self.prefix(least)?;
        while let Some(level) = self.infix_level() {
            if level < least {
                break;
            }
            if !matches!(level, Level::Or | Level::And) {
                self.depth += 1;
                if self.depth > MAX_NESTING {
                    return Err(too_deep(self.position()));
                }
            }
            left = self.infix(left, level)?;
        }
        Ok(left)
    }

    fn binary(left: Expr, operator: BinaryOperator, right: Expr) -> Expr {
        let position = left.position;
        Expr {
            kind: ExprKind::Binary(operator, Box::new(left), Box::new(right)),
            position,
        }
    }

    fn joined(left: Expr, right: Expr, level: Level) -> Expr {
        let position = left.position;
        let kind = match (left.kind, level) {
            (ExprKind::Or(mut operands), Level::Or) => {
                operands.push(right);
                ExprKind::Or(operands)
            }
            (ExprKind::And(mut operands), Level::And) => {
                operands.push(right);
                ExprKind::And(operands)
            }
            (kind, _) => {
                let operands = vec![Expr { kind, position }, right];
                if level == Level::Or {
                    ExprKind::Or(operands)
                } else {
                    ExprKind::And(operands)
                }
            }
        };
        Expr { kind, position }
    }

    fn prefix(&mut self, least: Level) -> SqlResult<Expr> {
        let position = self.position();
        if self.mysql() && self.take_operator("!") {
            let operand = self.expression_above(Level::Unary)?;
            return Ok(Expr {
                kind: ExprKind::Unary(UnaryOperator::Not, Box::new(operand)),
                position,
            });
        }
        if self.is_word("not") {
            if least > Level::Not {
                return Err(self.unexpected());
            }
            self.at += 1;
            let operand = self.expression_above(Level::Not)?;
            let operand = self.condition(operand);
            return Ok(Expr {
                kind: ExprKind::Unary(UnaryOperator::Not, Box::new(operand)),
                position,
            });
        }
        let operator = if self.take_operator("-") {
            UnaryOperator::Minus
        } else if self.take_operator("+") {
            UnaryOperator::Plus
        } else {
            return self.postfix();
        };
        let operand = self.expression_above(Level::Unary)?;
        if operator == UnaryOperator::Minus
            && let ExprKind::Literal(literal) = &operand.kind
        {
            let negated = match literal {
                Literal::Integer(value) => value.checked_neg().map(Literal::Integer),
                Literal::Decimal(text) if text == "9223372036854775808" => {
                    Some(Literal::Integer(i64::MIN))
                }
                Literal::Decimal(text) if !text.starts_with('-') => {
                    Some(Literal::Decimal(format!("-{text}")))
                }
                _ => None,
            };
            if let Some(literal) = negated {
                return Ok(Expr {
                    kind: ExprKind::Literal(literal),
                    position,
                });
            }
        }
        Ok(Expr {
            kind: ExprKind::Unary(operator, Box::new(operand)),
            position,
        })
    }

    fn infix_level(&self) -> Option<Level> {
        if self.mysql()
            && let Some(level) = self.mysql_infix_level()
        {
            return Some(level);
        }
        if self.sqlite()
            && let Some(level) = self.sqlite_infix_level()
        {
            return Some(level);
        }
        if self.oracle() && self.is_operator("%") {
            return None;
        }
        match self.peek_kind() {
            TokenKind::Word(word) => match word.as_ref() {
                "operator"
                    if self.postgresql() && self.peek_kind_at(1) == &TokenKind::LeftParen =>
                {
                    Some(Level::Other)
                }
                "or" => Some(Level::Or),
                "and" => Some(Level::And),
                "is" | "isnull" | "notnull" => Some(Level::Is),
                "between" | "in" | "like" | "ilike" => Some(Level::Pattern),
                "similar" if self.is_word_at(1, "to") => Some(Level::Pattern),
                "not"
                    if ["between", "in", "like", "ilike"]
                        .iter()
                        .any(|word| self.is_word_at(1, word))
                        || (self.is_word_at(1, "similar") && self.is_word_at(2, "to")) =>
                {
                    Some(Level::Pattern)
                }
                _ => None,
            },
            TokenKind::Operator(symbol) => Some(match *symbol {
                "=" | "<>" | "!=" | "<" | "<=" | ">" | ">=" => Level::Comparison,
                "+" | "-" => Level::Additive,
                "*" | "/" | "%" => Level::Multiplicative,
                "^" => Level::Power,
                _ => Level::Other,
            }),
            _ => None,
        }
    }

    fn infix(&mut self, left: Expr, level: Level) -> SqlResult<Expr> {
        if self.mysql() && self.mysql_infix_level().is_some() {
            return self.mysql_infix(left, level);
        }
        match level {
            Level::Or => {
                self.at += 1;
                let right = self.expression_above(Level::And)?;
                let (left, right) = (self.condition(left), self.condition(right));
                Ok(Self::joined(left, right, Level::Or))
            }
            Level::And => {
                self.at += 1;
                let right = self.expression_above(Level::Not)?;
                let (left, right) = (self.condition(left), self.condition(right));
                Ok(Self::joined(left, right, Level::And))
            }
            Level::Is => self.is_test(left),
            Level::Comparison => {
                let operator = self
                    .comparison_operator()
                    .ok_or_else(|| self.unexpected())?;
                self.at += 1;
                if self.at_quantified() {
                    return self.quantified_comparison(left, operator);
                }
                let right = self.expression_above(Level::Pattern)?;
                if self.infix_level() == Some(Level::Comparison) {
                    return Err(self.unexpected());
                }
                Ok(Self::binary(left, operator, right))
            }
            Level::Pattern => self.pattern(left),
            Level::Other => {
                if self.postgresql() && self.is_word("operator") {
                    return self.explicit_operator(left);
                }
                if self.postgresql()
                    && let Some(operator) = self.regex_operator()
                {
                    self.at += 1;
                    let right = self.expression_above(Level::Additive)?;
                    return Ok(Self::binary(left, operator, right));
                }
                if self.postgresql()
                    && let Some(operator) = self.json_operator()
                {
                    self.at += 1;
                    let right = self.expression_above(Level::Additive)?;
                    return Ok(Self::binary(left, operator, right));
                }
                if self.postgresql()
                    && let TokenKind::Operator(symbol) = self.peek_kind()
                    && let Some((_, function)) = crate::ranges::OPERATORS
                        .iter()
                        .find(|(written, _)| written == symbol)
                {
                    self.at += 1;
                    let right = self.expression_above(Level::Additive)?;
                    let position = left.position;
                    return Ok(Expr {
                        kind: ExprKind::Function {
                            name: (*function).to_owned(),
                            arguments: vec![left, right],
                        },
                        position,
                    });
                }
                if !self.take_operator("||") {
                    let TokenKind::Operator(symbol) = self.peek_kind() else {
                        return Err(self.unexpected());
                    };
                    return Err(SqlError::new(
                        SqlState::UNDEFINED_FUNCTION,
                        format!("operator does not exist: {symbol}"),
                    )
                    .at(self.position()));
                }
                let right = self.expression_above(Level::Additive)?;
                if self.oracle() {
                    return Ok(oracle::concatenation(left, right));
                }
                Ok(Self::binary(left, BinaryOperator::Concat, right))
            }
            Level::Additive => {
                let operator = if self.take_operator("+") {
                    BinaryOperator::Add
                } else {
                    self.at += 1;
                    BinaryOperator::Subtract
                };
                let right = self.expression_above(Level::Multiplicative)?;
                Ok(Self::binary(left, operator, right))
            }
            Level::Multiplicative => {
                let operator = if self.take_operator("*") {
                    BinaryOperator::Multiply
                } else if self.take_operator("/") {
                    BinaryOperator::Divide
                } else {
                    self.at += 1;
                    BinaryOperator::Modulo
                };
                let right = self.expression_above(Level::Power)?;
                if self.oracle() && operator == BinaryOperator::Divide {
                    return Ok(oracle::division(left, right));
                }
                if self.sqlite() && operator != BinaryOperator::Multiply {
                    return Ok(Self::binary(left, operator, sqlite::null_if_zero(right)));
                }
                Ok(Self::binary(left, operator, right))
            }
            Level::Power => {
                self.at += 1;
                let right = self.expression_above(Level::Unary)?;
                Ok(Self::binary(left, BinaryOperator::Power, right))
            }
            Level::Not | Level::Unary => Err(self.unexpected()),
        }
    }

    fn is_test(&mut self, expr: Expr) -> SqlResult<Expr> {
        let position = expr.position;
        if !self.sqlserver() && (self.is_word("isnull") || self.is_word("notnull")) {
            let negated = self.is_word("notnull");
            self.at += 1;
            return Ok(Expr {
                kind: ExprKind::IsNull {
                    expr: Box::new(expr),
                    negated,
                },
                position,
            });
        }
        self.expect_word("is")?;
        let negated = self.take_word("not");
        let kind = if self.take_word("null") {
            ExprKind::IsNull {
                expr: Box::new(expr),
                negated,
            }
        } else if self.sqlserver() {
            return Err(self.unexpected());
        } else if self.take_word("true") {
            truth(expr, TruthTest::True, negated)
        } else if self.take_word("false") {
            truth(expr, TruthTest::False, negated)
        } else if self.take_word("unknown") {
            truth(expr, TruthTest::Unknown, negated)
        } else if self.take_word("distinct") {
            self.expect_word("from")?;
            let right = self.expression_above(Level::Comparison)?;
            ExprKind::IsDistinctFrom {
                left: Box::new(expr),
                right: Box::new(right),
                negated,
            }
        } else if self.sqlite() {
            let right = self.expression_above(Level::Comparison)?;
            ExprKind::IsDistinctFrom {
                left: Box::new(expr),
                right: Box::new(right),
                negated: !negated,
            }
        } else {
            return Err(self.unexpected());
        };
        Ok(Expr { kind, position })
    }

    fn json_operator(&self) -> Option<BinaryOperator> {
        let TokenKind::Operator(symbol) = self.peek_kind() else {
            return None;
        };
        Some(match *symbol {
            "->" => BinaryOperator::JsonField,
            "->>" => BinaryOperator::JsonFieldText,
            "#>" => BinaryOperator::JsonPath,
            "#>>" => BinaryOperator::JsonPathText,
            "@>" => BinaryOperator::JsonContains,
            "<@" => BinaryOperator::JsonContainedIn,
            "?" => BinaryOperator::JsonHasKey,
            "?|" => BinaryOperator::JsonHasAnyKey,
            "?&" => BinaryOperator::JsonHasAllKeys,
            "#-" => BinaryOperator::JsonDeletePath,
            _ => return None,
        })
    }

    fn comparison_operator(&self) -> Option<BinaryOperator> {
        let TokenKind::Operator(symbol) = self.peek_kind() else {
            return None;
        };
        Some(match *symbol {
            "=" => BinaryOperator::Equal,
            "<>" | "!=" => BinaryOperator::NotEqual,
            "<" => BinaryOperator::Less,
            "<=" => BinaryOperator::LessOrEqual,
            ">" => BinaryOperator::Greater,
            ">=" => BinaryOperator::GreaterOrEqual,
            "==" if self.sqlite() => BinaryOperator::Equal,
            _ => return None,
        })
    }

    fn pattern(&mut self, expr: Expr) -> SqlResult<Expr> {
        let position = expr.position;
        let negated = self.take_word("not");
        let kind = if self.take_word("between") {
            if self.is_word("symmetric") || self.is_word("asymmetric") {
                return Err(self.not_supported("BETWEEN SYMMETRIC"));
            }
            let low = self.expression_above(Level::Other)?;
            self.expect_word("and")?;
            let high = self.expression_above(Level::Other)?;
            ExprKind::Between {
                expr: Box::new(expr),
                low: Box::new(low),
                high: Box::new(high),
                negated,
            }
        } else if self.take_word("in") {
            if self.at_subquery() {
                return self
                    .subquery_expression(crate::ast::SubqueryTest::In { expr, negated }, position);
            }
            self.expect(&TokenKind::LeftParen)?;
            let mut list = vec![self.expression()?];
            while self.take(&TokenKind::Comma) {
                list.push(self.expression()?);
            }
            self.expect(&TokenKind::RightParen)?;
            ExprKind::InList {
                expr: Box::new(expr),
                list,
                negated,
            }
        } else if self.sqlite() && self.is_word("glob") {
            self.at += 1;
            let pattern = self.expression_above(Level::Other)?;
            return sqlite::glob(expr, pattern, negated);
        } else if self.is_word("like") || (self.is_word("ilike") && !self.sqlserver()) {
            let case_insensitive = self.is_word("ilike") || self.sqlite();
            self.at += 1;
            let pattern = self.expression_above(Level::Other)?;
            if self.is_word("escape") {
                return Err(self.not_supported("LIKE ... ESCAPE"));
            }
            ExprKind::Like {
                expr: Box::new(expr),
                pattern: Box::new(pattern),
                negated,
                case_insensitive,
            }
        } else if self.postgresql() && self.take_word("similar") {
            self.expect_word("to")?;
            let pattern = self.expression_above(Level::Other)?;
            let mut arguments = vec![pattern];
            if self.take_word("escape") {
                arguments.push(self.expression_above(Level::Other)?);
            }
            let converted = Expr {
                kind: ExprKind::Function {
                    name: "similar_to_escape".to_owned(),
                    arguments,
                },
                position,
            };
            let operator = if negated {
                BinaryOperator::RegexNoMatch
            } else {
                BinaryOperator::RegexMatch
            };
            return Ok(Self::binary(expr, operator, converted));
        } else {
            return Err(self.not_supported("SIMILAR TO"));
        };
        Ok(Expr { kind, position })
    }

    fn postfix(&mut self) -> SqlResult<Expr> {
        let entry_depth = self.depth;
        let result = self.casts();
        self.depth = entry_depth;
        result
    }

    fn casts(&mut self) -> SqlResult<Expr> {
        let mut expr = self.primary()?;
        loop {
            if self.take(&TokenKind::DoubleColon) {
                self.depth += 1;
                if self.depth > MAX_NESTING {
                    return Err(too_deep(self.position()));
                }
                if self.postgresql() && self.is_regclass_type() {
                    let position = expr.position;
                    expr = regclass_cast(expr, position);
                    continue;
                }
                let type_position = self.position();
                let (sql_type, array) = self.cast_target()?;
                let position = expr.position;
                if array && crate::arrays::ArrayElement::of(sql_type).is_none() {
                    let Some(cast) = elements_cast(expr, sql_type) else {
                        return Err(crate::arrays::unsupported_element(sql_type).at(type_position));
                    };
                    expr = cast;
                    continue;
                }
                let element_type = sql_type;
                let sql_type = if array {
                    SqlType::Array(
                        crate::arrays::ArrayElement::required(sql_type)
                            .map_err(|error| error.at(type_position))?,
                    )
                } else {
                    sql_type
                };
                expr = Expr {
                    kind: ExprKind::Cast {
                        expr: Box::new(expr),
                        sql_type,
                    },
                    position,
                };
                if array {
                    expr = cast_with_element_modifier(expr, element_type);
                }
            } else if self.peek_kind() == &TokenKind::LeftBracket && self.postgresql() {
                self.depth += 1;
                if self.depth > MAX_NESTING {
                    return Err(too_deep(self.position()));
                }
                expr = self.subscript(expr)?;
            } else if self.peek_kind() == &TokenKind::LeftBracket {
                return Err(self.not_supported("array subscripts"));
            } else if self.is_word("collate") && self.postgresql() {
                expr = self.collate(expr)?;
            } else if self.is_word("collate") && self.sqlserver() {
                self.at += 1;
                self.name()?;
            } else if self.is_word("collate") {
                return Err(self.not_supported("COLLATE"));
            } else if self.is_word("at") && self.is_word_at(1, "time") && self.postgresql() {
                self.at += 2;
                self.expect_word("zone")?;
                self.depth += 1;
                if self.depth > MAX_NESTING {
                    return Err(too_deep(self.position()));
                }
                let zone = self.primary()?;
                let position = expr.position;
                expr = Expr {
                    kind: ExprKind::Function {
                        name: "timezone".to_owned(),
                        arguments: vec![zone, expr],
                    },
                    position,
                };
            } else if self.is_word("at") && self.is_word_at(1, "time") {
                return Err(self.not_supported("AT TIME ZONE"));
            } else {
                return Ok(expr);
            }
        }
    }

    fn primary(&mut self) -> SqlResult<Expr> {
        if let Some(expr) = self.routine_primary()? {
            return Ok(expr);
        }
        if let Some(expr) = self.trigger_primary()? {
            return Ok(expr);
        }
        let token = self.peek();
        let position = token.start;
        if self.mysql()
            && let TokenKind::Word(word) = &token.kind
            && let Some(expr) = self.mysql_primary(word, position)?
        {
            return Ok(expr);
        }
        let literal = |literal| {
            Ok(Expr {
                kind: ExprKind::Literal(literal),
                position,
            })
        };
        if self.sqlite()
            && let TokenKind::Word(word) = &token.kind
            && let Some(expr) = self.sqlite_primary(word, position)?
        {
            return Ok(expr);
        }
        if self.sqlite()
            && let TokenKind::Binary(digits) = &token.kind
        {
            self.at += 1;
            return sqlserver_functions::binary_literal(digits, position);
        }
        if self.oracle()
            && let Some(expr) = self.oracle_primary(&token.kind, position)?
        {
            return Ok(expr);
        }
        if self.sqlserver()
            && let Some(expr) = self.sqlserver_primary(&token.kind, position)?
        {
            return Ok(expr);
        }
        match &token.kind {
            TokenKind::Integer(digits) => {
                self.at += 1;
                match digits.parse::<i64>() {
                    Ok(value) => literal(Literal::Integer(value)),
                    Err(_) => literal(Literal::Decimal((*digits).to_owned())),
                }
            }
            TokenKind::Decimal(text) if self.mysql() || self.sqlite() => {
                self.at += 1;
                Ok(Expr {
                    kind: ExprKind::Cast {
                        expr: Box::new(Expr {
                            kind: ExprKind::Literal(Literal::Decimal((*text).to_owned())),
                            position,
                        }),
                        sql_type: SqlType::Double,
                    },
                    position,
                })
            }
            TokenKind::Decimal(text) => {
                self.at += 1;
                literal(Literal::Decimal((*text).to_owned()))
            }
            TokenKind::String(text) => {
                self.at += 1;
                literal(Literal::String(text.as_ref().to_owned()))
            }
            TokenKind::Parameter(digits) if !self.mysql() && !self.sqlite() => {
                self.parameter(digits, position)
            }
            TokenKind::NumberedParameter(number) => self.sqlite_parameter(*number, position),
            TokenKind::Placeholder => {
                self.placeholders_seen += 1;
                let digits = self.placeholders_seen.to_string();
                self.parameter(&digits, position)
            }
            TokenKind::Variable(name) => self.mysql_variable(name, position),
            TokenKind::LeftParen if self.at_subquery() => {
                self.subquery_expression(crate::ast::SubqueryTest::Scalar, position)
            }
            TokenKind::LeftParen => {
                self.at += 1;
                if self.is_word("values") {
                    return Err(self.not_supported("a VALUES list in an expression"));
                }
                let mut expr = self.expression()?;
                if self.peek_kind() == &TokenKind::Comma {
                    if !self.postgresql() {
                        return Err(self.not_supported("row constructors"));
                    }
                    let mut fields = vec![expr];
                    while self.take(&TokenKind::Comma) {
                        fields.push(self.expression()?);
                    }
                    self.expect(&TokenKind::RightParen)?;
                    let row = Expr {
                        kind: ExprKind::Function {
                            name: crate::records::ROW_FUNCTION.to_owned(),
                            arguments: fields,
                        },
                        position,
                    };
                    return self.field_selections(row);
                }
                self.expect(&TokenKind::RightParen)?;
                expr.position = position;
                let expr = Expr {
                    kind: expr.kind,
                    position,
                };
                if self.postgresql() {
                    return self.field_selections(expr);
                }
                Ok(expr)
            }
            TokenKind::QuotedIdentifier(_) => self.column_or_function(),
            TokenKind::Word(word) => match word.as_ref() {
                "null" => {
                    self.at += 1;
                    literal(Literal::Null)
                }
                "true" => {
                    self.at += 1;
                    literal(Literal::Boolean(true))
                }
                "false" => {
                    self.at += 1;
                    literal(Literal::Boolean(false))
                }
                "cast" => self.cast(),
                "case" => self.case(),
                "exists" if self.peek_kind_at(1) == &TokenKind::LeftParen => {
                    self.at += 1;
                    self.subquery_expression(crate::ast::SubqueryTest::Exists, position)
                }
                "array"
                    if self.postgresql()
                        && (self.peek_kind_at(1) == &TokenKind::LeftParen
                            || self.peek_kind_at(1) == &TokenKind::LeftBracket) =>
                {
                    self.array_constructor()
                }
                "row" if self.postgresql() && self.peek_kind_at(1) == &TokenKind::LeftParen => {
                    self.at += 2;
                    let mut fields = Vec::new();
                    if !self.take(&TokenKind::RightParen) {
                        loop {
                            fields.push(self.expression()?);
                            if !self.take(&TokenKind::Comma) {
                                break;
                            }
                        }
                        self.expect(&TokenKind::RightParen)?;
                    }
                    Ok(Expr {
                        kind: ExprKind::Function {
                            name: crate::records::ROW_FUNCTION.to_owned(),
                            arguments: fields,
                        },
                        position,
                    })
                }
                "array" | "row"
                    if self.peek_kind_at(1) == &TokenKind::LeftParen
                        || self.peek_kind_at(1) == &TokenKind::LeftBracket =>
                {
                    Err(self.not_supported("ARRAY and ROW constructors"))
                }
                "current_date" | "current_timestamp" | "localtimestamp" if self.postgresql() => {
                    self.at += 1;
                    self.clock_function(word, position)
                }
                "current_time" | "localtime" if self.postgresql() => {
                    self.at += 1;
                    self.clock_function(word, position)
                }
                "current_user" | "session_user" | "user" | "current_role" => {
                    self.at += 1;
                    let name = if word == "session_user" {
                        "session_user"
                    } else {
                        "current_user"
                    };
                    self.session_value(name, position)
                }
                "current_catalog" | "current_schema"
                    if self.peek_kind_at(1) != &TokenKind::LeftParen =>
                {
                    self.at += 1;
                    let name = if word == "current_catalog" {
                        "current_catalog"
                    } else {
                        "current_schema"
                    };
                    self.session_value(name, position)
                }
                "interval" | "time"
                    if !self.typed_literal_allowed(word)
                        && matches!(self.peek_kind_at(1), TokenKind::String(_)) =>
                {
                    Err(
                        SqlError::not_supported(format!("type {word} is not supported yet"))
                            .at(position),
                    )
                }
                "integer" | "int" | "int4" | "smallint" | "int2" | "bigint" | "int8" | "real"
                | "float4" | "float8" | "boolean" | "bool" | "text" | "varchar" | "double"
                | "numeric" | "decimal" | "date" | "timestamp" | "timestamptz" | "json"
                | "jsonb" | "time" | "timetz" | "interval" | "bytea" | "uuid" | "bpchar"
                | "char" | "character"
                    if self.typed_literal_allowed(word)
                        && (matches!(self.peek_kind_at(1), TokenKind::String(_))
                            || (word == "double"
                                && self.is_word_at(1, "precision")
                                && matches!(self.peek_kind_at(2), TokenKind::String(_)))
                            || self.typed_literal_with_modifiers(word)) =>
                {
                    let sql_type = if self.mysql() && word == "timestamp" {
                        self.at += 1;
                        SqlType::Timestamp(None)
                    } else {
                        self.sql_type()?
                    };
                    let TokenKind::String(text) = self.peek_kind() else {
                        return Err(self.unexpected());
                    };
                    let operand = Expr {
                        kind: ExprKind::Literal(Literal::String(text.as_ref().to_owned())),
                        position: self.position(),
                    };
                    self.at += 1;
                    if self.oracle()
                        && matches!(sql_type, SqlType::Timestamp(_) | SqlType::TimestampTz(_))
                    {
                        return Ok(oracle::ansi_literal(word == "date", operand, position));
                    }
                    let sql_type = match sql_type {
                        SqlType::Interval {
                            fields: None,
                            precision,
                        } if self.postgresql() => {
                            let fields = self.interval_fields()?;
                            let precision = match fields {
                                Some(fields) if fields.ends_in_seconds() && precision.is_none() => {
                                    self.time_precision(position, "interval")?
                                }
                                _ => precision,
                            };
                            SqlType::Interval { fields, precision }
                        }
                        other => other,
                    };
                    Ok(Expr {
                        kind: ExprKind::Cast {
                            expr: Box::new(operand),
                            sql_type,
                        },
                        position,
                    })
                }
                _ if self.reserved(word) => Err(self.unexpected()),
                _ => self.column_or_function(),
            },
            _ => Err(self.unexpected()),
        }
    }

    fn field_selections(&mut self, mut expr: Expr) -> SqlResult<Expr> {
        while self.peek_kind() == &TokenKind::Dot {
            self.at += 1;
            if self.is_operator("*") {
                return Err(self.not_supported("(expression).* outside FROM"));
            }
            let field = self.name()?;
            self.depth += 1;
            if self.depth > MAX_NESTING {
                return Err(too_deep(field.position));
            }
            let position = expr.position;
            expr = Expr {
                kind: ExprKind::Function {
                    name: crate::records::FIELD_FUNCTION.to_owned(),
                    arguments: vec![
                        expr,
                        Expr {
                            kind: ExprKind::Literal(Literal::String(field.text)),
                            position: field.position,
                        },
                    ],
                },
                position,
            };
        }
        Ok(expr)
    }

    fn is_regclass_type(&mut self) -> bool {
        let qualified = self.is_word("pg_catalog") && self.peek_kind_at(1) == &TokenKind::Dot;
        let at = if qualified { 2 } else { 0 };
        let named = match self.peek_kind_at(at) {
            TokenKind::Word(word) => word == "regclass",
            TokenKind::QuotedIdentifier(word) => word == "regclass",
            _ => false,
        };
        if !named
            || matches!(self.peek_kind_at(at + 1), TokenKind::LeftBracket)
            || self.is_word_at(at + 1, "array")
        {
            return false;
        }
        self.at += at + 1;
        true
    }

    fn column_or_function(&mut self) -> SqlResult<Expr> {
        let first = self.name()?;
        if self.peek_kind() == &TokenKind::LeftParen {
            return self.function(first);
        }
        if self.postgresql()
            && first.text == "pg_catalog"
            && self.peek_kind() == &TokenKind::Dot
            && self.peek_kind_at(2) == &TokenKind::LeftParen
        {
            self.at += 1;
            let name = self.name()?;
            return self.function(name);
        }
        if self.oracle()
            && self.peek_kind() == &TokenKind::Dot
            && (self.is_word_at(1, "nextval") || self.is_word_at(1, "currval"))
            && self.peek_kind_at(2) != &TokenKind::LeftParen
        {
            let function = if self.is_word_at(1, "nextval") {
                "oracle_nextval"
            } else {
                "oracle_currval"
            };
            self.at += 2;
            return Ok(Expr {
                kind: ExprKind::Function {
                    name: function.to_owned(),
                    arguments: vec![Expr {
                        kind: ExprKind::Literal(Literal::String(first.text)),
                        position: first.position,
                    }],
                },
                position: first.position,
            });
        }
        if self.postgresql()
            && first.text == crate::schemas::INFORMATION_SCHEMA
            && self.peek_kind() == &TokenKind::Dot
            && self.is_word_at(1, crate::records::EXPAND_ARRAY_FUNCTION)
            && self.peek_kind_at(2) == &TokenKind::LeftParen
        {
            self.at += 1;
            let name = self.name()?;
            return self.function(name);
        }
        if self.take(&TokenKind::Dot) {
            if self.is_operator("*") {
                return Err(self.not_supported("a qualified * outside the select list"));
            }
            let column = self.name()?;
            if self.sqlserver() && self.peek_kind() == &TokenKind::LeftParen {
                return self.sqlserver_user_function(first, column);
            }
            if self.postgresql() && self.peek_kind() == &TokenKind::LeftParen {
                return self.function(Name {
                    text: crate::schemas::written_qualified(&first.text, &column.text),
                    position: first.position,
                });
            }
            if self.oracle() && self.peek_kind() == &TokenKind::LeftParen {
                let qualifier = if swap_uniform_case(&first.text) == self.oracle_user() {
                    self.oracle_user_schema()
                } else {
                    first.text
                };
                return self.function(Name {
                    text: crate::schemas::written_qualified(&qualifier, &column.text),
                    position: first.position,
                });
            }
            if self.peek_kind() == &TokenKind::Dot || self.peek_kind() == &TokenKind::LeftParen {
                return Err(self.not_supported("names with more than two parts"));
            }
            return Ok(Expr {
                kind: ExprKind::Column {
                    qualifier: Some(first.text),
                    name: column.text,
                },
                position: first.position,
            });
        }
        Ok(Expr {
            kind: ExprKind::Column {
                qualifier: None,
                name: first.text,
            },
            position: first.position,
        })
    }

    fn clock_function(&mut self, word: &str, position: usize) -> SqlResult<Expr> {
        if word != "current_date" && self.take(&TokenKind::LeftParen) {
            let TokenKind::Integer(_) = self.peek_kind() else {
                return Err(self.unexpected());
            };
            self.at += 1;
            self.expect(&TokenKind::RightParen)?;
        }
        Ok(Expr {
            kind: ExprKind::Function {
                name: word.to_owned(),
                arguments: Vec::new(),
            },
            position,
        })
    }

    fn extract(&mut self, name: Name) -> SqlResult<Expr> {
        self.expect(&TokenKind::LeftParen)?;
        let field = match self.peek_kind() {
            TokenKind::Word(word) => word.to_ascii_lowercase(),
            TokenKind::QuotedIdentifier(word) if self.postgresql() => word.clone(),
            TokenKind::String(text) if self.postgresql() => text.to_string(),
            _ => return Err(self.unexpected()),
        };
        let field_position = self.position();
        let internal = if self.mysql() {
            if !crate::datetime::MYSQL_UNITS.contains(&field.as_str()) {
                return Err(self.unexpected());
            }
            "mysql_extract"
        } else if self.oracle() {
            if !crate::datetime::ORACLE_FIELDS.contains(&field.as_str()) {
                return Err(
                    SqlError::new(SqlState::SYNTAX_ERROR, "missing right parenthesis")
                        .with_oracle_code(907)
                        .at(field_position),
                );
            }
            "oracle_extract"
        } else {
            "extract"
        };
        self.at += 1;
        self.expect_word("from")?;
        let source = self.expression()?;
        self.expect(&TokenKind::RightParen)?;
        Ok(Expr {
            kind: ExprKind::Function {
                name: internal.to_owned(),
                arguments: vec![
                    Expr {
                        kind: ExprKind::Literal(Literal::String(field)),
                        position: field_position,
                    },
                    source,
                ],
            },
            position: name.position,
        })
    }

    fn function(&mut self, name: Name) -> SqlResult<Expr> {
        let shown = name.text.clone();
        let expr = self.function_call(name)?;
        self.call_clauses(expr, &shown)
    }

    fn function_call(&mut self, name: Name) -> SqlResult<Expr> {
        if name.text == "extract" && !self.sqlite() {
            return self.extract(name);
        }
        if self.mysql()
            && let Some(expr) = self.mysql_function(&name)?
        {
            return Ok(expr);
        }
        if self.sqlite()
            && let Some(expr) = self.sqlite_function(&name)?
        {
            return Ok(expr);
        }
        if self.oracle()
            && let Some(expr) = self.oracle_function(&name)?
        {
            return Ok(expr);
        }
        if self.sqlserver()
            && let Some(expr) = self.sqlserver_function(&name)?
        {
            return Ok(expr);
        }
        if self.postgresql()
            && let Some(expr) = self.reg_function_cast(&name)?
        {
            return Ok(expr);
        }
        let text = name.text.as_str();
        let postgresql_only = (!self.postgresql() && is_postgresql_only(text))
            || (self.mysql() && NOT_IN_MYSQL.contains(&text))
            || (self.sqlite() && NOT_IN_SQLITE.contains(&text));
        if postgresql_only || INTERNAL_FUNCTIONS.contains(&name.text.as_str()) {
            return Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("function {}() does not exist", name.text),
            )
            .with_hint(
                "No function matches the given name and argument types. You might need to add explicit type casts.",
            )
            .at(name.position));
        }
        if text == "substring" && self.postgresql() {
            return self.substring(name);
        }
        if text == "trim" && self.postgresql() {
            return self.postgresql_trim(name);
        }
        if text == "string_agg" && self.postgresql() {
            return self.string_agg(name);
        }
        if self.postgresql()
            && let Some(expr) = self.postgresql_function(&name)?
        {
            return Ok(expr);
        }
        if !self.sqlite()
            && !self.oracle()
            && let Some(expr) = self.session_function(&name)?
        {
            return Ok(expr);
        }
        if let Some(function) = AggregateFunction::named(&name.text) {
            let expr = self.aggregate(name, function)?;
            return Ok(if self.sqlite() {
                sqlite::scalar_min_max(expr)
            } else {
                expr
            });
        }
        if !self.mysql()
            && !self.sqlite()
            && let Some(function) = AggregateFunction::ordered_set_named(&name.text)
        {
            return self.ordered_set_aggregate(name, function);
        }
        if AGGREGATES.contains(&name.text.as_str()) {
            return Err(SqlError::not_supported(format!(
                "aggregate functions such as {}() are not supported yet",
                name.text
            ))
            .at(name.position));
        }
        self.expect(&TokenKind::LeftParen)?;
        let mut arguments = Vec::new();
        let mut named: Vec<(Name, Expr)> = Vec::new();
        if !self.take(&TokenKind::RightParen) {
            if self.is_word("distinct") || self.is_operator("*") {
                return Err(self.not_supported("DISTINCT and * arguments"));
            }
            loop {
                if let Some(parameter) = self.named_argument_name()? {
                    let value = self.expression()?;
                    named.push((parameter, value));
                } else if let Some((parameter, _)) = named.first() {
                    return Err(SqlError::syntax(
                        "positional argument cannot follow named argument",
                        parameter.position,
                    ));
                } else {
                    arguments.push(self.expression()?);
                }
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
        }
        if self.is_word("over")
            && named.is_empty()
            && let Some(function) = crate::window::WindowFunction::named(&name.text)
        {
            return self.window_call(function, arguments, None, &name);
        }
        if self.is_word("filter")
            && self.peek_kind_at(1) == &TokenKind::LeftParen
            && named.is_empty()
            && let Some(function) = crate::window::WindowFunction::named(&name.text)
        {
            let filter = self.aggregate_filter()?;
            return self.window_call(function, arguments, Some(filter), &name);
        }
        if !named.is_empty() {
            if self.postgresql()
                && !NAMED_PARAMETERS
                    .iter()
                    .any(|(known, _)| *known == name.text)
            {
                let mut names: Vec<Option<String>> = vec![None; arguments.len()];
                for (parameter, value) in named {
                    names.push(Some(parameter.text));
                    arguments.push(value);
                }
                return Ok(crate::routines::marked_call(
                    name.text,
                    names,
                    arguments,
                    name.position,
                ));
            }
            arguments = named_arguments(&name, arguments, named)?;
        }
        Ok(Expr {
            kind: ExprKind::Function {
                name: name.text,
                arguments,
            },
            position: name.position,
        })
    }

    fn named_argument_name(&mut self) -> SqlResult<Option<Name>> {
        if !(self.postgresql() || self.oracle()) || self.peek_kind_at(1) != &TokenKind::Operator("=>") {
            return Ok(None);
        }
        let parameter = self.name()?;
        self.at += 1;
        Ok(Some(parameter))
    }

    fn substring(&mut self, name: Name) -> SqlResult<Expr> {
        self.expect(&TokenKind::LeftParen)?;
        let mut arguments = vec![self.expression()?];
        if self.take_word("similar") {
            let pattern = self.expression()?;
            self.expect_word("escape")?;
            let escape = self.expression()?;
            self.expect(&TokenKind::RightParen)?;
            let position = pattern.position;
            arguments.push(Expr {
                kind: ExprKind::Function {
                    name: "similar_substring_pattern".to_owned(),
                    arguments: vec![pattern, escape],
                },
                position,
            });
            return Ok(Expr {
                kind: ExprKind::Function {
                    name: name.text,
                    arguments,
                },
                position: name.position,
            });
        }
        if self.take_word("from") {
            arguments.push(self.expression()?);
            if self.take_word("for") {
                arguments.push(self.expression()?);
            }
        } else if self.take_word("for") {
            arguments.push(Expr {
                kind: ExprKind::Literal(Literal::Integer(1)),
                position: name.position,
            });
            arguments.push(self.expression()?);
        } else {
            while self.take(&TokenKind::Comma) {
                arguments.push(self.expression()?);
            }
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(Expr {
            kind: ExprKind::Function {
                name: name.text,
                arguments,
            },
            position: name.position,
        })
    }

    fn aggregate(&mut self, name: Name, function: AggregateFunction) -> SqlResult<Expr> {
        self.expect(&TokenKind::LeftParen)?;
        let (argument, distinct) = if self.is_operator("*") {
            self.at += 1;
            if function != AggregateFunction::Count {
                return Err(SqlError::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!(
                        "{}(*) specified, but {} is not a parameterless aggregate function",
                        name.text, name.text
                    ),
                )
                .at(name.position));
            }
            (None, false)
        } else if self.peek_kind() == &TokenKind::RightParen {
            return Err(SqlError::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!(
                    "{}(*) must be used to call a parameterless aggregate function",
                    name.text
                ),
            )
            .at(name.position));
        } else {
            let distinct = self.take_word("distinct");
            if !distinct {
                self.take_word("all");
            }
            let first = self.expression()?;
            if self.peek_kind() == &TokenKind::Comma {
                let mut arguments = vec![first];
                while self.take(&TokenKind::Comma) {
                    arguments.push(self.expression()?);
                }
                self.expect(&TokenKind::RightParen)?;
                return Ok(Expr {
                    kind: ExprKind::Function {
                        name: name.text,
                        arguments,
                    },
                    position: name.position,
                });
            }
            (Some(Box::new(self.aggregate_order(first)?)), distinct)
        };
        self.expect(&TokenKind::RightParen)?;
        Ok(Expr {
            kind: ExprKind::Aggregate {
                function,
                argument,
                distinct,
            },
            position: name.position,
        })
    }

    fn aggregate_order(&mut self, argument: Expr) -> SqlResult<Expr> {
        if !self.take_word("order") {
            return Ok(argument);
        }
        self.expect_word("by")?;
        let order = self.order_items()?;
        Ok(crate::aggregate::ordered_arguments(argument, order))
    }

    fn cast(&mut self) -> SqlResult<Expr> {
        let position = self.position();
        self.at += 1;
        self.expect(&TokenKind::LeftParen)?;
        let expr = self.expression()?;
        self.expect_word("as")?;
        if self.postgresql() && self.is_regclass_type() {
            self.expect(&TokenKind::RightParen)?;
            return Ok(regclass_cast(expr, position));
        }
        let sql_type = self.cast_type()?;
        self.expect(&TokenKind::RightParen)?;
        Ok(Expr {
            kind: ExprKind::Cast {
                expr: Box::new(expr),
                sql_type,
            },
            position,
        })
    }

    fn reg_function_cast(&mut self, name: &Name) -> SqlResult<Option<Expr>> {
        let Some(sql_type @ SqlType::Catalog(crate::pg_catalog::CatalogType::Reg(kind))) =
            crate::pg_catalog::cast_type_named(&name.text)
        else {
            return Ok(None);
        };
        if self.peek_kind_at(1) == &TokenKind::RightParen {
            return Ok(None);
        }
        self.expect(&TokenKind::LeftParen)?;
        let expr = self.expression()?;
        self.expect(&TokenKind::RightParen)?;
        Ok(Some(if kind == crate::pg_catalog::RegKind::Class {
            regclass_cast(expr, name.position)
        } else {
            Expr {
                kind: ExprKind::Cast {
                    expr: Box::new(expr),
                    sql_type,
                },
                position: name.position,
            }
        }))
    }

    fn case(&mut self) -> SqlResult<Expr> {
        let position = self.position();
        self.at += 1;
        let operand = if self.is_word("when") {
            None
        } else {
            Some(Box::new(self.expression()?))
        };
        let mut branches = Vec::new();
        while self.take_word("when") {
            let condition = self.expression()?;
            let condition = if operand.is_none() {
                self.condition(condition)
            } else {
                condition
            };
            self.expect_word("then")?;
            let result = self.expression()?;
            branches.push((condition, result));
        }
        if branches.is_empty() {
            return Err(self.unexpected());
        }
        let otherwise = if self.take_word("else") {
            Some(Box::new(self.expression()?))
        } else {
            None
        };
        self.expect_word("end")?;
        Ok(Expr {
            kind: ExprKind::Case {
                operand,
                branches,
                otherwise,
            },
            position,
        })
    }

    fn binary_operand(&mut self) -> SqlResult<Expr> {
        self.expression_above(Level::Other)
    }
}

fn regclass_cast(expr: Expr, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Function {
            name: "regclass_cast".to_owned(),
            arguments: vec![expr],
        },
        position,
    }
}

fn is_clause_word(word: &str) -> bool {
    matches!(
        word,
        "where"
            | "order"
            | "limit"
            | "offset"
            | "group"
            | "having"
            | "join"
            | "inner"
            | "left"
            | "right"
            | "full"
            | "cross"
            | "natural"
            | "set"
            | "returning"
            | "union"
            | "intersect"
            | "except"
            | "fetch"
            | "for"
            | "window"
            | "using"
            | "on"
            | "values"
            | "default"
            | "select"
            | "into"
            | "is"
            | "isnull"
            | "notnull"
            | "between"
            | "like"
            | "ilike"
            | "similar"
            | "escape"
    )
}

fn truth(expr: Expr, test: TruthTest, negated: bool) -> ExprKind {
    ExprKind::IsTruth {
        expr: Box::new(expr),
        test,
        negated,
    }
}

fn cast_with_element_modifier(array_cast: Expr, element_type: SqlType) -> Expr {
    if !matches!(
        element_type,
        SqlType::Varchar(Some(_)) | SqlType::Bpchar(Some(_)) | SqlType::Numeric(Some(_))
    ) {
        return array_cast;
    }
    let position = array_cast.position;
    let element_target = Expr {
        kind: ExprKind::Cast {
            expr: Box::new(Expr {
                kind: ExprKind::Literal(Literal::Null),
                position,
            }),
            sql_type: element_type,
        },
        position,
    };
    Expr {
        kind: ExprKind::Function {
            name: "array_element_cast".to_owned(),
            arguments: vec![array_cast, element_target],
        },
        position,
    }
}

fn elements_cast(expr: Expr, sql_type: SqlType) -> Option<Expr> {
    let position = expr.position;
    let ExprKind::Function { name, arguments } = expr.kind else {
        return None;
    };
    if name != "array_constructor" {
        return None;
    }
    let arguments = arguments
        .into_iter()
        .map(|item| {
            let position = item.position;
            Expr {
                kind: ExprKind::Cast {
                    expr: Box::new(item),
                    sql_type,
                },
                position,
            }
        })
        .collect();
    Some(Expr {
        kind: ExprKind::Function { name, arguments },
        position,
    })
}

fn named_arguments(
    function: &Name,
    positional: Vec<Expr>,
    named: Vec<(Name, Expr)>,
) -> SqlResult<Vec<Expr>> {
    let undefined = || {
        SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("function {}() does not exist", function.text),
        )
        .with_hint(
            "No function matches the given name and argument types. You might need to add explicit argument names or casts.",
        )
        .at(function.position)
    };
    let Some((_, parameters)) = NAMED_PARAMETERS
        .iter()
        .find(|(name, _)| *name == function.text)
    else {
        return Err(undefined());
    };
    if positional.len() > parameters.len() {
        return Err(undefined());
    }
    let mut slots: Vec<Option<Expr>> = positional.into_iter().map(Some).collect();
    slots.resize(parameters.len(), None);
    for (parameter, value) in named {
        let Some(at) = parameters.iter().position(|known| *known == parameter.text) else {
            return Err(undefined());
        };
        if slots[at].is_some() {
            return Err(SqlError::syntax(
                format!("argument name \"{}\" used more than once", parameter.text),
                parameter.position,
            ));
        }
        slots[at] = Some(value);
    }
    let defaulted = function.text == "make_interval";
    let last_given = slots
        .iter()
        .rposition(Option::is_some)
        .map_or(0, |at| at + 1);
    let mut arguments = Vec::with_capacity(slots.len());
    for (at, slot) in slots.into_iter().enumerate() {
        match slot {
            Some(value) => arguments.push(value),
            None if defaulted => arguments.push(Expr {
                kind: ExprKind::Literal(Literal::Integer(0)),
                position: function.position,
            }),
            None if at >= last_given && function.text == "make_timestamptz" && at == 6 => {}
            None => return Err(undefined()),
        }
    }
    Ok(arguments)
}

fn too_deep(position: usize) -> SqlError {
    SqlError::new(
        SqlState::STATEMENT_TOO_COMPLEX,
        "stack depth limit exceeded",
    )
    .with_hint("The statement nests expressions, or chains operators, more than 64 levels deep.")
    .at(position)
}

fn multiple_primary_keys(table: &Name, position: usize) -> SqlError {
    SqlError::new(
        SqlState::INVALID_TABLE_DEFINITION,
        format!(
            "multiple primary keys for table \"{}\" are not allowed",
            table.text
        ),
    )
    .at(position)
}
