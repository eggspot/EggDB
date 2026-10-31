use super::{Level, Parser};
use crate::ast::{
    BinaryOperator, ColumnDefinition, Expr, ExprKind, Literal, Name, OnDuplicate, Pragma,
    Statement, TableReference, UnaryOperator,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::children_mut;
use crate::lexer::TokenKind;
use crate::settings::Settings;
use crate::types::SqlType;

/// The SQLite release whose documented behaviour EggDB's SQLite dialect follows, as
/// `sqlite_version()` reports it.
pub const SQLITE_COMPATIBLE_VERSION: &str = "3.46.0";

pub(super) fn is_reserved(word: &str) -> bool {
    matches!(
        word,
        "add"
            | "all"
            | "alter"
            | "and"
            | "as"
            | "autoincrement"
            | "between"
            | "case"
            | "check"
            | "collate"
            | "commit"
            | "constraint"
            | "create"
            | "default"
            | "deferrable"
            | "delete"
            | "distinct"
            | "drop"
            | "else"
            | "escape"
            | "except"
            | "exists"
            | "foreign"
            | "from"
            | "group"
            | "having"
            | "in"
            | "index"
            | "insert"
            | "intersect"
            | "into"
            | "is"
            | "isnull"
            | "join"
            | "limit"
            | "not"
            | "notnull"
            | "null"
            | "on"
            | "or"
            | "order"
            | "primary"
            | "references"
            | "select"
            | "set"
            | "table"
            | "then"
            | "to"
            | "transaction"
            | "union"
            | "unique"
            | "update"
            | "using"
            | "values"
            | "when"
            | "where"
    )
}

pub(super) fn is_clause_word(word: &str) -> bool {
    matches!(
        word,
        "glob" | "regexp" | "match" | "collate" | "without" | "strict"
    )
}

fn literal(literal: Literal, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Literal(literal),
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

fn sqlite_function(name: &'static str, arguments: Vec<Expr>, position: usize) -> Expr {
    Expr {
        kind: ExprKind::SqliteFunction { name, arguments },
        position,
    }
}

fn text_value(value: String, position: usize) -> Expr {
    Expr {
        kind: ExprKind::SessionValue {
            name: "value",
            value,
        },
        position,
    }
}

fn integer_value(value: i64, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Cast {
            expr: Box::new(literal(Literal::Integer(value), position)),
            sql_type: SqlType::BigInt,
        },
        position,
    }
}

pub(super) fn null_if_zero(divisor: Expr) -> Expr {
    let position = divisor.position;
    function(
        "nullif",
        vec![divisor, literal(Literal::Integer(0), position)],
        position,
    )
}

pub(super) fn without_negative_limit(limit: Expr) -> Expr {
    match limit.kind {
        ExprKind::Literal(Literal::Integer(number)) if number < 0 => {
            literal(Literal::Null, limit.position)
        }
        _ => limit,
    }
}

pub(super) fn without_negative_offset(offset: Expr) -> Expr {
    match offset.kind {
        ExprKind::Literal(Literal::Integer(number)) if number < 0 => {
            literal(Literal::Integer(0), offset.position)
        }
        _ => offset,
    }
}

pub(super) fn scalar_min_max(expr: Expr) -> Expr {
    match expr.kind {
        ExprKind::Function { name, arguments } if name == "min" || name == "max" => {
            let internal = if name == "min" {
                "sqlite_min"
            } else {
                "sqlite_max"
            };
            sqlite_function(internal, arguments, expr.position)
        }
        kind => Expr {
            kind,
            position: expr.position,
        },
    }
}

fn is_boolean_valued(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Binary(operator, _, _) => matches!(
            operator,
            BinaryOperator::Equal
                | BinaryOperator::NotEqual
                | BinaryOperator::Less
                | BinaryOperator::LessOrEqual
                | BinaryOperator::Greater
                | BinaryOperator::GreaterOrEqual
        ),
        ExprKind::Unary(UnaryOperator::Not, _)
        | ExprKind::And(_)
        | ExprKind::Or(_)
        | ExprKind::IsNull { .. }
        | ExprKind::IsTruth { .. }
        | ExprKind::IsDistinctFrom { .. }
        | ExprKind::Between { .. }
        | ExprKind::InList { .. }
        | ExprKind::Like { .. }
        | ExprKind::Literal(Literal::Null | Literal::Boolean(_)) => true,
        ExprKind::SqliteFunction { name, .. } => *name == "sqlite_truth",
        _ => false,
    }
}

pub(super) fn glob(expr: Expr, pattern: Expr, negated: bool) -> SqlResult<Expr> {
    let position = expr.position;
    let ExprKind::Literal(Literal::String(text)) = &pattern.kind else {
        return Err(
            SqlError::not_supported("GLOB with a pattern other than a constant string")
                .at(pattern.position),
        );
    };
    let mut converted = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '*' => converted.push('%'),
            '?' => converted.push('_'),
            '%' | '_' | '\\' => {
                converted.push('\\');
                converted.push(character);
            }
            '[' => {
                return Err(
                    SqlError::not_supported("character classes ([...]) in GLOB patterns")
                        .at(pattern.position),
                );
            }
            other => converted.push(other),
        }
    }
    Ok(Expr {
        kind: ExprKind::Like {
            expr: Box::new(expr),
            pattern: Box::new(literal(Literal::String(converted), pattern.position)),
            negated,
            case_insensitive: false,
        },
        position,
    })
}

fn point_excluded_values(expr: &mut Expr) {
    if let ExprKind::Column {
        qualifier: Some(qualifier),
        name,
    } = &expr.kind
        && qualifier == "excluded"
    {
        let column = Expr {
            kind: ExprKind::Column {
                qualifier: None,
                name: name.clone(),
            },
            position: expr.position,
        };
        expr.kind = ExprKind::Function {
            name: "values".to_owned(),
            arguments: vec![column],
        };
        return;
    }
    for child in children_mut(expr) {
        point_excluded_values(child);
    }
}

fn wrong_argument_count(name: &Name) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("wrong number of arguments to function {}()", name.text),
    )
    .with_sqlite_code(1)
    .at(name.position)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Affinity {
    Integer,
    Text,
    Blob,
    Real,
    Numeric,
}

fn affinity(declared: &str) -> Affinity {
    let upper = declared.to_ascii_uppercase();
    if upper.contains("INT") {
        Affinity::Integer
    } else if upper.contains("CHAR") || upper.contains("CLOB") || upper.contains("TEXT") {
        Affinity::Text
    } else if upper.contains("BLOB") || upper.is_empty() {
        Affinity::Blob
    } else if upper.contains("REAL") || upper.contains("FLOA") || upper.contains("DOUB") {
        Affinity::Real
    } else {
        Affinity::Numeric
    }
}

impl<'t> Parser<'t> {
    fn sqlite_settings<T>(&self, read: impl FnOnce(&Settings) -> T) -> T {
        match self.settings {
            Some(settings) => read(settings),
            None => read(&Settings::default()),
        }
    }

    pub(super) fn condition(&self, expr: Expr) -> Expr {
        if !self.sqlite() || is_boolean_valued(&expr) {
            return expr;
        }
        let position = expr.position;
        sqlite_function("sqlite_truth", vec![expr], position)
    }

    pub(super) fn sqlite_statement(&mut self) -> SqlResult<Option<Statement>> {
        let TokenKind::Word(word) = self.peek_kind() else {
            return Ok(None);
        };
        let statement = match word.as_ref() {
            "pragma" => self.pragma()?,
            "begin" => {
                self.at += 1;
                let _ = self.take_word("deferred")
                    || self.take_word("immediate")
                    || self.take_word("exclusive");
                self.take_word("transaction");
                Statement::Begin { read_only: false }
            }
            "replace" if self.is_word_at(1, "into") => self.insert()?,
            "attach" | "detach" | "vacuum" | "reindex" => {
                return Err(self.not_supported(&word.to_ascii_uppercase()));
            }
            _ => return Ok(None),
        };
        Ok(Some(statement))
    }

    fn pragma(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        let mut name = self.label()?.to_ascii_lowercase();
        if self.take(&TokenKind::Dot) {
            if name != "main" {
                return Err(SqlError::new(
                    SqlState::INVALID_CATALOG_NAME,
                    format!("unknown database {name}"),
                ));
            }
            name = self.label()?.to_ascii_lowercase();
        }
        let (argument, assigned) = if self.take_operator("=") {
            (Some(self.pragma_value()?), true)
        } else if self.take(&TokenKind::LeftParen) {
            let value = self.pragma_value()?;
            self.expect(&TokenKind::RightParen)?;
            (Some(value), false)
        } else {
            (None, false)
        };
        Ok(Statement::Pragma(Pragma {
            name,
            argument,
            assigned,
        }))
    }

    fn pragma_value(&mut self) -> SqlResult<String> {
        let negative = self.take_operator("-");
        if !negative {
            self.take_operator("+");
        }
        let value = match self.peek_kind() {
            TokenKind::Word(word) => word.as_ref().to_owned(),
            TokenKind::QuotedIdentifier(name) => name.clone(),
            TokenKind::String(text) => text.as_ref().to_owned(),
            TokenKind::Integer(digits) | TokenKind::Decimal(digits) => (*digits).to_owned(),
            _ => return Err(self.unexpected()),
        };
        self.at += 1;
        Ok(if negative { format!("-{value}") } else { value })
    }

    pub(super) fn sqlite_type(&mut self) -> SqlResult<(SqlType, bool)> {
        let mut words = Vec::new();
        while let TokenKind::Word(word) = self.peek_kind() {
            if matches!(
                word.as_ref(),
                "constraint"
                    | "primary"
                    | "not"
                    | "null"
                    | "unique"
                    | "check"
                    | "default"
                    | "collate"
                    | "references"
                    | "generated"
                    | "as"
            ) {
                break;
            }
            words.push(word.as_ref().to_owned());
            self.at += 1;
        }
        if !words.is_empty() && self.take(&TokenKind::LeftParen) {
            for _ in 0..2 {
                self.take_operator("-");
                self.take_operator("+");
                match self.peek_kind() {
                    TokenKind::Integer(_) | TokenKind::Decimal(_) => self.at += 1,
                    _ => return Err(self.unexpected()),
                }
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
        }
        let declared = words.join(" ");
        let exact_integer = declared == "integer";
        let sql_type = match affinity(&declared) {
            Affinity::Integer => SqlType::BigInt,
            Affinity::Text => SqlType::Text,
            Affinity::Real => SqlType::Double,
            Affinity::Blob if declared.is_empty() => SqlType::Text,
            Affinity::Blob => SqlType::Dynamic,
            Affinity::Numeric if matches!(declared.as_str(), "boolean" | "bool") => SqlType::BigInt,
            Affinity::Numeric if ["date", "time"].iter().any(|word| declared.contains(word)) => {
                SqlType::Text
            }
            Affinity::Numeric => SqlType::Double,
        };
        Ok((sql_type, exact_integer))
    }

    pub(super) fn sqlite_conflict_clause(&mut self) -> SqlResult<()> {
        if self.is_word("on") && self.is_word_at(1, "conflict") {
            self.at += 2;
            if !["rollback", "abort", "fail", "ignore", "replace"]
                .iter()
                .any(|word| self.is_word(word))
            {
                return Err(self.unexpected());
            }
            if !self.is_word("abort") && !self.is_word("fail") {
                return Err(self.not_supported("ON CONFLICT in a constraint other than ABORT"));
            }
            self.at += 1;
        }
        Ok(())
    }

    pub(super) fn sqlite_column_definition(&mut self) -> SqlResult<(ColumnDefinition, bool)> {
        let name = self.name()?;
        let (sql_type, exact_integer) = self.sqlite_type()?;
        let mut column = ColumnDefinition {
            name,
            sql_type,
            not_null: false,
            primary_key: false,
            default: None,
            default_dialect: crate::Dialect::PostgreSql,
            auto_increment: false,
            identity_seed: None,
            generated_always: false,
            unique: None,
            constraints: Vec::new(),
            comment: None,
        };
        let mut autoincrement = None;
        loop {
            let constraint = if self.take_word("constraint") {
                Some(self.name()?)
            } else {
                None
            };
            if self.column_constraint(&mut column, constraint)? {
                continue;
            }
            if self.take_word("primary") {
                self.expect_word("key")?;
                let descending = self.take_word("desc");
                if !descending {
                    self.take_word("asc");
                }
                self.sqlite_conflict_clause()?;
                column.primary_key = true;
                column.auto_increment = exact_integer && !descending;
                if self.is_word("autoincrement") {
                    autoincrement = Some(self.position());
                    self.at += 1;
                }
            } else if self.take_word("not") {
                self.expect_word("null")?;
                self.sqlite_conflict_clause()?;
                column.not_null = true;
            } else if self.take_word("null") {
                column.not_null = false;
            } else if self.is_word("default") {
                self.at += 1;
                let start = self.position();
                let expr = if self.peek_kind() == &TokenKind::LeftParen {
                    self.expression()?
                } else {
                    self.binary_operand()?
                };
                let end = self.tokens[self.at.saturating_sub(1)].end;
                let source = self.text.get(start..end).unwrap_or("").to_owned();
                let expr = match expr.kind {
                    ExprKind::Literal(Literal::Boolean(flag)) => {
                        literal(Literal::Integer(i64::from(flag)), expr.position)
                    }
                    _ => expr,
                };
                column.default = Some((expr, source));
            } else if self.take_word("unique") {
                self.sqlite_conflict_clause()?;
                column.unique = Some(None);
            } else if self.is_word("collate") {
                self.at += 1;
                let collation = self.label()?;
                if !collation.eq_ignore_ascii_case("binary") {
                    return Err(self.not_supported("COLLATE other than BINARY"));
                }
            } else if ["generated", "as"].iter().any(|word| self.is_word(word)) {
                return Err(self.not_supported(
                    "a column constraint other than PRIMARY KEY, UNIQUE, NOT NULL, DEFAULT, CHECK and REFERENCES",
                ));
            } else {
                break;
            }
        }
        if let Some(position) = autoincrement
            && !column.auto_increment
        {
            return Err(SqlError::new(
                SqlState::INVALID_TABLE_DEFINITION,
                "AUTOINCREMENT is only allowed on an INTEGER PRIMARY KEY",
            )
            .with_sqlite_code(1)
            .at(position));
        }
        Ok((column, exact_integer))
    }

    pub(super) fn sqlite_table_finish(
        &mut self,
        table: &Name,
        columns: &mut [ColumnDefinition],
        primary_key: &Option<(Vec<Name>, usize)>,
        integer_columns: &[bool],
    ) -> SqlResult<()> {
        if table.text.starts_with("sqlite_") {
            return Err(SqlError::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!("object name reserved for internal use: {}", table.text),
            )
            .with_sqlite_code(1)
            .at(table.position));
        }
        if let Some((names, _)) = primary_key
            && let [only] = names.as_slice()
            && let Some(index) = columns
                .iter()
                .position(|column| column.name.text == only.text)
            && integer_columns.get(index) == Some(&true)
        {
            columns[index].auto_increment = true;
        }
        let mut without_rowid = false;
        loop {
            if self.take_word("without") {
                let position = self.position();
                let word = self.label()?;
                if word != "rowid" {
                    return Err(SqlError::syntax(
                        format!("unknown table option: {word}"),
                        position,
                    ));
                }
                without_rowid = true;
            } else if !self.take_word("strict") {
                break;
            }
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        if without_rowid {
            let has_key = primary_key.is_some() || columns.iter().any(|column| column.primary_key);
            if !has_key {
                return Err(SqlError::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    format!("PRIMARY KEY missing on table {}", table.text),
                )
                .with_sqlite_code(1)
                .at(table.position));
            }
            for column in columns.iter_mut() {
                column.auto_increment = false;
            }
        }
        Ok(())
    }

    pub(super) fn sqlite_conflict_resolution(&mut self) -> SqlResult<(OnDuplicate, bool)> {
        let resolution = if self.take_word("replace") {
            (OnDuplicate::Replace, false)
        } else if self.take_word("ignore") {
            (OnDuplicate::Ignore, false)
        } else if self.take_word("rollback") {
            (OnDuplicate::Fail, true)
        } else if self.take_word("abort") || self.take_word("fail") {
            (OnDuplicate::Fail, false)
        } else {
            return Err(self.unexpected());
        };
        Ok(resolution)
    }

    pub(super) fn sqlite_upsert(&mut self) -> SqlResult<OnDuplicate> {
        self.expect_word("on")?;
        self.expect_word("conflict")?;
        if self.peek_kind() == &TokenKind::LeftParen {
            self.name_list()?;
            if self.is_word("where") {
                return Err(self.not_supported("ON CONFLICT (...) WHERE"));
            }
        }
        self.expect_word("do")?;
        if self.take_word("nothing") {
            return Ok(OnDuplicate::Ignore);
        }
        self.expect_word("update")?;
        self.expect_word("set")?;
        let mut assignments = Vec::new();
        loop {
            if self.peek_kind() == &TokenKind::LeftParen {
                return Err(self.not_supported("multiple-column assignment in DO UPDATE"));
            }
            let name = self.name()?;
            if !self.take_operator("=") {
                return Err(self.unexpected());
            }
            let mut expr = self.expression()?;
            point_excluded_values(&mut expr);
            assignments.push((name, expr));
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        if self.is_word("where") {
            return Err(self.not_supported("DO UPDATE ... WHERE"));
        }
        if self.is_word("on") {
            return Err(self.not_supported("more than one ON CONFLICT clause"));
        }
        Ok(OnDuplicate::Update(assignments))
    }

    pub(super) fn sqlite_infix_level(&self) -> Option<Level> {
        match self.peek_kind() {
            TokenKind::Operator("==") => Some(Level::Comparison),
            TokenKind::Word(word) if word == "glob" => Some(Level::Pattern),
            TokenKind::Word(word) if word == "not" && self.is_word_at(1, "glob") => {
                Some(Level::Pattern)
            }
            TokenKind::Word(word) if word == "regexp" || word == "match" => Some(Level::Other),
            _ => None,
        }
    }

    pub(super) fn sqlite_parameter(&mut self, number: usize, position: usize) -> SqlResult<Expr> {
        let bound = match self.parameters {
            super::Parameters::Absent | super::Parameters::Named(_) => false,
            super::Parameters::Values(values) => number <= values.len(),
            super::Parameters::Types(_) => true,
        };
        if !bound {
            self.at += 1;
            return Ok(literal(Literal::Null, position));
        }
        self.parameter(&number.to_string(), position)
    }

    pub(super) fn sqlite_primary(
        &mut self,
        word: &str,
        position: usize,
    ) -> SqlResult<Option<Expr>> {
        let expr = match word {
            "true" | "false" => {
                self.at += 1;
                literal(Literal::Integer(i64::from(word == "true")), position)
            }
            "current_user" | "session_user" | "user" | "current_role" | "current_catalog"
            | "current_schema" => return self.column_or_function().map(Some),
            "current_date" | "current_time" | "current_timestamp" => {
                return Err(self.not_supported("date and time values"));
            }
            _ => return Ok(None),
        };
        Ok(Some(expr))
    }

    fn call_arguments(&mut self) -> SqlResult<Vec<Expr>> {
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

    pub(super) fn sqlite_function(&mut self, name: &Name) -> SqlResult<Option<Expr>> {
        let position = name.position;
        let (internal, counts): (&'static str, &[usize]) = match name.text.as_str() {
            "sqlite_version" => ("sqlite_version", &[0]),
            "last_insert_rowid" => ("last_insert_rowid", &[0]),
            "changes" => ("changes", &[0]),
            "total_changes" => ("total_changes", &[0]),
            "random" => ("sqlite_random", &[0]),
            "ifnull" => ("coalesce", &[2]),
            "coalesce" => ("coalesce", &[]),
            "nullif" => ("nullif", &[2]),
            "iif" => ("iif", &[3]),
            "typeof" => ("sqlite_typeof", &[1]),
            "length" => ("sqlite_length", &[1]),
            "substr" | "substring" => ("sqlite_substr", &[2, 3]),
            "upper" => ("sqlite_upper", &[1]),
            "lower" => ("sqlite_lower", &[1]),
            "trim" => ("sqlite_trim", &[1, 2]),
            "ltrim" => ("sqlite_ltrim", &[1, 2]),
            "rtrim" => ("sqlite_rtrim", &[1, 2]),
            "replace" => ("sqlite_replace", &[3]),
            "instr" => ("sqlite_instr", &[2]),
            "abs" => ("abs", &[1]),
            "round" => ("sqlite_round", &[1, 2]),
            "hex" => ("sqlite_hex", &[1]),
            "quote" => ("sqlite_quote", &[1]),
            "printf" | "format" => ("sqlite_printf", &[]),
            "like" => ("like", &[2]),
            "glob" => ("glob", &[2]),
            "date" => ("sqlite_date", &[]),
            "time" => ("sqlite_time", &[]),
            "datetime" => ("sqlite_datetime", &[]),
            "julianday" => ("sqlite_julianday", &[]),
            "unixepoch" => ("sqlite_unixepoch", &[]),
            "strftime" => ("sqlite_strftime", &[]),
            "timediff" => ("sqlite_timediff", &[2]),
            "unicode" => ("sqlite_unicode", &[1]),
            "char" => ("sqlite_char", &[]),
            "sign" => ("sqlite_sign", &[1]),
            _ => return Ok(None),
        };
        let arguments = self.call_arguments()?;
        let count_ok = if counts.is_empty() {
            match internal {
                "coalesce" => arguments.len() >= 2,
                "sqlite_date" | "sqlite_time" | "sqlite_datetime" | "sqlite_julianday"
                | "sqlite_unixepoch" | "sqlite_char" => true,
                _ => !arguments.is_empty(),
            }
        } else {
            counts.contains(&arguments.len())
        };
        if !count_ok {
            return Err(wrong_argument_count(name));
        }
        let expr = match internal {
            "sqlite_version" => text_value(SQLITE_COMPATIBLE_VERSION.to_owned(), position),
            "last_insert_rowid" => integer_value(
                self.sqlite_settings(|settings| settings.sqlite.last_insert_rowid),
                position,
            ),
            "changes" => integer_value(
                self.sqlite_settings(|settings| settings.sqlite.changes),
                position,
            ),
            "total_changes" => integer_value(
                self.sqlite_settings(|settings| settings.sqlite.total_changes),
                position,
            ),
            "sqlite_random" => sqlite_function("sqlite_random", arguments, position),
            "iif" => {
                let mut arguments = arguments.into_iter();
                let (Some(condition), Some(then), Some(otherwise)) =
                    (arguments.next(), arguments.next(), arguments.next())
                else {
                    return Err(wrong_argument_count(name));
                };
                Expr {
                    kind: ExprKind::Case {
                        operand: None,
                        branches: vec![(self.condition(condition), then)],
                        otherwise: Some(Box::new(otherwise)),
                    },
                    position,
                }
            }
            "like" | "glob" => {
                let mut arguments = arguments.into_iter();
                let (Some(pattern), Some(subject)) = (arguments.next(), arguments.next()) else {
                    return Err(wrong_argument_count(name));
                };
                if internal == "glob" {
                    glob(subject, pattern, false)?
                } else {
                    Expr {
                        kind: ExprKind::Like {
                            expr: Box::new(subject),
                            pattern: Box::new(pattern),
                            negated: false,
                            case_insensitive: true,
                        },
                        position,
                    }
                }
            }
            _ if internal.starts_with("sqlite_") => sqlite_function(internal, arguments, position),
            _ => function(internal, arguments, position),
        };
        Ok(Some(expr))
    }
}

pub(crate) fn is_schema_table(table: &TableReference) -> bool {
    table.derived.is_none()
        && matches!(
            table.name.text.as_str(),
            "sqlite_master" | "sqlite_schema" | "sqlite_temp_master" | "sqlite_temp_schema"
        )
}
