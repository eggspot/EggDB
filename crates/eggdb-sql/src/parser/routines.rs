use super::Parser;
use crate::ast::{
    AlterRoutine, AlterRoutineAction, CallRoutine, CreateRoutine, DoBlock, DropRoutine,
    RoutineTarget, Statement,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;
use crate::routines::{
    MAX_PARAMETERS, ParameterMode, ReturnType, RoutineDefinition, RoutineKind, RoutineLanguage,
    RoutineParameter, RoutineReturns, Volatility,
};
use crate::types::SqlType;

const MAX_DROPPED: usize = 1000;
const PARAMETER_MODES: [(&str, ParameterMode); 4] = [
    ("in", ParameterMode::In),
    ("out", ParameterMode::Out),
    ("inout", ParameterMode::InOut),
    ("variadic", ParameterMode::Variadic),
];

fn too_many_parameters(position: usize) -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("functions cannot have more than {MAX_PARAMETERS} arguments"),
    )
    .at(position)
}

fn ends_list(kind: &TokenKind<'_>) -> bool {
    matches!(kind, TokenKind::Comma | TokenKind::RightParen)
}

struct RoutineAttributes {
    returns: Option<RoutineReturns>,
    language: Option<RoutineLanguage>,
    volatility: Volatility,
    strict: bool,
    security_definer: bool,
    settings: Vec<(String, String)>,
    body: Option<String>,
    body_position: Option<usize>,
}

impl<'t> Parser<'t> {
    pub(super) fn routine_statement(&mut self) -> SqlResult<Option<Statement>> {
        let TokenKind::Word(word) = self.peek_kind() else {
            return Ok(None);
        };
        match word.as_ref() {
            "create" if self.creates_routine() => self.create_routine().map(Some),
            "drop" if self.names_routine_kind_at(1) => self.drop_routine().map(Some),
            "alter" if self.names_routine_kind_at(1) => self.alter_routine().map(Some),
            "call" => self.call_routine().map(Some),
            "do" => self.do_block().map(Some),
            _ => Ok(None),
        }
    }

    fn call_routine(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        let name = self.table_name()?;
        self.expect(&TokenKind::LeftParen)?;
        let mut arguments = Vec::new();
        let mut argument_names: Vec<Option<String>> = Vec::new();
        if !self.take(&TokenKind::RightParen) {
            loop {
                if arguments.len() >= MAX_PARAMETERS {
                    return Err(too_many_parameters(self.position()));
                }
                let position = self.position();
                let named = self.named_argument_name()?;
                if named.is_none() && argument_names.iter().any(Option::is_some) {
                    return Err(SqlError::syntax(
                        "positional argument cannot follow named argument",
                        position,
                    ));
                }
                if let Some(parameter) = &named
                    && argument_names
                        .iter()
                        .flatten()
                        .any(|known| *known == parameter.text)
                {
                    return Err(SqlError::syntax(
                        format!("argument name \"{}\" used more than once", parameter.text),
                        parameter.position,
                    ));
                }
                argument_names.push(named.map(|parameter| parameter.text));
                arguments.push(self.expression()?);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
        }
        Ok(Statement::Call(CallRoutine {
            name,
            arguments,
            argument_names,
        }))
    }

    fn do_block(&mut self) -> SqlResult<Statement> {
        let start = self.position();
        self.at += 1;
        let mut language = None;
        let mut code = None;
        loop {
            if language.is_none() && self.take_word("language") {
                language = Some((self.position(), self.routine_language()?));
                continue;
            }
            if code.is_none() && matches!(self.peek_kind(), TokenKind::String(_)) {
                let position = self.position();
                let (text, _) = self.routine_source()?;
                code = Some((text, position));
                continue;
            }
            break;
        }
        if let Some((position, RoutineLanguage::Sql)) = language {
            return Err(SqlError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "language \"sql\" does not support inline code execution",
            )
            .at(position));
        }
        let Some((code, position)) = code else {
            return Err(SqlError::syntax("no inline code specified", start));
        };
        Ok(Statement::Do(DoBlock { code, position }))
    }

    fn names_routine_kind_at(&self, ahead: usize) -> bool {
        self.is_word_at(ahead, "function")
            || self.is_word_at(ahead, "procedure")
            || self.is_word_at(ahead, "routine")
    }

    fn creates_routine(&self) -> bool {
        let ahead = if self.is_word_at(1, "or") && self.is_word_at(2, "replace") {
            3
        } else {
            1
        };
        self.is_word_at(ahead, "function") || self.is_word_at(ahead, "procedure")
    }

    fn routine_kind_word(&mut self) -> SqlResult<Option<RoutineKind>> {
        if self.take_word("function") {
            return Ok(Some(RoutineKind::Function));
        }
        if self.take_word("procedure") {
            return Ok(Some(RoutineKind::Procedure));
        }
        self.expect_word("routine")?;
        Ok(None)
    }

    fn create_routine(&mut self) -> SqlResult<Statement> {
        let start = self.peek().start;
        self.at += 1;
        let replace = if self.take_word("or") {
            self.expect_word("replace")?;
            true
        } else {
            false
        };
        let kind = if self.take_word("function") {
            RoutineKind::Function
        } else {
            self.expect_word("procedure")?;
            RoutineKind::Procedure
        };
        let name = self.table_name()?;
        let parameters = self.routine_parameters()?;
        let attributes = self.routine_attributes(kind)?;
        let header_end = self.position();
        let Some(body) = attributes.body else {
            return Err(SqlError::new(
                SqlState::INVALID_FUNCTION_DEFINITION,
                "no function body specified",
            )
            .at(header_end));
        };
        let Some(language) = attributes.language else {
            return Err(SqlError::new(
                SqlState::INVALID_FUNCTION_DEFINITION,
                "no language specified",
            )
            .at(header_end));
        };
        let returns = match (kind, attributes.returns) {
            (RoutineKind::Procedure, _) => RoutineReturns::Nothing,
            (RoutineKind::Function, Some(returns)) => returns,
            (RoutineKind::Function, None) => self.returns_from_outputs(&parameters, header_end)?,
        };
        let definition = RoutineDefinition {
            name: name.text.clone(),
            kind,
            dialect: self.dialect,
            language,
            parameters,
            returns,
            volatility: attributes.volatility,
            strict: attributes.strict,
            security_definer: attributes.security_definer,
            settings: attributes.settings,
            body,
            text: self.slice(start, self.previous_end()),
            tsql: None,
        };
        Ok(Statement::CreateRoutine(Box::new(CreateRoutine {
            name,
            replace,
            must_exist: false,
            definition,
            body_position: attributes.body_position,
        })))
    }

    fn returns_from_outputs(
        &self,
        parameters: &[RoutineParameter],
        position: usize,
    ) -> SqlResult<RoutineReturns> {
        let outputs: Vec<SqlType> = parameters
            .iter()
            .filter(|parameter| parameter.mode.is_output())
            .map(|parameter| parameter.sql_type)
            .collect();
        match outputs.as_slice() {
            [] => Err(SqlError::new(
                SqlState::INVALID_FUNCTION_DEFINITION,
                "function result type must be specified",
            )
            .at(position)),
            [only] => Ok(RoutineReturns::One(ReturnType::Type(*only))),
            _ => Ok(RoutineReturns::One(ReturnType::Record)),
        }
    }

    fn routine_parameters(&mut self) -> SqlResult<Vec<RoutineParameter>> {
        self.expect(&TokenKind::LeftParen)?;
        let mut parameters = Vec::new();
        if self.take(&TokenKind::RightParen) {
            return Ok(parameters);
        }
        loop {
            if parameters.len() >= MAX_PARAMETERS {
                return Err(too_many_parameters(self.position()));
            }
            parameters.push(self.routine_parameter()?);
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(parameters)
    }

    fn parameter_mode(&mut self) -> ParameterMode {
        for (word, mode) in PARAMETER_MODES {
            if self.is_word(word) && !ends_list(self.peek_kind_at(1)) {
                self.at += 1;
                if mode == ParameterMode::In
                    && self.is_word("out")
                    && !ends_list(self.peek_kind_at(1))
                {
                    self.at += 1;
                    return ParameterMode::InOut;
                }
                return mode;
            }
        }
        ParameterMode::In
    }

    fn ends_parameter(&self) -> bool {
        ends_list(self.peek_kind()) || self.is_word("default") || self.is_operator("=")
    }

    fn routine_parameter(&mut self) -> SqlResult<RoutineParameter> {
        let mode = self.parameter_mode();
        let (name, sql_type) = self.named_parameter_type()?;
        let default = if self.take_word("default") || self.take_operator("=") {
            let start = self.peek().start;
            self.expression()?;
            Some(self.slice(start, self.previous_end()))
        } else {
            None
        };
        Ok(RoutineParameter {
            name,
            mode,
            sql_type,
            default,
            declared: None,
        })
    }

    fn named_parameter_type(&mut self) -> SqlResult<(Option<String>, SqlType)> {
        let at = self.at;
        let written = match self.peek_kind() {
            TokenKind::Word(word) => Some(word.as_ref().to_owned()),
            TokenKind::QuotedIdentifier(name) => Some(name.clone()),
            _ => None,
        };
        let next = self.peek_kind_at(1);
        let may_be_named = !ends_list(next)
            && !matches!(
                next,
                TokenKind::Dot | TokenKind::LeftParen | TokenKind::LeftBracket
            )
            && !self.is_word_at(1, "default")
            && !matches!(next, TokenKind::Operator(symbol) if *symbol == "=" || *symbol == "%");
        if let Some(name) = written
            && may_be_named
        {
            self.at += 1;
            if let Ok(sql_type) = self.sql_type()
                && self.ends_parameter()
            {
                return Ok((Some(name), sql_type));
            }
            self.at = at;
        }
        let position = self.position();
        let sql_type = self.sql_type()?;
        if self.is_operator("%") {
            return Err(SqlError::not_supported(
                "a %TYPE reference in a routine's parameter list is not supported yet",
            )
            .at(position));
        }
        Ok((None, sql_type))
    }

    fn routine_attributes(&mut self, kind: RoutineKind) -> SqlResult<RoutineAttributes> {
        let mut attributes = RoutineAttributes {
            returns: None,
            language: None,
            volatility: Volatility::Volatile,
            strict: false,
            security_definer: false,
            settings: Vec::new(),
            body: None,
            body_position: None,
        };
        loop {
            if kind == RoutineKind::Function && self.take_word("returns") {
                if self.take_word("null") {
                    for word in ["on", "null", "input"] {
                        self.expect_word(word)?;
                    }
                    attributes.strict = true;
                } else {
                    attributes.returns = Some(self.routine_returns()?);
                }
            } else if self.take_word("language") {
                attributes.language = Some(self.routine_language()?);
            } else if self.take_word("immutable") {
                attributes.volatility = Volatility::Immutable;
            } else if self.take_word("stable") {
                attributes.volatility = Volatility::Stable;
            } else if self.take_word("volatile") {
                attributes.volatility = Volatility::Volatile;
            } else if self.take_word("strict") {
                attributes.strict = true;
            } else if self.take_word("called") {
                for word in ["on", "null", "input"] {
                    self.expect_word(word)?;
                }
                attributes.strict = false;
            } else if self.is_word("external") || self.is_word("security") {
                self.take_word("external");
                self.expect_word("security")?;
                attributes.security_definer = if self.take_word("definer") {
                    true
                } else {
                    self.expect_word("invoker")?;
                    false
                };
            } else if self.take_word("leakproof") {
            } else if self.take_word("not") {
                self.expect_word("leakproof")?;
            } else if self.take_word("parallel") {
                if !(self.take_word("unsafe")
                    || self.take_word("restricted")
                    || self.take_word("safe"))
                {
                    return Err(self.unexpected());
                }
            } else if self.take_word("cost") || self.take_word("rows") {
                match self.peek_kind() {
                    TokenKind::Integer(_) | TokenKind::Decimal(_) => self.at += 1,
                    _ => return Err(self.unexpected()),
                }
            } else if self.take_word("support") {
                self.table_name()?;
            } else if self.take_word("set") {
                attributes.settings.push(self.routine_setting()?);
            } else if self.take_word("as") {
                let (body, body_position) = self.routine_source()?;
                attributes.body = Some(body);
                attributes.body_position = body_position;
            } else if self.is_word("return") {
                self.at += 1;
                let start = self.peek().start;
                self.expression()?;
                attributes.body = Some(format!(
                    "SELECT {}",
                    self.slice(start, self.previous_end())
                ));
                attributes.language.get_or_insert(RoutineLanguage::Sql);
            } else if self.is_word("begin") && self.is_word_at(1, "atomic") {
                self.at += 2;
                attributes.body = Some(self.atomic_body()?);
                attributes.language.get_or_insert(RoutineLanguage::Sql);
            } else if self.is_word("window") || self.is_word("transform") {
                return Err(self.not_supported("WINDOW and TRANSFORM routines"));
            } else {
                return Ok(attributes);
            }
        }
    }

    fn routine_setting(&mut self) -> SqlResult<(String, String)> {
        let position = self.position();
        let name = self.parameter_name()?;
        if self.take_word("from") {
            return Err(SqlError::not_supported(
                "SET ... FROM CURRENT on a routine is not supported yet",
            )
            .at(position));
        }
        if !(self.take_word("to") || self.take_operator("=")) {
            return Err(self.unexpected());
        }
        let value = self.setting_value()?.unwrap_or_default();
        Ok((name, value))
    }

    fn routine_source(&mut self) -> SqlResult<(String, Option<usize>)> {
        let token = self.peek();
        let TokenKind::String(text) = &token.kind else {
            return Err(self.unexpected());
        };
        let text = text.as_ref().to_owned();
        self.at += 1;
        if self.peek_kind() == &TokenKind::Comma {
            return Err(self.not_supported("a routine in a loadable object file (AS 'file', 'symbol')"));
        }
        let position = self.source_position(token.start, token.end, &text);
        Ok((text, position))
    }

    fn source_position(&self, start: usize, end: usize, source: &str) -> Option<usize> {
        let written = self.text.get(start..end)?;
        let quoting = written.len().checked_sub(source.len())?;
        let opening = quoting / 2;
        (quoting % 2 == 0 && written.get(opening..opening + source.len()) == Some(source))
            .then_some(start + opening)
    }

    fn atomic_body(&mut self) -> SqlResult<String> {
        let start = self.peek().start;
        let mut depth = 0usize;
        loop {
            match self.peek_kind() {
                TokenKind::End => return Err(self.unexpected()),
                TokenKind::Word(word) if word == "case" => depth += 1,
                TokenKind::Word(word) if word == "end" => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                _ => {}
            }
            self.at += 1;
        }
        let body = self.slice(start, self.peek().start).trim().to_owned();
        self.at += 1;
        Ok(body)
    }

    fn routine_language(&mut self) -> SqlResult<RoutineLanguage> {
        let position = self.position();
        let name = match self.peek_kind() {
            TokenKind::Word(word) => word.as_ref().to_owned(),
            TokenKind::String(text) => text.to_ascii_lowercase(),
            TokenKind::QuotedIdentifier(name) => name.clone(),
            _ => return Err(self.unexpected()),
        };
        self.at += 1;
        match name.as_str() {
            "sql" => Ok(RoutineLanguage::Sql),
            "plpgsql" => Ok(RoutineLanguage::PlPgSql),
            other => Err(SqlError::not_supported(format!(
                "routines in LANGUAGE {other} are not supported"
            ))
            .with_hint("EggDB runs routines written in SQL and PL/pgSQL.")
            .at(position)),
        }
    }

    fn routine_returns(&mut self) -> SqlResult<RoutineReturns> {
        if self.take_word("table") {
            self.expect(&TokenKind::LeftParen)?;
            let mut columns = Vec::new();
            loop {
                if columns.len() >= MAX_PARAMETERS {
                    return Err(too_many_parameters(self.position()));
                }
                let column = self.name()?;
                let sql_type = self.sql_type()?;
                columns.push((column.text, sql_type));
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
            return Ok(RoutineReturns::Table(columns));
        }
        let set = self.take_word("setof");
        let returned = self.return_type()?;
        Ok(if set {
            RoutineReturns::Set(returned)
        } else {
            RoutineReturns::One(returned)
        })
    }

    fn return_type(&mut self) -> SqlResult<ReturnType> {
        let special = match self.peek_kind() {
            TokenKind::Word(word) => match word.as_ref() {
                "void" => Some(ReturnType::Void),
                "trigger" => Some(ReturnType::Trigger),
                "record" => Some(ReturnType::Record),
                "event_trigger" => return Err(self.not_supported("event trigger functions")),
                _ => None,
            },
            _ => None,
        };
        if let Some(special) = special {
            self.at += 1;
            return Ok(special);
        }
        let at = self.at;
        match self.sql_type() {
            Ok(sql_type) => Ok(ReturnType::Type(sql_type)),
            Err(error) => {
                self.at = at;
                match self.table_name() {
                    Ok(name) => Ok(ReturnType::Relation(name.text)),
                    Err(_) => Err(error),
                }
            }
        }
    }

    fn routine_target(&mut self) -> SqlResult<RoutineTarget> {
        let name = self.table_name()?;
        let arguments = if self.peek_kind() == &TokenKind::LeftParen {
            Some(
                self.routine_parameters()?
                    .into_iter()
                    .filter(|parameter| parameter.mode.is_input())
                    .map(|parameter| parameter.sql_type)
                    .collect(),
            )
        } else {
            None
        };
        Ok(RoutineTarget { name, arguments })
    }

    fn drop_routine(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        let kind = self.routine_kind_word()?;
        let if_exists = if self.take_word("if") {
            self.expect_word("exists")?;
            true
        } else {
            false
        };
        let mut targets = vec![self.routine_target()?];
        while self.take(&TokenKind::Comma) {
            if targets.len() >= MAX_DROPPED {
                return Err(self.not_supported("dropping more than 1000 routines in one statement"));
            }
            targets.push(self.routine_target()?);
        }
        let cascade = self.take_word("cascade");
        if !cascade {
            self.take_word("restrict");
        }
        Ok(Statement::DropRoutine(DropRoutine {
            kind,
            targets,
            if_exists,
            cascade,
        }))
    }

    fn alter_routine(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        let kind = self.routine_kind_word()?;
        let target = self.routine_target()?;
        let action = if self.take_word("rename") {
            self.expect_word("to")?;
            AlterRoutineAction::RenameTo(self.name()?)
        } else if self.is_word("set") && self.is_word_at(1, "schema") {
            self.at += 2;
            AlterRoutineAction::SetSchema(self.name()?)
        } else if self.is_word("depends") || (self.is_word("no") && self.is_word_at(1, "depends")) {
            return Err(self.not_supported("ALTER ... DEPENDS ON EXTENSION"));
        } else {
            while !matches!(self.peek_kind(), TokenKind::Semicolon | TokenKind::End) {
                self.at += 1;
            }
            AlterRoutineAction::Accepted
        };
        Ok(Statement::AlterRoutine(AlterRoutine {
            kind,
            target,
            action,
        }))
    }
}
