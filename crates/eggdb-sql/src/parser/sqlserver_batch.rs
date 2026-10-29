use super::Parser;
use super::sqlserver::not_supported;
use crate::ast::{
    GeneratedTable, Name, ProcedureArgument, ProcedureCall, SqlServerStatement, Statement,
    TableReference,
};
use crate::bulk_load::InsertBulk;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;
use crate::types::SqlType;

const ACCEPTED_BULK_OPTIONS: [&str; 4] =
    ["tablock", "check_constraints", "fire_triggers", "keepidentity"];

fn unknown_bulk_option(shown: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        format!("'{shown}' is not a recognized BULK INSERT option."),
    )
    .with_sqlserver_code(195)
    .at(position)
}

fn named_after_positional(position: usize) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        "Must pass parameter number 2 and subsequent parameters as '@name = value'. After the form '@name = value' has been used, all subsequent parameters must be passed in the form '@name = value'.",
    )
    .with_sqlserver_code(119)
    .at(position)
}

impl Parser<'_> {
    pub(super) fn first_in_batch(&self) -> bool {
        self.tokens
            .get(..self.at)
            .unwrap_or_default()
            .iter()
            .all(|token| token.kind == TokenKind::Semicolon)
    }

    fn name_part(&mut self) -> SqlResult<Name> {
        let token = self.peek();
        let text = match &token.kind {
            TokenKind::Word(word) => word.as_ref().to_owned(),
            TokenKind::QuotedIdentifier(name) => name.to_lowercase(),
            _ => return Err(self.unexpected()),
        };
        self.at += 1;
        Ok(Name {
            text,
            position: token.start,
        })
    }

    fn procedure_name(&mut self) -> SqlResult<Name> {
        while self.take(&TokenKind::Dot) {}
        let mut name = self.name_part()?;
        while self.take(&TokenKind::Dot) {
            while self.take(&TokenKind::Dot) {}
            name = self.name_part()?;
        }
        Ok(name)
    }

    pub(super) fn sqlserver_execute(&mut self) -> SqlResult<Statement> {
        let position = self.position();
        self.at += 1;
        Ok(Statement::SqlServer(SqlServerStatement::Execute(
            self.procedure_call(position)?,
        )))
    }

    pub(super) fn sqlserver_bare_procedure(&mut self) -> SqlResult<Statement> {
        let position = self.position();
        Ok(Statement::SqlServer(SqlServerStatement::Execute(
            self.procedure_call(position)?,
        )))
    }

    fn procedure_call(&mut self, position: usize) -> SqlResult<ProcedureCall> {
        if self.peek_kind() == &TokenKind::LeftParen {
            return Err(not_supported("EXEC of a character string (dynamic SQL)").at(position));
        }
        if matches!(self.peek_kind(), TokenKind::Variable(_))
            && matches!(self.peek_kind_at(1), TokenKind::Operator("="))
        {
            return Err(not_supported("EXEC with a return status variable").at(position));
        }
        let procedure = self.procedure_name()?;
        let mut arguments: Vec<ProcedureArgument> = Vec::new();
        if self.at_procedure_end() {
            return Ok(ProcedureCall {
                procedure,
                arguments,
            });
        }
        loop {
            let argument_position = self.position();
            let name = match self.peek_kind() {
                TokenKind::Variable(name)
                    if !name.starts_with("@@")
                        && matches!(self.peek_kind_at(1), TokenKind::Operator("=")) =>
                {
                    let name = name.as_ref().to_owned();
                    self.at += 2;
                    Some(name)
                }
                _ => None,
            };
            if name.is_none() && arguments.iter().any(|argument| argument.name.is_some()) {
                return Err(named_after_positional(argument_position));
            }
            let value = if self.take_word("default") {
                None
            } else {
                Some(self.expression()?)
            };
            if !self.take_word("output") {
                self.take_word("out");
            }
            arguments.push(ProcedureArgument { name, value });
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        if self.is_word("with") {
            return Err(not_supported("EXEC ... WITH RECOMPILE or RESULT SETS").at(self.position()));
        }
        Ok(ProcedureCall {
            procedure,
            arguments,
        })
    }

    fn at_procedure_end(&self) -> bool {
        match self.peek_kind() {
            TokenKind::Semicolon | TokenKind::End => true,
            TokenKind::Word(word) => super::sqlserver::starts_statement(word) || word == "with",
            _ => false,
        }
    }

    pub(super) fn sqlserver_declare(&mut self) -> SqlResult<Option<Statement>> {
        let position = self.position();
        let TokenKind::Variable(name) = self.peek_kind_at(1) else {
            return Ok(None);
        };
        let table_at = if self.is_word_at(2, "as") { 3 } else { 2 };
        if name.starts_with("@@") || !self.is_word_at(table_at, "table") {
            return Ok(None);
        }
        let name = Name {
            text: name.as_ref().to_owned(),
            position: self
                .tokens
                .get(self.at + 1)
                .map_or(position, |token| token.start),
        };
        self.at += table_at + 1;
        self.expect(&TokenKind::LeftParen)?;
        let mut columns: Vec<(Name, SqlType)> = Vec::new();
        loop {
            let column = self.name_part()?;
            if columns
                .iter()
                .any(|(existing, _)| existing.text == column.text)
            {
                return Err(SqlError::new(
                    SqlState::DUPLICATE_COLUMN,
                    format!(
                        "Column names in each table must be unique. Column name '{}' in table '{}' is specified more than once.",
                        column.text, name.text
                    ),
                )
                .with_sqlserver_code(2705)
                .at(column.position));
            }
            let sql_type = self.sqlserver_type()?;
            if self.take_word("not") {
                self.expect_word("null")?;
            } else {
                self.take_word("null");
            }
            if !matches!(self.peek_kind(), TokenKind::Comma | TokenKind::RightParen) {
                return Err(not_supported(
                    "Constraints, defaults and identity in a table variable",
                )
                .at(self.position()));
            }
            columns.push((column, sql_type));
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(Some(Statement::SqlServer(
            SqlServerStatement::DeclareTable { name, columns },
        )))
    }

    pub(super) fn at_insert_into_variable(&self) -> bool {
        let at = if self.is_word_at(1, "into") { 2 } else { 1 };
        matches!(self.peek_kind_at(at), TokenKind::Variable(name) if !name.starts_with("@@"))
    }

    pub(super) fn sqlserver_insert_execute(&mut self) -> SqlResult<Statement> {
        let position = self.position();
        self.at += 1;
        self.take_word("into");
        let TokenKind::Variable(name) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let variable = Name {
            text: name.as_ref().to_owned(),
            position: self.position(),
        };
        self.at += 1;
        let columns = if self.peek_kind() == &TokenKind::LeftParen {
            self.at += 1;
            let mut names = Vec::new();
            loop {
                names.push(self.name_part()?);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
            Some(names)
        } else {
            None
        };
        if !(self.is_word("exec") || self.is_word("execute")) {
            return Err(
                not_supported("INSERT into a table variable other than INSERT ... EXEC")
                    .at(position),
            );
        }
        let call_position = self.position();
        self.at += 1;
        let call = self.procedure_call(call_position)?;
        Ok(Statement::SqlServer(SqlServerStatement::InsertExecute {
            variable,
            columns,
            call,
        }))
    }

    pub(super) fn sqlserver_insert_bulk(&mut self) -> SqlResult<Statement> {
        self.at += 2;
        let table = self.table_name()?;
        let mut columns: Vec<Name> = Vec::new();
        if self.take(&TokenKind::LeftParen) {
            loop {
                let column = self.name_part()?;
                if columns.iter().any(|existing| existing.text == column.text) {
                    return Err(SqlError::new(
                        SqlState::DUPLICATE_COLUMN,
                        format!(
                            "The column name '{}' is specified more than once in the column list of an INSERT BULK.",
                            column.text
                        ),
                    )
                    .with_sqlserver_code(264)
                    .at(column.position));
                }
                self.skip_bulk_column_type()?;
                columns.push(column);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
        }
        let mut keep_nulls = false;
        if self.take_word("with") {
            self.expect(&TokenKind::LeftParen)?;
            loop {
                let position = self.position();
                let shown = self
                    .text
                    .get(self.peek().start..self.peek().end)
                    .unwrap_or("")
                    .to_ascii_uppercase();
                let TokenKind::Word(option) = self.peek_kind() else {
                    return Err(self.unexpected());
                };
                let option = option.as_ref().to_owned();
                self.at += 1;
                match option.as_str() {
                    "keep_nulls" => keep_nulls = true,
                    accepted if ACCEPTED_BULK_OPTIONS.contains(&accepted) => {}
                    "rows_per_batch" | "kilobytes_per_batch" => {
                        if !self.take_operator("=") {
                            return Err(self.unexpected());
                        }
                        let TokenKind::Integer(_) = self.peek_kind() else {
                            return Err(self.unexpected());
                        };
                        self.at += 1;
                    }
                    "order" => self.skip_bulk_order()?,
                    "allow_encrypted_value_modifications" => {
                        return Err(not_supported(
                            "INSERT BULK ... WITH (ALLOW_ENCRYPTED_VALUE_MODIFICATIONS)",
                        )
                        .at(position));
                    }
                    _ => return Err(unknown_bulk_option(&shown, position)),
                }
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
        }
        Ok(Statement::SqlServer(SqlServerStatement::InsertBulk(
            InsertBulk {
                table,
                columns,
                keep_nulls,
            },
        )))
    }

    fn skip_bulk_column_type(&mut self) -> SqlResult<()> {
        if !matches!(
            self.peek_kind(),
            TokenKind::Word(_) | TokenKind::QuotedIdentifier(_)
        ) {
            return Err(self.unexpected());
        }
        self.at += 1;
        if self.peek_kind() == &TokenKind::LeftParen {
            let mut depth = 0usize;
            loop {
                match self.peek_kind() {
                    TokenKind::LeftParen => depth += 1,
                    TokenKind::RightParen => depth = depth.saturating_sub(1),
                    TokenKind::End => return Err(self.unexpected()),
                    _ => {}
                }
                self.at += 1;
                if depth == 0 {
                    break;
                }
            }
        }
        if self.take_word("collate") {
            self.name_part()?;
        }
        Ok(())
    }

    fn skip_bulk_order(&mut self) -> SqlResult<()> {
        self.expect(&TokenKind::LeftParen)?;
        loop {
            self.name_part()?;
            if !self.take_word("asc") {
                self.take_word("desc");
            }
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen)
    }

    pub(super) fn table_variable_reference(&mut self) -> SqlResult<TableReference> {
        let TokenKind::Variable(name) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let name = Name {
            text: name.as_ref().to_owned(),
            position: self.position(),
        };
        self.at += 1;
        let alias = if self.take_word("as") || self.at_bare_alias() {
            Some(self.name()?.text)
        } else {
            None
        };
        let generated = GeneratedTable::Variable(name.text.clone());
        Ok(TableReference {
            alias: Some(alias.unwrap_or_else(|| name.text.clone())),
            name,
            derived: None,
            generated: Some(Box::new(generated)),
        })
    }
}
