use super::Parser;
use crate::ast::{Name, Statement};
use crate::error::SqlResult;
use crate::lexer::TokenKind;
use crate::roles::{Privilege, RoleOptions, RoleStatement};

impl Parser<'_> {
    pub(super) fn at_role_definition(&self) -> bool {
        self.is_word("role")
            || self.is_word("user")
            || (self.sqlserver() && self.is_word("login"))
    }

    pub(super) fn create_role(&mut self) -> SqlResult<Statement> {
        let user = !self.take_word("role");
        if user {
            self.at += 1;
        }
        let if_not_exists = self.if_not_exists()?;
        let name = self.role_name()?;
        let mut options = self.role_options()?;
        if user && options.login.is_none() {
            options.login = Some(true);
        }
        Ok(Statement::Role(RoleStatement::Create {
            name,
            options,
            if_not_exists,
        }))
    }

    pub(super) fn alter_role(&mut self) -> SqlResult<Statement> {
        self.at += 2;
        let name = self.role_name()?;
        let options = self.role_options()?;
        Ok(Statement::Role(RoleStatement::Alter { name, options }))
    }

    pub(super) fn drop_role(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        let if_exists = self.if_exists()?;
        let mut names = vec![self.role_name()?];
        while self.take(&TokenKind::Comma) {
            names.push(self.role_name()?);
        }
        if self.oracle() {
            self.take_word("cascade");
        }
        Ok(Statement::Role(RoleStatement::Drop { names, if_exists }))
    }

    fn role_name(&mut self) -> SqlResult<Name> {
        if let TokenKind::String(text) = self.peek_kind()
            && self.mysql()
        {
            let text = text.as_ref().to_owned();
            let position = self.position();
            self.at += 1;
            self.skip_mysql_host()?;
            return Ok(Name { text, position });
        }
        let name = self.name()?;
        if self.mysql() {
            self.skip_mysql_host()?;
        }
        Ok(name)
    }

    fn skip_mysql_host(&mut self) -> SqlResult<()> {
        if let TokenKind::Variable(name) = self.peek_kind()
            && !name.starts_with("@@")
        {
            self.at += 1;
            if name.len() == 1 {
                let _ = self.label()?;
            }
        } else if self.take_operator("@") {
            let _ = self.label()?;
        }
        Ok(())
    }

    fn password_text(&mut self) -> SqlResult<Option<String>> {
        if self.take_word("null") {
            return Ok(None);
        }
        match self.peek_kind() {
            TokenKind::String(text) => {
                let text = text.as_ref().to_owned();
                self.at += 1;
                Ok(Some(text))
            }
            TokenKind::Word(_) | TokenKind::QuotedIdentifier(_) if self.oracle() => {
                Ok(Some(self.label()?))
            }
            _ => Err(self.unexpected()),
        }
    }

    fn role_options(&mut self) -> SqlResult<RoleOptions> {
        let mut options = RoleOptions::default();
        self.take_word("with");
        loop {
            if self.take_word("login") {
                options.login = Some(true);
            } else if self.take_word("nologin") {
                options.login = Some(false);
            } else if self.take_word("superuser") {
                options.superuser = Some(true);
            } else if self.take_word("nosuperuser") {
                options.superuser = Some(false);
            } else if self.take_word("password") {
                self.take_operator("=");
                options.password = Some(self.password_text()?);
            } else if self.take_word("encrypted") {
                self.expect_word("password")?;
                options.password = Some(self.password_text()?);
            } else if self.take_word("identified") {
                self.expect_word("by")?;
                options.password = Some(self.password_text()?);
            } else if self.take_word("in") {
                self.expect_word("role")?;
                options.member_of.push(self.role_name()?.text);
                while self.take(&TokenKind::Comma) {
                    options.member_of.push(self.role_name()?.text);
                }
            } else if ["createdb", "nocreatedb", "createrole", "nocreaterole", "inherit", "noinherit",
                "replication", "noreplication", "bypassrls", "nobypassrls"]
                .iter()
                .any(|word| self.is_word(word))
            {
                self.at += 1;
            } else if self.take_word("connection") {
                self.expect_word("limit")?;
                self.at += 1;
            } else if self.take_word("valid") {
                self.expect_word("until")?;
                self.at += 1;
            } else if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        Ok(options)
    }

    pub(super) fn grant_or_revoke_privileges(&mut self, granting: bool) -> SqlResult<Statement> {
        self.at += 1;
        if !granting {
            self.take_word("grant");
            if self.is_word("option") {
                self.at += 1;
                self.expect_word("for")?;
            }
        }
        let mut privileges = Vec::new();
        let mut role_names = Vec::new();
        loop {
            if self.take_word("all") {
                self.take_word("privileges");
                privileges.extend(Privilege::ALL);
            } else if let Some(privilege) = self.privilege_word() {
                privileges.push(privilege);
                if self.peek_kind() == &TokenKind::LeftParen {
                    return Err(self.not_supported("column privileges"));
                }
            } else {
                role_names.push(self.role_name()?.text);
            }
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        if !role_names.is_empty() && !privileges.is_empty() {
            return Err(self.unexpected());
        }
        if role_names.is_empty() {
            self.expect_word("on")?;
            if self.take_word("all") {
                return Err(self.not_supported("GRANT ON ALL TABLES IN SCHEMA"));
            }
            if self.is_word("schema") || self.is_word("database") || self.is_word("sequence")
                || self.is_word("function") || self.is_word("procedure")
            {
                return Err(self.not_supported(&format!(
                    "privileges on a {}",
                    self.text.get(self.peek().start..self.peek().end).unwrap_or("")
                )));
            }
            self.take_word("table");
        }
        let mut tables = Vec::new();
        if role_names.is_empty() {
            tables.push(self.table_name()?);
            while self.take(&TokenKind::Comma) {
                tables.push(self.table_name()?);
            }
        }
        if granting {
            self.expect_word("to")?;
        } else if !self.take_word("from") {
            self.expect_word("to")?;
        }
        let mut grantees = vec![self.grantee()?];
        while self.take(&TokenKind::Comma) {
            grantees.push(self.grantee()?);
        }
        if granting && self.take_word("with") {
            if self.take_word("grant") || self.take_word("admin") {
                self.expect_word("option")?;
            }
        }
        if !granting {
            let _ = self.take_word("cascade") || self.take_word("restrict");
        }
        Ok(Statement::Role(if role_names.is_empty() {
            RoleStatement::Privileges {
                granting,
                privileges,
                tables,
                grantees,
            }
        } else {
            RoleStatement::Membership {
                granting,
                roles: role_names,
                members: grantees,
            }
        }))
    }

    fn privilege_word(&mut self) -> Option<Privilege> {
        let privilege = Privilege::ALL
            .into_iter()
            .find(|privilege| self.is_word(privilege.keyword()))?;
        self.at += 1;
        Some(privilege)
    }

    fn grantee(&mut self) -> SqlResult<String> {
        self.take_word("group");
        if self.take_word("public") {
            return Ok(crate::roles::PUBLIC.to_owned());
        }
        Ok(self.role_name()?.text)
    }
}
