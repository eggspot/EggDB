use super::Parser;
use crate::ast::{
    AlterPackage, AlterRoutine, AlterRoutineAction, CreatePackage, CreateRoutine, DropPackage,
    DropRoutine, PackagePart, PlsqlBlock, RoutineTarget, Statement,
};
use crate::error::SqlResult;
use crate::lexer::TokenKind;
use crate::routines::{RoutineKind, RoutineLanguage};

const EDITIONING: [&str; 2] = ["editionable", "noneditionable"];

fn is_plsql_unit(statement: &Statement) -> bool {
    match statement {
        Statement::PlsqlBlock(_) | Statement::CreatePackage(_) => true,
        Statement::CreateRoutine(create) => create.definition.language == RoutineLanguage::PlSql,
        _ => false,
    }
}

impl<'t> Parser<'t> {
    pub(super) fn plsql_statement(&mut self) -> SqlResult<Option<Statement>> {
        if self.starts_plsql_label() {
            return self.plsql_block().map(Some);
        }
        let TokenKind::Word(word) = self.peek_kind() else {
            return Ok(None);
        };
        match word.as_ref() {
            "declare" | "begin" => self.plsql_block().map(Some),
            "create" => self.plsql_create(),
            "drop" if self.names_plsql_kind_at(1) => self.plsql_drop().map(Some),
            "alter" if self.names_plsql_kind_at(1) => self.plsql_alter().map(Some),
            "call" => self.call_routine().map(Some),
            _ => Ok(None),
        }
    }

    pub(super) fn ended_plsql_unit(&mut self, statement: &Statement) -> bool {
        let ended = self.at > 0
            && self
                .tokens
                .get(self.at - 1)
                .is_some_and(|token| token.kind == TokenKind::Semicolon);
        if !(ended && is_plsql_unit(statement)) {
            return false;
        }
        self.take_operator("/");
        true
    }

    fn starts_plsql_label(&self) -> bool {
        self.is_operator("<")
            && matches!(self.peek_kind_at(1), TokenKind::Operator(symbol) if *symbol == "<")
    }

    fn names_plsql_kind_at(&self, ahead: usize) -> bool {
        self.is_word_at(ahead, "procedure")
            || self.is_word_at(ahead, "function")
            || self.is_word_at(ahead, "package")
    }

    fn take_editioning(&mut self) -> bool {
        EDITIONING.iter().any(|word| self.take_word(word))
    }

    fn plsql_block(&mut self) -> SqlResult<Statement> {
        let position = self.position();
        let (program, code) = self.plsql_anonymous_block()?;
        Ok(Statement::PlsqlBlock(Box::new(PlsqlBlock {
            code,
            program,
            position,
        })))
    }

    fn plsql_create(&mut self) -> SqlResult<Option<Statement>> {
        let resume = self.at;
        let start = self.position();
        self.at += 1;
        let replace = if self.is_word("or") && self.is_word_at(1, "replace") {
            self.at += 2;
            true
        } else {
            false
        };
        self.take_editioning();
        if self.is_word("procedure") || self.is_word("function") {
            let body_position = self.position();
            let unit = self.plsql_subprogram()?;
            let mut definition = unit.definition;
            definition.text = self.slice(start, self.previous_end());
            return Ok(Some(Statement::CreateRoutine(Box::new(CreateRoutine {
                name: unit.name,
                replace,
                must_exist: false,
                definition,
                body_position: Some(body_position),
            }))));
        }
        if self.is_word("package") {
            let package = self.plsql_package(None)?;
            return Ok(Some(Statement::CreatePackage(Box::new(CreatePackage {
                replace,
                text: self.slice(start, self.previous_end()),
                package,
            }))));
        }
        self.at = resume;
        Ok(None)
    }

    fn plsql_routine_kind(&mut self) -> SqlResult<RoutineKind> {
        if self.take_word("procedure") {
            return Ok(RoutineKind::Procedure);
        }
        self.expect_word("function")?;
        Ok(RoutineKind::Function)
    }

    fn plsql_if_exists(&mut self) -> bool {
        if self.is_word("if") && self.is_word_at(1, "exists") {
            self.at += 2;
            return true;
        }
        false
    }

    fn plsql_drop(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        if self.take_word("package") {
            let body = self.take_word("body");
            let if_exists = self.plsql_if_exists();
            let name = self.table_name()?;
            return Ok(Statement::DropPackage(DropPackage {
                name,
                body,
                if_exists,
            }));
        }
        let kind = self.plsql_routine_kind()?;
        let if_exists = self.plsql_if_exists();
        let name = self.table_name()?;
        Ok(Statement::DropRoutine(DropRoutine {
            kind: Some(kind),
            targets: vec![RoutineTarget {
                name,
                arguments: None,
            }],
            if_exists,
            cascade: false,
        }))
    }

    fn plsql_alter(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        if self.take_word("package") {
            let name = self.table_name()?;
            let part = self.plsql_package_compilation()?;
            return Ok(Statement::AlterPackage(AlterPackage { name, part }));
        }
        let kind = self.plsql_routine_kind()?;
        let name = self.table_name()?;
        if !self.take_editioning() {
            self.expect_word("compile")?;
            self.take_word("debug");
            self.plsql_compiler_settings()?;
        }
        Ok(Statement::AlterRoutine(AlterRoutine {
            kind: Some(kind),
            target: RoutineTarget {
                name,
                arguments: None,
            },
            action: AlterRoutineAction::Accepted,
        }))
    }

    fn plsql_package_compilation(&mut self) -> SqlResult<PackagePart> {
        if self.take_editioning() {
            return Ok(PackagePart::Whole);
        }
        self.expect_word("compile")?;
        self.take_word("debug");
        let part = if self.take_word("body") {
            PackagePart::Body
        } else if self.take_word("specification") {
            PackagePart::Specification
        } else {
            self.take_word("package");
            PackagePart::Whole
        };
        self.plsql_compiler_settings()?;
        Ok(part)
    }

    fn plsql_compiler_settings(&mut self) -> SqlResult<()> {
        while matches!(self.peek_kind(), TokenKind::Word(_))
            && matches!(self.peek_kind_at(1), TokenKind::Operator(symbol) if *symbol == "=")
        {
            self.at += 2;
            match self.peek_kind() {
                TokenKind::Word(_)
                | TokenKind::QuotedIdentifier(_)
                | TokenKind::String(_)
                | TokenKind::Integer(_) => self.at += 1,
                _ => return Err(self.unexpected()),
            }
        }
        if self.take_word("reuse") {
            self.expect_word("settings")?;
        }
        Ok(())
    }
}
