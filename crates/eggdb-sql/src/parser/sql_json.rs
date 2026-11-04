use super::Parser;
use crate::ast::{Expr, ExprKind, GeneratedTable, Literal, Name, TableReference};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;
use crate::sql_json::{
    Behavior, BehaviorClause, QueryCall, QueryKind, Quotes, RegularColumn, TableCall, TableColumn,
    TablePlan, Wrapper,
};
use crate::sql_json_constructors::JsonShape;
use crate::types::{DefinedKind, SqlType};

const ROOT_PATH: &str = "$";

fn label(kind: QueryKind) -> &'static str {
    match kind {
        QueryKind::Exists => "JSON_EXISTS()",
        QueryKind::Value => "JSON_VALUE()",
        QueryKind::Query => "JSON_QUERY()",
    }
}

fn allowed_on_error(kind: QueryKind) -> &'static str {
    match kind {
        QueryKind::Exists => "ERROR, TRUE, FALSE, or UNKNOWN",
        QueryKind::Value => "ERROR, NULL, or DEFAULT expression",
        QueryKind::Query => "ERROR, NULL, EMPTY ARRAY, EMPTY OBJECT, or DEFAULT expression",
    }
}

fn behavior_allowed(kind: QueryKind, behavior: Behavior) -> bool {
    match kind {
        QueryKind::Exists => matches!(
            behavior,
            Behavior::Error | Behavior::True | Behavior::False | Behavior::Unknown
        ),
        QueryKind::Value => matches!(
            behavior,
            Behavior::Error | Behavior::Null | Behavior::Default
        ),
        QueryKind::Query => matches!(
            behavior,
            Behavior::Error
                | Behavior::Null
                | Behavior::Default
                | Behavior::EmptyArray
                | Behavior::EmptyObject
        ),
    }
}

fn invalid_behavior(clause: &str, kind: QueryKind, position: usize) -> SqlError {
    SqlError::syntax(format!("invalid {clause} behavior"), position).with_detail(format!(
        "Only {} is allowed in {clause} for {}.",
        allowed_on_error(kind),
        label(kind)
    ))
}

fn escaped_member(name: &str) -> String {
    let mut path = String::with_capacity(name.len() + 5);
    path.push_str("$.\"");
    for character in name.chars() {
        if matches!(character, '"' | '\\') {
            path.push('\\');
        }
        path.push(character);
    }
    path.push('"');
    path
}

fn is_json_shaped(sql_type: SqlType) -> bool {
    match sql_type {
        SqlType::Json | SqlType::Jsonb | SqlType::Array(_) => true,
        SqlType::Defined(defined) => defined.kind == DefinedKind::Composite,
        _ => false,
    }
}

#[derive(Default)]
struct TableColumns {
    names: Vec<Name>,
    types: Vec<SqlType>,
    defaults: Vec<Expr>,
}

impl TableColumns {
    fn add(&mut self, name: Name, sql_type: SqlType) -> SqlResult<()> {
        if self.names.iter().any(|found| found.text == name.text) {
            return Err(SqlError::new(
                SqlState::DUPLICATE_ALIAS,
                format!("duplicate JSON_TABLE column or path name: {}", name.text),
            )
            .at(name.position));
        }
        self.names.push(name);
        self.types.push(sql_type);
        Ok(())
    }

    fn default_index(&mut self, clause: &mut BehaviorClause) -> Option<usize> {
        let expr = clause.default.take()?;
        self.defaults.push(expr);
        Some(self.defaults.len() - 1)
    }
}

impl Parser<'_> {
    fn sql_json_dialect(&self) -> bool {
        self.postgresql() || self.oracle()
    }

    fn json_format_clause(&mut self) -> SqlResult<bool> {
        if !(self.is_word("format") && self.is_word_at(1, "json")) {
            return Ok(false);
        }
        self.at += 2;
        if self.take_word("encoding") {
            let position = self.position();
            if !self.take_word("utf8") {
                return Err(SqlError::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "only UTF8 JSON encoding is supported",
                )
                .at(position));
            }
        }
        Ok(true)
    }

    fn json_context_item(&mut self) -> SqlResult<Expr> {
        let document = self.expression()?;
        self.json_format_clause()?;
        Ok(document)
    }

    fn json_passing(&mut self) -> SqlResult<Vec<(String, Expr)>> {
        let mut passing = Vec::new();
        if !self.take_word("passing") {
            return Ok(passing);
        }
        loop {
            let value = self.expression()?;
            self.expect_word("as")?;
            let name = self.name()?;
            if passing.iter().any(|(found, _)| *found == name.text) {
                return Err(SqlError::new(
                    SqlState::DUPLICATE_ALIAS,
                    format!("duplicate JSON argument name \"{}\"", name.text),
                )
                .at(name.position));
            }
            passing.push((name.text, value));
            if !self.take(&TokenKind::Comma) {
                return Ok(passing);
            }
        }
    }

    fn skip_oracle_returning_options(&mut self) {
        if !self.oracle() {
            return;
        }
        while self.take_word("pretty") || self.take_word("ascii") || self.take_word("truncate") {}
    }

    fn json_wrapper_and_quotes(&mut self) -> SqlResult<(Wrapper, Quotes, bool)> {
        let position = self.position();
        let mut given = false;
        let mut wrapper = Wrapper::Without;
        if self.is_word("without") && (self.is_word_at(1, "wrapper") || self.is_word_at(1, "array"))
        {
            self.at += 1;
            self.take_word("array");
            self.expect_word("wrapper")?;
            given = true;
        } else if self.is_word("with")
            && ["wrapper", "array", "conditional", "unconditional"]
                .iter()
                .any(|word| self.is_word_at(1, word))
        {
            self.at += 1;
            wrapper = if self.take_word("conditional") {
                Wrapper::Conditional
            } else {
                self.take_word("unconditional");
                Wrapper::Unconditional
            };
            self.take_word("array");
            self.expect_word("wrapper")?;
            given = true;
        }
        let mut quotes = Quotes::Keep;
        if (self.is_word("keep") || self.is_word("omit")) && self.is_word_at(1, "quotes") {
            if self.is_word("omit") {
                quotes = Quotes::Omit;
            }
            self.at += 2;
            given = true;
            if self.is_word("on") && self.is_word_at(1, "scalar") {
                self.at += 2;
                self.expect_word("string")?;
            }
        }
        if quotes == Quotes::Omit && wrapper != Wrapper::Without {
            return Err(SqlError::syntax(
                "SQL/JSON QUOTES behavior must not be specified when WITH WRAPPER is used",
                position,
            ));
        }
        Ok((wrapper, quotes, given))
    }

    fn json_behavior(&mut self) -> SqlResult<Option<BehaviorClause>> {
        let before_on =
            |parser: &Self, word: &str| parser.is_word(word) && parser.is_word_at(1, "on");
        let simple = [
            ("error", Behavior::Error),
            ("null", Behavior::Null),
            ("true", Behavior::True),
            ("false", Behavior::False),
            ("unknown", Behavior::Unknown),
        ];
        for (word, behavior) in simple {
            if before_on(self, word) {
                self.at += 1;
                return Ok(Some(BehaviorClause::of(behavior)));
            }
        }
        if self.take_word("empty") {
            let behavior = if self.take_word("object") {
                Behavior::EmptyObject
            } else {
                self.take_word("array");
                Behavior::EmptyArray
            };
            return Ok(Some(BehaviorClause::of(behavior)));
        }
        if self.take_word("default") {
            let default = self.expression()?;
            return Ok(Some(BehaviorClause {
                behavior: Behavior::Default,
                default: Some(default),
            }));
        }
        Ok(None)
    }

    fn json_behaviors(&mut self, kind: QueryKind) -> SqlResult<(BehaviorClause, BehaviorClause)> {
        let mut on_empty: Option<BehaviorClause> = None;
        let mut on_error: Option<BehaviorClause> = None;
        loop {
            let position = self.position();
            let Some(clause) = self.json_behavior()? else {
                break;
            };
            self.expect_word("on")?;
            if kind != QueryKind::Exists && on_empty.is_none() && self.is_word("empty") {
                self.at += 1;
                if !behavior_allowed(kind, clause.behavior) {
                    return Err(invalid_behavior("ON EMPTY", kind, position));
                }
                on_empty = Some(clause);
            } else if on_error.is_none() && self.take_word("error") {
                if !behavior_allowed(kind, clause.behavior) {
                    return Err(invalid_behavior("ON ERROR", kind, position));
                }
                on_error = Some(clause);
            } else {
                return Err(self.unexpected());
            }
        }
        let fallback = if kind == QueryKind::Exists {
            Behavior::False
        } else {
            Behavior::Null
        };
        Ok((
            on_empty.unwrap_or_else(|| BehaviorClause::of(Behavior::Null)),
            on_error.unwrap_or_else(|| BehaviorClause::of(fallback)),
        ))
    }

    fn json_unique_keys(&mut self) -> bool {
        let with = self.is_word("with");
        if !(with || self.is_word("without")) || !self.is_word_at(1, "unique") {
            return false;
        }
        self.at += 2;
        self.take_word("keys");
        with
    }

    /// Parses what follows `subject IS [NOT] JSON` in PostgreSQL: an optional `VALUE`,
    /// `SCALAR`, `ARRAY` or `OBJECT` and an optional `WITH | WITHOUT UNIQUE [KEYS]`.
    pub(super) fn is_json_predicate(&mut self, subject: Expr, negated: bool) -> Expr {
        let position = subject.position;
        let named = ["value", "scalar", "array", "object"]
            .into_iter()
            .find(|word| self.is_word(word))
            .and_then(JsonShape::named);
        let shape = match named {
            Some(shape) => {
                self.at += 1;
                shape
            }
            None => JsonShape::Value,
        };
        let unique_keys = self.json_unique_keys();
        crate::sql_json_constructors::is_json_call(subject, shape, unique_keys, negated, position)
    }

    fn sql_json_constructor(&mut self, name: &Name) -> SqlResult<Option<Expr>> {
        let position = name.position;
        match name.text.as_str() {
            "json" => {
                self.at += 1;
                let subject = self.json_context_item()?;
                let unique_keys = self.json_unique_keys();
                self.expect(&TokenKind::RightParen)?;
                Ok(Some(crate::sql_json_constructors::json_call(
                    subject,
                    unique_keys,
                    position,
                )))
            }
            "json_scalar" => {
                self.at += 1;
                let subject = self.expression()?;
                self.expect(&TokenKind::RightParen)?;
                Ok(Some(crate::sql_json_constructors::json_scalar_call(
                    subject, position,
                )))
            }
            "json_serialize" => {
                self.at += 1;
                let subject = self.json_context_item()?;
                let returning = if self.take_word("returning") {
                    let type_position = self.position();
                    let sql_type = self.cast_type()?;
                    if !crate::sql_json_constructors::serializes_to(sql_type) {
                        return Err(crate::sql_json_constructors::unsupported_serialize_type(
                            sql_type,
                        )
                        .at(type_position));
                    }
                    self.json_format_clause()?;
                    Some(sql_type)
                } else {
                    None
                };
                self.expect(&TokenKind::RightParen)?;
                Ok(Some(crate::sql_json_constructors::json_serialize_call(
                    subject, returning, position,
                )))
            }
            _ => Ok(None),
        }
    }

    /// Parses `JSON_EXISTS`, `JSON_VALUE`, `JSON_QUERY` or, in PostgreSQL, `JSON`,
    /// `JSON_SCALAR` or `JSON_SERIALIZE` after its name, as PostgreSQL 17 and
    /// Oracle write them; `None` for any other name.
    pub(super) fn sql_json_function(&mut self, name: &Name) -> SqlResult<Option<Expr>> {
        if !self.sql_json_dialect() || self.peek_kind() != &TokenKind::LeftParen {
            return Ok(None);
        }
        if self.postgresql()
            && let Some(expr) = self.sql_json_constructor(name)?
        {
            return Ok(Some(expr));
        }
        let kind = match name.text.as_str() {
            "json_exists" => QueryKind::Exists,
            "json_value" => QueryKind::Value,
            "json_query" => QueryKind::Query,
            _ => return Ok(None),
        };
        self.at += 1;
        let document = self.json_context_item()?;
        self.expect(&TokenKind::Comma)?;
        let path = self.expression()?;
        let passing = self.json_passing()?;
        let mut returning = None;
        if kind != QueryKind::Exists && self.take_word("returning") {
            returning = Some(self.cast_type()?);
            self.json_format_clause()?;
            self.skip_oracle_returning_options();
        }
        let (wrapper, quotes) = if kind == QueryKind::Query {
            let (wrapper, quotes, _) = self.json_wrapper_and_quotes()?;
            (wrapper, quotes)
        } else {
            (Wrapper::Without, Quotes::Keep)
        };
        let (on_empty, on_error) = self.json_behaviors(kind)?;
        self.expect(&TokenKind::RightParen)?;
        if returning.is_none() && kind == QueryKind::Query && self.oracle() {
            returning = Some(SqlType::Text);
        }
        Ok(Some(crate::sql_json::query_call(QueryCall {
            kind,
            document,
            path,
            passing,
            returning,
            wrapper,
            quotes,
            on_empty,
            on_error,
            position: name.position,
        })))
    }

    pub(super) fn at_json_table(&self) -> bool {
        self.sql_json_dialect()
            && self.is_word("json_table")
            && self.peek_kind_at(1) == &TokenKind::LeftParen
    }

    fn json_table_path(&mut self) -> SqlResult<String> {
        let position = self.position();
        match self.peek_kind() {
            TokenKind::String(text) => {
                let text = text.as_ref().to_owned();
                self.at += 1;
                Ok(text)
            }
            _ => Err(SqlError::syntax(
                "only string constants are supported in JSON_TABLE path specification",
                position,
            )),
        }
    }

    fn json_table_path_name(&mut self) -> SqlResult<()> {
        if self.take_word("as") {
            self.name()?;
        }
        Ok(())
    }

    fn json_table_on_error(&mut self) -> SqlResult<Option<Behavior>> {
        let ends_clause = |parser: &Self, at: usize| {
            parser.is_word_at(at, "on") && parser.is_word_at(at + 1, "error")
        };
        let (behavior, words) = if self.is_word("error") && ends_clause(self, 1) {
            (Behavior::Error, 3)
        } else if self.is_word("empty") && ends_clause(self, 1) {
            (Behavior::EmptyArray, 3)
        } else if self.is_word("empty") && self.is_word_at(1, "array") && ends_clause(self, 2) {
            (Behavior::EmptyArray, 4)
        } else if self.oracle() && self.is_word("null") && ends_clause(self, 1) {
            (Behavior::EmptyArray, 3)
        } else {
            return Ok(None);
        };
        self.at += words;
        Ok(Some(behavior))
    }

    fn json_table_columns(
        &mut self,
        found: &mut TableColumns,
        depth: usize,
    ) -> SqlResult<Vec<TableColumn>> {
        if depth > crate::sql_json::MAX_NESTING {
            return Err(SqlError::new(
                SqlState::STATEMENT_TOO_COMPLEX,
                "JSON_TABLE nests NESTED PATH too deeply",
            )
            .at(self.position()));
        }
        self.expect(&TokenKind::LeftParen)?;
        let mut columns = Vec::new();
        loop {
            if self.is_word("nested") {
                self.at += 1;
                self.take_word("path");
                let path = self.json_table_path()?;
                self.json_table_path_name()?;
                self.expect_word("columns")?;
                let nested = self.json_table_columns(found, depth + 1)?;
                columns.push(TableColumn::Nested {
                    path,
                    columns: nested,
                });
            } else {
                columns.push(self.json_table_column(found)?);
            }
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(columns)
    }

    fn json_table_column(&mut self, found: &mut TableColumns) -> SqlResult<TableColumn> {
        let token = self.peek();
        let written = self.text.get(token.start..token.end).unwrap_or("");
        let quoted = matches!(token.kind, TokenKind::QuotedIdentifier(_));
        let written = written.to_owned();
        let name = self.name()?;
        let key = if self.oracle() && !quoted {
            written
        } else {
            name.text.clone()
        };
        if self.is_word("for") && self.is_word_at(1, "ordinality") {
            self.at += 2;
            found.add(name, SqlType::Integer)?;
            return Ok(TableColumn::Ordinality);
        }
        let untyped = self.oracle()
            && (self.is_word("path") || self.is_word("format") || self.is_word("exists"));
        let sql_type = if untyped {
            SqlType::Text
        } else {
            self.cast_type()?
        };
        if self.take_word("exists") {
            let path = if self.take_word("path") {
                self.json_table_path()?
            } else {
                escaped_member(&key)
            };
            let (_, on_error) = self.json_behaviors(QueryKind::Exists)?;
            found.add(name, sql_type)?;
            return Ok(TableColumn::Exists {
                path,
                on_error: on_error.behavior,
            });
        }
        let format_json = self.json_format_clause()?;
        let path = if self.take_word("path") {
            self.json_table_path()?
        } else {
            escaped_member(&key)
        };
        let (wrapper, quotes, shaped) = self.json_wrapper_and_quotes()?;
        let query = format_json || shaped || is_json_shaped(sql_type);
        let kind = if query {
            QueryKind::Query
        } else {
            QueryKind::Value
        };
        let (mut on_empty, mut on_error) = self.json_behaviors(kind)?;
        let column = RegularColumn {
            path,
            query,
            wrapper,
            quotes,
            on_empty: on_empty.behavior,
            on_empty_default: found.default_index(&mut on_empty),
            on_error: on_error.behavior,
            on_error_default: found.default_index(&mut on_error),
        };
        found.add(name, sql_type)?;
        Ok(TableColumn::Regular(column))
    }

    /// Parses `JSON_TABLE(...)` in `FROM`, as PostgreSQL 17 and Oracle write it.
    pub(super) fn json_table(&mut self) -> SqlResult<TableReference> {
        let position = self.position();
        self.at += 2;
        let document = self.json_context_item()?;
        let path = if self.take(&TokenKind::Comma) {
            let path = self.json_table_path()?;
            self.json_table_path_name()?;
            path
        } else {
            ROOT_PATH.to_owned()
        };
        let passing = self.json_passing()?;
        let mut on_error = self.json_table_on_error()?;
        self.expect_word("columns")?;
        let mut found = TableColumns::default();
        let columns = self.json_table_columns(&mut found, 0)?;
        if let Some(behavior) = self.json_table_on_error()? {
            on_error = Some(behavior);
        }
        self.expect(&TokenKind::RightParen)?;
        let alias = if self.take_word("as") || self.at_bare_alias() {
            Some(self.name()?.text)
        } else {
            None
        };
        let function = crate::sql_json::table_function(TableCall {
            document,
            plan: TablePlan {
                path,
                on_error: on_error.unwrap_or(Behavior::EmptyArray),
                columns,
            },
            passing,
            names: found.names,
            types: found.types,
            defaults: found.defaults,
            position,
        });
        Ok(TableReference {
            name: Name {
                text: alias.clone().unwrap_or_else(|| "json_table".to_owned()),
                position,
            },
            alias,
            derived: None,
            generated: Some(Box::new(GeneratedTable::Function(function))),
        })
    }

    fn row_type_marker(&mut self) -> Option<Expr> {
        let position = self.position();
        let cast = self.is_word("cast") && self.peek_kind_at(1) == &TokenKind::LeftParen;
        if cast {
            self.at += 2;
        }
        if !self.take_word("null") {
            return None;
        }
        let introduced = if cast {
            self.take_word("as")
        } else {
            self.take(&TokenKind::DoubleColon)
        };
        if !introduced {
            return None;
        }
        let first = self.name().ok()?;
        let table = if self.take(&TokenKind::Dot) {
            let table = self.name().ok()?;
            crate::schemas::written_qualified(&first.text, &table.text)
        } else {
            first.text
        };
        if cast && !self.take(&TokenKind::RightParen) {
            return None;
        }
        Some(Expr {
            kind: ExprKind::Function {
                name: crate::sql_json::ROW_TYPE.to_owned(),
                arguments: vec![Expr {
                    kind: ExprKind::Literal(Literal::String(table)),
                    position: first.position,
                }],
            },
            position,
        })
    }

    /// Parses the first argument of `json(b)_populate_record(set)`: an expression, or
    /// `NULL::t` / `CAST(NULL AS t)` where `t` names a table or view rather than a type.
    pub(super) fn populate_base(&mut self) -> SqlResult<Expr> {
        let start = self.at;
        match self.expression() {
            Ok(expr) => Ok(expr),
            Err(error)
                if error.state() == SqlState::UNDEFINED_OBJECT
                    && crate::user_types::current().is_some() =>
            {
                self.at = start;
                let marker = self.row_type_marker();
                let ends = matches!(self.peek_kind(), TokenKind::Comma | TokenKind::RightParen);
                match marker {
                    Some(marker) if ends => Ok(marker),
                    _ => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }
}
