use crate::ast::{
    AggregateFunction, BinaryOperator, ColumnDefinition, CreateTable, Delete, Expr, ExprKind,
    FromClause, Insert, InsertSource, InsertValue, Join, JoinCondition, JoinKind, Literal, Name,
    OrderItem, Select, SelectItem, Statement, TableReference, TruthTest, UnaryOperator, Update,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::{Token, TokenKind};
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
        }
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
        if !self.is_word("insert") || !self.is_word_at(1, "into") {
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
                {
                    at += 2;
                } else if is_plain_literal(&tokens.get(at)?.kind) {
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
            TokenKind::Word(word) if !is_reserved(word) => word.as_ref().to_owned(),
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
            "alter" | "truncate" | "savepoint" | "release" | "explain" | "copy" | "set"
            | "show" | "grant" | "revoke" | "prepare" | "execute" | "analyze" | "vacuum" => {
                Err(self.not_supported(&word.to_ascii_uppercase()))
            }
            _ => Err(self.unexpected()),
        }
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
        loop {
            if self.is_word("primary") || self.is_word("constraint") {
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
                } else {
                    return Err(self.table_constraint_not_supported());
                }
            } else if self.is_word("unique")
                || self.is_word("check")
                || self.is_word("foreign")
                || self.is_word("exclude")
                || self.is_word("like")
            {
                return Err(self.table_constraint_not_supported());
            } else {
                columns.push(self.column_definition()?);
            }
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen)?;
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
        };
        loop {
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
        matches!(self.peek_kind(), TokenKind::Word(word) if !is_reserved(word) && !is_clause_word(word))
            || matches!(self.peek_kind(), TokenKind::QuotedIdentifier(_))
    }

    fn insert(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        self.expect_word("into")?;
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
        } else if self.take_word("values") {
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
        if self.is_word("on") {
            return Err(self.not_supported("ON CONFLICT"));
        }
        let returning = self.returning()?;
        Ok(Statement::Insert(Insert {
            table,
            columns,
            source,
            returning,
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
        self.expect_word("from")?;
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
            Ok(Some(self.expression()?))
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
        let items = if self.is_clause_end() {
            Vec::new()
        } else {
            self.select_items()?
        };
        if self.is_word("into") {
            return Err(self.not_supported("SELECT INTO"));
        }
        let from = if self.take_word("from") {
            Some(self.joined_tables()?)
        } else {
            None
        };
        let filter = self.filter()?;
        let group_by = if self.take_word("group") {
            self.expect_word("by")?;
            self.group_by()?
        } else {
            Vec::new()
        };
        let having = if self.take_word("having") {
            Some(self.expression()?)
        } else {
            None
        };
        if self.is_word("window") {
            return Err(self.not_supported("WINDOW"));
        }
        if self.is_word("union") || self.is_word("intersect") || self.is_word("except") {
            return Err(self.not_supported("UNION, INTERSECT and EXCEPT"));
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
            } else if self.take_word("offset") {
                if offset.is_some() {
                    return Err(SqlError::syntax(
                        "multiple OFFSET clauses not allowed",
                        self.tokens[self.at - 1].start,
                    ));
                }
                offset = Some(self.expression()?);
                let _ = self.take_word("rows") || self.take_word("row");
            } else if self.is_word("fetch") {
                return Err(self.not_supported("FETCH FIRST"));
            } else if self.is_word("for") {
                return Err(self.not_supported("row locking (FOR UPDATE, FOR SHARE)"));
            } else {
                break;
            }
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
                        JoinCondition::On(self.expression()?)
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
        ) || matches!(self.peek_kind(), TokenKind::Word(word) if is_clause_word(word) || word == "from")
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
        if (matches!(self.peek_kind(), TokenKind::Word(word) if !is_reserved(word))
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
        let expr = self.expression()?;
        let alias = if self.take_word("as") {
            Some(self.label()?)
        } else if matches!(self.peek_kind(), TokenKind::Word(word) if !is_reserved(word) && !is_clause_word(word))
            || matches!(self.peek_kind(), TokenKind::QuotedIdentifier(_))
        {
            Some(self.name()?.text)
        } else {
            None
        };
        Ok(SelectItem::Expr { expr, alias })
    }

    fn label(&mut self) -> SqlResult<String> {
        match self.peek_kind() {
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
        if self.is_word("not") {
            if least > Level::Not {
                return Err(self.unexpected());
            }
            self.at += 1;
            let operand = self.expression_above(Level::Not)?;
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
        match level {
            Level::Or => {
                self.at += 1;
                let right = self.expression_above(Level::And)?;
                Ok(Self::joined(left, right, Level::Or))
            }
            Level::And => {
                self.at += 1;
                let right = self.expression_above(Level::Not)?;
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
        } else if self.is_word("like") || self.is_word("ilike") {
            let case_insensitive = self.is_word("ilike");
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
        let literal = |literal| {
            Ok(Expr {
                kind: ExprKind::Literal(literal),
                position,
            })
        };
        match &token.kind {
            TokenKind::Integer(digits) => {
                self.at += 1;
                match digits.parse::<i64>() {
                    Ok(value) => literal(Literal::Integer(value)),
                    Err(_) => literal(Literal::Decimal((*digits).to_owned())),
                }
            }
            TokenKind::Decimal(text) => {
                self.at += 1;
                literal(Literal::Decimal((*text).to_owned()))
            }
            TokenKind::String(text) => {
                self.at += 1;
                literal(Literal::String(text.as_ref().to_owned()))
            }
            TokenKind::Parameter => {
                Err(SqlError::not_supported("parameters ($1) are not supported yet").at(position))
            }
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
                "current_user" | "session_user" | "user" => {
                    Err(self.not_supported("roles and users"))
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
                _ if is_reserved(word) => Err(self.unexpected()),
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
        if let Some(function) = AggregateFunction::named(&name.text) {
            return self.aggregate(name, function);
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
