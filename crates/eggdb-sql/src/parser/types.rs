use super::Parser;
use crate::ast::{
    AlterDomain, AlterDomainAction, AlterType, AlterTypeAction, CreateDomain, CreateType,
    DomainConstraintSpec, Name, Statement, TypeSpec,
};
use crate::error::{SqlError, SqlResult};
use crate::lexer::TokenKind;

const MAX_LISTED: usize = 100_000;

impl Parser<'_> {
    fn label_literal(&mut self) -> SqlResult<String> {
        let TokenKind::String(text) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let text = text.as_ref().to_owned();
        self.at += 1;
        Ok(text)
    }

    fn too_long_list(&self) -> SqlError {
        SqlError::new(
            crate::error::SqlState::STATEMENT_TOO_COMPLEX,
            format!("a list may hold at most {MAX_LISTED} items"),
        )
        .at(self.position())
    }

    pub(super) fn create_type(&mut self) -> SqlResult<Statement> {
        self.expect_word("type")?;
        let name = self.table_name()?;
        if !self.take_word("as") {
            return Err(self.not_supported("CREATE TYPE without AS (base and shell types)"));
        }
        let body = if self.take_word("enum") {
            self.expect(&TokenKind::LeftParen)?;
            let mut labels = Vec::new();
            if !self.take(&TokenKind::RightParen) {
                loop {
                    if labels.len() >= MAX_LISTED {
                        return Err(self.too_long_list());
                    }
                    labels.push(self.label_literal()?);
                    if !self.take(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(&TokenKind::RightParen)?;
            }
            TypeSpec::Enum(labels)
        } else if self.take_word("range") {
            self.range_options()?
        } else if self.peek_kind() == &TokenKind::LeftParen {
            self.at += 1;
            let mut attributes = Vec::new();
            if !self.take(&TokenKind::RightParen) {
                loop {
                    if attributes.len() >= MAX_LISTED {
                        return Err(self.too_long_list());
                    }
                    let attribute = self.name()?;
                    let sql_type = self.sql_type()?;
                    if self.take_word("collate") {
                        self.collation_name()?;
                    }
                    attributes.push((attribute, sql_type));
                    if !self.take(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(&TokenKind::RightParen)?;
            }
            TypeSpec::Composite(attributes)
        } else {
            return Err(self.unexpected());
        };
        Ok(Statement::CreateType(Box::new(CreateType { name, body })))
    }

    fn collation_name(&mut self) -> SqlResult<()> {
        match self.peek_kind() {
            TokenKind::Word(_) | TokenKind::QuotedIdentifier(_) => {
                self.at += 1;
                if self.take(&TokenKind::Dot) {
                    self.name()?;
                }
                Ok(())
            }
            _ => Err(self.unexpected()),
        }
    }

    fn skip_option_value(&mut self) -> SqlResult<()> {
        let mut depth = 0usize;
        loop {
            match self.peek_kind() {
                TokenKind::End => return Err(self.unexpected()),
                TokenKind::LeftParen => depth += 1,
                TokenKind::RightParen if depth == 0 => return Ok(()),
                TokenKind::RightParen => depth -= 1,
                TokenKind::Comma if depth == 0 => return Ok(()),
                _ => {}
            }
            self.at += 1;
        }
    }

    fn range_options(&mut self) -> SqlResult<TypeSpec> {
        self.expect(&TokenKind::LeftParen)?;
        let mut subtype = None;
        loop {
            let position = self.position();
            let option = match self.peek_kind() {
                TokenKind::Word(word) => word.as_ref().to_owned(),
                _ => return Err(self.unexpected()),
            };
            self.at += 1;
            if !self.take_operator("=") {
                return Err(self.unexpected());
            }
            match option.as_str() {
                "subtype" => {
                    let position = self.position();
                    subtype = Some((self.sql_type()?, position));
                }
                "subtype_diff" | "multirange_type_name" | "collation" | "subtype_opclass" => {
                    self.skip_option_value()?;
                }
                "canonical" => {
                    return Err(SqlError::not_supported(
                        "a range type with a CANONICAL function is not supported yet",
                    )
                    .at(position));
                }
                _ => {
                    return Err(SqlError::syntax(
                        format!("type attribute \"{option}\" not recognized"),
                        position,
                    ));
                }
            }
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen)?;
        let Some((subtype, position)) = subtype else {
            return Err(SqlError::syntax(
                "type attribute \"subtype\" is required",
                self.position(),
            ));
        };
        Ok(TypeSpec::Range { subtype, position })
    }

    fn domain_check(&mut self, name: Option<Name>) -> SqlResult<DomainConstraintSpec> {
        let position = self.position();
        self.expect_word("check")?;
        self.expect(&TokenKind::LeftParen)?;
        let start = self.position();
        let expr = self.expression()?;
        let end = self.tokens[self.at.saturating_sub(1)].end;
        self.expect(&TokenKind::RightParen)?;
        let text = self.text.get(start..end).unwrap_or("").to_owned();
        Ok(DomainConstraintSpec {
            name,
            expr,
            text,
            position,
        })
    }

    fn written_default(&mut self) -> SqlResult<(crate::ast::Expr, String)> {
        let start = self.position();
        let expr = self.binary_operand()?;
        let end = self.tokens[self.at.saturating_sub(1)].end;
        Ok((expr, self.text.get(start..end).unwrap_or("").to_owned()))
    }

    pub(super) fn create_domain(&mut self) -> SqlResult<Statement> {
        self.expect_word("domain")?;
        let name = self.table_name()?;
        self.take_word("as");
        let base = self.sql_type()?;
        let mut create = CreateDomain {
            name,
            base,
            default: None,
            not_null: false,
            checks: Vec::new(),
        };
        loop {
            if self.take_word("collate") {
                self.collation_name()?;
                continue;
            }
            if self.take_word("default") {
                create.default = Some(self.written_default()?);
                continue;
            }
            let constraint = if self.take_word("constraint") {
                Some(self.name()?)
            } else {
                None
            };
            if self.take_word("not") {
                self.expect_word("null")?;
                create.not_null = true;
            } else if self.take_word("null") {
                create.not_null = false;
            } else if self.is_word("check") {
                if create.checks.len() >= MAX_LISTED {
                    return Err(self.too_long_list());
                }
                let check = self.domain_check(constraint)?;
                if self.is_word("not") && self.is_word_at(1, "valid") {
                    return Err(SqlError::syntax(
                        "check constraints for domains cannot be marked NOT VALID",
                        self.position(),
                    ));
                }
                create.checks.push(check);
            } else if constraint.is_some() {
                return Err(self.unexpected());
            } else {
                break;
            }
        }
        Ok(Statement::CreateDomain(Box::new(create)))
    }

    pub(super) fn drop_type(&mut self) -> SqlResult<Statement> {
        let domain = self.take_word("domain");
        if !domain {
            self.expect_word("type")?;
        }
        let if_exists = if self.take_word("if") {
            self.expect_word("exists")?;
            true
        } else {
            false
        };
        let mut names = vec![self.table_name()?];
        while self.take(&TokenKind::Comma) {
            if names.len() >= MAX_LISTED {
                return Err(self.too_long_list());
            }
            names.push(self.table_name()?);
        }
        let cascade = self.take_word("cascade");
        if !cascade {
            self.take_word("restrict");
        }
        Ok(Statement::DropType {
            names,
            if_exists,
            cascade,
            domain,
        })
    }

    fn owner_or_schema(&mut self) -> SqlResult<Option<AlterTypeAction>> {
        if self.take_word("owner") {
            self.expect_word("to")?;
            self.label()?;
            return Ok(Some(AlterTypeAction::Accepted));
        }
        if self.is_word("set") && self.is_word_at(1, "schema") {
            self.at += 2;
            let schema = self.name()?;
            return Ok(Some(AlterTypeAction::SetSchema(
                crate::schemas::schema_named(&schema.text, schema.position)?,
            )));
        }
        if self.is_word("rename") && self.is_word_at(1, "to") {
            self.at += 2;
            return Ok(Some(AlterTypeAction::RenameTo(self.name()?)));
        }
        Ok(None)
    }

    pub(super) fn alter_type(&mut self) -> SqlResult<Statement> {
        self.expect_word("type")?;
        let name = self.table_name()?;
        if let Some(action) = self.owner_or_schema()? {
            return Ok(Statement::AlterType(Box::new(AlterType { name, action })));
        }
        let action = if self.take_word("add") {
            if self.is_word("attribute") {
                return Err(self.not_supported("ALTER TYPE ... ADD ATTRIBUTE"));
            }
            self.expect_word("value")?;
            let if_not_exists = if self.take_word("if") {
                self.expect_word("not")?;
                self.expect_word("exists")?;
                true
            } else {
                false
            };
            let label = self.label_literal()?;
            let neighbor = if self.take_word("before") {
                Some((self.label_literal()?, true))
            } else if self.take_word("after") {
                Some((self.label_literal()?, false))
            } else {
                None
            };
            AlterTypeAction::AddValue {
                label,
                if_not_exists,
                neighbor,
            }
        } else if self.is_word("rename") && self.is_word_at(1, "value") {
            self.at += 2;
            let from = self.label_literal()?;
            self.expect_word("to")?;
            let to = self.label_literal()?;
            AlterTypeAction::RenameValue { from, to }
        } else if self.is_word("rename")
            || self.is_word("drop")
            || self.is_word("alter")
            || self.is_word("set")
        {
            return Err(self.not_supported(
                "ALTER TYPE other than ADD VALUE, RENAME VALUE, RENAME TO, OWNER TO and SET SCHEMA",
            ));
        } else {
            return Err(self.unexpected());
        };
        Ok(Statement::AlterType(Box::new(AlterType { name, action })))
    }

    pub(super) fn alter_domain(&mut self) -> SqlResult<Statement> {
        self.expect_word("domain")?;
        let name = self.table_name()?;
        if let Some(action) = self.owner_or_schema()? {
            let action = match action {
                AlterTypeAction::SetSchema(schema) => AlterDomainAction::SetSchema(schema),
                AlterTypeAction::RenameTo(to) => AlterDomainAction::RenameTo(to),
                _ => AlterDomainAction::Accepted,
            };
            return Ok(Statement::AlterDomain(Box::new(AlterDomain { name, action })));
        }
        let action = if self.take_word("set") {
            if self.take_word("default") {
                AlterDomainAction::SetDefault(Some(self.written_default()?))
            } else {
                self.expect_word("not")?;
                self.expect_word("null")?;
                AlterDomainAction::SetNotNull(true)
            }
        } else if self.take_word("drop") {
            if self.take_word("default") {
                AlterDomainAction::SetDefault(None)
            } else if self.take_word("not") {
                self.expect_word("null")?;
                AlterDomainAction::SetNotNull(false)
            } else {
                self.expect_word("constraint")?;
                let if_exists = if self.take_word("if") {
                    self.expect_word("exists")?;
                    true
                } else {
                    false
                };
                let constraint = self.name()?;
                if !self.take_word("cascade") {
                    self.take_word("restrict");
                }
                AlterDomainAction::DropConstraint {
                    name: constraint,
                    if_exists,
                }
            }
        } else if self.take_word("add") {
            let constraint = if self.take_word("constraint") {
                Some(self.name()?)
            } else {
                None
            };
            if self.is_word("not") && self.is_word_at(1, "null") {
                self.at += 2;
                AlterDomainAction::SetNotNull(true)
            } else {
                let spec = self.domain_check(constraint)?;
                let not_valid = if self.is_word("not") && self.is_word_at(1, "valid") {
                    self.at += 2;
                    true
                } else {
                    false
                };
                AlterDomainAction::AddCheck { spec, not_valid }
            }
        } else if self.take_word("validate") {
            self.expect_word("constraint")?;
            AlterDomainAction::ValidateConstraint(self.name()?)
        } else if self.is_word("rename") && self.is_word_at(1, "constraint") {
            self.at += 2;
            let from = self.name()?;
            self.expect_word("to")?;
            let to = self.name()?;
            AlterDomainAction::RenameConstraint { from, to }
        } else {
            return Err(self.unexpected());
        };
        Ok(Statement::AlterDomain(Box::new(AlterDomain { name, action })))
    }

    pub(super) fn set_constraints(&mut self) -> SqlResult<Statement> {
        self.expect_word("constraints")?;
        let names = if self.take_word("all") {
            None
        } else {
            let mut names = vec![self.table_name()?];
            while self.take(&TokenKind::Comma) {
                if names.len() >= MAX_LISTED {
                    return Err(self.too_long_list());
                }
                names.push(self.table_name()?);
            }
            Some(names)
        };
        let deferred = if self.take_word("deferred") {
            true
        } else {
            self.expect_word("immediate")?;
            false
        };
        Ok(Statement::SetConstraints { names, deferred })
    }
}
