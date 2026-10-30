use super::Parser;
use super::sqlserver::not_supported;
use super::sqlserver_batch::already_declared;
use crate::ast::{CreateRoutine, DropRoutine, Name, RoutineTarget, Statement};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;
use crate::routines::{
    MAX_PARAMETERS, ParameterMode, ReturnType, RoutineDefinition, RoutineKind, RoutineLanguage,
    RoutineParameter, RoutineReturns, Volatility,
};
use crate::tsql::{Instruction, Program, ScalarVariable, TsqlReturns, TsqlRoutine};

const ROUTINE_OPTIONS: [&str; 5] = [
    "recompile",
    "encryption",
    "schemabinding",
    "native_compilation",
    "view_metadata",
];

fn not_first_in_batch(kind: RoutineKind, position: usize) -> SqlError {
    let shown = match kind {
        RoutineKind::Procedure => "CREATE/ALTER PROCEDURE",
        RoutineKind::Function => "CREATE FUNCTION",
    };
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        format!("'{shown}' must be the first statement in a query batch."),
    )
    .with_sqlserver_code(111)
    .at(position)
}

fn too_many_parameters(position: usize) -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!(
            "There are too many parameters in this CREATE statement. The maximum number is {MAX_PARAMETERS}."
        ),
    )
    .with_sqlserver_code(180)
    .at(position)
}

fn output_in_function(position: usize) -> SqlError {
    SqlError::new(
        SqlState::INVALID_FUNCTION_DEFINITION,
        "Cannot use the OUTPUT option in a DECLARE, CREATE AGGREGATE or CREATE FUNCTION statement.",
    )
    .with_sqlserver_code(181)
    .at(position)
}

fn not_table_valued(name: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::INVALID_FUNCTION_DEFINITION,
        format!(
            "The parameter \"{name}\" can not be declared READONLY since it is not a table-valued parameter."
        ),
    )
    .with_sqlserver_code(346)
    .at(position)
}

fn missing_final_return(position: usize) -> SqlError {
    SqlError::new(
        SqlState::INVALID_FUNCTION_DEFINITION,
        "The last statement included within a function must be a return statement.",
    )
    .with_sqlserver_code(455)
    .at(position)
}

fn generic_returns(kind: RoutineKind, returns: &TsqlReturns) -> RoutineReturns {
    match (kind, returns) {
        (RoutineKind::Procedure, _) | (_, TsqlReturns::Status) => RoutineReturns::Nothing,
        (_, TsqlReturns::Scalar(declared)) => {
            RoutineReturns::One(ReturnType::Type(declared.engine_type()))
        }
        (_, TsqlReturns::InlineTable) => RoutineReturns::Set(ReturnType::Record),
        (_, TsqlReturns::Table { columns, .. }) => RoutineReturns::Table(columns.clone()),
    }
}

fn parameter_variable(parameter: &RoutineParameter) -> Option<ScalarVariable> {
    Some(ScalarVariable::declared_as(
        parameter.name.clone()?,
        parameter.declared?,
    ))
}

fn stored_text(text: String, altered: bool) -> String {
    if !altered {
        return text;
    }
    let trimmed = text.trim_start();
    match trimmed.get(..5) {
        Some(keyword) if keyword.eq_ignore_ascii_case("alter") => {
            format!("CREATE{}", trimmed.get(5..).unwrap_or_default())
        }
        _ => text,
    }
}

impl Parser<'_> {
    pub(super) fn at_sqlserver_routine(&self) -> bool {
        let ahead = if self.is_word("create") && self.is_word_at(1, "or") {
            if !self.is_word_at(2, "alter") {
                return false;
            }
            3
        } else {
            1
        };
        self.is_word_at(ahead, "proc")
            || self.is_word_at(ahead, "procedure")
            || self.is_word_at(ahead, "function")
    }

    pub(super) fn sqlserver_routine(&mut self) -> SqlResult<Statement> {
        if self.is_word("drop") {
            return self.sqlserver_drop_routine();
        }
        self.sqlserver_create_routine()
    }

    fn sqlserver_routine_kind(&mut self) -> SqlResult<RoutineKind> {
        if self.take_word("function") {
            return Ok(RoutineKind::Function);
        }
        if self.take_word("proc") || self.take_word("procedure") {
            return Ok(RoutineKind::Procedure);
        }
        Err(self.unexpected())
    }

    fn sqlserver_create_routine(&mut self) -> SqlResult<Statement> {
        let start = self.peek().start;
        let position = self.position();
        let at_batch_start = self.first_in_batch();
        let (replace, must_exist) = if self.take_word("alter") {
            (true, true)
        } else {
            self.expect_word("create")?;
            if self.take_word("or") {
                self.expect_word("alter")?;
                (true, false)
            } else {
                (false, false)
            }
        };
        let kind = self.sqlserver_routine_kind()?;
        if !at_batch_start {
            return Err(not_first_in_batch(kind, position));
        }
        if matches!(self.peek_kind(), TokenKind::Word(word) if word.starts_with('#')) {
            return Err(not_supported("Temporary procedures and functions (#name)").at(self.position()));
        }
        let name = self.table_name()?;
        if self.peek_kind() == &TokenKind::Semicolon
            && matches!(self.peek_kind_at(1), TokenKind::Integer(_))
        {
            return Err(not_supported("Numbered procedures (name;number)").at(self.position()));
        }
        let parameters = self.sqlserver_routine_parameters(kind)?;
        let returns = match kind {
            RoutineKind::Procedure => TsqlReturns::Status,
            RoutineKind::Function => self.sqlserver_function_returns()?,
        };
        let strict = self.sqlserver_routine_options()?;
        if kind == RoutineKind::Procedure && self.take_word("for") {
            self.expect_word("replication")?;
        }
        if kind == RoutineKind::Procedure {
            self.expect_word("as")?;
        } else {
            self.take_word("as");
        }
        if self.is_word("external") {
            return Err(not_supported("CLR routines (AS EXTERNAL NAME)").at(self.position()));
        }
        let body_start = self.peek().start;
        let body_position = self.position();
        let body = match &returns {
            TsqlReturns::InlineTable => self.inline_function_body(&parameters)?,
            _ => {
                if kind == RoutineKind::Function && !self.is_word("begin") {
                    return Err(self.unexpected());
                }
                if self.at_end() {
                    return Err(self.unexpected());
                }
                let program = self.compile_routine_body(&parameters, &returns)?;
                if kind == RoutineKind::Function
                    && !matches!(program.instructions.last(), Some(Instruction::Return))
                {
                    return Err(missing_final_return(body_position));
                }
                self.slice(body_start, self.previous_end())
            }
        };
        let text = stored_text(self.slice(start, self.previous_end()), must_exist);
        let definition = RoutineDefinition {
            name: name.text.clone(),
            kind,
            dialect: Dialect::SqlServer,
            language: RoutineLanguage::Tsql,
            returns: generic_returns(kind, &returns),
            parameters,
            volatility: match kind {
                RoutineKind::Procedure => Volatility::Volatile,
                RoutineKind::Function => Volatility::Stable,
            },
            strict,
            security_definer: false,
            settings: Vec::new(),
            body,
            text,
            tsql: Some(Box::new(TsqlRoutine { returns })),
        };
        Ok(Statement::CreateRoutine(Box::new(CreateRoutine {
            name,
            replace,
            must_exist,
            definition,
            body_position: None,
        })))
    }

    fn sqlserver_routine_parameters(
        &mut self,
        kind: RoutineKind,
    ) -> SqlResult<Vec<RoutineParameter>> {
        let parenthesized = match kind {
            RoutineKind::Function => {
                self.expect(&TokenKind::LeftParen)?;
                true
            }
            RoutineKind::Procedure => self.take(&TokenKind::LeftParen),
        };
        let mut parameters: Vec<RoutineParameter> = Vec::new();
        let empty = if parenthesized {
            self.take(&TokenKind::RightParen)
        } else {
            !self.at_variable(0)
        };
        if empty {
            return Ok(parameters);
        }
        loop {
            if parameters.len() >= MAX_PARAMETERS {
                return Err(too_many_parameters(self.position()));
            }
            let parameter = self.sqlserver_routine_parameter(kind, &parameters)?;
            parameters.push(parameter);
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        if parenthesized {
            self.expect(&TokenKind::RightParen)?;
        }
        Ok(parameters)
    }

    fn sqlserver_routine_parameter(
        &mut self,
        kind: RoutineKind,
        earlier: &[RoutineParameter],
    ) -> SqlResult<RoutineParameter> {
        let position = self.position();
        let name = match self.peek_kind() {
            TokenKind::Variable(name) if !name.starts_with("@@") => name.as_ref().to_owned(),
            _ => return Err(self.unexpected()),
        };
        self.at += 1;
        if earlier
            .iter()
            .any(|parameter| parameter.name.as_deref() == Some(name.as_str()))
        {
            return Err(already_declared(&name, position));
        }
        self.take_word("as");
        let declared = self.sqlserver_declared_type(false)?;
        self.take_word("varying");
        if self.is_word("not") && self.is_word_at(1, "null") {
            self.at += 2;
        } else {
            self.take_word("null");
        }
        let default = if self.take_operator("=") {
            let start = self.peek().start;
            self.expression()?;
            Some(self.slice(start, self.previous_end()))
        } else {
            None
        };
        let mut mode = ParameterMode::In;
        let option_position = self.position();
        if self.take_word("output") || self.take_word("out") {
            if kind == RoutineKind::Function {
                return Err(output_in_function(option_position));
            }
            mode = ParameterMode::InOut;
        }
        if self.take_word("readonly") {
            return Err(not_table_valued(&name, option_position));
        }
        Ok(RoutineParameter {
            name: Some(name),
            mode,
            sql_type: declared.engine_type(),
            default,
            declared: Some(declared),
        })
    }

    fn sqlserver_function_returns(&mut self) -> SqlResult<TsqlReturns> {
        self.expect_word("returns")?;
        if let TokenKind::Variable(variable) = self.peek_kind() {
            let variable = variable.as_ref().to_owned();
            self.at += 1;
            self.expect_word("table")?;
            let columns = self
                .table_variable_columns(&variable)?
                .into_iter()
                .map(|(column, sql_type)| (column.text, sql_type))
                .collect();
            return Ok(TsqlReturns::Table { variable, columns });
        }
        if self.take_word("table") {
            return Ok(TsqlReturns::InlineTable);
        }
        Ok(TsqlReturns::Scalar(self.sqlserver_declared_type(false)?))
    }

    fn sqlserver_routine_options(&mut self) -> SqlResult<bool> {
        let mut strict = false;
        if !self.take_word("with") {
            return Ok(strict);
        }
        loop {
            if self.take_word("returns") {
                self.expect_word("null")?;
                self.expect_word("on")?;
                self.expect_word("null")?;
                self.expect_word("input")?;
                strict = true;
            } else if self.take_word("called") {
                self.expect_word("on")?;
                self.expect_word("null")?;
                self.expect_word("input")?;
            } else if self.take_word("execute") || self.take_word("exec") {
                self.expect_word("as")?;
                if matches!(
                    self.peek_kind(),
                    TokenKind::Word(_) | TokenKind::String(_) | TokenKind::QuotedIdentifier(_)
                ) {
                    self.at += 1;
                } else {
                    return Err(self.unexpected());
                }
            } else if self.take_word("inline") {
                if !self.take_operator("=") {
                    return Err(self.unexpected());
                }
                if !(self.take_word("on") || self.take_word("off")) {
                    return Err(self.unexpected());
                }
            } else if ROUTINE_OPTIONS.iter().any(|option| self.is_word(option)) {
                self.at += 1;
            } else {
                return Err(self.unexpected());
            }
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        Ok(strict)
    }

    fn inline_function_body(&mut self, parameters: &[RoutineParameter]) -> SqlResult<String> {
        self.expect_word("return")?;
        let outer = std::mem::take(&mut self.declared_variables);
        self.declared_variables = parameters.iter().filter_map(parameter_variable).collect();
        let parsed = self.inline_function_query();
        self.declared_variables = outer;
        let body = parsed?;
        self.skip_empty_statements();
        if !self.at_end() {
            return Err(self.unexpected());
        }
        Ok(body)
    }

    fn inline_function_query(&mut self) -> SqlResult<String> {
        let parenthesized = self.take(&TokenKind::LeftParen);
        let start = self.peek().start;
        let position = self.position();
        let statement = self.statement()?;
        if !matches!(statement, Statement::Select(_) | Statement::Query(_)) {
            return Err(SqlError::new(
                SqlState::INVALID_FUNCTION_DEFINITION,
                "The RETURN statement of an inline table-valued function must be a SELECT query.",
            )
            .with_sqlserver_code(156)
            .at(position));
        }
        let body = self.slice(start, self.previous_end());
        if parenthesized {
            self.expect(&TokenKind::RightParen)?;
        }
        Ok(body)
    }

    pub(crate) fn compile_routine_body(
        &mut self,
        parameters: &[RoutineParameter],
        returns: &TsqlReturns,
    ) -> SqlResult<Program> {
        let outer_variables = std::mem::take(&mut self.declared_variables);
        let outer_tables = std::mem::take(&mut self.declared_tables);
        let outer_returns = std::mem::replace(&mut self.returns_values, returns.returns_value());
        self.declared_variables = parameters.iter().filter_map(parameter_variable).collect();
        if let TsqlReturns::Table { variable, .. } = returns {
            self.declared_tables.push(variable.clone());
        }
        let compiled = self.compile_batch(true);
        self.declared_variables = outer_variables;
        self.declared_tables = outer_tables;
        self.returns_values = outer_returns;
        compiled
    }

    fn sqlserver_drop_routine(&mut self) -> SqlResult<Statement> {
        self.expect_word("drop")?;
        let kind = self.sqlserver_routine_kind()?;
        let if_exists = if self.is_word("if") && self.is_word_at(1, "exists") {
            self.at += 2;
            true
        } else {
            false
        };
        let mut targets = Vec::new();
        loop {
            let name: Name = self.table_name()?;
            targets.push(RoutineTarget {
                name,
                arguments: None,
            });
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        Ok(Statement::DropRoutine(DropRoutine {
            kind: Some(kind),
            targets,
            if_exists,
            cascade: false,
        }))
    }
}
