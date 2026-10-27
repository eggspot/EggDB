mod mysql;
mod oracle;
mod sqlite;

pub use oracle::ORACLE_COMPATIBLE_VERSION;
pub(crate) use oracle::{is_dictionary_view, is_reserved as is_oracle_reserved};

pub use sqlite::SQLITE_COMPATIBLE_VERSION;
pub(crate) use sqlite::is_schema_table;

use crate::ast::{
    AggregateFunction, BinaryOperator, ColumnDefinition, CreateTable, Delete, Expr, ExprKind,
    FromClause, Insert, InsertSource, InsertValue, Join, JoinCondition, JoinKind, Literal, Name,
    OnDuplicate, OrderItem, Select, SelectItem, Statement, TableReference, TruthTest,
    UnaryOperator, Update,
};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::{Token, TokenKind, swap_uniform_case};
use crate::session::Parameter;
use crate::settings::Settings;
use crate::types::SqlType;
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

pub(crate) struct Parser<'t> {
    text: &'t str,
    tokens: &'t [Token<'t>],
    at: usize,
    depth: usize,
    parameters: Parameters<'t>,
    settings: Option<&'t Settings>,
    dialect: Dialect,
    placeholders_seen: usize,
}

#[derive(Clone, Copy)]
pub(crate) enum Parameters<'t> {
    Absent,
    Values(&'t [Parameter]),
    Types(&'t [Option<SqlType>]),
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
        }
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

    fn reserved(&self, word: &str) -> bool {
        match self.dialect {
            Dialect::PostgreSql => is_reserved(word),
            Dialect::MySql => mysql::is_reserved(word),
            Dialect::Sqlite => sqlite::is_reserved(word),
            Dialect::Oracle => oracle::is_reserved(word),
        }
    }

    fn clause_word(&self, word: &str) -> bool {
        is_clause_word(word)
            || self.mysql() && mysql::is_clause_word(word)
            || self.sqlite() && sqlite::is_clause_word(word)
            || self.oracle() && oracle::is_clause_word(word)
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
        if self.oracle() || !self.is_word("insert") || !self.is_word_at(1, "into") {
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
        match self.peek_kind() {
            TokenKind::Semicolon => {
                self.at += 1;
                Ok(statement)
            }
            TokenKind::End => Ok(statement),
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
            self.check_database(&name)?;
            return Ok(table);
        }
        if self.sqlite() && self.peek_kind() == &TokenKind::Dot {
            self.at += 1;
            let table = self.name()?;
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
            if swap_uniform_case(&name.text) != self.oracle_user() {
                return Err(SqlError::not_supported(
                    "schemas other than the session user's are not supported yet",
                )
                .at(name.position));
            }
            self.at += 1;
            return self.name();
        }
        if self.peek_kind() == &TokenKind::Dot {
            if name.text == "public" {
                self.at += 1;
                return self.name();
            }
            return Err(
                SqlError::not_supported("schemas other than public are not supported yet")
                    .at(name.position),
            );
        }
        Ok(name)
    }

    fn statement_body(&mut self) -> SqlResult<Statement> {
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
        let TokenKind::Word(word) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        match word.as_ref() {
            "create" => self.create(),
            "drop" => self.drop(),
            "insert" => self.insert(),
            "select" => self.select().map(Statement::Select),
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
            "with" => Err(self.not_supported("WITH")),
            "values" | "table" => Err(self.not_supported("a VALUES or TABLE statement")),
            "set" => self.set(),
            "show" => self.show(),
            "reset" => self.reset(),
            "alter" | "truncate" | "savepoint" | "release" | "explain" | "copy" | "grant"
            | "revoke" | "prepare" | "execute" | "analyze" | "vacuum" | "discard" | "listen"
            | "notify" | "unlisten" | "deallocate" => {
                Err(self.not_supported(&word.to_ascii_uppercase()))
            }
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
            return Err(self.not_supported("SET TRANSACTION"));
        }
        if self.is_word("role") || self.is_word("authorization") || self.is_word("constraints") {
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

    fn reset(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        if self.take_word("all") {
            return Ok(Statement::Reset { name: None });
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
            Parameters::Absent => return Err(missing()),
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
        Ok(match declared_type {
            Some(sql_type) => Expr {
                kind: ExprKind::Cast {
                    expr: Box::new(literal),
                    sql_type,
                },
                position,
            },
            None => literal,
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

    fn session_function(&mut self, name: &Name) -> SqlResult<Option<Expr>> {
        let known: &'static str = match name.text.as_str() {
            "version" => "version",
            "current_database" => "current_database",
            "current_schema" => "current_schema",
            "current_user" => "current_user",
            "session_user" => "session_user",
            "current_setting" => "current_setting",
            _ => return Ok(None),
        };
        self.expect(&TokenKind::LeftParen)?;
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
                self.expect_word("level")?;
                if self.take_word("serializable") {
                } else if self.take_word("repeatable") || self.take_word("read") {
                    if !(self.take_word("read") || self.take_word("committed")) {
                        self.take_word("uncommitted");
                    }
                } else {
                    return Err(self.unexpected());
                }
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
                    "index"
                        | "unique"
                        | "view"
                        | "sequence"
                        | "schema"
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
        let mut integer_columns = Vec::new();
        loop {
            if self.sqlite() && self.sqlite_ignored_table_constraint()? {
            } else if self.is_word("primary") || self.is_word("constraint") {
                let position = self.position();
                if self.take_word("constraint") {
                    self.name()?;
                }
                if self.take_word("primary") {
                    self.expect_word("key")?;
                    let key = self.name_list()?;
                    if primary_key.is_some() {
                        return Err(multiple_primary_keys(&name, position));
                    }
                    primary_key = Some((key, position));
                    if self.sqlite() {
                        self.sqlite_conflict_clause()?;
                    }
                } else {
                    return Err(self.table_constraint_not_supported());
                }
            } else if self.mysql() && self.mysql_index_entry()? {
            } else if self.is_word("unique")
                || self.is_word("check")
                || self.is_word("foreign")
                || self.is_word("exclude")
                || self.is_word("like")
            {
                return Err(self.table_constraint_not_supported());
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
        if self.mysql() {
            self.mysql_table_options()?;
        }
        if self.sqlite() {
            self.sqlite_table_finish(&name, &mut columns, &primary_key, &integer_columns)?;
        }
        if self.is_word("partition") || self.is_word("inherits") || self.is_word("with") {
            return Err(self.not_supported("table options"));
        }
        Ok(Statement::CreateTable(CreateTable {
            name,
            if_not_exists,
            columns,
            primary_key,
        }))
    }

    fn table_constraint_not_supported(&self) -> SqlError {
        self.not_supported("a table constraint other than PRIMARY KEY")
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
        let sql_type = self.sql_type()?;
        let mut column = ColumnDefinition {
            name,
            sql_type,
            not_null: false,
            primary_key: false,
            default: None,
            auto_increment: false,
        };
        loop {
            if self.mysql() && self.mysql_column_option(&mut column)? {
                continue;
            }
            if self.take_word("constraint") {
                self.name()?;
            }
            if self.take_word("not") {
                self.expect_word("null")?;
                column.not_null = true;
            } else if self.take_word("null") {
                column.not_null = false;
            } else if self.take_word("primary") {
                self.expect_word("key")?;
                column.primary_key = true;
            } else if self.is_word("default") {
                self.at += 1;
                let start = self.position();
                let expr = self.binary_operand()?;
                let end = self.tokens[self.at.saturating_sub(1)].end;
                let source = self.text.get(start..end).unwrap_or("").to_owned();
                column.default = Some((expr, source));
            } else if self.oracle() && self.is_word("generated") {
                self.oracle_identity(&mut column)?;
            } else if ["unique", "check", "references", "generated", "collate"]
                .iter()
                .any(|word| self.is_word(word))
            {
                return Err(self.not_supported(
                    "a column constraint other than PRIMARY KEY, NOT NULL and DEFAULT",
                ));
            } else {
                return Ok(column);
            }
        }
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
            "numeric" | "decimal" | "dec" | "timestamp" | "timestamptz" | "date" | "time"
            | "timetz" | "interval" | "json" | "jsonb" | "uuid" | "bytea" | "serial"
            | "serial4" | "bigserial" | "serial8" | "smallserial" | "serial2" | "character"
            | "char" | "bpchar" | "money" | "inet" | "cidr" | "xml" | "bit" | "point" => {
                return Err(
                    SqlError::not_supported(format!("type {word} is not supported yet"))
                        .at(token.start),
                );
            }
            _ => {
                return Err(SqlError::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!("type \"{word}\" does not exist"),
                )
                .at(token.start));
            }
        };
        if self.peek_kind() == &TokenKind::LeftBracket || self.is_word("array") {
            return Err(self.not_supported("array types"));
        }
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
        if self.is_word("cascade") {
            return Err(self.not_supported("DROP TABLE ... CASCADE"));
        }
        self.take_word("restrict");
        Ok(Statement::DropTable { names, if_exists })
    }

    fn table_reference(&mut self) -> SqlResult<TableReference> {
        if self.peek_kind() == &TokenKind::LeftParen {
            return Err(self.not_supported("a subquery in FROM"));
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
        Ok(TableReference { name, alias })
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
        } else {
            self.expect_word("into")?;
        }
        let table = self.table_reference()?;
        let columns = if self.peek_kind() == &TokenKind::LeftParen
            && !self.is_word_at(1, "select")
            && !self.is_word_at(1, "values")
        {
            Some(self.name_list()?)
        } else {
            None
        };
        if self.is_word("overriding") {
            return Err(self.not_supported("OVERRIDING"));
        }
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
        } else if self.is_word("select") || self.peek_kind() == &TokenKind::LeftParen {
            return Err(self.not_supported("INSERT ... SELECT"));
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
        if self.take_word("returning") {
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
        if self.is_word("from") {
            return Err(self.not_supported("UPDATE ... FROM"));
        }
        let filter = self.filter()?;
        let returning = self.returning()?;
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
        let filter = self.filter()?;
        let returning = self.returning()?;
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

    fn select(&mut self) -> SqlResult<Select> {
        self.at += 1;
        let distinct = self.take_word("distinct");
        if distinct && self.is_word("on") {
            return Err(self.not_supported("DISTINCT ON"));
        }
        if !distinct {
            self.take_word("all");
        }
        let called = matches!(self.peek_kind(), TokenKind::Word(_))
            && self.peek_kind_at(1) == &TokenKind::LeftParen;
        let items = if self.is_clause_end() && !(self.sqlite() && called) {
            if self.sqlite() || self.oracle() {
                return Err(self.unexpected());
            }
            Vec::new()
        } else {
            self.select_items()?
        };
        if self.is_word("into") {
            return Err(self.not_supported("SELECT INTO"));
        }
        let from = if self.take_word("from") {
            if self.mysql() && self.take_word("dual") || self.oracle() && self.oracle_dual() {
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
        if self.is_word("window") {
            return Err(self.not_supported("WINDOW"));
        }
        if self.is_word("union")
            || self.is_word("intersect")
            || self.is_word("except")
            || self.oracle() && self.is_word("minus")
        {
            return Err(self.not_supported("UNION, INTERSECT, MINUS and EXCEPT"));
        }
        if self.oracle() && (self.is_word("connect") || self.is_word("start")) {
            return Err(self.not_supported("hierarchical queries (CONNECT BY)"));
        }
        let mut order_by = Vec::new();
        if self.take_word("order") {
            self.expect_word("by")?;
            loop {
                let expr = self.expression()?;
                let descending = if self.take_word("desc") {
                    true
                } else {
                    self.take_word("asc");
                    false
                };
                if self.is_word("using") {
                    return Err(self.not_supported("ORDER BY ... USING"));
                }
                let nulls_first = if self.take_word("nulls") {
                    if self.take_word("first") {
                        Some(true)
                    } else {
                        self.expect_word("last")?;
                        Some(false)
                    }
                } else {
                    None
                };
                order_by.push(OrderItem {
                    expr,
                    descending,
                    nulls_first,
                });
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
        }
        let mut limit = None;
        let mut offset = None;
        loop {
            if self.oracle() && self.is_word("limit") {
                return Err(SqlError::syntax(
                    "SQL command not properly ended",
                    self.position(),
                ));
            }
            if self.oracle() && self.is_word("fetch") {
                if limit.is_some() {
                    return Err(self.unexpected());
                }
                limit = Some(self.oracle_fetch()?);
                continue;
            }
            if self.take_word("limit") {
                if limit.is_some() {
                    return Err(SqlError::syntax(
                        "multiple LIMIT clauses not allowed",
                        self.tokens[self.at - 1].start,
                    ));
                }
                limit = Some(if self.is_word("all") {
                    let position = self.position();
                    self.at += 1;
                    Expr {
                        kind: ExprKind::Literal(Literal::Null),
                        position,
                    }
                } else {
                    self.expression()?
                });
                if (self.mysql() || self.sqlite()) && self.take(&TokenKind::Comma) {
                    if offset.is_some() {
                        return Err(self.unexpected());
                    }
                    offset = limit.take();
                    limit = Some(self.expression()?);
                }
            } else if self.take_word("offset") {
                if offset.is_some() {
                    return Err(SqlError::syntax(
                        "multiple OFFSET clauses not allowed",
                        self.tokens[self.at - 1].start,
                    ));
                }
                offset = Some(self.expression()?);
                if self.oracle() {
                    offset = offset.map(oracle::not_below_zero);
                    if !self.take_word("rows") {
                        self.expect_word("row")?;
                    }
                }
                let _ = self.take_word("rows") || self.take_word("row");
            } else if self.is_word("fetch") {
                return Err(self.not_supported("FETCH FIRST"));
            } else if self.is_word("for") || self.mysql() && self.is_word("lock") {
                return Err(self.not_supported("row locking (FOR UPDATE, FOR SHARE)"));
            } else {
                break;
            }
        }
        if self.sqlite() {
            limit = limit.map(sqlite::without_negative_limit);
            offset = offset.map(sqlite::without_negative_offset);
        }
        if self.oracle() {
            oracle::reject_rownum_outside_where(&items, &group_by, having.as_ref(), &order_by)?;
        }
        Ok(Select {
            distinct,
            items,
            from,
            filter,
            group_by,
            having,
            order_by,
            limit,
            offset,
            source_row_limit,
        })
    }

    fn joined_tables(&mut self) -> SqlResult<FromClause> {
        let first = self.table_reference()?;
        let mut joins = Vec::new();
        loop {
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
            } else if self.is_word("right") || self.is_word("full") {
                return Err(self.not_supported("RIGHT and FULL joins"));
            } else if self.is_word("natural") {
                return Err(self.not_supported("NATURAL joins"));
            } else {
                break;
            };
            if self.is_word("lateral") {
                return Err(self.not_supported("LATERAL"));
            }
            if joins.len() + 1 >= MAX_JOINED_TABLES {
                return Err(SqlError::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    format!("a FROM clause can join at most {MAX_JOINED_TABLES} tables"),
                )
                .at(self.position()));
            }
            let table = self.table_reference()?;
            let condition = match kind {
                JoinKind::Comma | JoinKind::Cross => JoinCondition::Always,
                JoinKind::Inner | JoinKind::Left => {
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
        let first_token = self.at;
        let expr = self.expression()?;
        let alias = if self.take_word("as")
            || matches!(self.peek_kind(), TokenKind::Word(word) if !self.reserved(word) && !self.clause_word(word))
            || matches!(self.peek_kind(), TokenKind::QuotedIdentifier(_))
            || (self.mysql() || self.sqlite()) && matches!(self.peek_kind(), TokenKind::String(_))
        {
            Some(self.label()?)
        } else if self.mysql()
            && let ExprKind::Literal(Literal::String(text)) = &expr.kind
        {
            Some(text.clone())
        } else if self.oracle() && !matches!(expr.kind, ExprKind::Column { .. }) {
            Some(oracle::label(&self.source_text(first_token)))
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
            TokenKind::String(text) if self.mysql() || self.sqlite() => {
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
        match self.peek_kind() {
            TokenKind::Word(word) => match word.as_ref() {
                "or" => Some(Level::Or),
                "and" => Some(Level::And),
                "is" | "isnull" | "notnull" => Some(Level::Is),
                "between" | "in" | "like" | "ilike" | "similar" => Some(Level::Pattern),
                "not"
                    if ["between", "in", "like", "ilike", "similar"]
                        .iter()
                        .any(|word| self.is_word_at(1, word)) =>
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
                let right = self.expression_above(Level::Pattern)?;
                if self.infix_level() == Some(Level::Comparison) {
                    return Err(self.unexpected());
                }
                Ok(Self::binary(left, operator, right))
            }
            Level::Pattern => self.pattern(left),
            Level::Other => {
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
        if self.is_word("isnull") || self.is_word("notnull") {
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
            self.expect(&TokenKind::LeftParen)?;
            if self.is_word("select") {
                return Err(self.not_supported("subqueries"));
            }
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
        } else if self.is_word("like") || self.is_word("ilike") {
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
                let sql_type = self.sql_type()?;
                let position = expr.position;
                expr = Expr {
                    kind: ExprKind::Cast {
                        expr: Box::new(expr),
                        sql_type,
                    },
                    position,
                };
            } else if self.peek_kind() == &TokenKind::LeftBracket {
                return Err(self.not_supported("array subscripts"));
            } else if self.is_word("collate") {
                return Err(self.not_supported("COLLATE"));
            } else if self.is_word("at") && self.is_word_at(1, "time") {
                return Err(self.not_supported("AT TIME ZONE"));
            } else {
                return Ok(expr);
            }
        }
    }

    fn primary(&mut self) -> SqlResult<Expr> {
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
        if self.oracle()
            && let Some(expr) = self.oracle_primary(&token.kind, position)?
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
            TokenKind::Decimal(text) if self.mysql() || self.sqlite() || self.oracle() => {
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
            TokenKind::LeftParen => {
                self.at += 1;
                if self.is_word("select") || self.is_word("values") {
                    return Err(self.not_supported("subqueries"));
                }
                let mut expr = self.expression()?;
                if self.peek_kind() == &TokenKind::Comma {
                    return Err(self.not_supported("row constructors"));
                }
                self.expect(&TokenKind::RightParen)?;
                expr.position = position;
                Ok(Expr {
                    kind: expr.kind,
                    position,
                })
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
                    Err(self.not_supported("EXISTS"))
                }
                "array" | "row"
                    if self.peek_kind_at(1) == &TokenKind::LeftParen
                        || self.peek_kind_at(1) == &TokenKind::LeftBracket =>
                {
                    Err(self.not_supported("ARRAY and ROW constructors"))
                }
                "current_date" | "current_time" | "current_timestamp" | "localtime"
                | "localtimestamp" => Err(self.not_supported("date and time values")),
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
                "interval" | "timestamp" | "date" | "time" | "numeric" | "decimal" | "json"
                | "jsonb"
                    if matches!(self.peek_kind_at(1), TokenKind::String(_)) =>
                {
                    Err(
                        SqlError::not_supported(format!("type {word} is not supported yet"))
                            .at(position),
                    )
                }
                "integer" | "int" | "int4" | "smallint" | "int2" | "bigint" | "int8" | "real"
                | "float4" | "float8" | "boolean" | "bool" | "text" | "varchar" | "double"
                    if matches!(self.peek_kind_at(1), TokenKind::String(_))
                        || (word == "double"
                            && self.is_word_at(1, "precision")
                            && matches!(self.peek_kind_at(2), TokenKind::String(_))) =>
                {
                    let sql_type = self.sql_type()?;
                    let TokenKind::String(text) = self.peek_kind() else {
                        return Err(self.unexpected());
                    };
                    let operand = Expr {
                        kind: ExprKind::Literal(Literal::String(text.as_ref().to_owned())),
                        position: self.position(),
                    };
                    self.at += 1;
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

    fn column_or_function(&mut self) -> SqlResult<Expr> {
        let first = self.name()?;
        if self.peek_kind() == &TokenKind::LeftParen {
            return self.function(first);
        }
        if self.take(&TokenKind::Dot) {
            if self.is_operator("*") {
                return Err(self.not_supported("a qualified * outside the select list"));
            }
            let column = self.name()?;
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

    fn function(&mut self, name: Name) -> SqlResult<Expr> {
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
        if AGGREGATES.contains(&name.text.as_str()) {
            return Err(SqlError::not_supported(format!(
                "aggregate functions such as {}() are not supported yet",
                name.text
            ))
            .at(name.position));
        }
        self.expect(&TokenKind::LeftParen)?;
        let mut arguments = Vec::new();
        if !self.take(&TokenKind::RightParen) {
            if self.is_word("distinct") || self.is_operator("*") {
                return Err(self.not_supported("DISTINCT and * arguments"));
            }
            loop {
                arguments.push(self.expression()?);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
        }
        if self.is_word("over") || self.is_word("filter") || self.is_word("within") {
            return Err(self.not_supported("window and aggregate clauses"));
        }
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
            (Some(Box::new(first)), distinct)
        };
        if self.is_word("order") {
            return Err(self.not_supported("ORDER BY inside an aggregate's arguments"));
        }
        self.expect(&TokenKind::RightParen)?;
        if self.is_word("over") || self.is_word("filter") || self.is_word("within") {
            return Err(self.not_supported("window and aggregate clauses"));
        }
        Ok(Expr {
            kind: ExprKind::Aggregate {
                function,
                argument,
                distinct,
            },
            position: name.position,
        })
    }

    fn cast(&mut self) -> SqlResult<Expr> {
        let position = self.position();
        self.at += 1;
        self.expect(&TokenKind::LeftParen)?;
        let expr = self.expression()?;
        self.expect_word("as")?;
        let sql_type = self.sql_type()?;
        self.expect(&TokenKind::RightParen)?;
        Ok(Expr {
            kind: ExprKind::Cast {
                expr: Box::new(expr),
                sql_type,
            },
            position,
        })
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
