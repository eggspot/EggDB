use super::Parser;
use crate::ast::{Name, Statement};
use crate::error::SqlResult;
use crate::lexer::TokenKind;
use crate::publications::{PublicationCommand, PublishedFrom, PublishedTable, TableChange};

impl Parser<'_> {
    fn publication_tables(&mut self) -> SqlResult<(Vec<PublishedTable>, Vec<Name>)> {
        let mut tables = Vec::new();
        let mut schemas = Vec::new();
        loop {
            if self.take_word("tables") {
                self.expect_word("in")?;
                self.expect_word("schema")?;
                schemas.push(self.name()?);
            } else {
                self.take_word("table");
                self.take_word("only");
                let name = self.table_name()?;
                self.take_operator("*");
                let mut columns = Vec::new();
                if self.take(&TokenKind::LeftParen) {
                    loop {
                        columns.push(self.name()?);
                        if !self.take(&TokenKind::Comma) {
                            break;
                        }
                    }
                    self.expect(&TokenKind::RightParen)?;
                }
                let mut filter = None;
                if self.take_word("where") {
                    self.expect(&TokenKind::LeftParen)?;
                    let first = self.at;
                    self.expression()?;
                    filter = Some(self.source_text(first));
                    self.expect(&TokenKind::RightParen)?;
                }
                tables.push(PublishedTable { name, columns, filter });
            }
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        Ok((tables, schemas))
    }

    fn publication_option_list(&mut self) -> SqlResult<Option<String>> {
        self.expect(&TokenKind::LeftParen)?;
        let mut publish = None;
        loop {
            let option = self.name()?.text.to_ascii_lowercase();
            if !self.take_operator("=") {
                return Err(self.unexpected());
            }
            let value = match self.peek_kind() {
                TokenKind::String(text) => text.to_string(),
                TokenKind::Word(word) => word.to_string(),
                _ => return Err(self.unexpected()),
            };
            self.at += 1;
            match option.as_str() {
                "publish" => publish = Some(value),
                "publish_via_partition_root" | "publish_generated_columns" => {}
                _ => return Err(self.not_supported(&format!("publication option \"{option}\""))),
            }
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(publish)
    }

    fn publication_options(&mut self) -> SqlResult<Option<String>> {
        if self.take_word("with") {
            self.publication_option_list()
        } else {
            Ok(None)
        }
    }

    pub(super) fn create_publication(&mut self) -> SqlResult<Statement> {
        self.expect_word("publication")?;
        let name = self.name()?;
        let from = if self.take_word("for") {
            if self.take_word("all") {
                self.expect_word("tables")?;
                PublishedFrom::AllTables
            } else {
                let (tables, schemas) = self.publication_tables()?;
                PublishedFrom::Listed { tables, schemas }
            }
        } else {
            PublishedFrom::Listed {
                tables: Vec::new(),
                schemas: Vec::new(),
            }
        };
        let publish = self.publication_options()?;
        Ok(Statement::Publication(PublicationCommand::Create { name, from, publish }))
    }

    pub(super) fn alter_publication(&mut self) -> SqlResult<Statement> {
        self.expect_word("publication")?;
        let name = self.name()?;
        let change = if self.take_word("add") {
            Some(TableChange::Add)
        } else if self.take_word("drop") {
            Some(TableChange::Drop)
        } else if self.is_word("set") && self.peek_kind_at(1) != &TokenKind::LeftParen {
            self.at += 1;
            Some(TableChange::Set)
        } else {
            None
        };
        if let Some(change) = change {
            let (tables, schemas) = self.publication_tables()?;
            return Ok(Statement::Publication(PublicationCommand::AlterTables { name, change, tables, schemas }));
        }
        if self.is_word("set") {
            self.at += 1;
            let publish = self.publication_option_list()?;
            return Ok(Statement::Publication(PublicationCommand::AlterPublish {
                name,
                publish: publish.unwrap_or_else(|| "insert, update, delete, truncate".to_owned()),
            }));
        }
        if self.take_word("rename") {
            self.expect_word("to")?;
            let to = self.name()?;
            return Ok(Statement::Publication(PublicationCommand::Rename { name, to }));
        }
        if self.take_word("owner") {
            self.expect_word("to")?;
            self.name()?;
            return Ok(Statement::Publication(PublicationCommand::Owner { name }));
        }
        Err(self.unexpected())
    }

    pub(super) fn drop_publication(&mut self) -> SqlResult<Statement> {
        self.expect_word("publication")?;
        let if_exists = self.take_word("if");
        if if_exists {
            self.expect_word("exists")?;
        }
        let mut names = vec![self.name()?];
        while self.take(&TokenKind::Comma) {
            names.push(self.name()?);
        }
        if !self.take_word("restrict") {
            self.take_word("cascade");
        }
        Ok(Statement::Publication(PublicationCommand::Drop { names, if_exists }))
    }
}
