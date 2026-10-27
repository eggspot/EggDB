use super::Parser;
use crate::ast::{CreateIndex, DropIndex, Explain, ExplainStyle, IndexSpec, Name, Statement};
use crate::error::SqlResult;
use crate::lexer::TokenKind;

impl Parser<'_> {
    pub(super) fn at_index_definition(&self) -> bool {
        self.is_word("index") || self.is_word("unique") && self.is_word_at(1, "index")
    }

    pub(super) fn create_index(&mut self) -> SqlResult<Statement> {
        let position = self.position();
        let unique = self.take_word("unique");
        self.expect_word("index")?;
        if self.is_word("concurrently") {
            return Err(self.not_supported("CREATE INDEX CONCURRENTLY"));
        }
        let if_not_exists = if self.is_word("if") && self.is_word_at(1, "not") {
            self.at += 1;
            self.expect_word("not")?;
            self.expect_word("exists")?;
            true
        } else {
            false
        };
        let name = if self.is_word("on") && !self.mysql() && !self.sqlite() && !self.oracle() {
            None
        } else {
            Some(self.table_name()?)
        };
        if self.mysql() {
            self.index_method()?;
        }
        self.expect_word("on")?;
        self.take_word("only");
        let table = self.table_name()?;
        if !self.mysql() {
            self.index_method()?;
        }
        let columns = self.index_column_list(unique)?;
        self.index_tail()?;
        Ok(Statement::CreateIndex(CreateIndex {
            index: IndexSpec {
                name,
                unique,
                columns,
                position,
            },
            table,
            if_not_exists,
        }))
    }

    fn index_method(&mut self) -> SqlResult<()> {
        if !self.take_word("using") {
            return Ok(());
        }
        let method = self.label()?.to_ascii_lowercase();
        if method != "btree" && method != "hash" {
            return Err(self.not_supported(&format!("index method {method}")));
        }
        Ok(())
    }

    fn index_tail(&mut self) -> SqlResult<()> {
        loop {
            if self.mysql() && (self.is_word("using") || self.is_word("comment")) {
                self.index_options_mysql()?;
            } else if self.mysql() && (self.is_word("algorithm") || self.is_word("lock")) {
                self.at += 1;
                self.take_operator("=");
                self.label()?;
            } else if self.oracle()
                && [
                    "online",
                    "logging",
                    "nologging",
                    "noparallel",
                    "compress",
                    "nocompress",
                ]
                .iter()
                .any(|word| self.is_word(word))
            {
                self.at += 1;
            } else if self.oracle() && self.take_word("tablespace") {
                self.name()?;
            } else if self.oracle() && self.is_word("compute") {
                self.at += 1;
                self.expect_word("statistics")?;
            } else if self.is_word("where") {
                return Err(self.not_supported("partial indexes (CREATE INDEX ... WHERE)"));
            } else if self.is_word("include") || self.is_word("with") || self.is_word("tablespace")
            {
                return Err(self.not_supported("index options"));
            } else {
                return Ok(());
            }
        }
    }

    fn index_options_mysql(&mut self) -> SqlResult<()> {
        if self.take_word("using") {
            let method = self.label()?.to_ascii_lowercase();
            if method != "btree" && method != "hash" {
                return Err(self.not_supported(&format!("index method {method}")));
            }
        } else if self.take_word("comment") {
            let TokenKind::String(_) = self.peek_kind() else {
                return Err(self.unexpected());
            };
            self.at += 1;
        }
        Ok(())
    }

    pub(super) fn index_column_list(&mut self, unique: bool) -> SqlResult<Vec<Name>> {
        self.expect(&TokenKind::LeftParen)?;
        let mut columns = Vec::new();
        loop {
            if self.peek_kind() == &TokenKind::LeftParen {
                return Err(self.not_supported("indexes on expressions"));
            }
            let column = self.name()?;
            if self.mysql() && self.peek_kind() == &TokenKind::LeftParen {
                if unique {
                    return Err(self.not_supported("UNIQUE indexes on a column prefix"));
                }
                self.at += 1;
                let TokenKind::Integer(_) = self.peek_kind() else {
                    return Err(self.unexpected());
                };
                self.at += 1;
                self.expect(&TokenKind::RightParen)?;
            }
            if self.peek_kind() == &TokenKind::LeftParen {
                return Err(self.not_supported("indexes on expressions"));
            }
            if self.take_word("collate") {
                let collation = self.label()?;
                if !(collation.eq_ignore_ascii_case("binary") || collation == "C") {
                    return Err(self.not_supported("index columns with a collation"));
                }
            }
            let _ = self.take_word("asc") || self.take_word("desc");
            if self.take_word("nulls") && !self.take_word("first") {
                self.expect_word("last")?;
            }
            if columns.iter().any(|known: &Name| known.text == column.text) {
                return Err(crate::error::SqlError::new(
                    crate::error::SqlState::DUPLICATE_COLUMN,
                    format!(
                        "column \"{}\" appears twice in index definition",
                        column.text
                    ),
                )
                .at(column.position));
            }
            columns.push(column);
            if columns.len() > crate::encoding::MAX_INDEX_COLUMNS {
                return Err(crate::error::SqlError::new(
                    crate::error::SqlState::TOO_MANY_COLUMNS,
                    format!(
                        "cannot use more than {} columns in an index",
                        crate::encoding::MAX_INDEX_COLUMNS
                    ),
                ));
            }
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(columns)
    }

    pub(super) fn drop_index(&mut self) -> SqlResult<Statement> {
        self.expect_word("index")?;
        if self.is_word("concurrently") {
            return Err(self.not_supported("DROP INDEX CONCURRENTLY"));
        }
        let if_exists = if self.is_word("if") && self.is_word_at(1, "exists") {
            self.at += 2;
            true
        } else {
            false
        };
        let mut names = vec![self.table_name()?];
        while !self.mysql() && !self.sqlite() && !self.oracle() && self.take(&TokenKind::Comma) {
            names.push(self.table_name()?);
        }
        let table = if self.mysql() {
            self.expect_word("on")?;
            let table = self.table_name()?;
            while self.is_word("algorithm") || self.is_word("lock") {
                self.at += 1;
                self.take_operator("=");
                self.label()?;
            }
            Some(table)
        } else {
            None
        };
        if self.oracle() {
            let _ = self.take_word("online") || self.take_word("force");
        }
        if !self.mysql() && !self.sqlite() && !self.oracle() {
            if self.is_word("cascade") {
                return Err(self.not_supported("DROP INDEX ... CASCADE"));
            }
            self.take_word("restrict");
        }
        Ok(Statement::DropIndex(DropIndex {
            names,
            table,
            if_exists,
        }))
    }

    pub(super) fn explain(&mut self) -> SqlResult<Statement> {
        self.expect_word("explain")?;
        let style = if self.sqlite() {
            if !(self.is_word("query") && self.is_word_at(1, "plan")) {
                return Err(self.not_supported("EXPLAIN without QUERY PLAN (bytecode listings)"));
            }
            self.at += 2;
            ExplainStyle::QueryPlan
        } else {
            if self.oracle() {
                return Err(self.not_supported("EXPLAIN PLAN"));
            }
            if self.peek_kind() == &TokenKind::LeftParen
                || self.is_word("analyze")
                || self.is_word("verbose")
                || self.is_word("format")
                || self.is_word("analyse")
            {
                return Err(self.not_supported("EXPLAIN options"));
            }
            ExplainStyle::Plan
        };
        if !(self.is_word("select") || self.is_word("update") || self.is_word("delete")) {
            return Err(
                self.not_supported("EXPLAIN of a statement other than SELECT, UPDATE or DELETE")
            );
        }
        let statement = self.statement_body()?;
        Ok(Statement::Explain(Explain {
            style,
            statement: Box::new(statement),
        }))
    }
}
