use super::Parser;
use crate::ast::{Name, Statement};
use crate::error::SqlResult;
use crate::lexer::TokenKind;
use crate::text_search_catalog::{ConfigurationSource, MappingChange, TextSearchCommand, TextSearchKind};

impl Parser<'_> {
    pub(super) fn at_text_search_object(&self) -> bool {
        self.is_word("text")
            && self.is_word_at(1, "search")
            && (self.is_word_at(2, "dictionary")
                || self.is_word_at(2, "configuration")
                || self.is_word_at(2, "parser")
                || self.is_word_at(2, "template"))
    }

    fn text_search_kind(&mut self) -> SqlResult<TextSearchKind> {
        self.at += 2;
        if self.take_word("dictionary") {
            Ok(TextSearchKind::Dictionary)
        } else if self.take_word("configuration") {
            Ok(TextSearchKind::Configuration)
        } else {
            Err(self.not_supported("text search parsers and templates"))
        }
    }

    fn text_search_word(&mut self) -> SqlResult<Name> {
        let token = self.peek();
        let text = match &token.kind {
            TokenKind::Word(word) => word.as_ref().to_owned(),
            TokenKind::QuotedIdentifier(name) => name.clone(),
            _ => return Err(self.unexpected()),
        };
        self.at += 1;
        Ok(Name {
            text,
            position: token.start,
        })
    }

    fn text_search_name(&mut self) -> SqlResult<Name> {
        let mut name = self.text_search_word()?;
        while self.take(&TokenKind::Dot) {
            let part = self.text_search_word()?;
            name.text = format!("{}.{}", name.text, part.text);
        }
        Ok(name)
    }

    fn text_search_names(&mut self) -> SqlResult<Vec<Name>> {
        let mut names = vec![self.text_search_name()?];
        while self.take(&TokenKind::Comma) {
            names.push(self.text_search_name()?);
        }
        Ok(names)
    }

    fn text_search_value(&mut self) -> SqlResult<String> {
        let text = match self.peek_kind() {
            TokenKind::String(text) => text.as_ref().to_owned(),
            TokenKind::Integer(text) | TokenKind::Decimal(text) => (*text).to_owned(),
            TokenKind::Word(_) | TokenKind::QuotedIdentifier(_) => return Ok(self.text_search_name()?.text),
            _ => return Err(self.unexpected()),
        };
        self.at += 1;
        Ok(text)
    }

    fn text_search_options(&mut self, values_required: bool) -> SqlResult<Vec<(Name, Option<String>)>> {
        self.expect(&TokenKind::LeftParen)?;
        let mut options = Vec::new();
        loop {
            let name = self.text_search_word()?;
            let value = if self.take_operator("=") {
                Some(self.text_search_value()?)
            } else if values_required {
                return Err(self.unexpected());
            } else {
                None
            };
            options.push((name, value));
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(options)
    }

    pub(super) fn create_text_search(&mut self) -> SqlResult<Statement> {
        let kind = self.text_search_kind()?;
        let name = self.text_search_name()?;
        let command = match kind {
            TextSearchKind::Dictionary => TextSearchCommand::CreateDictionary {
                name,
                options: self.text_search_options(true)?,
            },
            TextSearchKind::Configuration => {
                self.expect(&TokenKind::LeftParen)?;
                let copy = if self.take_word("copy") {
                    true
                } else {
                    self.expect_word("parser")?;
                    false
                };
                if !self.take_operator("=") {
                    return Err(self.unexpected());
                }
                let from = self.text_search_name()?;
                self.expect(&TokenKind::RightParen)?;
                let source = if copy {
                    ConfigurationSource::Copy(from)
                } else {
                    ConfigurationSource::Parser(from)
                };
                TextSearchCommand::CreateConfiguration { name, source }
            }
        };
        Ok(Statement::TextSearch(command))
    }

    pub(super) fn drop_text_search(&mut self) -> SqlResult<Statement> {
        let kind = self.text_search_kind()?;
        let if_exists = self.take_word("if");
        if if_exists {
            self.expect_word("exists")?;
        }
        let names = self.text_search_names()?;
        if !self.take_word("restrict") && self.is_word("cascade") {
            return Err(self.not_supported("DROP TEXT SEARCH ... CASCADE"));
        }
        Ok(Statement::TextSearch(TextSearchCommand::Drop { kind, names, if_exists }))
    }

    fn token_types(&mut self) -> SqlResult<Vec<Name>> {
        self.expect_word("for")?;
        let mut token_types = vec![self.text_search_word()?];
        while self.take(&TokenKind::Comma) {
            token_types.push(self.text_search_word()?);
        }
        Ok(token_types)
    }

    fn replaced_dictionary(&mut self, token_types: Option<Vec<Name>>) -> SqlResult<MappingChange> {
        self.expect_word("replace")?;
        let old = self.text_search_name()?;
        self.expect_word("with")?;
        let new = self.text_search_name()?;
        Ok(MappingChange::Replace { token_types, old, new })
    }

    fn mapping_change(&mut self) -> SqlResult<MappingChange> {
        if self.take_word("add") {
            self.expect_word("mapping")?;
            let token_types = self.token_types()?;
            self.expect_word("with")?;
            let dictionaries = self.text_search_names()?;
            return Ok(MappingChange::Add { token_types, dictionaries });
        }
        if self.take_word("drop") {
            self.expect_word("mapping")?;
            let if_exists = self.take_word("if");
            if if_exists {
                self.expect_word("exists")?;
            }
            let token_types = self.token_types()?;
            return Ok(MappingChange::Drop { token_types, if_exists });
        }
        self.expect_word("alter")?;
        self.expect_word("mapping")?;
        if self.is_word("replace") {
            return self.replaced_dictionary(None);
        }
        let token_types = self.token_types()?;
        if self.is_word("replace") {
            return self.replaced_dictionary(Some(token_types));
        }
        self.expect_word("with")?;
        let dictionaries = self.text_search_names()?;
        Ok(MappingChange::Alter { token_types, dictionaries })
    }

    pub(super) fn alter_text_search(&mut self) -> SqlResult<Statement> {
        let kind = self.text_search_kind()?;
        let name = self.text_search_name()?;
        let command = if self.take_word("rename") {
            self.expect_word("to")?;
            let to = self.text_search_word()?;
            TextSearchCommand::Rename { kind, name, to }
        } else if self.take_word("owner") {
            self.expect_word("to")?;
            self.text_search_word()?;
            TextSearchCommand::Owner { kind, name }
        } else if self.is_word("set") && self.is_word_at(1, "schema") {
            return Err(self.not_supported("ALTER TEXT SEARCH ... SET SCHEMA"));
        } else {
            match kind {
                TextSearchKind::Dictionary => TextSearchCommand::AlterDictionary {
                    name,
                    options: self.text_search_options(false)?,
                },
                TextSearchKind::Configuration => TextSearchCommand::AlterMapping {
                    name,
                    change: self.mapping_change()?,
                },
            }
        };
        Ok(Statement::TextSearch(command))
    }
}
