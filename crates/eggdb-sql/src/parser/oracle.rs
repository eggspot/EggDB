use super::Parser;
use crate::ast::{
    BinaryOperator, ColumnDefinition, Expr, ExprKind, Literal, Name, OrderItem, SelectItem,
    Statement, TableReference, UnaryOperator,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::children;
use crate::lexer::{TokenKind, swap_uniform_case};
use crate::settings::Settings;
use crate::types::SqlType;

/// The Oracle Database release whose documented behaviour EggDB's Oracle dialect follows.
pub const ORACLE_COMPATIBLE_VERSION: &str = "23ai";

const MAX_DECODE_ARGUMENTS: usize = 255;

const DICTIONARY_VIEWS: [&str; 10] = [
    "user_tables",
    "all_tables",
    "user_tab_columns",
    "all_tab_columns",
    "user_tab_cols",
    "all_tab_cols",
    "user_indexes",
    "all_indexes",
    "user_ind_columns",
    "all_ind_columns",
];

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

pub(crate) fn is_dictionary_view(table: &TableReference) -> bool {
    table.derived.is_none() && DICTIONARY_VIEWS.contains(&table.name.text.as_str())
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

    pub(super) fn oracle_statement(&mut self) -> SqlResult<Option<Statement>> {
        let TokenKind::Word(word) = self.peek_kind() else {
            return Ok(None);
        };
        let statement = match word.as_ref() {
            "drop" if self.is_word_at(1, "table") => self.oracle_drop_table()?,
            "truncate" => {
                self.at += 1;
                self.expect_word("table")?;
                let name = self.table_name()?;
                Statement::Truncate(TableReference::named(name, None))
            }
            "create" | "drop" | "alter" if self.is_word_at(1, "sequence") => {
                return Err(self.not_supported("sequences (CREATE SEQUENCE, NEXTVAL, CURRVAL)"));
            }
            "merge" => return Err(self.not_supported("MERGE")),
            "begin" | "declare" => {
                return Err(self.not_supported(
                    "PL/SQL blocks (Oracle starts a transaction with the first change)",
                ));
            }
            "start" | "end" | "abort" | "show" | "reset" | "values" | "table" | "listen"
            | "notify" | "unlisten" | "vacuum" | "discard" | "copy" => {
                return Err(self.unexpected());
            }
            "insert" if self.is_word_at(1, "all") || self.is_word_at(1, "first") => {
                return Err(self.not_supported("INSERT ALL and INSERT FIRST"));
            }
            _ => return Ok(None),
        };
        Ok(Some(statement))
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
        if self.take_word("cascade") {
            self.expect_word("constraints")?;
        }
        self.take_word("purge");
        Ok(Statement::DropTable {
            names: vec![name],
            if_exists,
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
        if self.is_word("percent") {
            return Err(self.not_supported("FETCH ... PERCENT"));
        }
        if !self.take_word("rows") {
            self.expect_word("row")?;
        }
        if self.is_word("with") {
            return Err(self.not_supported("FETCH ... WITH TIES"));
        }
        self.expect_word("only")?;
        Ok(not_below_zero(count))
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
                SqlType::Varchar(Some(self.oracle_length(false)?.unwrap_or(1)))
            }
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
            "interval" | "raw" | "long" | "blob" | "bfile" | "rowid" | "urowid" | "xmltype"
            | "vector" | "sdo_geometry" => {
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
        if !self.take_word("always") && self.take_word("by") {
            self.expect_word("default")?;
            if self.take_word("on") {
                self.expect_word("null")?;
            }
        }
        self.expect_word("as")?;
        self.expect_word("identity")?;
        if self.is(&TokenKind::LeftParen) {
            return Err(self.not_supported("identity options"));
        }
        if !matches!(
            column.sql_type,
            SqlType::SmallInt | SqlType::Integer | SqlType::BigInt
        ) {
            column.sql_type = SqlType::BigInt;
        }
        column.auto_increment = true;
        Ok(())
    }

    fn is(&self, kind: &TokenKind) -> bool {
        self.peek_kind() == kind
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
                _ if matches!(self.peek_kind_at(1), TokenKind::Dot)
                    && (self.is_word_at(2, "nextval") || self.is_word_at(2, "currval")) =>
                {
                    return Err(self.not_supported("sequences (CREATE SEQUENCE, NEXTVAL, CURRVAL)"));
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
            "to_char" => ("oracle_to_char", 1, 2),
            "to_number" => ("oracle_to_number", 1, 2),
            "to_date" | "to_timestamp" | "add_months" | "months_between" | "last_day"
            | "next_day" => {
                return Err(self.not_supported("date and time values"));
            }
            "listagg" => return Err(self.not_supported("LISTAGG")),
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
