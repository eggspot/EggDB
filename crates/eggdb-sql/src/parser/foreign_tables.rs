use super::Parser;
use crate::ast::Statement;
use crate::error::SqlResult;
use crate::foreign_tables::ForeignDataCommand;
use crate::lexer::TokenKind;

impl Parser<'_> {
    pub(super) fn at_foreign_data_definition(&self) -> bool {
        self.is_word("server")
            || (self.is_word("foreign") && (self.is_word_at(1, "table") || (self.is_word_at(1, "data") && self.is_word_at(2, "wrapper"))))
    }

    fn foreign_text(&mut self) -> SqlResult<String> {
        let TokenKind::String(text) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let text = text.as_ref().to_owned();
        self.at += 1;
        Ok(text)
    }

    fn foreign_options(&mut self) -> SqlResult<Vec<(String, String)>> {
        let mut options = Vec::new();
        if !self.take_word("options") {
            return Ok(options);
        }
        self.expect(&TokenKind::LeftParen)?;
        loop {
            let _ = self.take_word("add") || self.take_word("set");
            let name = self.name()?.text.to_ascii_lowercase();
            options.push((name, self.foreign_text()?));
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(options)
    }

    fn skip_to_statement_end(&mut self) {
        while !self.at_end() && self.peek_kind() != &TokenKind::Semicolon {
            self.at += 1;
        }
    }

    fn skip_column_extras(&mut self) -> SqlResult<()> {
        let mut depth = 0usize;
        loop {
            match self.peek_kind() {
                TokenKind::End | TokenKind::Semicolon => return Err(self.unexpected()),
                TokenKind::Comma | TokenKind::RightParen if depth == 0 => return Ok(()),
                TokenKind::LeftParen => depth += 1,
                TokenKind::RightParen => depth -= 1,
                _ => {}
            }
            self.at += 1;
        }
    }

    pub(super) fn create_foreign_data(&mut self) -> SqlResult<Statement> {
        let command = if self.take_word("server") {
            let if_not_exists = self.if_not_exists()?;
            let name = self.name()?;
            if self.take_word("type") {
                self.foreign_text()?;
            }
            if self.take_word("version") {
                self.foreign_text()?;
            }
            self.expect_word("foreign")?;
            self.expect_word("data")?;
            self.expect_word("wrapper")?;
            let wrapper = self.name()?.text.to_ascii_lowercase();
            let options = self.foreign_options()?;
            ForeignDataCommand::CreateServer {
                name,
                wrapper,
                options,
                if_not_exists,
            }
        } else {
            self.expect_word("foreign")?;
            if self.take_word("data") {
                self.expect_word("wrapper")?;
                self.skip_to_statement_end();
                return Ok(Statement::ForeignData(Box::new(ForeignDataCommand::CreateWrapper)));
            }
            self.expect_word("table")?;
            let if_not_exists = self.if_not_exists()?;
            let name = self.table_name()?;
            self.expect(&TokenKind::LeftParen)?;
            let mut columns = Vec::new();
            loop {
                let column = self.name()?.text;
                let start = self.peek().start;
                self.sql_type()?;
                let type_text = self.slice(start, self.previous_end());
                self.skip_column_extras()?;
                columns.push((column, type_text));
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
            self.expect_word("server")?;
            let server = self.name()?.text;
            let options = self.foreign_options()?;
            ForeignDataCommand::CreateTable {
                name,
                columns,
                server,
                options,
                if_not_exists,
            }
        };
        Ok(Statement::ForeignData(Box::new(command)))
    }

    pub(super) fn drop_foreign_data(&mut self) -> SqlResult<Statement> {
        let server = self.take_word("server");
        if !server {
            self.expect_word("foreign")?;
            if self.take_word("data") {
                self.expect_word("wrapper")?;
                self.skip_to_statement_end();
                return Ok(Statement::ForeignData(Box::new(ForeignDataCommand::DropWrapper)));
            }
            self.expect_word("table")?;
        }
        let if_exists = self.if_exists()?;
        let mut names = vec![if server { self.name()? } else { self.table_name()? }];
        while self.take(&TokenKind::Comma) {
            names.push(if server { self.name()? } else { self.table_name()? });
        }
        let cascade = self.take_word("cascade");
        if !cascade {
            self.take_word("restrict");
        }
        Ok(Statement::ForeignData(Box::new(if server {
            ForeignDataCommand::DropServer { names, if_exists, cascade }
        } else {
            ForeignDataCommand::DropTable { names, if_exists }
        })))
    }
}
