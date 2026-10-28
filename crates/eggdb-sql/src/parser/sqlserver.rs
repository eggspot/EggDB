use super::Parser;
use crate::ast::{
    ColumnDefinition, Expr, ExprKind, Literal, Name, SqlServerStatement, Statement, TableReference,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;
use crate::settings::Settings;
use crate::types::{NumericModifier, SqlType};

/// The SQL Server version EggDB's Transact-SQL dialect and TDS server present themselves as
/// compatible with: SQL Server 2022's major and minor version and a build number, as clients
/// read it from `@@VERSION` and the TDS login acknowledgement.
pub const SQLSERVER_COMPATIBLE_VERSION: &str = "16.0.1000";

const RESERVED: [&str; 185] = [
    "add",
    "all",
    "alter",
    "and",
    "any",
    "as",
    "asc",
    "authorization",
    "backup",
    "begin",
    "between",
    "break",
    "browse",
    "bulk",
    "by",
    "cascade",
    "case",
    "check",
    "checkpoint",
    "close",
    "clustered",
    "coalesce",
    "collate",
    "column",
    "commit",
    "compute",
    "constraint",
    "contains",
    "containstable",
    "continue",
    "convert",
    "create",
    "cross",
    "current",
    "current_date",
    "current_time",
    "current_timestamp",
    "current_user",
    "cursor",
    "database",
    "dbcc",
    "deallocate",
    "declare",
    "default",
    "delete",
    "deny",
    "desc",
    "disk",
    "distinct",
    "distributed",
    "double",
    "drop",
    "dump",
    "else",
    "end",
    "errlvl",
    "escape",
    "except",
    "exec",
    "execute",
    "exists",
    "exit",
    "external",
    "fetch",
    "file",
    "fillfactor",
    "for",
    "foreign",
    "freetext",
    "freetexttable",
    "from",
    "full",
    "function",
    "goto",
    "grant",
    "group",
    "having",
    "holdlock",
    "identity",
    "identity_insert",
    "identitycol",
    "if",
    "in",
    "index",
    "inner",
    "insert",
    "intersect",
    "into",
    "is",
    "join",
    "key",
    "kill",
    "left",
    "like",
    "lineno",
    "load",
    "merge",
    "national",
    "nocheck",
    "nonclustered",
    "not",
    "null",
    "nullif",
    "of",
    "off",
    "offsets",
    "on",
    "open",
    "opendatasource",
    "openquery",
    "openrowset",
    "openxml",
    "option",
    "or",
    "order",
    "outer",
    "over",
    "percent",
    "pivot",
    "plan",
    "precision",
    "primary",
    "print",
    "proc",
    "procedure",
    "public",
    "raiserror",
    "read",
    "readtext",
    "reconfigure",
    "references",
    "replication",
    "restore",
    "restrict",
    "return",
    "revert",
    "revoke",
    "right",
    "rollback",
    "rowcount",
    "rowguidcol",
    "rule",
    "save",
    "schema",
    "securityaudit",
    "select",
    "semantickeyphrasetable",
    "semanticsimilaritydetailstable",
    "semanticsimilaritytable",
    "session_user",
    "set",
    "setuser",
    "shutdown",
    "some",
    "statistics",
    "system_user",
    "table",
    "tablesample",
    "textsize",
    "then",
    "to",
    "top",
    "tran",
    "transaction",
    "trigger",
    "truncate",
    "try_convert",
    "tsequal",
    "union",
    "unique",
    "unpivot",
    "update",
    "updatetext",
    "use",
    "user",
    "values",
    "varying",
    "view",
    "waitfor",
    "when",
    "where",
    "while",
    "with",
    "within",
    "writetext",
];

const STATEMENT_WORDS: [&str; 29] = [
    "alter",
    "begin",
    "commit",
    "create",
    "declare",
    "delete",
    "deny",
    "drop",
    "exec",
    "execute",
    "go",
    "grant",
    "if",
    "insert",
    "merge",
    "print",
    "raiserror",
    "return",
    "revoke",
    "rollback",
    "save",
    "select",
    "set",
    "throw",
    "truncate",
    "update",
    "use",
    "while",
    "with",
];

const NOT_SUPPORTED_STATEMENTS: [&str; 22] = [
    "declare",
    "print",
    "if",
    "while",
    "exec",
    "execute",
    "return",
    "raiserror",
    "throw",
    "goto",
    "waitfor",
    "merge",
    "grant",
    "revoke",
    "deny",
    "backup",
    "restore",
    "dbcc",
    "kill",
    "open",
    "fetch",
    "bulk",
];

const NOT_TRANSACT_SQL_STATEMENTS: [&str; 20] = [
    "start",
    "end",
    "abort",
    "show",
    "reset",
    "explain",
    "copy",
    "values",
    "table",
    "comment",
    "vacuum",
    "analyze",
    "discard",
    "listen",
    "notify",
    "unlisten",
    "prepare",
    "savepoint",
    "release",
    "pragma",
];

const ENGINE_FUNCTIONS: [&str; 10] = [
    "coalesce", "nullif", "upper", "lower", "abs", "count", "sum", "avg", "min", "max",
];

const ON_ONLY_OPTIONS: [&str; 7] = [
    "quoted_identifier",
    "ansi_nulls",
    "ansi_padding",
    "ansi_warnings",
    "arithabort",
    "concat_null_yields_null",
    "ansi_null_dflt_on",
];

const NOT_SUPPORTED_OPTIONS: [&str; 7] = [
    "xact_abort",
    "identity_insert",
    "rowcount",
    "numeric_roundabort",
    "fmtonly",
    "parseonly",
    "noexec",
];

pub(crate) fn is_reserved(word: &str) -> bool {
    RESERVED.binary_search(&word).is_ok()
}

pub(crate) fn starts_statement(word: &str) -> bool {
    STATEMENT_WORDS.contains(&word)
}

pub(crate) fn not_supported(what: &str) -> SqlError {
    SqlError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("{what} is not supported in this version of EggDB."),
    )
}

pub(crate) fn version_text() -> String {
    format!(
        "EggDB {} (compatible with Microsoft SQL Server 2022, {SQLSERVER_COMPATIBLE_VERSION}) on {}/{}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

fn undeclared_variable(name: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_PARAMETER,
        format!("Must declare the scalar variable \"{name}\"."),
    )
    .with_sqlserver_code(137)
    .at(position)
}

fn unknown_function(name: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("'{name}' is not a recognized built-in function name."),
    )
    .with_sqlserver_code(195)
    .at(position)
}

fn unknown_type(name: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_OBJECT,
        format!("Cannot find data type {name}."),
    )
    .with_sqlserver_code(2715)
    .at(position)
}

fn clock(precision: u8, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Cast {
            expr: Box::new(Expr {
                kind: ExprKind::Function {
                    name: "localtimestamp".to_owned(),
                    arguments: Vec::new(),
                },
                position,
            }),
            sql_type: SqlType::Timestamp(Some(precision)),
        },
        position,
    }
}

fn session_text(value: String, position: usize) -> Expr {
    Expr {
        kind: ExprKind::SessionValue {
            name: "value",
            value,
        },
        position,
    }
}

fn integer(value: i64, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Literal(Literal::Integer(value)),
        position,
    }
}

pub(super) fn is_string_literal(expr: &Expr) -> bool {
    matches!(&expr.kind, ExprKind::Literal(Literal::String(_)))
}

pub(super) fn default_source(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Cast {
            expr: clock,
            sql_type: SqlType::Timestamp(Some(precision)),
        } if matches!(&clock.kind, ExprKind::Function { name, arguments }
                if name == "localtimestamp" && arguments.is_empty()) =>
        {
            Some(format!("CAST(localtimestamp AS timestamp({precision}))"))
        }
        _ => super::mysql::postgresql_source(expr),
    }
}

impl<'t> Parser<'t> {
    fn sqlserver_settings<T>(&self, read: impl FnOnce(&Settings) -> T) -> T {
        match self.settings {
            Some(settings) => read(settings),
            None => read(&Settings::default()),
        }
    }

    fn keyword_text(&self) -> String {
        let token = self.peek();
        self.text
            .get(token.start..token.end)
            .unwrap_or("")
            .to_ascii_uppercase()
    }

    pub(super) fn sqlserver_statement(&mut self) -> SqlResult<Option<Statement>> {
        let TokenKind::Word(word) = self.peek_kind() else {
            if let TokenKind::Variable(name) = self.peek_kind() {
                return Err(undeclared_variable(name, self.position()));
            }
            return Ok(None);
        };
        let position = self.position();
        let statement = match word.as_ref() {
            "begin" => self.sqlserver_begin()?,
            "commit" | "rollback" => {
                let commit = word == "commit";
                self.at += 1;
                if !self.take_word("work")
                    && (self.take_word("tran") || self.take_word("transaction"))
                {
                    self.optional_transaction_name();
                }
                if commit {
                    Statement::Commit
                } else {
                    Statement::Rollback
                }
            }
            "save" => return Err(not_supported("SAVE TRANSACTION").at(position)),
            "create" | "alter"
                if self.is_word_at(1, "view")
                    || (self.is_word_at(1, "or") && self.is_word_at(3, "view")) =>
            {
                return Err(not_supported("Views in the SQL Server dialect").at(position));
            }
            "use" => {
                self.at += 1;
                let name = self.name()?;
                Statement::SqlServer(SqlServerStatement::Use(name))
            }
            "set" => self.sqlserver_set()?,
            "truncate" => {
                self.at += 1;
                self.expect_word("table")?;
                let name = self.table_name()?;
                Statement::Truncate(TableReference::named(name, None))
            }
            "update" | "delete" if self.is_word_at(1, "top") => {
                return Err(not_supported("TOP in UPDATE and DELETE").at(position));
            }
            "insert" if self.is_word_at(1, "top") => {
                return Err(not_supported("TOP in INSERT").at(position));
            }
            word if NOT_SUPPORTED_STATEMENTS.contains(&word) => {
                return Err(not_supported(&self.keyword_text()).at(position));
            }
            word if NOT_TRANSACT_SQL_STATEMENTS.contains(&word) => return Err(self.unexpected()),
            _ => return Ok(None),
        };
        Ok(Some(statement))
    }

    fn optional_transaction_name(&mut self) {
        if matches!(
            self.peek_kind(),
            TokenKind::Word(word) if !is_reserved(word) && !starts_statement(word)
        ) || matches!(self.peek_kind(), TokenKind::QuotedIdentifier(_))
        {
            self.at += 1;
        }
    }

    fn sqlserver_begin(&mut self) -> SqlResult<Statement> {
        let position = self.position();
        self.at += 1;
        if self.take_word("tran") || self.take_word("transaction") {
            if self.is_word("with") {
                return Err(not_supported("BEGIN TRANSACTION WITH MARK").at(position));
            }
            self.optional_transaction_name();
            return Ok(Statement::Begin { read_only: false });
        }
        let what = if self.is_word("try") {
            "BEGIN TRY"
        } else if self.is_word("distributed") {
            "BEGIN DISTRIBUTED TRANSACTION"
        } else {
            "BEGIN ... END blocks"
        };
        Err(not_supported(what).at(position))
    }

    fn sqlserver_set(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        let position = self.position();
        if let TokenKind::Variable(name) = self.peek_kind() {
            return Err(undeclared_variable(name, position));
        }
        if self.take_word("transaction") {
            self.expect_word("isolation")?;
            self.expect_word("level")?;
            let level = if self.take_word("read") {
                self.take_word("uncommitted") || self.take_word("committed")
            } else if self.take_word("repeatable") {
                self.take_word("read")
            } else {
                self.take_word("snapshot") || self.take_word("serializable")
            };
            if !level {
                return Err(self.unexpected());
            }
            return Ok(accepted_set());
        }
        let option = match self.peek_kind() {
            TokenKind::Word(word) => word.as_ref().to_owned(),
            _ => return Err(self.unexpected()),
        };
        let shown = self.keyword_text();
        self.at += 1;
        match option.as_str() {
            "nocount" => {
                let on = self.on_or_off()?;
                Ok(Statement::SqlServer(SqlServerStatement::NoCount(on)))
            }
            "implicit_transactions" => {
                if self.on_or_off()? {
                    return Err(not_supported("SET IMPLICIT_TRANSACTIONS ON").at(position));
                }
                Ok(accepted_set())
            }
            "textsize" | "lock_timeout" => {
                self.take_operator("-");
                let TokenKind::Integer(_) = self.peek_kind() else {
                    return Err(self.unexpected());
                };
                self.at += 1;
                Ok(accepted_set())
            }
            "language" => {
                let language = match self.peek_kind() {
                    TokenKind::Word(word) => word.as_ref().to_owned(),
                    TokenKind::String(text) => text.to_ascii_lowercase(),
                    TokenKind::QuotedIdentifier(text) => text.to_ascii_lowercase(),
                    _ => return Err(self.unexpected()),
                };
                self.at += 1;
                if language != "us_english" && language != "english" {
                    return Err(not_supported(&format!("SET LANGUAGE {language}")).at(position));
                }
                Ok(accepted_set())
            }
            "dateformat" => {
                let format = match self.peek_kind() {
                    TokenKind::Word(word) => word.as_ref().to_owned(),
                    TokenKind::String(text) => text.to_ascii_lowercase(),
                    _ => return Err(self.unexpected()),
                };
                self.at += 1;
                if format != "mdy" {
                    return Err(not_supported(&format!("SET DATEFORMAT {format}")).at(position));
                }
                Ok(accepted_set())
            }
            option if ON_ONLY_OPTIONS.contains(&option) => {
                let mut names = vec![shown];
                while self.take(&TokenKind::Comma) {
                    let TokenKind::Word(word) = self.peek_kind() else {
                        return Err(self.unexpected());
                    };
                    if !ON_ONLY_OPTIONS.contains(&word.as_ref()) {
                        return Err(unknown_option(&self.keyword_text(), self.position()));
                    }
                    names.push(self.keyword_text());
                    self.at += 1;
                }
                if !self.on_or_off()? {
                    return Err(
                        not_supported(&format!("SET {} OFF", names.join(", "))).at(position)
                    );
                }
                Ok(accepted_set())
            }
            option if NOT_SUPPORTED_OPTIONS.contains(&option) => {
                Err(not_supported(&format!("SET {shown}")).at(position))
            }
            _ => Err(unknown_option(&shown, position)),
        }
    }

    fn on_or_off(&mut self) -> SqlResult<bool> {
        if self.take_word("on") {
            Ok(true)
        } else if self.take_word("off") {
            Ok(false)
        } else {
            Err(self.unexpected())
        }
    }

    pub(super) fn sqlserver_type(&mut self) -> SqlResult<SqlType> {
        let token = self.peek();
        let word = match &token.kind {
            TokenKind::Word(word) => word.as_ref().to_owned(),
            TokenKind::QuotedIdentifier(name) => name.clone(),
            _ => return Err(self.unexpected()),
        };
        self.at += 1;
        let position = token.start;
        Ok(match word.as_str() {
            "int" | "integer" => SqlType::Integer,
            "bigint" => SqlType::BigInt,
            "smallint" => SqlType::SmallInt,
            "real" => SqlType::Real,
            "float" => match self.sqlserver_type_argument(position)? {
                None => SqlType::Double,
                Some(1..=24) => SqlType::Real,
                Some(25..=53) => SqlType::Double,
                Some(bits) => {
                    return Err(SqlError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!(
                            "The size ({bits}) given to the type 'float' exceeds the maximum allowed for any data type (53)."
                        ),
                    )
                    .with_sqlserver_code(131)
                    .at(position));
                }
            },
            "decimal" | "numeric" | "dec" => self.sqlserver_decimal(position)?,
            "char" | "nchar" | "character" => {
                let length = self.sqlserver_length(&word, position)?;
                SqlType::Bpchar(Some(length.unwrap_or(1)))
            }
            "varchar" | "nvarchar" => {
                if self.is_word_at(1, "max")
                    && self.peek_kind() == &TokenKind::LeftParen
                    && self.peek_kind_at(2) == &TokenKind::RightParen
                {
                    self.at += 3;
                    SqlType::Text
                } else {
                    let length = self.sqlserver_length(&word, position)?;
                    SqlType::Varchar(Some(length.unwrap_or(1)))
                }
            }
            "text" | "ntext" => SqlType::Text,
            "date" => SqlType::Date,
            "datetime2" => {
                let digits = self.sqlserver_type_argument(position)?.unwrap_or(7);
                if !(0..=7).contains(&digits) {
                    return Err(SqlError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!(
                            "Specified scale {digits} is invalid. 'datetime2' datatype must have scale between 0 and 7."
                        ),
                    )
                    .with_sqlserver_code(1002)
                    .at(position));
                }
                SqlType::Timestamp(Some(digits.min(6) as u8))
            }
            _ => return Err(unknown_type(&word, position)),
        })
    }

    fn sqlserver_type_argument(&mut self, position: usize) -> SqlResult<Option<i64>> {
        if !self.take(&TokenKind::LeftParen) {
            return Ok(None);
        }
        let TokenKind::Integer(digits) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let value = digits.parse::<i64>().map_err(|_| {
            SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                "The type argument is too large.",
            )
            .at(position)
        })?;
        self.at += 1;
        self.expect(&TokenKind::RightParen)?;
        Ok(Some(value))
    }

    fn sqlserver_length(&mut self, word: &str, position: usize) -> SqlResult<Option<u32>> {
        let Some(length) = self.sqlserver_type_argument(position)? else {
            return Ok(None);
        };
        let maximum = if word.starts_with('n') { 4000 } else { 8000 };
        if !(1..=maximum).contains(&length) {
            return Err(SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!(
                    "The size ({length}) given to the type '{word}' exceeds the maximum allowed for any data type ({maximum})."
                ),
            )
            .with_sqlserver_code(131)
            .at(position));
        }
        Ok(Some(length as u32))
    }

    fn sqlserver_decimal(&mut self, position: usize) -> SqlResult<SqlType> {
        let (precision, scale) = if self.take(&TokenKind::LeftParen) {
            let TokenKind::Integer(digits) = self.peek_kind() else {
                return Err(self.unexpected());
            };
            let precision: i64 = digits.parse().unwrap_or(i64::MAX);
            self.at += 1;
            let scale = if self.take(&TokenKind::Comma) {
                let TokenKind::Integer(digits) = self.peek_kind() else {
                    return Err(self.unexpected());
                };
                let scale: i64 = digits.parse().unwrap_or(i64::MAX);
                self.at += 1;
                scale
            } else {
                0
            };
            self.expect(&TokenKind::RightParen)?;
            (precision, scale)
        } else {
            (18, 0)
        };
        if !(1..=38).contains(&precision) {
            return Err(SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!(
                    "Specified column precision {precision} is greater than the maximum precision of 38."
                ),
            )
            .with_sqlserver_code(2750)
            .at(position));
        }
        if scale > precision {
            return Err(SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!(
                    "The scale ({scale}) must be less than or equal to the precision ({precision})."
                ),
            )
            .with_sqlserver_code(192)
            .at(position));
        }
        Ok(SqlType::Numeric(Some(NumericModifier {
            precision: precision as u16,
            scale: scale as i16,
        })))
    }

    pub(super) fn sqlserver_identity(&mut self, column: &mut ColumnDefinition) -> SqlResult<()> {
        let position = self.position();
        self.at += 1;
        if self.take(&TokenKind::LeftParen) {
            let seed = self.signed_type_argument()?;
            self.expect(&TokenKind::Comma)?;
            let increment = self.signed_type_argument()?;
            self.expect(&TokenKind::RightParen)?;
            if seed != 1 || increment != 1 {
                return Err(
                    not_supported("an IDENTITY seed or increment other than (1, 1)").at(position),
                );
            }
        }
        if !matches!(
            column.sql_type,
            SqlType::SmallInt | SqlType::Integer | SqlType::BigInt
        ) {
            return Err(
                not_supported("IDENTITY on a column that is not int, bigint or smallint")
                    .at(position),
            );
        }
        column.auto_increment = true;
        column.not_null = true;
        Ok(())
    }

    pub(super) fn sqlserver_top(&mut self) -> SqlResult<Expr> {
        self.at += 1;
        let position = self.position();
        let count = if self.take(&TokenKind::LeftParen) {
            let expr = self.expression()?;
            self.expect(&TokenKind::RightParen)?;
            expr
        } else {
            let TokenKind::Integer(digits) = self.peek_kind() else {
                return Err(self.unexpected());
            };
            self.at += 1;
            match digits.parse::<i64>() {
                Ok(value) => integer(value, position),
                Err(_) => {
                    return Err(SqlError::new(
                        SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
                        "The number of rows provided for a TOP clause is out of range.",
                    )
                    .at(position));
                }
            }
        };
        if self.is_word("percent") {
            return Err(not_supported("TOP ... PERCENT").at(self.position()));
        }
        if self.is_word("with") && self.is_word_at(1, "ties") {
            return Err(not_supported("TOP ... WITH TIES").at(self.position()));
        }
        Ok(count)
    }

    pub(super) fn sqlserver_primary(
        &mut self,
        kind: &TokenKind<'_>,
        position: usize,
    ) -> SqlResult<Option<Expr>> {
        let expr = match kind {
            TokenKind::Variable(name) => {
                self.at += 1;
                self.system_variable(name, position)?
            }
            TokenKind::Word(word) => match word.as_ref() {
                "current_timestamp" => {
                    self.at += 1;
                    clock(3, position)
                }
                "true" | "false" | "current_date" | "current_time" | "localtimestamp"
                | "localtime" => {
                    let name = Name {
                        text: word.as_ref().to_owned(),
                        position,
                    };
                    self.at += 1;
                    if self.peek_kind() == &TokenKind::LeftParen {
                        return Err(unknown_function(&name.text, position));
                    }
                    Expr {
                        kind: ExprKind::Column {
                            qualifier: None,
                            name: name.text,
                        },
                        position,
                    }
                }
                "coalesce" | "nullif" if self.peek_kind_at(1) == &TokenKind::LeftParen => {
                    let name = Name {
                        text: word.as_ref().to_owned(),
                        position,
                    };
                    self.at += 1;
                    self.function(name)?
                }
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };
        Ok(Some(expr))
    }

    fn system_variable(&self, name: &str, position: usize) -> SqlResult<Expr> {
        let state = self.sqlserver_settings(|settings| settings.sqlserver.clone());
        Ok(match name {
            "@@version" => session_text(version_text(), position),
            "@@spid" => integer(i64::from(state.connection_id), position),
            "@@servername" => session_text("EggDB".to_owned(), position),
            "@@trancount" => integer(i64::from(state.transaction_count), position),
            "@@language" => session_text("us_english".to_owned(), position),
            _ => return Err(undeclared_variable(name, position)),
        })
    }

    pub(super) fn sqlserver_function(&mut self, name: &Name) -> SqlResult<Option<Expr>> {
        let clock_precision = match name.text.as_str() {
            "getdate" | "getutcdate" => Some(3),
            "sysdatetime" | "sysutcdatetime" => Some(6),
            _ => None,
        };
        if let Some(precision) = clock_precision {
            self.expect(&TokenKind::LeftParen)?;
            self.expect(&TokenKind::RightParen)?;
            return Ok(Some(clock(precision, name.position)));
        }
        if ENGINE_FUNCTIONS.contains(&name.text.as_str()) {
            return Ok(None);
        }
        Err(unknown_function(&name.text, name.position))
    }

    pub(super) fn sqlserver_qualified_name(&mut self, schema: Name) -> SqlResult<Name> {
        if schema.text == "dbo" {
            return self.name();
        }
        self.qualified_part(&schema)
    }
}

fn accepted_set() -> Statement {
    Statement::Ignored {
        tag: "SET",
        notice: None,
    }
}

fn unknown_option(shown: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        format!("'{shown}' is not a recognized SET option."),
    )
    .with_sqlserver_code(195)
    .at(position)
}

#[cfg(test)]
mod tests {
    use super::{RESERVED, STATEMENT_WORDS, is_reserved};

    #[test]
    fn the_reserved_words_are_sorted_and_hold_every_word_that_starts_a_statement() {
        assert!(RESERVED.windows(2).all(|pair| pair[0] < pair[1]));
        for word in STATEMENT_WORDS {
            assert!(
                is_reserved(word) || ["go", "throw"].contains(&word),
                "{word}"
            );
        }
    }
}
