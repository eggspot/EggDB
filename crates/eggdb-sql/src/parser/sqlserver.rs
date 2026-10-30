use super::{Parameters, Parser};
use crate::ast::{
    AlterSequence, ColumnDefinition, Comment, CommentTarget, CreateSequence, Expr, ExprKind,
    Literal, Name, ProcedureCall, Query, RelationKind, SequenceOptions, SetExpr,
    SqlServerStatement, Statement, TableReference,
};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope};
use crate::lexer::{Token, TokenKind, tokenize_in};
use crate::session::sqlserver::ParameterDeclaration;
use crate::settings::Settings;
use crate::sqlserver_functions::Function;
use crate::sqlserver_types::{
    Length, MAX_DECIMAL_PRECISION, MAX_TIME_SCALE, SqlServerType, UNSPECIFIED_CAST_LENGTH,
};
use crate::types::SqlType;
use crate::types::Value;

/// The SQL Server version EggDB's Transact-SQL dialect and TDS server present themselves as
/// compatible with: SQL Server 2022's major and minor version and a build number, as clients
/// read it from `@@VERSION` and the TDS login acknowledgement.
pub const SQLSERVER_COMPATIBLE_VERSION: &str = "16.0.1000";

const RESERVED: [&str; 184] = [
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

const NOT_SUPPORTED_STATEMENTS: [&str; 12] = [
    "goto",
    "waitfor",
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

const BATCH_LOGIC_STATEMENTS: [&str; 5] = ["if", "while", "break", "continue", "return"];

const BLOCK_ENDS: [&str; 4] = ["end", "else", "break", "continue"];

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

const NOT_SUPPORTED_OPTIONS: [&str; 3] = ["rowcount", "parseonly", "noexec"];

const EXTENDED_PROPERTY_PARAMETERS: [&str; 8] = [
    "@name",
    "@value",
    "@level0type",
    "@level0name",
    "@level1type",
    "@level1name",
    "@level2type",
    "@level2name",
];

const DESCRIPTION_PROPERTY: &str = "ms_description";

const SWITCHED_INDEX_OPTIONS: [&str; 8] = [
    "pad_index",
    "statistics_norecompute",
    "statistics_incremental",
    "allow_row_locks",
    "allow_page_locks",
    "optimize_for_sequential_key",
    "sort_in_tempdb",
    "online",
];

const OFF_ONLY_INDEX_OPTIONS: [&str; 3] = ["ignore_dup_key", "drop_existing", "resumable"];

const OFF_ONLY_TABLE_OPTIONS: [&str; 3] = ["memory_optimized", "system_versioning", "ledger"];

const COMPRESSIONS: [&str; 3] = ["none", "row", "page"];

fn index_option_accepted(option: &str, value: &str) -> bool {
    let number = !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit());
    match option {
        "fillfactor" | "maxdop" => number,
        "data_compression" => COMPRESSIONS.contains(&value),
        _ if SWITCHED_INDEX_OPTIONS.contains(&option) => value == "on" || value == "off",
        _ if OFF_ONLY_INDEX_OPTIONS.contains(&option) => value == "off",
        _ => false,
    }
}

fn table_option_accepted(option: &str, value: &str) -> bool {
    match option {
        "data_compression" => COMPRESSIONS.contains(&value),
        _ if OFF_ONLY_TABLE_OPTIONS.contains(&option) => value == "off",
        _ => false,
    }
}

fn filters_out_nulls_only(filter: &Expr, key: &[Name]) -> bool {
    match &filter.kind {
        ExprKind::And(operands) => operands
            .iter()
            .all(|operand| filters_out_nulls_only(operand, key)),
        ExprKind::IsNull {
            expr,
            negated: true,
        } => matches!(&expr.kind, ExprKind::Column { qualifier: None, name }
            if key.iter().any(|column| &column.text == name)),
        _ => false,
    }
}

pub(crate) fn is_reserved(word: &str) -> bool {
    RESERVED.binary_search(&word).is_ok()
}

pub(crate) fn starts_statement(word: &str) -> bool {
    STATEMENT_WORDS.contains(&word)
}

pub(crate) fn ends_a_block(word: &str) -> bool {
    BLOCK_ENDS.contains(&word)
}

pub(super) fn outside_a_batch(what: &str) -> SqlError {
    not_supported(&format!("{what} outside a Transact-SQL batch"))
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

fn microsoft_version() -> i64 {
    let mut parts = SQLSERVER_COMPATIBLE_VERSION
        .split('.')
        .map(|part| part.parse::<i64>().unwrap_or(0));
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    let build = parts.next().unwrap_or(0);
    (major << 24) | (minor << 16) | build
}

pub(super) fn login_text(name: &'static str, value: String, position: usize) -> Expr {
    Expr {
        kind: ExprKind::SessionValue { name, value },
        position,
    }
}

pub(super) fn undeclared_variable(name: &str, position: usize) -> SqlError {
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

fn calls(expr: &Expr, function: &str) -> bool {
    matches!(&expr.kind, ExprKind::Function { name, arguments }
        if name == function && arguments.is_empty())
}

fn is_constant(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Literal(_) | ExprKind::Constant { .. } => true,
        ExprKind::Unary(_, operand) => is_constant(operand),
        ExprKind::SqlServerFunction {
            function: Function::Convert { .. },
            arguments,
        } => arguments.iter().all(is_constant),
        _ => false,
    }
}

fn postgresql_literal(value: &Value) -> Option<String> {
    Some(match value {
        Value::Null => "NULL".to_owned(),
        Value::Boolean(flag) => flag.to_string(),
        Value::SmallInt(_)
        | Value::Integer(_)
        | Value::BigInt(_)
        | Value::Real(_)
        | Value::Double(_)
        | Value::Numeric(_) => value.to_text()?,
        Value::Bytea(bytes) => format!("'{}'", crate::bytea::format_hex(bytes)),
        other => format!("'{}'", other.to_text()?.replace('\'', "''")),
    })
}

fn folded_default(expr: &Expr, column_type: SqlType) -> Option<String> {
    let evaluator = Evaluator {
        scope: Scope::empty(),
        row: &[],
        correlation: None,
    };
    let value = evaluator.eval(expr, Some(column_type)).ok()?;
    let value = crate::sqlserver_conversion::implicit(value, column_type).ok()?;
    postgresql_literal(&value)
}

fn default_source(expr: &Expr, column_type: SqlType) -> Option<String> {
    match &expr.kind {
        ExprKind::Cast {
            expr: clock,
            sql_type: SqlType::Timestamp(Some(precision)),
        } if calls(clock, "localtimestamp") => {
            Some(format!("CAST(localtimestamp AS timestamp({precision}))"))
        }
        ExprKind::Cast {
            expr: clock,
            sql_type: SqlType::TimestampTz(Some(precision)),
        } if calls(clock, "now") => Some(format!(
            "CAST(now() AS timestamp({precision}) with time zone)"
        )),
        ExprKind::SqlServerFunction {
            function: Function::NewId,
            ..
        } => Some("gen_random_uuid()".to_owned()),
        _ if is_constant(expr) => folded_default(expr, column_type),
        _ => None,
    }
}

pub(crate) fn kept_default(
    expr: &Expr,
    written: &str,
    column_type: SqlType,
) -> SqlResult<(String, Dialect)> {
    if let Some(folded) = default_source(expr, column_type) {
        return Ok((folded, Dialect::PostgreSql));
    }
    let kept = without_outer_parentheses(written);
    if !reads_alone_as_expression(kept) {
        return Err(not_supported(
            "A DEFAULT expression that cannot be read again on its own",
        ));
    }
    Ok((kept.to_owned(), Dialect::SqlServer))
}

fn without_outer_parentheses(written: &str) -> &str {
    let trimmed = written.trim();
    let Ok(tokens) = tokenize_in(trimmed, Dialect::SqlServer) else {
        return trimmed;
    };
    let meaningful: Vec<&Token<'_>> = tokens
        .iter()
        .filter(|token| token.kind != TokenKind::End)
        .collect();
    let partners = closing_partners(&meaningful);
    let (mut first, mut last) = (0, meaningful.len());
    while last - first >= 3
        && meaningful[first].kind == TokenKind::LeftParen
        && partners[first] == Some(last - 1)
    {
        first += 1;
        last -= 1;
    }
    if first == 0 {
        return trimmed;
    }
    trimmed
        .get(meaningful[first - 1].end..meaningful[last].start)
        .map_or(trimmed, str::trim)
}

fn closing_partners(tokens: &[&Token<'_>]) -> Vec<Option<usize>> {
    let mut partners = vec![None; tokens.len()];
    let mut open = Vec::new();
    for (at, token) in tokens.iter().enumerate() {
        match token.kind {
            TokenKind::LeftParen => open.push(at),
            TokenKind::RightParen => {
                if let Some(opened) = open.pop() {
                    partners[opened] = Some(at);
                }
            }
            _ => {}
        }
    }
    partners
}

fn reads_alone_as_expression(text: &str) -> bool {
    let Ok(tokens) = tokenize_in(text, Dialect::SqlServer) else {
        return false;
    };
    let names_a_local_variable = tokens.iter().any(
        |token| matches!(&token.kind, TokenKind::Variable(name) if !name.starts_with("@@")),
    );
    if names_a_local_variable {
        return false;
    }
    let mut parser = Parser::new(text, &tokens).in_dialect(Dialect::SqlServer);
    parser.expression().is_ok() && parser.at_end()
}

const GENERIC_ERROR: u16 = 50000;

#[cold]
#[inline(never)]
fn invalid_identity_argument(
    column: &str,
    column_type: SqlType,
    rule: &str,
    position: usize,
) -> SqlError {
    let type_name = match column_type {
        SqlType::SmallInt => "smallint",
        SqlType::Integer => "int",
        _ => "bigint",
    };
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("The IDENTITY {rule} of column '{column}' must be a whole number within the range of {type_name}."),
    )
    .with_sqlserver_code(GENERIC_ERROR)
    .at(position)
}

impl<'t> Parser<'t> {
    pub(super) fn sqlserver_settings<T>(&self, read: impl FnOnce(&Settings) -> T) -> T {
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
                let name = if !self.take_word("work")
                    && (self.take_word("tran") || self.take_word("transaction"))
                {
                    self.transaction_name()?
                } else {
                    None
                };
                match (commit, name) {
                    (true, _) => Statement::Commit,
                    (false, Some(name)) => {
                        Statement::SqlServer(SqlServerStatement::RollbackTo(name))
                    }
                    (false, None) => Statement::Rollback,
                }
            }
            "save" => {
                self.at += 1;
                if !self.take_word("tran") {
                    self.expect_word("transaction")?;
                }
                match self.transaction_name()? {
                    Some(name) => Statement::SqlServer(SqlServerStatement::SaveTransaction(name)),
                    None => return Err(self.unexpected()),
                }
            }
            "return" => {
                self.at += 1;
                if self.at_statement_end() {
                    return Err(outside_a_batch("RETURN").at(position));
                }
                Statement::SqlServer(SqlServerStatement::Return(self.expression()?))
            }
            word if BATCH_LOGIC_STATEMENTS.contains(&word) => {
                return Err(outside_a_batch(&self.keyword_text()).at(position));
            }
            "exec" | "execute" => self.sqlserver_execute()?,
            "declare" => self.sqlserver_declare()?,
            "print" => self.sqlserver_print()?,
            "raiserror" => self.sqlserver_raiserror()?,
            "throw" => self.sqlserver_throw()?,
            "select" if self.select_assigns_variables() => self.sqlserver_select_assignment()?,
            "insert" if self.at_insert_into_variable() => self.sqlserver_insert_variable()?,
            "update" if self.at_variable(1) => self.sqlserver_update_variable()?,
            "delete" if self.at_variable(1) || (self.is_word_at(1, "from") && self.at_variable(2)) => {
                self.sqlserver_delete_variable()?
            }
            word if self.first_in_batch()
                && crate::sqlserver_catalog::CatalogProcedure::named(word).is_some() =>
            {
                self.sqlserver_bare_procedure()?
            }
            "create" | "alter"
                if self.is_word_at(1, "view")
                    || (self.is_word_at(1, "or") && self.is_word_at(3, "view")) =>
            {
                self.sqlserver_view()?
            }
            "create" | "alter" | "drop" if self.at_sqlserver_routine() => self.sqlserver_routine()?,
            "create" if self.is_word_at(1, "sequence") => {
                self.at += 1;
                self.sqlserver_create_sequence()?
            }
            "alter" if self.is_word_at(1, "sequence") => {
                self.at += 1;
                self.sqlserver_alter_sequence()?
            }
            "drop" if self.is_word_at(1, "sequence") => {
                self.at += 1;
                self.drop_sequence()?
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
            "insert" if self.is_word_at(1, "bulk") => self.sqlserver_insert_bulk()?,
            "merge" => self.sqlserver_merge()?,
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

    pub(super) fn variable_reference(&mut self, name: &str, position: usize) -> SqlResult<Expr> {
        let constant = match self.batch.and_then(|batch| batch.variable(name)) {
            Some(variable) => Some(variable.constant(position)),
            None => self
                .declared_variables
                .iter()
                .find(|variable| variable.name == name)
                .map(|variable| variable.constant(position)),
        };
        if let Some(constant) = constant {
            self.variables_read.push(name.to_owned());
            return Ok(constant);
        }
        self.bound_parameter(name, position)
    }

    fn bound_parameter(&self, name: &str, position: usize) -> SqlResult<Expr> {
        let Parameters::Named(parameters) = self.parameters else {
            return Err(undeclared_variable(name, position));
        };
        let Some(parameter) = parameters
            .iter()
            .find(|parameter| parameter.name.eq_ignore_ascii_case(name))
        else {
            return Err(undeclared_variable(name, position));
        };
        let constant = Expr {
            kind: ExprKind::Constant {
                value: parameter.value.clone(),
                sql_type: parameter.value.sql_type().or(parameter.declared_type),
            },
            position,
        };
        Ok(match parameter.declared_type {
            Some(sql_type) if !parameter.value.is_null() => Expr {
                kind: ExprKind::Cast {
                    expr: Box::new(constant),
                    sql_type,
                },
                position,
            },
            _ => constant,
        })
    }

    fn parameter_declaration_list(&mut self) -> SqlResult<Vec<ParameterDeclaration>> {
        let mut declarations: Vec<ParameterDeclaration> = Vec::new();
        if self.at_end() {
            return Ok(declarations);
        }
        loop {
            let position = self.position();
            let name = match self.peek_kind() {
                TokenKind::Variable(name) if !name.starts_with("@@") => name.as_ref().to_owned(),
                _ => return Err(self.unexpected()),
            };
            self.at += 1;
            self.take_word("as");
            let sql_type = self.sqlserver_type()?;
            let output = self.take_word("output") || self.take_word("out");
            self.take_word("readonly");
            if declarations
                .iter()
                .any(|declaration| declaration.name == name)
            {
                return Err(SqlError::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!(
                        "The variable name '{name}' has already been declared. Variable names must be unique within a query batch or stored procedure."
                    ),
                )
                .with_sqlserver_code(134)
                .at(position));
            }
            declarations.push(ParameterDeclaration {
                name,
                sql_type,
                output,
            });
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        if !self.at_end() {
            return Err(self.unexpected());
        }
        Ok(declarations)
    }

    fn transaction_name(&mut self) -> SqlResult<Option<Name>> {
        let named = matches!(
            self.peek_kind(),
            TokenKind::Word(word) if !is_reserved(word) && !starts_statement(word)
        ) || matches!(self.peek_kind(), TokenKind::QuotedIdentifier(_));
        if named {
            Ok(Some(self.name()?))
        } else {
            Ok(None)
        }
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
        if self.is_word("distributed") {
            return Err(not_supported("BEGIN DISTRIBUTED TRANSACTION").at(position));
        }
        if self.is_word("dialog") || self.is_word("conversation") {
            return Err(not_supported("BEGIN DIALOG CONVERSATION").at(position));
        }
        let what = if self.is_word("try") {
            "BEGIN TRY"
        } else {
            "BEGIN ... END"
        };
        Err(outside_a_batch(what).at(position))
    }

    fn sqlserver_set(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        let position = self.position();
        if matches!(self.peek_kind(), TokenKind::Variable(_)) {
            return self.sqlserver_set_variable();
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
        if self.is_word("eggdb")
            && self.peek_kind_at(1) == &TokenKind::Dot
            && self.is_word_at(2, "dialect")
        {
            self.at += 3;
            self.take_operator("=");
            let value = match self.peek_kind() {
                TokenKind::Word(word) => word.as_ref().to_owned(),
                TokenKind::String(text) => text.as_ref().to_owned(),
                TokenKind::QuotedIdentifier(name) => name.clone(),
                _ => return Err(self.unexpected()),
            };
            self.at += 1;
            return Ok(Statement::Set {
                name: crate::settings::DIALECT_SETTING.to_owned(),
                value: Some(value),
            });
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
                let on = self.on_or_off()?;
                Ok(Statement::SqlServer(
                    SqlServerStatement::ImplicitTransactions(on),
                ))
            }
            "fmtonly" => {
                let on = self.on_or_off()?;
                Ok(Statement::SqlServer(SqlServerStatement::FormatOnly(on)))
            }
            "xact_abort" => {
                let on = self.on_or_off()?;
                Ok(Statement::SqlServer(SqlServerStatement::XactAbort(on)))
            }
            "numeric_roundabort" => {
                if self.on_or_off()? {
                    return Err(not_supported(&format!("SET {shown} ON")).at(position));
                }
                Ok(accepted_set())
            }
            "identity_insert" => {
                let table = self.table_name()?;
                let on = self.on_or_off()?;
                Ok(Statement::SqlServer(SqlServerStatement::IdentityInsert {
                    table,
                    on,
                }))
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

    fn sqlserver_view(&mut self) -> SqlResult<Statement> {
        let position = self.position();
        let first_in_batch = self
            .tokens
            .get(..self.at)
            .unwrap_or_default()
            .iter()
            .all(|token| token.kind == TokenKind::Semicolon);
        let existing_only = self.is_word("alter");
        self.at += 1;
        let or_alter = !existing_only && self.take_word("or");
        if or_alter {
            self.expect_word("alter")?;
        }
        if !self.is_word("view") {
            return Err(self.unexpected());
        }
        let shown = if existing_only {
            "ALTER VIEW"
        } else {
            "CREATE VIEW"
        };
        let mut statement = self.create_view(or_alter || existing_only)?;
        if let Statement::CreateView(view) = &mut statement {
            view.existing_only = existing_only;
            self.skip_empty_statements();
            if !first_in_batch || !self.at_end() {
                return Err(SqlError::new(
                    SqlState::SYNTAX_ERROR,
                    format!("'{shown}' must be the first statement in a query batch."),
                )
                .with_sqlserver_code(111)
                .at(position));
            }
            if orders_without_a_limit(&view.query) {
                return Err(order_by_in_view(view.query.position));
            }
        }
        Ok(statement)
    }

    pub(super) fn sqlserver_view_attributes(&mut self) -> SqlResult<()> {
        loop {
            let position = self.position();
            if self.take_word("encryption") {
                return Err(not_supported("WITH ENCRYPTION on a view").at(position));
            }
            if !(self.take_word("schemabinding") || self.take_word("view_metadata")) {
                return Err(self.unexpected());
            }
            if !self.take(&TokenKind::Comma) {
                return Ok(());
            }
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
        Ok(self.sqlserver_declared_type(false)?.engine_type())
    }

    pub(super) fn sqlserver_declared_type(&mut self, in_cast: bool) -> SqlResult<SqlServerType> {
        let token = self.peek();
        let word = match &token.kind {
            TokenKind::Word(word) => word.as_ref().to_owned(),
            TokenKind::QuotedIdentifier(name) => name.to_ascii_lowercase(),
            _ => return Err(self.unexpected()),
        };
        self.at += 1;
        let position = token.start;
        let unspecified = if in_cast { UNSPECIFIED_CAST_LENGTH } else { 1 };
        Ok(match word.as_str() {
            "int" | "integer" => SqlServerType::Int,
            "bigint" => SqlServerType::BigInt,
            "smallint" => SqlServerType::SmallInt,
            "tinyint" => SqlServerType::TinyInt,
            "bit" => SqlServerType::Bit,
            "real" => SqlServerType::Real,
            "float" => match self.sqlserver_type_argument(position)? {
                None | Some(25..=53) => SqlServerType::Float,
                Some(1..=24) => SqlServerType::Real,
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
            "money" => SqlServerType::Money,
            "smallmoney" => SqlServerType::SmallMoney,
            "char" | "character" => {
                SqlServerType::Char(self.sqlserver_length(&word, position, unspecified, false)?)
            }
            "nchar" => {
                SqlServerType::NChar(self.sqlserver_length(&word, position, unspecified, false)?)
            }
            "varchar" => {
                SqlServerType::VarChar(self.sqlserver_length(&word, position, unspecified, true)?)
            }
            "nvarchar" => SqlServerType::NVarChar(self.sqlserver_length(
                &word,
                position,
                unspecified,
                true,
            )?),
            "sysname" => SqlServerType::NVarChar(Length::Given(128)),
            "text" => SqlServerType::Text,
            "ntext" => SqlServerType::NText,
            "binary" => {
                SqlServerType::Binary(self.sqlserver_length(&word, position, unspecified, false)?)
            }
            "varbinary" => SqlServerType::VarBinary(self.sqlserver_length(
                &word,
                position,
                unspecified,
                true,
            )?),
            "image" => SqlServerType::Image,
            "uniqueidentifier" => SqlServerType::UniqueIdentifier,
            "date" => SqlServerType::Date,
            "datetime" => SqlServerType::DateTime,
            "smalldatetime" => SqlServerType::SmallDateTime,
            "time" => SqlServerType::Time {
                scale: self.sqlserver_time_scale(&word, position)?,
            },
            "datetime2" => SqlServerType::DateTime2 {
                scale: self.sqlserver_time_scale(&word, position)?,
            },
            "datetimeoffset" => SqlServerType::DateTimeOffset {
                scale: self.sqlserver_time_scale(&word, position)?,
            },
            "rowversion" | "timestamp" | "sql_variant" | "xml" | "hierarchyid" | "geography"
            | "geometry" => {
                return Err(not_supported(&format!("The data type {word}")).at(position));
            }
            _ => return Err(unknown_type(&word, position)),
        })
    }

    fn sqlserver_time_scale(&mut self, word: &str, position: usize) -> SqlResult<u8> {
        let digits = self
            .sqlserver_type_argument(position)?
            .unwrap_or(i64::from(MAX_TIME_SCALE));
        if !(0..=i64::from(MAX_TIME_SCALE)).contains(&digits) {
            return Err(SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!(
                    "Specified scale {digits} is invalid. '{word}' datatype must have scale between 0 and 7."
                ),
            )
            .with_sqlserver_code(1002)
            .at(position));
        }
        Ok(digits as u8)
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

    fn sqlserver_length(
        &mut self,
        word: &str,
        position: usize,
        unspecified: u32,
        max_allowed: bool,
    ) -> SqlResult<Length> {
        if max_allowed
            && self.peek_kind() == &TokenKind::LeftParen
            && self.is_word_at(1, "max")
            && self.peek_kind_at(2) == &TokenKind::RightParen
        {
            self.at += 3;
            return Ok(Length::Max);
        }
        let Some(length) = self.sqlserver_type_argument(position)? else {
            return Ok(Length::Given(unspecified));
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
        Ok(Length::Given(length as u32))
    }

    fn sqlserver_decimal(&mut self, position: usize) -> SqlResult<SqlServerType> {
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
        if !(1..=i64::from(MAX_DECIMAL_PRECISION)).contains(&precision) {
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
        Ok(SqlServerType::Decimal {
            precision: precision as u8,
            scale: scale as u8,
        })
    }

    pub(super) fn sqlserver_identity(&mut self, column: &mut ColumnDefinition) -> SqlResult<()> {
        let position = self.position();
        self.at += 1;
        let arguments = if self.take(&TokenKind::LeftParen) {
            let seed = self.identity_argument()?;
            self.expect(&TokenKind::Comma)?;
            let increment = self.identity_argument()?;
            self.expect(&TokenKind::RightParen)?;
            Some((seed, increment))
        } else {
            None
        };
        let Some((lowest, highest)) = crate::encoding::integer_type_range(column.sql_type) else {
            return Err(
                not_supported("IDENTITY on a column that is not int, bigint or smallint")
                    .at(position),
            );
        };
        if let Some((seed, increment)) = arguments {
            let fits = |value: Option<i64>| value.filter(|value| (lowest..=highest).contains(value));
            let invalid = |rule: &str| {
                invalid_identity_argument(&column.name.text, column.sql_type, rule, position)
            };
            let seed = fits(seed).ok_or_else(|| invalid("seed"))?;
            let increment = fits(increment)
                .filter(|&increment| increment != 0)
                .ok_or_else(|| invalid("increment, other than zero,"))?;
            let given = crate::encoding::IdentitySeed { seed, increment };
            column.identity_seed =
                (given != crate::encoding::IdentitySeed::ONE_BY_ONE).then_some(given);
        }
        column.auto_increment = true;
        column.not_null = true;
        Ok(())
    }

    fn identity_argument(&mut self) -> SqlResult<Option<i64>> {
        let negative = self.take_operator("-");
        if !negative {
            self.take_operator("+");
        }
        let TokenKind::Integer(digits) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let magnitude = digits.parse::<i128>().ok();
        self.at += 1;
        Ok(magnitude
            .map(|magnitude| if negative { -magnitude } else { magnitude })
            .and_then(|value| i64::try_from(value).ok()))
    }

    fn sqlserver_create_sequence(&mut self) -> SqlResult<Statement> {
        self.expect_word("sequence")?;
        let name = self.table_name()?;
        let (mut options, range) = self.sqlserver_sequence_options(true)?;
        let (low, high) = range.unwrap_or((i64::MIN, i64::MAX));
        let min = match options.min {
            Some(Some(min)) => min,
            _ => low,
        };
        let max = match options.max {
            Some(Some(max)) => max,
            _ => high,
        };
        options.min = Some(Some(min));
        options.max = Some(Some(max));
        if options.start.is_none() {
            let descending = options.increment.is_some_and(|increment| increment < 0);
            options.start = Some(if descending { max } else { min });
        }
        Ok(Statement::CreateSequence(CreateSequence {
            name,
            if_not_exists: false,
            options,
        }))
    }

    fn sqlserver_alter_sequence(&mut self) -> SqlResult<Statement> {
        self.expect_word("sequence")?;
        let name = self.table_name()?;
        let (options, _) = self.sqlserver_sequence_options(false)?;
        Ok(Statement::AlterSequence(AlterSequence {
            name,
            if_exists: false,
            options,
            rename: None,
        }))
    }

    fn sqlserver_sequence_options(
        &mut self,
        creating: bool,
    ) -> SqlResult<(SequenceOptions, Option<(i64, i64)>)> {
        let mut options = SequenceOptions::default();
        let mut range = None;
        loop {
            let position = self.position();
            if creating && self.take_word("as") {
                let declared = self.sqlserver_declared_type(false)?;
                let Some((sql_type, low, high)) = sequence_type(declared) else {
                    return Err(SqlError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "Sequence objects can only be of type tinyint, smallint, int, bigint, or decimal and numeric with a scale of 0 and a precision of 18 or less in this version of EggDB.",
                    )
                    .with_sqlserver_code(11702)
                    .at(position));
                };
                options.sql_type = Some((sql_type, position));
                range = Some((low, high));
            } else if self.take_word("start") {
                self.expect_word("with")?;
                options.start = Some(self.sequence_constant()?);
            } else if !creating && self.take_word("restart") {
                options.restart = Some(if self.take_word("with") {
                    Some(self.sequence_constant()?)
                } else {
                    None
                });
            } else if self.take_word("increment") {
                self.expect_word("by")?;
                options.increment = Some(self.sequence_constant()?);
            } else if self.take_word("minvalue") {
                options.min = Some(self.optional_sequence_constant()?);
            } else if self.take_word("maxvalue") {
                options.max = Some(self.optional_sequence_constant()?);
            } else if self.is_word("no") && self.is_word_at(1, "minvalue") {
                self.at += 2;
                options.min = Some(None);
            } else if self.is_word("no") && self.is_word_at(1, "maxvalue") {
                self.at += 2;
                options.max = Some(None);
            } else if self.is_word("no") && self.is_word_at(1, "cycle") {
                self.at += 2;
                options.cycle = Some(false);
            } else if self.is_word("no") && self.is_word_at(1, "cache") {
                self.at += 2;
            } else if self.take_word("cycle") {
                options.cycle = Some(true);
            } else if self.take_word("cache") {
                if matches!(self.peek_kind(), TokenKind::Integer(_)) {
                    self.at += 1;
                }
            } else {
                return Ok((options, range));
            }
        }
    }

    fn optional_sequence_constant(&mut self) -> SqlResult<Option<i64>> {
        let signed = self.is_operator("-") || self.is_operator("+");
        if signed || matches!(self.peek_kind(), TokenKind::Integer(_)) {
            return self.sequence_constant().map(Some);
        }
        Ok(None)
    }

    fn sequence_constant(&mut self) -> SqlResult<i64> {
        let position = self.position();
        self.identity_argument()?.ok_or_else(|| {
            SqlError::new(
                SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
                "The sequence option value is out of the range of bigint.",
            )
            .with_sqlserver_code(GENERIC_ERROR)
            .at(position)
        })
    }

    pub(super) fn sqlserver_top(&mut self) -> SqlResult<Option<Expr>> {
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
        let whole_view = self.is_word("percent");
        if whole_view {
            if !(self.at_view_top_level()
                && matches!(count.kind, ExprKind::Literal(Literal::Integer(100))))
            {
                return Err(not_supported("TOP ... PERCENT").at(self.position()));
            }
            self.at += 1;
        }
        if self.is_word("with") && self.is_word_at(1, "ties") {
            return Err(not_supported("TOP ... WITH TIES").at(self.position()));
        }
        if whole_view {
            self.ignored_view_top = true;
            return Ok(None);
        }
        Ok(Some(count))
    }

    pub(super) fn at_view_top_level(&self) -> bool {
        self.view_query_depth == Some(self.query_depth)
    }

    pub(super) fn skip_not_for_replication(&mut self) -> bool {
        if self.sqlserver()
            && self.is_word("not")
            && self.is_word_at(1, "for")
            && self.is_word_at(2, "replication")
        {
            self.at += 3;
            return true;
        }
        false
    }

    pub(super) fn sqlserver_column_option(&mut self) -> SqlResult<bool> {
        let position = self.position();
        if self.skip_not_for_replication() || self.take_word("rowguidcol") {
            return Ok(true);
        }
        if self.take_word("collate") {
            self.label()?;
            return Ok(true);
        }
        let refused = if self.is_word("sparse") {
            "SPARSE columns"
        } else if self.is_word("filestream") {
            "FILESTREAM columns"
        } else if self.is_word("masked") {
            "MASKED WITH (dynamic data masking)"
        } else if self.is_word("encrypted") {
            "ENCRYPTED WITH (Always Encrypted)"
        } else if self.is_word("generated") {
            "GENERATED ALWAYS AS ROW (system-versioned temporal tables)"
        } else {
            return Ok(false);
        };
        Err(not_supported(refused).at(position))
    }

    pub(super) fn skip_clustering(&mut self) {
        let _ = self.take_word("clustered") || self.take_word("nonclustered");
    }

    pub(super) fn sqlserver_key_columns(&mut self) -> SqlResult<Vec<Name>> {
        self.skip_clustering();
        let columns = self.index_column_list(true)?;
        self.skip_key_options()?;
        Ok(columns)
    }

    pub(super) fn skip_key_options(&mut self) -> SqlResult<()> {
        self.skip_index_options()?;
        self.skip_storage_clause()
    }

    pub(super) fn skip_index_options(&mut self) -> SqlResult<()> {
        if !self.is_word("with") {
            return Ok(());
        }
        if self.peek_kind_at(1) == &TokenKind::LeftParen {
            self.at += 2;
            loop {
                self.sqlserver_option("index", index_option_accepted)?;
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
        } else if self.is_word_at(1, "fillfactor") {
            self.at += 1;
            self.sqlserver_option("index", index_option_accepted)?;
        }
        Ok(())
    }

    fn sqlserver_option(&mut self, kind: &str, accepted: fn(&str, &str) -> bool) -> SqlResult<()> {
        let position = self.position();
        let TokenKind::Word(word) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let option = word.as_ref().to_owned();
        let shown = self.keyword_text();
        self.at += 1;
        if !self.take_operator("=") {
            return Err(self.unexpected());
        }
        let value = match self.peek_kind() {
            TokenKind::Word(word) => word.as_ref().to_owned(),
            TokenKind::Integer(digits) => (*digits).to_owned(),
            _ => return Err(self.unexpected()),
        };
        self.at += 1;
        let qualified = self.peek_kind() == &TokenKind::LeftParen
            || (self.is_word("on") && self.is_word_at(1, "partitions"));
        if !accepted(&option, &value) || qualified {
            return Err(not_supported(&format!(
                "The {kind} option {shown} = {}",
                value.to_ascii_uppercase()
            ))
            .at(position));
        }
        Ok(())
    }

    pub(super) fn skip_storage_clause(&mut self) -> SqlResult<()> {
        if !self.take_word("on") {
            return Ok(());
        }
        self.label()?;
        if self.take(&TokenKind::LeftParen) {
            self.label()?;
            self.expect(&TokenKind::RightParen)?;
        }
        Ok(())
    }

    pub(super) fn sqlserver_table_tail(&mut self) -> SqlResult<()> {
        loop {
            let position = self.position();
            if self.is_word("on") {
                self.skip_storage_clause()?;
            } else if self.take_word("textimage_on") {
                self.label()?;
            } else if self.is_word("filestream_on") {
                return Err(not_supported("FILESTREAM_ON").at(position));
            } else if self.is_word("with") && self.peek_kind_at(1) == &TokenKind::LeftParen {
                self.at += 2;
                loop {
                    self.sqlserver_option("table", table_option_accepted)?;
                    if !self.take(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(&TokenKind::RightParen)?;
            } else {
                return Ok(());
            }
        }
    }

    pub(super) fn sqlserver_table_element_refused(&self) -> Option<SqlError> {
        let refused = if self.is_word("period") && self.is_word_at(1, "for") {
            "PERIOD FOR SYSTEM_TIME (system-versioned temporal tables)"
        } else if self.is_word("index") {
            "An INDEX inside CREATE TABLE"
        } else {
            return None;
        };
        Some(not_supported(refused).at(self.position()))
    }

    pub(super) fn sqlserver_special_index(&self) -> Option<SqlError> {
        let first = usize::from(
            self.is_word("primary")
                || self.is_word("selective")
                || self.is_word("clustered")
                || self.is_word("nonclustered"),
        );
        let kind = if self.is_word_at(first, "xml") {
            "CREATE XML INDEX"
        } else if self.is_word_at(first, "spatial") {
            "CREATE SPATIAL INDEX"
        } else if self.is_word_at(first, "columnstore") {
            "CREATE COLUMNSTORE INDEX"
        } else if self.is_word_at(first, "fulltext") {
            "CREATE FULLTEXT INDEX"
        } else {
            return None;
        };
        self.is_word_at(first + 1, "index")
            .then(|| not_supported(kind).at(self.position()))
    }

    pub(super) fn sqlserver_index_tail(&mut self, unique: bool, key: &[Name]) -> SqlResult<()> {
        loop {
            let position = self.position();
            if self.take_word("include") {
                self.name_list()?;
            } else if self.take_word("where") {
                let filter = self.expression()?;
                if unique && !filters_out_nulls_only(&filter, key) {
                    return Err(not_supported(
                        "A filtered UNIQUE index other than WHERE <key column> IS NOT NULL",
                    )
                    .at(position));
                }
            } else if self.is_word("with")
                && (self.peek_kind_at(1) == &TokenKind::LeftParen
                    || self.is_word_at(1, "fillfactor"))
            {
                self.skip_index_options()?;
            } else if self.is_word("on") {
                self.skip_storage_clause()?;
            } else if self.is_word("filestream_on") {
                return Err(not_supported("FILESTREAM_ON").at(position));
            } else {
                return Ok(());
            }
        }
    }

    pub(super) fn sqlserver_primary(
        &mut self,
        kind: &TokenKind<'_>,
        position: usize,
    ) -> SqlResult<Option<Expr>> {
        let expr = match kind {
            TokenKind::Variable(name) if name.starts_with("@@") => {
                self.at += 1;
                self.system_variable(name, position)?
            }
            TokenKind::Variable(name) => {
                self.at += 1;
                self.variable_reference(name, position)?
            }
            TokenKind::Binary(digits) => {
                self.at += 1;
                super::sqlserver_functions::binary_literal(digits, position)?
            }
            TokenKind::Word(word) => match word.as_ref() {
                "cast" if self.peek_kind_at(1) == &TokenKind::LeftParen => {
                    self.sqlserver_cast(position)?
                }
                "convert" if self.peek_kind_at(1) == &TokenKind::LeftParen => {
                    self.sqlserver_convert(position)?
                }
                "left" | "right" | "try_convert"
                    if self.peek_kind_at(1) == &TokenKind::LeftParen =>
                {
                    let name = Name {
                        text: word.as_ref().to_owned(),
                        position,
                    };
                    self.at += 1;
                    match self.sqlserver_call(&name)? {
                        Some(expr) => expr,
                        None => return Err(unknown_function(&name.text, position)),
                    }
                }
                "next" if self.is_word_at(1, "value") && self.is_word_at(2, "for") => {
                    self.at += 3;
                    let sequence = self.table_name()?;
                    if self.is_word("over") {
                        return Err(
                            not_supported("NEXT VALUE FOR ... OVER (ORDER BY ...)").at(self.position())
                        );
                    }
                    Expr {
                        kind: ExprKind::Function {
                            name: "oracle_nextval".to_owned(),
                            arguments: vec![Expr {
                                kind: ExprKind::Literal(Literal::String(sequence.text)),
                                position: sequence.position,
                            }],
                        },
                        position,
                    }
                }
                "current_timestamp" => {
                    self.at += 1;
                    clock(3, position)
                }
                "current_user" | "session_user" | "user"
                    if self.peek_kind_at(1) != &TokenKind::LeftParen =>
                {
                    self.at += 1;
                    login_text(
                        "current_user",
                        crate::sqlserver_catalog::DBO.to_owned(),
                        position,
                    )
                }
                "system_user" if self.peek_kind_at(1) != &TokenKind::LeftParen => {
                    self.at += 1;
                    let login = self.sqlserver_login();
                    login_text("system_user", login, position)
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
            "@@error" => integer(
                i64::from(self.batch.map_or(0, |batch| batch.last_error)),
                position,
            ),
            "@@language" => session_text("us_english".to_owned(), position),
            "@@microsoftversion" => integer(microsoft_version(), position),
            "@@servicename" => session_text("MSSQLSERVER".to_owned(), position),
            "@@max_precision" => integer(38, position),
            "@@datefirst" => integer(7, position),
            "@@nestlevel" => integer(
                i64::from(self.batch.map_or(0, |batch| batch.nest_level)),
                position,
            ),
            "@@langid" | "@@procid" => integer(0, position),
            "@@lock_timeout" => integer(-1, position),
            "@@textsize" => integer(i64::from(i32::MAX), position),
            "@@max_connections" => integer(32_767, position),
            "@@rowcount" => Expr {
                kind: ExprKind::SqlServerFunction {
                    function: crate::sqlserver_functions::Function::RowCount,
                    arguments: Vec::new(),
                },
                position,
            },
            "@@identity" => Expr {
                kind: ExprKind::SqlServerFunction {
                    function: crate::sqlserver_functions::Function::ScopeIdentity,
                    arguments: Vec::new(),
                },
                position,
            },
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
        if let Some(expr) = self.sqlserver_call(name)? {
            return Ok(Some(expr));
        }
        if ENGINE_FUNCTIONS.contains(&name.text.as_str()) {
            return Ok(None);
        }
        Err(unknown_function(&name.text, name.position))
    }

    pub(super) fn sqlserver_login(&self) -> String {
        self.sqlserver_settings(|settings| {
            settings.get("session_authorization").unwrap_or_default()
        })
    }

    pub(super) fn sqlserver_qualified_name(&mut self, schema: Name) -> SqlResult<Name> {
        if schema.text == "dbo" {
            return self.name();
        }
        self.qualified_part(&schema)
    }
}

pub(crate) fn parameter_declarations(text: &str) -> SqlResult<Vec<ParameterDeclaration>> {
    let tokens = tokenize_in(text, Dialect::SqlServer)?;
    let mut parser = Parser::new(text, &tokens).in_dialect(Dialect::SqlServer);
    parser.parameter_declaration_list()
}

pub(crate) fn procedure_routine_name(text: &str) -> SqlResult<String> {
    let tokens = tokenize_in(text, Dialect::SqlServer)?;
    let mut parser = Parser::new(text, &tokens).in_dialect(Dialect::SqlServer);
    let (_, routine) = parser.procedure_name()?;
    if !parser.at_end() {
        return Err(parser.unexpected());
    }
    Ok(routine)
}

fn accepted_set() -> Statement {
    Statement::Ignored {
        tag: "SET",
        notice: None,
    }
}

fn orders_without_a_limit(query: &Query) -> bool {
    let (order_by, limit, offset) = match &query.body {
        SetExpr::Select(select) if query.order_by.is_empty() => {
            (&select.order_by, &select.limit, &select.offset)
        }
        _ => (&query.order_by, &query.limit, &query.offset),
    };
    !order_by.is_empty() && limit.is_none() && offset.is_none()
}

fn order_by_in_view(position: usize) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        "The ORDER BY clause is invalid in views, inline functions, derived tables, subqueries, and common table expressions, unless TOP, OFFSET or FOR XML is also specified.",
    )
    .with_sqlserver_code(1033)
    .at(position)
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

fn sequence_type(declared: SqlServerType) -> Option<(SqlType, i64, i64)> {
    Some(match declared {
        SqlServerType::TinyInt => (SqlType::SmallInt, 0, i64::from(u8::MAX)),
        SqlServerType::SmallInt => (
            SqlType::SmallInt,
            i64::from(i16::MIN),
            i64::from(i16::MAX),
        ),
        SqlServerType::Int => (
            SqlType::Integer,
            i64::from(i32::MIN),
            i64::from(i32::MAX),
        ),
        SqlServerType::BigInt => (SqlType::BigInt, i64::MIN, i64::MAX),
        SqlServerType::Decimal {
            precision,
            scale: 0,
        } if u32::from(precision) <= 18 => {
            let largest = 10i64.pow(u32::from(precision)) - 1;
            (SqlType::BigInt, -largest, largest)
        }
        _ => return None,
    })
}

pub(super) fn extended_property_comment(
    call: ProcedureCall,
    position: usize,
) -> SqlResult<Statement> {
    let dropping = call.procedure.text == "sp_dropextendedproperty";
    let procedure = call.procedure.text.clone();
    let mut values: [Option<String>; 8] = Default::default();
    for (index, argument) in call.arguments.iter().enumerate() {
        let slot = match &argument.name {
            Some(name) => EXTENDED_PROPERTY_PARAMETERS
                .iter()
                .position(|known| known.eq_ignore_ascii_case(name))
                .filter(|&slot| !(dropping && slot == 1)),
            None if dropping => Some(if index == 0 { 0 } else { index + 1 }),
            None => Some(index),
        };
        let Some(slot) = slot.filter(|&slot| slot < EXTENDED_PROPERTY_PARAMETERS.len()) else {
            return Err(SqlError::new(
                SqlState::UNDEFINED_PARAMETER,
                format!(
                    "{} is not a parameter for procedure {procedure}.",
                    argument.name.as_deref().unwrap_or("A parameter")
                ),
            )
            .with_sqlserver_code(8145)
            .at(position));
        };
        values[slot] = match &argument.value {
            Some(expr) => property_text(expr)?,
            None => None,
        };
    }
    let [name, value, level0_type, level0_name, level1_type, level1_name, level2_type, level2_name] =
        values;
    if !name
        .as_deref()
        .is_some_and(|name| name.eq_ignore_ascii_case(DESCRIPTION_PROPERTY))
    {
        return Err(not_supported("Extended properties other than MS_Description").at(position));
    }
    let named = |text: Option<String>, what: &str| -> SqlResult<Name> {
        let Some(text) = text.filter(|text| !text.is_empty()) else {
            return Err(SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("An invalid parameter or option was specified for procedure '{procedure}': {what} is missing."),
            )
            .with_sqlserver_code(15600)
            .at(position));
        };
        let folded = text.to_lowercase();
        if folded.contains(crate::schemas::QUALIFIER) {
            return Err(SqlError::new(
                SqlState::INVALID_NAME,
                format!("invalid name \"{}\"", folded.escape_default()),
            )
            .at(position));
        }
        Ok(Name {
            text: folded,
            position,
        })
    };
    let level0_type = level0_type.map(|kind| kind.to_ascii_lowercase());
    match level0_type.as_deref() {
        Some("schema") => {}
        None => {
            return Err(not_supported("An extended property of the database").at(position));
        }
        Some(other) => {
            return Err(
                not_supported(&format!("An extended property on a {}", other.to_ascii_uppercase()))
                    .at(position),
            );
        }
    }
    let schema = named(level0_name, "@level0name")?;
    let schema_text = if schema.text == "dbo" {
        crate::schemas::DEFAULT_SCHEMA.to_owned()
    } else {
        schema.text.clone()
    };
    let level1_type = level1_type.map(|kind| kind.to_ascii_lowercase());
    let kind = match level1_type.as_deref() {
        None => {
            return Ok(described(
                CommentTarget::Schema(Name {
                    text: schema_text,
                    position,
                }),
                value,
                dropping,
            ));
        }
        Some("table") => RelationKind::Table,
        Some("view") => RelationKind::View,
        Some(other) => {
            return Err(
                not_supported(&format!("An extended property on a {}", other.to_ascii_uppercase()))
                    .at(position),
            );
        }
    };
    let object = named(level1_name, "@level1name")?;
    let relation = Name {
        text: if schema.text == "dbo" {
            object.text
        } else {
            crate::schemas::written_qualified(&schema.text, &object.text)
        },
        position,
    };
    let level2_type = level2_type.map(|kind| kind.to_ascii_lowercase());
    let target = match level2_type.as_deref() {
        None => CommentTarget::Relation {
            kind,
            name: relation,
        },
        Some("column") => CommentTarget::Column {
            table: relation,
            column: named(level2_name, "@level2name")?,
        },
        Some("constraint") => CommentTarget::Constraint {
            table: relation,
            name: named(level2_name, "@level2name")?,
        },
        Some("index") => {
            let index = named(level2_name, "@level2name")?;
            CommentTarget::Index(Name {
                text: if schema.text == "dbo" {
                    index.text
                } else {
                    crate::schemas::written_qualified(&schema.text, &index.text)
                },
                position,
            })
        }
        Some(other) => {
            return Err(
                not_supported(&format!("An extended property on a {}", other.to_ascii_uppercase()))
                    .at(position),
            );
        }
    };
    Ok(described(target, value, dropping))
}

fn described(target: CommentTarget, value: Option<String>, dropping: bool) -> Statement {
    Statement::Comment(Comment {
        target,
        text: if dropping { None } else { value },
    })
}

fn property_text(expr: &Expr) -> SqlResult<Option<String>> {
    let evaluator = Evaluator {
        scope: Scope::empty(),
        row: &[],
        correlation: None,
    };
    let value = evaluator.eval(expr, None)?;
    Ok(if value.is_null() { None } else { value.to_text() })
}
