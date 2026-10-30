use super::Parser;
use crate::ast::{
    AggregateFunction, BinaryOperator, ColumnDefinition, Expr, ExprKind, Literal, Name, OrderItem,
    SelectItem, SequenceOptions, Statement, TableReference, UnaryOperator,
};
use crate::encoding::{IdentitySeed, integer_type_range};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::children;
use crate::lexer::{TokenKind, swap_uniform_case};
use crate::oracle_intervals::{
    DEFAULT_LEADING_PRECISION, IntervalField, MAX_PRECISION, Qualifier,
};
use crate::settings::Settings;
use crate::types::{SqlType, Value};

/// The Oracle Database release whose documented behaviour EggDB's Oracle dialect follows.
pub const ORACLE_COMPATIBLE_VERSION: &str = "23ai";

const MAX_DECODE_ARGUMENTS: usize = 255;

pub(crate) fn is_reserved(word: &str) -> bool {
    matches!(
        word,
        "access"
            | "add"
            | "all"
            | "alter"
            | "and"
            | "any"
            | "as"
            | "asc"
            | "audit"
            | "between"
            | "by"
            | "char"
            | "check"
            | "cluster"
            | "column"
            | "comment"
            | "compress"
            | "connect"
            | "create"
            | "current"
            | "date"
            | "decimal"
            | "default"
            | "delete"
            | "desc"
            | "distinct"
            | "drop"
            | "else"
            | "exclusive"
            | "exists"
            | "file"
            | "float"
            | "for"
            | "from"
            | "grant"
            | "group"
            | "having"
            | "identified"
            | "immediate"
            | "in"
            | "increment"
            | "index"
            | "initial"
            | "insert"
            | "integer"
            | "intersect"
            | "into"
            | "is"
            | "level"
            | "like"
            | "lock"
            | "long"
            | "maxextents"
            | "minus"
            | "mlslabel"
            | "mode"
            | "modify"
            | "noaudit"
            | "nocompress"
            | "not"
            | "nowait"
            | "null"
            | "number"
            | "of"
            | "offline"
            | "on"
            | "online"
            | "option"
            | "or"
            | "order"
            | "pctfree"
            | "prior"
            | "public"
            | "raw"
            | "rename"
            | "resource"
            | "revoke"
            | "row"
            | "rowid"
            | "rownum"
            | "rows"
            | "select"
            | "session"
            | "set"
            | "share"
            | "size"
            | "smallint"
            | "start"
            | "successful"
            | "synonym"
            | "sysdate"
            | "table"
            | "then"
            | "to"
            | "trigger"
            | "uid"
            | "union"
            | "unique"
            | "update"
            | "user"
            | "validate"
            | "values"
            | "varchar"
            | "varchar2"
            | "view"
            | "whenever"
            | "where"
            | "with"
    )
}

pub(super) fn is_clause_word(word: &str) -> bool {
    matches!(word, "minus" | "connect" | "start" | "model" | "pivot")
}

pub(super) fn is_dictionary_name(name: &str) -> bool {
    crate::oracle_dictionary::OracleRelation::named(name).is_some()
}

fn literal(literal: Literal, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Literal(literal),
        position,
    }
}

fn oracle_function(name: &'static str, arguments: Vec<Expr>, position: usize) -> Expr {
    Expr {
        kind: ExprKind::OracleFunction { name, arguments },
        position,
    }
}

fn function(name: &str, arguments: Vec<Expr>, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Function {
            name: name.to_owned(),
            arguments,
        },
        position,
    }
}

pub(super) fn concatenation(left: Expr, right: Expr) -> Expr {
    let position = left.position;
    oracle_function("oracle_concat", vec![left, right], position)
}

pub(super) fn division(left: Expr, right: Expr) -> Expr {
    let position = left.position;
    oracle_function("oracle_divide", vec![left, right], position)
}

pub(super) fn ansi_literal(date: bool, text: Expr, position: usize) -> Expr {
    let name = if date {
        "oracle_date_literal"
    } else {
        "oracle_timestamp_literal"
    };
    oracle_function(name, vec![text], position)
}

pub(super) fn not_below_zero(count: Expr) -> Expr {
    let position = count.position;
    oracle_function("oracle_row_count", vec![count], position)
}

fn invalid_identifier(name: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("function {name}() does not exist"),
    )
    .at(position)
}

fn wrong_argument_count(name: &Name, too_few: bool) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        if too_few {
            "not enough arguments for function"
        } else {
            "too many arguments for function"
        },
    )
    .at(name.position)
}

fn is_rownum(expr: &Expr) -> bool {
    matches!(&expr.kind, ExprKind::Column { qualifier: None, name } if name == "rownum")
}

fn mentions_rownum(expr: &Expr) -> bool {
    is_rownum(expr) || children(expr).into_iter().any(mentions_rownum)
}

fn integer_of(expr: &Expr) -> Option<i64> {
    match &expr.kind {
        ExprKind::Literal(Literal::Integer(number)) => Some(*number),
        ExprKind::Literal(Literal::String(text)) => text.trim().parse().ok(),
        ExprKind::Cast { expr, .. } => integer_of(expr),
        ExprKind::Unary(UnaryOperator::Minus, operand) => match &operand.kind {
            ExprKind::Literal(Literal::Integer(number)) => number.checked_neg(),
            _ => None,
        },
        _ => None,
    }
}

fn mirrored(operator: BinaryOperator) -> BinaryOperator {
    match operator {
        BinaryOperator::Less => BinaryOperator::Greater,
        BinaryOperator::LessOrEqual => BinaryOperator::GreaterOrEqual,
        BinaryOperator::Greater => BinaryOperator::Less,
        BinaryOperator::GreaterOrEqual => BinaryOperator::LessOrEqual,
        other => other,
    }
}

enum RownumBound {
    NoLimit,
    Limit(u64),
}

fn rownum_bound(expr: &Expr) -> Option<RownumBound> {
    let ExprKind::Binary(operator, left, right) = &expr.kind else {
        return None;
    };
    let (operator, bound) = if is_rownum(left) {
        (*operator, integer_of(right)?)
    } else if is_rownum(right) {
        (mirrored(*operator), integer_of(left)?)
    } else {
        return None;
    };
    let below = |count: i64| RownumBound::Limit(u64::try_from(count.max(0)).unwrap_or(0));
    Some(match operator {
        BinaryOperator::LessOrEqual => below(bound),
        BinaryOperator::Less => below(bound.saturating_sub(1)),
        BinaryOperator::Equal if bound == 1 => RownumBound::Limit(1),
        BinaryOperator::Equal => RownumBound::Limit(0),
        BinaryOperator::Greater if bound < 1 => RownumBound::NoLimit,
        BinaryOperator::GreaterOrEqual if bound <= 1 => RownumBound::NoLimit,
        BinaryOperator::Greater | BinaryOperator::GreaterOrEqual => RownumBound::Limit(0),
        _ => return None,
    })
}

fn rownum_not_supported(position: usize) -> SqlError {
    SqlError::not_supported(
        "ROWNUM is supported only in WHERE conditions such as ROWNUM <= n, joined by AND",
    )
    .at(position)
}

pub(super) fn split_rownum(filter: Option<Expr>) -> SqlResult<(Option<Expr>, Option<u64>)> {
    let Some(filter) = filter else {
        return Ok((None, None));
    };
    if !mentions_rownum(&filter) {
        return Ok((Some(filter), None));
    }
    let position = filter.position;
    let conjuncts = match filter.kind {
        ExprKind::And(parts) => parts,
        other => vec![Expr {
            kind: other,
            position,
        }],
    };
    let mut limit: Option<u64> = None;
    let mut kept = Vec::new();
    for conjunct in conjuncts {
        if !mentions_rownum(&conjunct) {
            kept.push(conjunct);
            continue;
        }
        match rownum_bound(&conjunct) {
            Some(RownumBound::Limit(count)) => {
                limit = Some(limit.map_or(count, |known| known.min(count)));
            }
            Some(RownumBound::NoLimit) => {}
            None => return Err(rownum_not_supported(conjunct.position)),
        }
    }
    let filter = match kept.len() {
        0 => None,
        1 => kept.pop(),
        _ => Some(Expr {
            kind: ExprKind::And(kept),
            position,
        }),
    };
    Ok((filter, limit))
}

pub(super) fn reject_rownum_outside_where(
    items: &[SelectItem],
    group_by: &[Expr],
    having: Option<&Expr>,
    order_by: &[OrderItem],
) -> SqlResult<()> {
    let item_exprs = items.iter().filter_map(|item| match item {
        SelectItem::Expr { expr, .. } => Some(expr),
        SelectItem::Wildcard { .. } => None,
    });
    let found = item_exprs
        .chain(group_by.iter())
        .chain(having)
        .chain(order_by.iter().map(|item| &item.expr))
        .find(|expr| mentions_rownum(expr));
    match found {
        Some(expr) => Err(rownum_not_supported(expr.position)),
        None => Ok(()),
    }
}

pub(super) fn label(source: &str) -> String {
    let mut shown = String::with_capacity(source.len());
    let mut quote = None;
    for character in source.chars() {
        match quote {
            Some('"') => {
                shown.push(character);
                if character == '"' {
                    quote = None;
                }
            }
            Some(_) => {
                shown.push(character.to_ascii_uppercase());
                if character == '\'' {
                    quote = None;
                }
            }
            None if character == '\'' || character == '"' => {
                quote = Some(character);
                shown.push(character);
            }
            None if character.is_whitespace() => {}
            None => shown.push(character.to_ascii_uppercase()),
        }
    }
    swap_uniform_case(&shown)
}

impl<'t> Parser<'t> {
    fn oracle_settings<T>(&self, read: impl FnOnce(&Settings) -> T) -> T {
        match self.settings {
            Some(settings) => read(settings),
            None => read(&Settings::default()),
        }
    }

    pub(super) fn oracle_user(&self) -> String {
        self.oracle_settings(|settings| {
            settings
                .get("session_authorization")
                .unwrap_or_default()
                .to_uppercase()
        })
    }

    pub(super) fn oracle_user_schema(&self) -> String {
        self.oracle_settings(|settings| {
            settings
                .oracle_user_schema
                .clone()
                .unwrap_or_else(|| crate::schemas::DEFAULT_SCHEMA.to_owned())
        })
    }

    fn oracle_current_schema(&self) -> String {
        let user_schema = self.oracle_user_schema();
        let first = self.oracle_settings(|settings| {
            crate::schemas::path_entries(&settings.get("search_path").unwrap_or_default())
                .into_iter()
                .next()
        });
        let schema = match first {
            Some(entry) if entry != "$user" => entry,
            _ => user_schema.clone(),
        };
        crate::oracle_dictionary::owner_name(&schema, &self.oracle_user(), &user_schema)
    }

    fn sys_context(&mut self, name: &Name) -> SqlResult<Expr> {
        let position = name.position;
        let arguments = self.oracle_arguments()?;
        if arguments.len() < 2 {
            return Err(wrong_argument_count(name, true));
        }
        if arguments.len() > 3 {
            return Err(wrong_argument_count(name, false));
        }
        let constant = |expr: &Expr| match &expr.kind {
            ExprKind::Literal(Literal::String(text)) => Some(text.clone()),
            _ => None,
        };
        let (Some(namespace), Some(parameter)) = (
            arguments.first().and_then(constant),
            arguments.get(1).and_then(constant),
        ) else {
            return Err(
                self.not_supported("SYS_CONTEXT with arguments other than text constants")
            );
        };
        if !namespace.trim().eq_ignore_ascii_case("userenv") {
            return Ok(literal(Literal::Null, position));
        }
        let user = self.oracle_user();
        let current_schema = self.oracle_current_schema();
        let (database, session_id) = self.oracle_settings(|settings| {
            (settings.database.clone(), settings.sqlserver.connection_id)
        });
        let facts = crate::oracle_dictionary::SessionFacts {
            user: &user,
            current_schema: &current_schema,
            database: &database,
            session_id,
        };
        let value = crate::oracle_dictionary::userenv(&parameter, &facts)
            .map_err(|error| error.at(position))?;
        Ok(match value {
            Some(value) => Expr {
                kind: ExprKind::SessionValue {
                    name: "sys_context",
                    value,
                },
                position,
            },
            None => literal(Literal::Null, position),
        })
    }

    pub(super) fn oracle_statement(&mut self) -> SqlResult<Option<Statement>> {
        let TokenKind::Word(word) = self.peek_kind() else {
            return self.plsql_statement();
        };
        let statement = match word.as_ref() {
            "drop" if self.is_word_at(1, "table") => self.oracle_drop_table()?,
            "truncate" => {
                self.at += 1;
                self.expect_word("table")?;
                let name = self.table_name()?;
                Statement::Truncate(TableReference::named(name, None))
            }
            "create" if self.is_word_at(1, "sequence") => {
                self.at += 1;
                self.create_sequence()?
            }
            "alter" if self.is_word_at(1, "sequence") => {
                self.at += 1;
                self.alter_sequence()?
            }
            "drop" if self.is_word_at(1, "sequence") => {
                self.at += 1;
                self.drop_sequence()?
            }
            "merge" => self.merge(&super::merge::MergeRules::oracle())?,
            "alter" if self.is_word_at(1, "session") => self.alter_session()?,
            "begin"
                if matches!(
                    self.peek_kind_at(1),
                    TokenKind::End | TokenKind::Semicolon
                ) =>
            {
                return Ok(None);
            }
            "start" | "end" | "abort" | "show" | "reset" | "values" | "table" | "listen"
            | "notify" | "unlisten" | "vacuum" | "discard" => {
                return Err(self.unexpected());
            }
            "insert" if self.is_word_at(1, "all") || self.is_word_at(1, "first") => {
                return Err(self.not_supported("INSERT ALL and INSERT FIRST"));
            }
            _ => return self.plsql_statement(),
        };
        Ok(Some(statement))
    }

    fn alter_session(&mut self) -> SqlResult<Statement> {
        self.at += 2;
        if !self.take_word("set") {
            return Err(self.not_supported("ALTER SESSION other than SET"));
        }
        let mut settings = Vec::new();
        loop {
            let name = match self.peek_kind() {
                TokenKind::Word(word) => word.to_ascii_lowercase(),
                TokenKind::QuotedIdentifier(name) => name.to_ascii_lowercase(),
                _ => break,
            };
            self.at += 1;
            let _ = self.take_operator("=");
            let value = match self.peek_kind() {
                TokenKind::Word(word) => word.to_ascii_lowercase(),
                TokenKind::QuotedIdentifier(name) => name.clone(),
                TokenKind::String(text) => format!("'{text}'"),
                TokenKind::Integer(digits) | TokenKind::Decimal(digits) => (*digits).to_owned(),
                _ => return Err(self.unexpected()),
            };
            self.at += 1;
            settings.push((name, value));
        }
        if settings.is_empty() {
            return Err(
                SqlError::new(SqlState::SYNTAX_ERROR, "invalid option for ALTER SESSION")
                    .with_oracle_code(2248)
                    .at(self.position()),
            );
        }
        Ok(Statement::AlterSession(settings))
    }

    fn oracle_drop_table(&mut self) -> SqlResult<Statement> {
        self.at += 2;
        let if_exists = if self.take_word("if") {
            self.expect_word("exists")?;
            true
        } else {
            false
        };
        let name = self.table_name()?;
        let cascade = self.take_word("cascade");
        if cascade {
            self.expect_word("constraints")?;
        }
        self.take_word("purge");
        Ok(Statement::DropTable {
            names: vec![name],
            if_exists,
            cascade,
        })
    }

    pub(super) fn oracle_dual(&mut self) -> bool {
        let named_dual = match self.peek_kind() {
            TokenKind::Word(word) => word == "dual",
            TokenKind::QuotedIdentifier(name) => name == "dual",
            _ => false,
        };
        if !named_dual || self.peek_kind_at(1) == &TokenKind::Dot {
            return false;
        }
        self.at += 1;
        if self.at_bare_alias() {
            self.at += 1;
        }
        true
    }

    pub(super) fn dual_items(items: Vec<SelectItem>) -> Vec<SelectItem> {
        items
            .into_iter()
            .map(|item| match item {
                SelectItem::Wildcard { position, .. } => SelectItem::Expr {
                    expr: literal(Literal::String("X".to_owned()), position),
                    alias: Some("dummy".to_owned()),
                },
                other => other,
            })
            .collect()
    }

    pub(super) fn oracle_from_expected(&self) -> SqlError {
        SqlError::new(
            SqlState::SYNTAX_ERROR,
            "FROM keyword not found where expected",
        )
        .at(self.position())
    }

    pub(super) fn oracle_fetch(&mut self) -> SqlResult<Expr> {
        let position = self.position();
        self.at += 1;
        if !self.take_word("first") {
            self.expect_word("next")?;
        }
        let count = if self.is_word("row") || self.is_word("rows") {
            literal(Literal::Integer(1), position)
        } else {
            self.expression()?
        };
        let percent = self.take_word("percent");
        if !self.take_word("rows") {
            self.expect_word("row")?;
        }
        let with_ties = if self.take_word("with") {
            self.expect_word("ties")?;
            true
        } else {
            self.expect_word("only")?;
            false
        };
        Ok(match (percent, with_ties) {
            (false, false) => not_below_zero(count),
            (false, true) => {
                oracle_function("oracle_fetch_with_ties", vec![not_below_zero(count)], position)
            }
            (true, false) => oracle_function("oracle_fetch_percent", vec![count], position),
            (true, true) => {
                oracle_function("oracle_fetch_percent_with_ties", vec![count], position)
            }
        })
    }

    pub(super) fn oracle_type(&mut self) -> SqlResult<SqlType> {
        let token = self.peek();
        let word: &str = match &token.kind {
            TokenKind::Word(word) => word,
            _ => return Err(self.unexpected()),
        };
        let start = token.start;
        self.at += 1;
        let sql_type = match word {
            "varchar2" | "varchar" | "nvarchar2" => {
                let length = self.oracle_length(true)?;
                SqlType::Varchar(length)
            }
            "char" | "nchar" | "character" => {
                if word == "character" && self.take_word("varying") {
                    return Ok(SqlType::Varchar(self.oracle_length(true)?));
                }
                SqlType::Bpchar(Some(self.oracle_length(false)?.unwrap_or(1)))
            }
            "raw" => {
                self.oracle_length(true)?;
                SqlType::Bytea
            }
            "long" if self.take_word("raw") => SqlType::Bytea,
            "blob" => SqlType::Bytea,
            "number" | "numeric" | "decimal" | "dec" => self.oracle_number()?,
            "integer" | "int" | "smallint" => SqlType::BigInt,
            "float" => {
                if self.take(&TokenKind::LeftParen) {
                    self.oracle_type_argument()?;
                    self.expect(&TokenKind::RightParen)?;
                }
                SqlType::Double
            }
            "real" => SqlType::Double,
            "double" => {
                self.expect_word("precision")?;
                SqlType::Double
            }
            "binary_float" => SqlType::Real,
            "binary_double" => SqlType::Double,
            "clob" | "nclob" => SqlType::Text,
            "long" if !self.is_word("raw") => SqlType::Text,
            "boolean" | "bool" => SqlType::Boolean,
            "date" => SqlType::Timestamp(Some(0)),
            "timestamp" => self.oracle_timestamp(start)?,
            "json" => SqlType::Jsonb,
            "interval" => self.oracle_interval_type(start)?,
            "long" | "bfile" | "rowid" | "urowid" | "xmltype" | "vector"
            | "sdo_geometry" => {
                return Err(SqlError::not_supported(format!(
                    "type {} is not supported yet",
                    word.to_ascii_uppercase()
                ))
                .at(start));
            }
            _ => {
                return Err(SqlError::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!("type \"{word}\" does not exist"),
                )
                .at(start));
            }
        };
        Ok(sql_type)
    }

    fn oracle_type_argument(&mut self) -> SqlResult<i64> {
        let position = self.position();
        match self.peek_kind() {
            TokenKind::Integer(digits) => {
                let value = digits.parse::<i64>().map_err(|_| self.unexpected())?;
                self.at += 1;
                Ok(value)
            }
            TokenKind::Operator("-") => {
                self.at += 1;
                match self.peek_kind() {
                    TokenKind::Integer(digits) => {
                        let value = digits.parse::<i64>().map_err(|_| self.unexpected())?;
                        self.at += 1;
                        Ok(-value)
                    }
                    _ => Err(self.unexpected()),
                }
            }
            _ => Err(SqlError::syntax("invalid number", position)),
        }
    }

    fn oracle_length(&mut self, required: bool) -> SqlResult<Option<u32>> {
        if !self.take(&TokenKind::LeftParen) {
            if required {
                return Err(SqlError::syntax(
                    "missing left parenthesis",
                    self.position(),
                ));
            }
            return Ok(None);
        }
        let position = self.position();
        let length = self.oracle_type_argument()?;
        let _ = self.take_word("byte") || self.take_word("char");
        self.expect(&TokenKind::RightParen)?;
        let length = u32::try_from(length)
            .ok()
            .filter(|length| (1..=32767).contains(length))
            .ok_or_else(|| {
                SqlError::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    "illegal zero-length or too long column length",
                )
                .at(position)
            })?;
        Ok(Some(length))
    }

    fn oracle_timestamp(&mut self, position: usize) -> SqlResult<SqlType> {
        let mut precision = 6;
        if self.take(&TokenKind::LeftParen) {
            let digits = self.oracle_type_argument()?;
            self.expect(&TokenKind::RightParen)?;
            if !(0..=9).contains(&digits) {
                return Err(SqlError::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    "datetime/interval precision is out of range",
                )
                .at(position));
            }
            precision = digits.min(6) as u8;
        }
        if self.take_word("with") {
            self.take_word("local");
            self.expect_word("time")?;
            self.expect_word("zone")?;
            return Ok(SqlType::TimestampTz(Some(precision)));
        }
        Ok(SqlType::Timestamp(Some(precision)))
    }

    fn oracle_interval_precision(&mut self, position: usize) -> SqlResult<u8> {
        let digits = self.oracle_type_argument()?;
        u8::try_from(digits)
            .ok()
            .filter(|digits| *digits <= MAX_PRECISION)
            .ok_or_else(|| {
                SqlError::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    "datetime/interval precision is out of range",
                )
                .with_oracle_code(30088)
                .at(position)
            })
    }

    fn oracle_interval_field(&mut self) -> SqlResult<IntervalField> {
        let TokenKind::Word(word) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let Some(field) = IntervalField::named(word.as_ref()) else {
            return Err(self.unexpected());
        };
        self.at += 1;
        Ok(field)
    }

    pub(super) fn oracle_interval_qualifier(&mut self, position: usize) -> SqlResult<Qualifier> {
        let leading = self.oracle_interval_field()?;
        let mut qualifier = Qualifier {
            leading,
            trailing: leading,
            leading_precision: DEFAULT_LEADING_PRECISION,
            fraction_digits: crate::oracle_intervals::DEFAULT_FRACTION_DIGITS,
        };
        if self.take(&TokenKind::LeftParen) {
            qualifier.leading_precision = self.oracle_interval_precision(position)?;
            if leading == IntervalField::Second && self.take(&TokenKind::Comma) {
                qualifier.fraction_digits = self.oracle_interval_precision(position)?;
            }
            self.expect(&TokenKind::RightParen)?;
        }
        if self.take_word("to") {
            qualifier.trailing = self.oracle_interval_field()?;
            if qualifier.trailing == IntervalField::Second && self.take(&TokenKind::LeftParen) {
                qualifier.fraction_digits = self.oracle_interval_precision(position)?;
                self.expect(&TokenKind::RightParen)?;
            }
        }
        if !qualifier.is_valid() {
            return Err(SqlError::syntax("missing keyword", self.position()).with_oracle_code(905));
        }
        if qualifier.trailing != IntervalField::Second {
            qualifier.fraction_digits = 0;
        }
        Ok(qualifier)
    }

    fn oracle_interval_type(&mut self, position: usize) -> SqlResult<SqlType> {
        let qualifier = self.oracle_interval_qualifier(position)?;
        match (qualifier.leading, qualifier.trailing) {
            (IntervalField::Year, IntervalField::Month)
            | (IntervalField::Day, IntervalField::Second) => Ok(qualifier.sql_type()),
            _ => Err(SqlError::syntax("missing keyword", position).with_oracle_code(905)),
        }
    }

    fn oracle_interval_literal(&mut self, position: usize) -> SqlResult<Option<Expr>> {
        let text = match self.peek_kind_at(1) {
            TokenKind::String(text) => text.as_ref().to_owned(),
            _ => return Ok(None),
        };
        self.at += 2;
        let qualifier = self.oracle_interval_qualifier(position)?;
        let value = crate::oracle_intervals::parse_literal(&text, qualifier)
            .map_err(|error| error.at(position))?;
        Ok(Some(Expr {
            kind: ExprKind::Constant {
                value: Value::Interval(value),
                sql_type: Some(qualifier.sql_type()),
            },
            position,
        }))
    }

    fn oracle_listagg(&mut self, name: &Name) -> SqlResult<Expr> {
        let position = name.position;
        self.expect(&TokenKind::LeftParen)?;
        let distinct = self.take_word("distinct");
        if !distinct {
            self.take_word("all");
        }
        let measure = self.expression()?;
        let delimiter = if self.take(&TokenKind::Comma) {
            self.expression()?
        } else {
            literal(Literal::Null, position)
        };
        let mut truncate = false;
        let mut indicator = crate::oracle_listagg::DEFAULT_TRUNCATION_INDICATOR.to_owned();
        let mut with_count = true;
        if self.take_word("on") {
            self.expect_word("overflow")?;
            if self.take_word("truncate") {
                truncate = true;
                if let TokenKind::String(text) = self.peek_kind() {
                    indicator = text.as_ref().to_owned();
                    self.at += 1;
                }
                if self.take_word("with") {
                    self.expect_word("count")?;
                } else if self.take_word("without") {
                    self.expect_word("count")?;
                    with_count = false;
                }
            } else {
                self.expect_word("error")?;
            }
        }
        self.expect(&TokenKind::RightParen)?;
        let mut order = Vec::new();
        if self.take_word("within") {
            self.expect_word("group")?;
            self.expect(&TokenKind::LeftParen)?;
            self.expect_word("order")?;
            self.expect_word("by")?;
            order = self.order_items()?;
            self.expect(&TokenKind::RightParen)?;
        }
        if self.is_word("over") {
            return Err(self.not_supported("LISTAGG as an analytic function (OVER)"));
        }
        let text = oracle_function("oracle_concat", vec![measure], position);
        let argument = if order.is_empty() {
            text
        } else {
            crate::aggregate::ordered_arguments(text, order)
        };
        let parts = Expr {
            kind: ExprKind::Aggregate {
                function: AggregateFunction::ArrayAgg,
                argument: Some(Box::new(argument)),
                distinct,
            },
            position,
        };
        Ok(oracle_function(
            "oracle_listagg",
            vec![
                parts,
                delimiter,
                literal(Literal::Boolean(truncate), position),
                literal(Literal::String(indicator), position),
                literal(Literal::Boolean(with_count), position),
            ],
            position,
        ))
    }

    fn oracle_number(&mut self) -> SqlResult<SqlType> {
        if !self.take(&TokenKind::LeftParen) {
            return Ok(SqlType::Numeric(None));
        }
        let position = self.position();
        let precision = if self.take_operator("*") {
            38
        } else {
            self.oracle_type_argument()?
        };
        let scale = if self.take(&TokenKind::Comma) {
            self.oracle_type_argument()?
        } else {
            0
        };
        self.expect(&TokenKind::RightParen)?;
        if !(1..=38).contains(&precision) || !(-84..=127).contains(&scale) {
            return Err(SqlError::new(
                SqlState::INVALID_TABLE_DEFINITION,
                "numeric precision specifier is out of range (1 to 38)",
            )
            .at(position));
        }
        let exact = SqlType::Numeric(Some(crate::types::NumericModifier {
            precision: precision as u16,
            scale: scale as i16,
        }));
        Ok(match (precision, scale) {
            (_, scale) if scale != 0 => exact,
            (1..=4, _) => SqlType::SmallInt,
            (5..=9, _) => SqlType::Integer,
            (10..=18, _) => SqlType::BigInt,
            _ => exact,
        })
    }

    pub(super) fn oracle_identity(&mut self, column: &mut ColumnDefinition) -> SqlResult<()> {
        self.at += 1;
        let by_default = !self.take_word("always") && self.take_word("by");
        if by_default {
            self.expect_word("default")?;
            if self.take_word("on") {
                self.expect_word("null")?;
                if self.take_word("for") {
                    self.expect_word("insert")?;
                    if !self.take_word("only") {
                        self.expect_word("and")?;
                        self.expect_word("update")?;
                    }
                }
            }
        }
        self.expect_word("as")?;
        self.expect_word("identity")?;
        let position = self.position();
        let options = if self.take(&TokenKind::LeftParen) {
            let options = self.sequence_options(false)?;
            self.expect(&TokenKind::RightParen)?;
            options
        } else {
            self.sequence_options(false)?
        };
        if !matches!(
            column.sql_type,
            SqlType::SmallInt | SqlType::Integer | SqlType::BigInt
        ) {
            column.sql_type = SqlType::BigInt;
        }
        column.identity_seed =
            oracle_identity_seed(&options, column.sql_type).map_err(|error| error.at(position))?;
        column.auto_increment = true;
        column.generated_always = !by_default;
        Ok(())
    }

    pub(super) fn oracle_primary(
        &mut self,
        kind: &TokenKind<'_>,
        position: usize,
    ) -> SqlResult<Option<Expr>> {
        let expr = match kind {
            TokenKind::String(text) if text.is_empty() => {
                self.at += 1;
                literal(Literal::Null, position)
            }
            TokenKind::NumberedParameter(number) => {
                return self.parameter(&number.to_string(), position).map(Some);
            }
            TokenKind::Word(word) => match word.as_ref() {
                "interval" => return self.oracle_interval_literal(position),
                "sessiontimezone" | "dbtimezone" => {
                    self.at += 1;
                    let (name, value) = if word.as_ref() == "sessiontimezone" {
                        let zone = self.oracle_settings(|settings| settings.time_zone);
                        let shown = match zone.region_name() {
                            Some(region) => region.to_owned(),
                            None => crate::oracle_datetime::zone_text(
                                zone.fixed_offset().unwrap_or(0),
                            ),
                        };
                        ("sessiontimezone", shown)
                    } else {
                        ("dbtimezone", crate::oracle_datetime::zone_text(0))
                    };
                    Expr {
                        kind: ExprKind::SessionValue { name, value },
                        position,
                    }
                }
                "rownum" => {
                    self.at += 1;
                    Expr {
                        kind: ExprKind::Column {
                            qualifier: None,
                            name: "rownum".to_owned(),
                        },
                        position,
                    }
                }
                "user" => {
                    self.at += 1;
                    Expr {
                        kind: ExprKind::SessionValue {
                            name: "user",
                            value: self.oracle_user(),
                        },
                        position,
                    }
                }
                "sysdate" | "systimestamp" | "current_date" | "current_timestamp"
                | "localtimestamp" => {
                    self.at += 1;
                    let clock = match word.as_ref() {
                        "sysdate" | "current_date" => "mysql_now",
                        "localtimestamp" => "localtimestamp",
                        _ => "now",
                    };
                    Expr {
                        kind: ExprKind::Function {
                            name: clock.to_owned(),
                            arguments: Vec::new(),
                        },
                        position,
                    }
                }
                "rowid" | "level" | "prior" | "uid" => {
                    return Err(self.not_supported(&word.to_ascii_uppercase()));
                }
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };
        Ok(Some(expr))
    }

    fn oracle_arguments(&mut self) -> SqlResult<Vec<Expr>> {
        self.expect(&TokenKind::LeftParen)?;
        let mut arguments = Vec::new();
        if !self.take(&TokenKind::RightParen) {
            loop {
                arguments.push(self.expression()?);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
        }
        Ok(arguments)
    }

    fn oracle_trim(&mut self, name: &Name) -> SqlResult<Expr> {
        self.expect(&TokenKind::LeftParen)?;
        let (left, right) = if self.take_word("leading") {
            (true, false)
        } else if self.take_word("trailing") {
            (false, true)
        } else {
            self.take_word("both");
            (true, true)
        };
        let first = if self.is_word("from") {
            None
        } else {
            Some(self.expression()?)
        };
        let (set, subject) = if self.take_word("from") {
            (first, self.expression()?)
        } else {
            match first {
                Some(subject) => (None, subject),
                None => return Err(self.unexpected()),
            }
        };
        self.expect(&TokenKind::RightParen)?;
        let internal = match (left, right) {
            (true, false) => "oracle_trim_leading",
            (false, true) => "oracle_trim_trailing",
            _ => "oracle_trim_both",
        };
        let set = set.unwrap_or_else(|| literal(Literal::String(" ".to_owned()), name.position));
        Ok(oracle_function(internal, vec![subject, set], name.position))
    }

    pub(super) fn oracle_function(&mut self, name: &Name) -> SqlResult<Option<Expr>> {
        let position = name.position;
        if name.text == "trim" {
            return self.oracle_trim(name).map(Some);
        }
        if name.text == "sys_context" {
            return self.sys_context(name).map(Some);
        }
        if name.text == "listagg" {
            return self.oracle_listagg(name).map(Some);
        }
        if crate::ast::AggregateFunction::named(&name.text).is_some() {
            return Ok(None);
        }
        let (internal, least, most): (&'static str, usize, usize) = match name.text.as_str() {
            "nvl" => ("nvl", 2, 2),
            "nvl2" => ("nvl2", 3, 3),
            "coalesce" => ("coalesce", 2, usize::MAX),
            "nullif" => ("nullif", 2, 2),
            "decode" => ("oracle_decode", 3, MAX_DECODE_ARGUMENTS),
            "concat" => ("oracle_concat", 2, 2),
            "length" => ("oracle_length", 1, 1),
            "substr" => ("oracle_substr", 2, 3),
            "instr" => ("oracle_instr", 2, 4),
            "upper" => ("oracle_upper", 1, 1),
            "lower" => ("oracle_lower", 1, 1),
            "initcap" => ("oracle_initcap", 1, 1),
            "ltrim" => ("oracle_ltrim", 1, 2),
            "rtrim" => ("oracle_rtrim", 1, 2),
            "lpad" => ("oracle_lpad", 2, 3),
            "rpad" => ("oracle_rpad", 2, 3),
            "replace" => ("oracle_replace", 2, 3),
            "chr" => ("oracle_chr", 1, 1),
            "ascii" => ("oracle_ascii", 1, 1),
            "abs" => ("oracle_abs", 1, 1),
            "round" => ("oracle_round", 1, 2),
            "trunc" => ("oracle_trunc", 1, 2),
            "mod" => ("oracle_mod", 2, 2),
            "ceil" => ("oracle_ceil", 1, 1),
            "floor" => ("oracle_floor", 1, 1),
            "power" => ("oracle_power", 2, 2),
            "sqrt" => ("oracle_sqrt", 1, 1),
            "sign" => ("oracle_sign", 1, 1),
            "greatest" => ("oracle_greatest", 1, usize::MAX),
            "least" => ("oracle_least", 1, usize::MAX),
            "to_char" => ("oracle_to_char", 1, 3),
            "to_number" => ("oracle_to_number", 1, 3),
            "to_date" => ("oracle_to_date", 1, 3),
            "to_timestamp" => ("oracle_to_timestamp", 1, 3),
            "add_months" => ("oracle_add_months", 2, 2),
            "last_day" => ("oracle_last_day", 1, 1),
            "months_between" => ("oracle_months_between", 2, 2),
            "next_day" => ("oracle_next_day", 2, 2),
            "to_timestamp_tz" => ("oracle_to_timestamp_tz", 1, 3),
            "from_tz" => ("oracle_from_tz", 2, 2),
            "sys_extract_utc" => ("oracle_sys_extract_utc", 1, 1),
            "tz_offset" => ("oracle_tz_offset", 1, 1),
            "numtodsinterval" => ("oracle_numtodsinterval", 2, 2),
            "numtoyminterval" => ("oracle_numtoyminterval", 2, 2),
            "to_dsinterval" => ("oracle_to_dsinterval", 1, 1),
            "to_yminterval" => ("oracle_to_yminterval", 1, 1),
            _ => return Err(invalid_identifier(&name.text, position)),
        };
        let arguments = self.oracle_arguments()?;
        if arguments.len() < least {
            return Err(wrong_argument_count(name, true));
        }
        if arguments.len() > most {
            return Err(wrong_argument_count(name, false));
        }
        let expr = match internal {
            "nvl" | "coalesce" => function("coalesce", arguments, position),
            "nullif" => function("nullif", arguments, position),
            "nvl2" => {
                let mut arguments = arguments.into_iter();
                let (Some(tested), Some(present), Some(absent)) =
                    (arguments.next(), arguments.next(), arguments.next())
                else {
                    return Err(wrong_argument_count(name, true));
                };
                Expr {
                    kind: ExprKind::Case {
                        operand: None,
                        branches: vec![(
                            Expr {
                                kind: ExprKind::IsNull {
                                    expr: Box::new(tested),
                                    negated: true,
                                },
                                position,
                            },
                            present,
                        )],
                        otherwise: Some(Box::new(absent)),
                    },
                    position,
                }
            }
            _ => oracle_function(internal, arguments, position),
        };
        Ok(Some(expr))
    }
}

pub(crate) fn oracle_table_name(text: &str, settings: &Settings) -> SqlResult<Name> {
    let tokens = crate::lexer::tokenize_in(text, crate::dialect::Dialect::Oracle)?;
    let mut parser = Parser::new(text, &tokens)
        .with_context(super::Parameters::Absent, settings)
        .in_dialect(crate::dialect::Dialect::Oracle);
    let name = parser.table_name()?;
    if !matches!(parser.peek_kind(), TokenKind::End | TokenKind::Semicolon) {
        return Err(parser.unexpected());
    }
    Ok(name)
}

const BINARY_COLLATIONS: [&str; 3] = ["using_nls_comp", "binary", "using_nls_sort"];

impl Parser<'_> {
    pub(super) fn oracle_collation(&mut self) -> SqlResult<()> {
        let position = self.position();
        let name = self.label()?.to_ascii_lowercase();
        if BINARY_COLLATIONS.contains(&name.as_str()) {
            return Ok(());
        }
        Err(self
            .not_supported(&format!("the collation {}", name.to_ascii_uppercase()))
            .at(position))
    }

    pub(super) fn oracle_view_options(&mut self) -> SqlResult<()> {
        loop {
            if self.is_word("default") && self.is_word_at(1, "collation") {
                self.at += 2;
                self.oracle_collation()?;
            } else if self.take_word("bequeath") {
                if !self.take_word("current_user") {
                    self.expect_word("definer")?;
                }
            } else {
                return Ok(());
            }
        }
    }

    fn skip_balanced(&mut self) -> SqlResult<()> {
        self.expect(&TokenKind::LeftParen)?;
        let mut depth = 1usize;
        while depth > 0 {
            match self.peek_kind() {
                TokenKind::End => return Err(self.unexpected()),
                TokenKind::LeftParen => depth += 1,
                TokenKind::RightParen => depth -= 1,
                _ => {}
            }
            self.at += 1;
        }
        Ok(())
    }

    fn take_integer(&mut self) -> bool {
        if matches!(self.peek_kind(), TokenKind::Integer(_)) {
            self.at += 1;
            return true;
        }
        false
    }

    fn skip_compression(&mut self) {
        if self.take_integer() {
            return;
        }
        let _ = self.take_word("basic") || self.take_word("advanced");
        if self.take_word("for") {
            while [
                "oltp",
                "query",
                "archive",
                "all",
                "direct_load",
                "operations",
                "low",
                "high",
            ]
            .iter()
            .any(|word| self.is_word(word))
            {
                self.at += 1;
            }
        }
    }

    fn skip_partitioning(&mut self) -> SqlResult<()> {
        loop {
            match self.peek_kind() {
                TokenKind::LeftParen => self.skip_balanced()?,
                TokenKind::Integer(_) => self.at += 1,
                TokenKind::Word(word)
                    if [
                        "partition",
                        "subpartition",
                        "by",
                        "range",
                        "list",
                        "hash",
                        "reference",
                        "system",
                        "interval",
                        "template",
                        "partitions",
                        "subpartitions",
                        "store",
                        "in",
                        "automatic",
                        "consistent",
                        "columns",
                    ]
                    .contains(&word.as_ref()) =>
                {
                    self.at += 1;
                }
                _ => return Ok(()),
            }
        }
    }

    pub(super) fn skip_physical_properties(&mut self) -> SqlResult<()> {
        loop {
            if self.is_word("segment") && self.is_word_at(1, "creation") {
                self.at += 2;
                if !self.take_word("immediate") {
                    self.expect_word("deferred")?;
                }
            } else if ["pctfree", "pctused", "initrans", "maxtrans", "pctthreshold"]
                .iter()
                .any(|word| self.is_word(word))
            {
                self.at += 1;
                if !self.take_integer() {
                    return Err(self.unexpected());
                }
            } else if (self.is_word("storage") || self.is_word("result_cache"))
                && self.peek_kind_at(1) == &TokenKind::LeftParen
            {
                self.at += 1;
                self.skip_balanced()?;
            } else if self.take_word("tablespace") {
                self.take_word("set");
                self.name()?;
            } else if [
                "logging",
                "nologging",
                "filesystem_like_logging",
                "nocompress",
                "cache",
                "nocache",
                "monitoring",
                "nomonitoring",
                "rowdependencies",
                "norowdependencies",
                "noparallel",
                "reverse",
                "visible",
                "invisible",
                "online",
                "overflow",
                "noinmemory",
            ]
            .iter()
            .any(|word| self.is_word(word))
            {
                self.at += 1;
            } else if self.take_word("compress") {
                self.skip_compression();
            } else if (self.is_word("row") || self.is_word("column"))
                && self.is_word_at(1, "store")
                && self.is_word_at(2, "compress")
            {
                self.at += 3;
                self.skip_compression();
            } else if self.take_word("parallel") {
                self.take_integer();
            } else if (self.is_word("enable") || self.is_word("disable"))
                && self.is_word_at(1, "row")
                && self.is_word_at(2, "movement")
            {
                self.at += 3;
            } else if self.is_word("no") && self.is_word_at(1, "inmemory") {
                self.at += 2;
            } else if self.take_word("inmemory") {
                if self.peek_kind() == &TokenKind::LeftParen {
                    self.skip_balanced()?;
                }
            } else if self.is_word("organization") {
                if self.is_word_at(1, "heap") || self.is_word_at(1, "index") {
                    self.at += 2;
                } else {
                    return Err(self.not_supported("ORGANIZATION EXTERNAL tables"));
                }
            } else if self.take_word("including") {
                self.name()?;
            } else if self.is_word("lob") && self.peek_kind_at(1) == &TokenKind::LeftParen {
                self.at += 1;
                self.skip_balanced()?;
                self.expect_word("store")?;
                self.expect_word("as")?;
                let _ = self.take_word("securefile") || self.take_word("basicfile");
                if !matches!(self.peek_kind(), TokenKind::LeftParen) && !self.at_physical_end() {
                    self.name()?;
                }
                if self.peek_kind() == &TokenKind::LeftParen {
                    self.skip_balanced()?;
                }
            } else if self.is_word("compute") && self.is_word_at(1, "statistics") {
                self.at += 2;
            } else if self.is_word("default") && self.is_word_at(1, "collation") {
                self.at += 2;
                self.oracle_collation()?;
            } else if self.is_word("partition") && self.is_word_at(1, "by") {
                self.skip_partitioning()?;
            } else if self.is_word("cluster") {
                return Err(self.not_supported("tables in a CLUSTER"));
            } else if self.is_word("nested") || self.is_word("xmltype") {
                return Err(self.not_supported("NESTED TABLE and XMLTYPE storage clauses"));
            } else {
                return Ok(());
            }
        }
    }

    fn at_physical_end(&self) -> bool {
        matches!(self.peek_kind(), TokenKind::End | TokenKind::Semicolon)
            || [
                "tablespace",
                "storage",
                "logging",
                "nologging",
                "enable",
                "disable",
            ]
            .iter()
            .any(|word| self.is_word(word))
    }

    pub(super) fn oracle_using_index(&mut self) -> SqlResult<Option<Name>> {
        if self.peek_kind() == &TokenKind::LeftParen {
            self.skip_balanced()?;
            return Ok(None);
        }
        let mut named = None;
        if matches!(
            self.peek_kind(),
            TokenKind::Word(_) | TokenKind::QuotedIdentifier(_)
        ) && !self.at_physical_end()
            && ![
                "pctfree",
                "initrans",
                "maxtrans",
                "compute",
                "compress",
                "nocompress",
                "pctused",
                "reverse",
                "local",
                "global",
                "online",
                "visible",
                "invisible",
                "noparallel",
                "parallel",
                "nosort",
                "sort",
                "validate",
                "novalidate",
                "rely",
                "norely",
                "segment",
                "indexing",
            ]
            .iter()
            .any(|word| self.is_word(word))
        {
            named = Some(self.table_name()?);
        }
        while self.take_word("local") || self.take_word("global") {
            if self.is_word("partition") && self.is_word_at(1, "by") {
                self.skip_partitioning()?;
            } else if self.peek_kind() == &TokenKind::LeftParen {
                self.skip_balanced()?;
            }
        }
        loop {
            if self.take_word("indexing") {
                if !self.take_word("partial") {
                    self.expect_word("full")?;
                }
            } else if !(self.take_word("nosort") || self.take_word("sort")) {
                break;
            }
        }
        self.skip_physical_properties()?;
        Ok(named)
    }

    pub(super) fn oracle_constraint_states(&mut self) -> SqlResult<bool> {
        Ok(self.oracle_constraint_state_list()?.enabled)
    }

    pub(super) fn oracle_constraint_state_list(&mut self) -> SqlResult<ConstraintStates> {
        let mut states = ConstraintStates {
            enabled: true,
            using_index: None,
        };
        loop {
            if self.take_word("enable") {
                states.enabled = true;
            } else if self.take_word("disable") {
                states.enabled = false;
            } else if self.take_word("validate")
                || self.take_word("novalidate")
                || self.take_word("rely")
                || self.take_word("norely")
            {
            } else if self.is_word("using") && self.is_word_at(1, "index") {
                self.at += 2;
                if let Some(index) = self.oracle_using_index()? {
                    states.using_index = Some(index);
                }
            } else {
                return Ok(states);
            }
        }
    }
}

fn oracle_identity_seed(
    options: &SequenceOptions,
    sql_type: SqlType,
) -> SqlResult<Option<IdentitySeed>> {
    let invalid = |code: u16, message: &str| {
        SqlError::new(SqlState::INVALID_PARAMETER_VALUE, message).with_oracle_code(code)
    };
    let (low, high) = integer_type_range(sql_type).unwrap_or((i64::MIN, i64::MAX));
    let increment = options.increment.unwrap_or(1);
    if increment == 0 {
        return Err(invalid(4002, "INCREMENT must be a nonzero integer"));
    }
    let ascending = increment > 0;
    let min = options.min.flatten();
    let max = options.max.flatten();
    if let (Some(min), Some(max)) = (min, max)
        && min >= max
    {
        return Err(invalid(4004, "MINVALUE must be less than MAXVALUE"));
    }
    let start = options.start.unwrap_or(if ascending {
        min.unwrap_or(1)
    } else {
        max.unwrap_or(-1)
    });
    if start < min.unwrap_or(low) {
        return Err(invalid(4006, "START WITH cannot be less than MINVALUE"));
    }
    if start > max.unwrap_or(high) {
        return Err(invalid(4008, "START WITH cannot be more than MAXVALUE"));
    }
    let limited = if ascending {
        max.is_some_and(|max| max < high)
    } else {
        min.is_some_and(|min| min > low)
    };
    if limited || options.cycle == Some(true) {
        return Err(SqlError::not_supported(
            "identity options that end or cycle the identity before its type's range (MAXVALUE, MINVALUE, CYCLE) are not supported yet",
        ));
    }
    let seed = IdentitySeed {
        seed: start,
        increment,
    };
    Ok((seed != IdentitySeed::ONE_BY_ONE).then_some(seed))
}

pub(super) struct ConstraintStates {
    pub(super) enabled: bool,
    pub(super) using_index: Option<Name>,
}
