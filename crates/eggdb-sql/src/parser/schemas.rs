use super::Parser;
use crate::ast::{
    AlterView, AlterViewAction, Comment, CommentTarget, CreateView, Name, Query, RelationKind,
    Statement,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;
use crate::schemas::{QUALIFIER, written_qualified};

const MOST_NAMES: usize = 1024;

/// How deep a parse starts: used to parse a view's text where the view is used.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Nesting {
    pub(crate) depth: usize,
    pub(crate) query_depth: usize,
    pub(crate) queries: usize,
}

impl<'t> Parser<'t> {
    pub(crate) fn nested_at(mut self, start: Nesting) -> Self {
        self.depth = start.depth;
        self.query_depth = start.query_depth;
        self.queries = start.queries;
        self
    }

    pub(crate) fn view_query(&mut self) -> SqlResult<(Query, usize)> {
        let query = self.query()?;
        if !self.at_end() {
            return Err(self.unexpected());
        }
        Ok((query, self.queries.max(1)))
    }

    pub(super) fn qualified_part(&mut self, schema: &Name) -> SqlResult<Name> {
        let name = self.name()?;
        if self.peek_kind() == &TokenKind::Dot {
            return Err(
                SqlError::not_supported("cross-database references are not implemented")
                    .at(schema.position),
            );
        }
        if schema.text.contains(QUALIFIER) || name.text.contains(QUALIFIER) {
            return Err(SqlError::new(
                SqlState::INVALID_NAME,
                format!("invalid name \"{}\"", name.text.escape_default()),
            )
            .at(name.position));
        }
        Ok(Name {
            text: written_qualified(&schema.text, &name.text),
            position: schema.position,
        })
    }

    fn schema_name(&mut self) -> SqlResult<Name> {
        let name = self.name()?;
        crate::schemas::schema_named(&name.text, name.position)
    }

    fn if_exists(&mut self) -> SqlResult<bool> {
        if self.take_word("if") {
            self.expect_word("exists")?;
            return Ok(true);
        }
        Ok(false)
    }

    fn if_not_exists(&mut self) -> SqlResult<bool> {
        if self.take_word("if") {
            self.expect_word("not")?;
            self.expect_word("exists")?;
            return Ok(true);
        }
        Ok(false)
    }

    fn schema_drop_behaviour(&mut self) -> bool {
        if self.take_word("cascade") {
            return true;
        }
        self.take_word("restrict");
        false
    }

    pub(super) fn skip_statement(&mut self) {
        while !matches!(self.peek_kind(), TokenKind::Semicolon | TokenKind::End) {
            self.at += 1;
        }
    }

    pub(super) fn create_schema(&mut self) -> SqlResult<Statement> {
        self.expect_word("schema")?;
        let if_not_exists = self.if_not_exists()?;
        let name = if self.take_word("authorization") {
            self.schema_name()?
        } else {
            let name = self.schema_name()?;
            if self.take_word("authorization") {
                self.name()?;
            }
            name
        };
        if !matches!(self.peek_kind(), TokenKind::Semicolon | TokenKind::End) {
            return Err(self.not_supported("CREATE SCHEMA with schema elements"));
        }
        Ok(Statement::CreateSchema {
            name,
            if_not_exists,
        })
    }

    pub(super) fn drop_schema(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        let if_exists = self.if_exists()?;
        let mut names = vec![self.schema_name()?];
        while self.take(&TokenKind::Comma) {
            if names.len() >= MOST_NAMES {
                return Err(self.not_supported("dropping more than 1024 schemas at once"));
            }
            names.push(self.schema_name()?);
        }
        let cascade = self.schema_drop_behaviour();
        Ok(Statement::DropSchema {
            names,
            if_exists,
            cascade,
        })
    }

    pub(super) fn alter_schema(&mut self) -> SqlResult<Statement> {
        self.expect_word("schema")?;
        let from = self.schema_name()?;
        if self.take_word("owner") {
            self.expect_word("to")?;
            self.label()?;
            return Ok(Statement::Ignored {
                tag: "ALTER SCHEMA",
                notice: None,
            });
        }
        self.expect_word("rename")?;
        self.expect_word("to")?;
        let to = self.schema_name()?;
        Ok(Statement::RenameSchema { from, to })
    }

    pub(super) fn create_view(&mut self, or_replace: bool) -> SqlResult<Statement> {
        if self.oracle() {
            let _ = self.take_word("force")
                || (self.is_word("no") && self.is_word_at(1, "force") && {
                    self.at += 2;
                    true
                });
            let _ = self.take_word("editioning")
                || self.take_word("editionable")
                || self.take_word("noneditionable");
        }
        if self.mysql() {
            self.mysql_view_options()?;
        }
        if self.is_word("temp") || self.is_word("temporary") {
            return Err(self.not_supported("CREATE TEMPORARY VIEW (temporary tables and views)"));
        }
        if self.is_word("recursive") {
            return Err(self.not_supported("CREATE RECURSIVE VIEW"));
        }
        if self.is_word("materialized") {
            return Err(self.not_supported("CREATE MATERIALIZED VIEW"));
        }
        if or_replace
            && !self.is_word("view")
            && let TokenKind::Word(word) = self.peek_kind()
        {
            return Err(
                self.not_supported(&format!("CREATE OR REPLACE {}", word.to_ascii_uppercase()))
            );
        }
        self.expect_word("view")?;
        let if_not_exists = if self.sqlite() || self.postgresql() {
            self.if_not_exists()?
        } else {
            false
        };
        if if_not_exists && or_replace {
            return Err(self.unexpected());
        }
        let name = self.table_name()?;
        let columns = if self.peek_kind() == &TokenKind::LeftParen {
            self.name_list()?
        } else {
            Vec::new()
        };
        if self.postgresql() && self.is_word("with") {
            return Err(self.not_supported("CREATE VIEW ... WITH (options)"));
        }
        self.expect_word("as")?;
        let start = self.at;
        if !self.starts_query() && self.peek_kind() != &TokenKind::LeftParen {
            return Err(self.unexpected());
        }
        let query = self.query()?;
        let first = self.tokens.get(start).map_or(0, |token| token.start);
        let last = self
            .tokens
            .get(self.at.saturating_sub(1))
            .map_or(first, |token| token.end);
        let text = self.text.get(first..last).unwrap_or("").to_owned();
        if self.is_word("with") {
            let position = self.position();
            self.at += 1;
            if self.oracle() && self.take_word("read") {
                self.expect_word("only")?;
            } else {
                let _ = self.take_word("cascaded") || self.take_word("local");
                self.expect_word("check")?;
                self.expect_word("option")?;
                return Err(SqlError::not_supported(
                    "CREATE VIEW ... WITH CHECK OPTION is not supported yet: views are read-only",
                )
                .at(position));
            }
        }
        Ok(Statement::CreateView(Box::new(CreateView {
            name,
            or_replace,
            if_not_exists,
            columns,
            query,
            text,
        })))
    }

    fn mysql_view_options(&mut self) -> SqlResult<()> {
        if self.take_word("algorithm") {
            self.expect_operator("=")?;
            if !(self.take_word("undefined")
                || self.take_word("merge")
                || self.take_word("temptable"))
            {
                return Err(self.unexpected());
            }
        }
        if self.take_word("definer") {
            self.expect_operator("=")?;
            while !self.is_word("sql")
                && !self.is_word("view")
                && !matches!(self.peek_kind(), TokenKind::Semicolon | TokenKind::End)
            {
                self.at += 1;
            }
        }
        if self.take_word("sql") {
            self.expect_word("security")?;
            if !(self.take_word("definer") || self.take_word("invoker")) {
                return Err(self.unexpected());
            }
        }
        Ok(())
    }

    fn expect_operator(&mut self, symbol: &str) -> SqlResult<()> {
        if self.take_operator(symbol) {
            Ok(())
        } else {
            Err(self.unexpected())
        }
    }

    pub(super) fn drop_view(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        let if_exists = self.if_exists()?;
        let mut names = vec![self.table_name()?];
        while !self.sqlite() && !self.oracle() && self.take(&TokenKind::Comma) {
            if names.len() >= MOST_NAMES {
                return Err(self.not_supported("dropping more than 1024 views at once"));
            }
            names.push(self.table_name()?);
        }
        let cascade = if self.oracle() {
            if self.take_word("cascade") {
                self.expect_word("constraints")?;
            }
            false
        } else if self.sqlite() {
            false
        } else {
            self.schema_drop_behaviour()
        };
        Ok(Statement::DropView {
            names,
            if_exists,
            cascade,
        })
    }

    pub(super) fn alter_view(&mut self) -> SqlResult<Statement> {
        self.expect_word("view")?;
        let if_exists = self.postgresql() && self.if_exists()?;
        let name = self.table_name()?;
        let action = if self.take_word("rename") {
            if self.is_word("column") || !self.is_word("to") {
                return Err(self.not_supported("ALTER VIEW ... RENAME COLUMN"));
            }
            self.expect_word("to")?;
            AlterViewAction::RenameTo(self.name()?)
        } else if self.is_word("set") && self.is_word_at(1, "schema") {
            self.at += 2;
            AlterViewAction::SetSchema(self.schema_name()?)
        } else if self.take_word("owner") {
            self.expect_word("to")?;
            self.label()?;
            AlterViewAction::Accepted
        } else {
            return Err(self.not_supported("this ALTER VIEW action"));
        };
        Ok(Statement::AlterView(AlterView {
            name,
            if_exists,
            action,
        }))
    }

    pub(super) fn comment(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        self.expect_word("on")?;
        let position = self.position();
        let TokenKind::Word(word) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let word = word.as_ref().to_owned();
        self.at += 1;
        let target = match word.as_str() {
            "table" => CommentTarget::Relation {
                kind: RelationKind::Table,
                name: self.table_name()?,
            },
            "view" if !self.oracle() => CommentTarget::Relation {
                kind: RelationKind::View,
                name: self.table_name()?,
            },
            "sequence" if self.postgresql() => CommentTarget::Relation {
                kind: RelationKind::Sequence,
                name: self.table_name()?,
            },
            "column" => {
                let (table, column) = self.column_path()?;
                CommentTarget::Column { table, column }
            }
            "schema" if self.postgresql() => CommentTarget::Schema(self.schema_name()?),
            "index" if self.postgresql() => CommentTarget::Index(self.table_name()?),
            "constraint" if self.postgresql() => {
                let name = self.name()?;
                self.expect_word("on")?;
                self.take_word("domain");
                let table = self.table_name()?;
                CommentTarget::Constraint { table, name }
            }
            "extension" if self.postgresql() => CommentTarget::Extension(self.name()?),
            "materialized" if self.oracle() => {
                return Err(SqlError::not_supported(
                    "COMMENT ON MATERIALIZED VIEW is not supported yet",
                )
                .at(position));
            }
            _ => {
                return Err(SqlError::not_supported(format!(
                    "COMMENT ON {} is not supported yet",
                    word.to_ascii_uppercase()
                ))
                .at(position));
            }
        };
        self.expect_word("is")?;
        let text = match self.peek_kind() {
            TokenKind::String(text) => Some(text.as_ref().to_owned()),
            TokenKind::Word(word) if word == "null" => None,
            _ => return Err(self.unexpected()),
        };
        self.at += 1;
        Ok(Statement::Comment(Comment { target, text }))
    }

    fn column_path(&mut self) -> SqlResult<(Name, Name)> {
        let first = self.name()?;
        self.expect(&TokenKind::Dot)?;
        let second = self.name()?;
        if !self.take(&TokenKind::Dot) {
            return Ok((self.plain_table(first)?, second));
        }
        let third = self.name()?;
        if self.peek_kind() == &TokenKind::Dot {
            return Err(
                SqlError::not_supported("cross-database references are not implemented")
                    .at(first.position),
            );
        }
        let table = self.schema_table(first, second)?;
        Ok((table, third))
    }

    fn plain_table(&self, name: Name) -> SqlResult<Name> {
        crate::constraints::check_identifier(&name)?;
        Ok(name)
    }

    fn schema_table(&self, schema: Name, table: Name) -> SqlResult<Name> {
        if self.oracle() {
            if crate::lexer::swap_uniform_case(&schema.text) != self.oracle_user() {
                return Err(SqlError::new(
                    SqlState::UNDEFINED_TABLE,
                    "table or view does not exist",
                )
                .with_oracle_code(942)
                .at(schema.position));
            }
            return Ok(table);
        }
        if self.mysql() {
            self.check_database(&schema)?;
            return Ok(table);
        }
        if self.sqlite() {
            return Err(self.unexpected());
        }
        if schema.text.contains(QUALIFIER) || table.text.contains(QUALIFIER) {
            return Err(SqlError::new(
                SqlState::INVALID_NAME,
                format!("invalid name \"{}\"", table.text.escape_default()),
            )
            .at(table.position));
        }
        Ok(Name {
            text: written_qualified(&schema.text, &table.text),
            position: schema.position,
        })
    }

    pub(super) fn grant_or_revoke(&mut self, tag: &'static str) -> SqlResult<Statement> {
        self.skip_statement();
        Ok(Statement::Ignored {
            tag,
            notice: Some(format!(
                "WARNING:  {tag} was accepted and ignored: EggDB has no roles or privileges yet"
            )),
        })
    }

    pub(super) fn create_extension(&mut self) -> SqlResult<Statement> {
        self.expect_word("extension")?;
        let _ = self.if_not_exists()?;
        let name = self.name()?;
        if name.text != "plpgsql" {
            return Err(SqlError::not_supported(format!(
                "extension \"{}\" is not supported",
                name.text
            ))
            .with_hint("EggDB accepts CREATE EXTENSION plpgsql only; other extensions are not available yet.")
            .at(name.position));
        }
        self.skip_statement();
        Ok(Statement::Ignored {
            tag: "CREATE EXTENSION",
            notice: None,
        })
    }
}
