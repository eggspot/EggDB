use super::Parser;
use crate::ast::{CreateTable, CreateTableAs, CreateTableAsSpelling, Name, OnDuplicate, Statement};
use crate::error::SqlResult;
use crate::lexer::TokenKind;

impl Parser<'_> {
    pub(super) fn at_create_table_as(&self) -> bool {
        if self.sqlserver() {
            return false;
        }
        if self.is_word("as") {
            return true;
        }
        if self.mysql() {
            return self.at_mysql_table_query();
        }
        (self.postgresql() || self.oracle()) && self.at_column_names_then_as()
    }

    pub(super) fn at_mysql_table_query(&self) -> bool {
        self.is_word("as")
            || ((self.is_word("ignore") || self.is_word("replace"))
                && (self.is_word_at(1, "as")
                    || self.is_word_at(1, "select")
                    || self.is_word_at(1, "with")
                    || self.peek_kind_at(1) == &TokenKind::LeftParen))
            || self.starts_query()
            || self.starts_query_in_parentheses()
    }

    fn at_column_names_then_as(&self) -> bool {
        if self.peek_kind() != &TokenKind::LeftParen {
            return false;
        }
        let mut ahead = 1;
        loop {
            if !matches!(
                self.peek_kind_at(ahead),
                TokenKind::Word(_) | TokenKind::QuotedIdentifier(_)
            ) {
                return false;
            }
            match self.peek_kind_at(ahead + 1) {
                TokenKind::Comma => ahead += 2,
                TokenKind::RightParen => return self.is_word_at(ahead + 2, "as"),
                _ => return false,
            }
            if ahead > crate::encoding::MAX_COLUMNS * 2 {
                return false;
            }
        }
    }

    pub(super) fn create_table_as(
        &mut self,
        name: Name,
        temporary: bool,
        if_not_exists: bool,
        declared: Option<CreateTable>,
    ) -> SqlResult<Statement> {
        let column_names = if declared.is_none()
            && !self.mysql()
            && self.peek_kind() == &TokenKind::LeftParen
        {
            self.name_list()?
        } else {
            Vec::new()
        };
        if self.oracle() {
            self.skip_physical_properties()?;
        }
        let mut on_duplicate = OnDuplicate::Fail;
        if self.mysql() {
            if self.take_word("ignore") {
                on_duplicate = OnDuplicate::Ignore;
            } else if self.take_word("replace") {
                on_duplicate = OnDuplicate::Replace;
            }
            self.take_word("as");
        } else {
            self.expect_word("as")?;
        }
        if self.postgresql() && self.is_word("execute") {
            return Err(self.not_supported("CREATE TABLE AS EXECUTE"));
        }
        if !self.starts_query() && !self.starts_query_in_parentheses() {
            return Err(self.unexpected());
        }
        let position = self.position();
        let query = self.query_statement()?;
        if !matches!(query, Statement::Select(_) | Statement::Query(_)) {
            return Err(self
                .not_supported("a data-modifying statement in CREATE TABLE AS")
                .at(position));
        }
        let mut with_data = true;
        if self.postgresql()
            && self.is_word("with")
            && (self.is_word_at(1, "data")
                || (self.is_word_at(1, "no") && self.is_word_at(2, "data")))
        {
            self.at += 1;
            with_data = !self.take_word("no");
            self.expect_word("data")?;
        }
        Ok(Statement::CreateTableAs(Box::new(CreateTableAs {
            name,
            temporary,
            query,
            spelling: CreateTableAsSpelling::CreateTableAs,
            if_not_exists,
            column_names,
            declared: declared.map(Box::new),
            with_data,
            on_duplicate,
        })))
    }
}
