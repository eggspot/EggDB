use super::Parser;
use crate::ast::Statement;
use crate::error::SqlResult;
use crate::lexer::TokenKind;
use crate::served_databases::DatabaseStatement;

impl Parser<'_> {
    pub(super) fn at_database_definition(&self) -> bool {
        (self.postgresql() || self.sqlserver() || self.mysql())
            && (self.is_word("database") || (self.mysql() && self.is_word("schema")))
    }

    pub(super) fn create_database(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        let if_not_exists = self.mysql() && self.if_not_exists()?;
        let name = self.name()?;
        let template = match self.postgresql() {
            true => self.database_template()?,
            false => None,
        };
        self.skip_database_options();
        Ok(Statement::Database(DatabaseStatement::Create {
            name,
            if_not_exists,
            template,
        }))
    }

    fn database_template(&mut self) -> SqlResult<Option<crate::ast::Name>> {
        let mut at = self.at;
        loop {
            match &self.tokens.get(at).map(|token| &token.kind) {
                None | Some(TokenKind::Semicolon | TokenKind::End) => return Ok(None),
                Some(TokenKind::Word(word)) if word.as_ref() == "template" => break,
                _ => at += 1,
            }
        }
        self.at = at + 1;
        self.take_operator("=");
        let template = self.name()?;
        Ok(match template.text.as_str() {
            "template0" | "template1" => None,
            _ => Some(template),
        })
    }

    pub(super) fn drop_database(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        let if_exists = self.if_exists()?;
        let mut names = vec![self.name()?];
        while self.sqlserver() && self.take(&TokenKind::Comma) {
            names.push(self.name()?);
        }
        self.skip_database_options();
        Ok(Statement::Database(DatabaseStatement::Drop {
            names,
            if_exists,
        }))
    }

    fn skip_database_options(&mut self) {
        let sqlserver_options = ["on", "collate", "with", "containment", "log"]
            .iter()
            .any(|word| self.is_word(word));
        if !self.sqlserver() || sqlserver_options {
            self.skip_statement();
        }
    }
}
