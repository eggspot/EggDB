use super::sqlserver::{ends_a_block, not_supported, starts_statement};
use super::{Parameters, Parser};
use crate::ast::{
    AssignmentTarget, BinaryOperator, DynamicSql, GeneratedTable, Name, ProcedureArgument,
    ProcedureCall, RaiseError, SelectItem, SetExpr, SqlServerStatement, Statement, TableReference,
    ThrowArguments, VariableDeclaration, VariableSource,
};
use crate::bulk_load::InsertBulk;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;
use crate::types::SqlType;

const MAX_RAISERROR_ARGUMENTS: usize = 20;

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

fn already_declared(name: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::DUPLICATE_OBJECT,
        format!(
            "The variable name '{name}' has already been declared. Variable names must be unique within a query batch or stored procedure."
        ),
    )
    .with_sqlserver_code(134)
    .at(position)
}

fn mixed_assignment(position: usize) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        "A SELECT statement that assigns a value to a variable must not be combined with data-retrieval operations.",
    )
    .with_sqlserver_code(141)
    .at(position)
}

fn dynamic_sql(call: ProcedureCall, position: usize) -> SqlResult<Statement> {
    let mut arguments = call.arguments.into_iter();
    let Some(ProcedureArgument {
        value: Some(statement),
        ..
    }) = arguments.next()
    else {
        return Err(SqlError::new(
            SqlState::UNDEFINED_PARAMETER,
            "Procedure or function 'sp_executesql' expects parameter '@statement', which was not supplied.",
        )
        .with_sqlserver_code(201)
        .at(position));
    };
    let declarations = arguments.next().and_then(|argument| argument.value);
    Ok(Statement::SqlServer(SqlServerStatement::ExecuteDynamic(
        DynamicSql {
            statement,
            declarations,
            arguments: arguments.collect(),
            position,
        },
    )))
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
        if self.take(&TokenKind::LeftParen) {
            let statement = self.expression()?;
            self.expect(&TokenKind::RightParen)?;
            if self.is_word("as") || self.is_word("at") {
                return Err(
                    not_supported("EXEC (...) AS USER, AS LOGIN or AT").at(self.position())
                );
            }
            return Ok(Statement::SqlServer(SqlServerStatement::ExecuteDynamic(
                DynamicSql {
                    statement,
                    declarations: None,
                    arguments: Vec::new(),
                    position,
                },
            )));
        }
        let call = self.procedure_call(position)?;
        if call.procedure.text == "sp_executesql" {
            return dynamic_sql(call, position);
        }
        Ok(Statement::SqlServer(SqlServerStatement::Execute(call)))
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
            let output_variable = match self.peek_kind() {
                TokenKind::Variable(variable)
                    if !variable.starts_with("@@")
                        && (self.is_word_at(1, "output") || self.is_word_at(1, "out")) =>
                {
                    Some(variable.as_ref().to_owned())
                }
                _ => None,
            };
            let value = if self.take_word("default") {
                None
            } else {
                Some(self.expression()?)
            };
            if !self.take_word("output") {
                self.take_word("out");
            }
            arguments.push(ProcedureArgument {
                name,
                value,
                output_variable,
            });
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
            TokenKind::Word(word) => starts_statement(word) || ends_a_block(word) || word == "with",
            _ => false,
        }
    }

    pub(super) fn at_variable(&self, ahead: usize) -> bool {
        matches!(self.peek_kind_at(ahead), TokenKind::Variable(name) if !name.starts_with("@@"))
    }

    fn variable_name(&mut self) -> SqlResult<Name> {
        let TokenKind::Variable(name) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        if name.starts_with("@@") {
            return Err(self.unexpected());
        }
        let name = Name {
            text: name.as_ref().to_owned(),
            position: self.position(),
        };
        self.at += 1;
        Ok(name)
    }

    fn check_new_variable(&self, name: &Name) -> SqlResult<()> {
        let parameter = match self.parameters {
            Parameters::Named(parameters) => parameters
                .iter()
                .any(|parameter| parameter.name.eq_ignore_ascii_case(&name.text)),
            _ => false,
        };
        if parameter
            || self
                .declared_variables
                .iter()
                .any(|variable| variable.name == name.text)
            || self.declared_tables.iter().any(|table| *table == name.text)
        {
            return Err(already_declared(&name.text, name.position));
        }
        Ok(())
    }

    pub(super) fn sqlserver_declare(&mut self) -> SqlResult<Statement> {
        let position = self.position();
        self.at += 1;
        if self.at_variable(0) {
            let table_at = if self.is_word_at(1, "as") { 2 } else { 1 };
            if self.is_word_at(table_at, "table") {
                let name = self.variable_name()?;
                self.at += table_at;
                return self.declare_table(name);
            }
        } else if matches!(self.peek_kind(), TokenKind::Word(_)) && self.is_word_at(1, "cursor")
        {
            return Err(not_supported("DECLARE CURSOR").at(position));
        }
        let mut declarations: Vec<VariableDeclaration> = Vec::new();
        loop {
            let name = self.variable_name()?;
            self.take_word("as");
            if self.is_word("cursor") {
                return Err(not_supported("DECLARE CURSOR").at(self.position()));
            }
            if self.is_word("table") {
                return Err(self.unexpected());
            }
            let declared = self.sqlserver_declared_type(false)?;
            let read_before = self.variables_read.len();
            let initial = if self.take_operator("=") {
                let initial_position = self.position();
                let initial = self.expression()?;
                let reads_own = self
                    .variables_read
                    .get(read_before..)
                    .unwrap_or_default()
                    .iter()
                    .any(|read| {
                        declarations
                            .iter()
                            .any(|declaration| declaration.name.text == *read)
                    });
                if reads_own {
                    return Err(not_supported(
                        "A DECLARE whose initial value reads a variable the same DECLARE declares",
                    )
                    .at(initial_position));
                }
                Some(initial)
            } else {
                None
            };
            self.check_new_variable(&name)?;
            self.declared_variables
                .push(crate::tsql::ScalarVariable::declared_as(name.text.clone(), declared));
            declarations.push(VariableDeclaration {
                name,
                declared,
                initial,
            });
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        Ok(Statement::SqlServer(SqlServerStatement::Declare(
            declarations,
        )))
    }

    fn declare_table(&mut self, name: Name) -> SqlResult<Statement> {
        self.check_new_variable(&name)?;
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
        self.declared_tables.push(name.text.clone());
        Ok(Statement::SqlServer(SqlServerStatement::DeclareTable {
            name,
            columns,
        }))
    }

    pub(super) fn at_insert_into_variable(&self) -> bool {
        let at = if self.is_word_at(1, "into") { 2 } else { 1 };
        self.at_variable(at)
    }

    pub(super) fn sqlserver_insert_variable(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        self.take_word("into");
        let variable = self.variable_name()?;
        let columns = if self.peek_kind() == &TokenKind::LeftParen
            && !self.is_word_at(1, "select")
            && !self.is_word_at(1, "with")
        {
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
        let position = self.position();
        let source = if self.is_word("exec") || self.is_word("execute") {
            self.at += 1;
            VariableSource::Execute(self.procedure_call(position)?)
        } else if self.take_word("values") {
            let mut rows = Vec::new();
            loop {
                self.expect(&TokenKind::LeftParen)?;
                let mut row = Vec::new();
                loop {
                    row.push(self.insert_value()?);
                    if !self.take(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(&TokenKind::RightParen)?;
                rows.push(row);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            VariableSource::Values(rows)
        } else if self.is_word("select")
            || self.is_word("with")
            || self.peek_kind() == &TokenKind::LeftParen
        {
            VariableSource::Query(Box::new(self.query_statement()?))
        } else if self.is_word("default") {
            return Err(not_supported("DEFAULT VALUES into a table variable").at(position));
        } else if self.is_word("output") {
            return Err(not_supported("OUTPUT in an INSERT into a table variable").at(position));
        } else {
            return Err(self.unexpected());
        };
        Ok(Statement::SqlServer(SqlServerStatement::InsertVariable {
            variable,
            columns,
            source,
        }))
    }

    pub(super) fn sqlserver_update_variable(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        let variable = self.variable_name()?;
        self.expect_word("set")?;
        let mut assignments = Vec::new();
        loop {
            let position = self.position();
            if matches!(self.peek_kind(), TokenKind::Variable(_)) {
                return Err(
                    not_supported("Assigning a variable in an UPDATE of a table variable")
                        .at(position),
                );
            }
            let column = self.name_part()?;
            if !self.take_operator("=") {
                return Err(self.unexpected());
            }
            if self.is_word("default") {
                return Err(
                    not_supported("DEFAULT in an UPDATE of a table variable").at(self.position())
                );
            }
            assignments.push((column, self.expression()?));
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.refuse_variable_dml_clauses("UPDATE")?;
        let filter = self.filter()?;
        Ok(Statement::SqlServer(SqlServerStatement::UpdateVariable {
            variable,
            assignments,
            filter,
        }))
    }

    pub(super) fn sqlserver_delete_variable(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        self.take_word("from");
        let variable = self.variable_name()?;
        self.refuse_variable_dml_clauses("DELETE")?;
        let filter = self.filter()?;
        Ok(Statement::SqlServer(SqlServerStatement::DeleteVariable {
            variable,
            filter,
        }))
    }

    fn refuse_variable_dml_clauses(&self, statement: &str) -> SqlResult<()> {
        let clause = if self.is_word("from") {
            "FROM"
        } else if self.is_word("output") {
            "OUTPUT"
        } else {
            return Ok(());
        };
        Err(not_supported(&format!("{statement} of a table variable with {clause}")).at(self.position()))
    }

    fn assignment_operator(&mut self) -> SqlResult<Option<BinaryOperator>> {
        if self.take_operator("=") {
            return Ok(None);
        }
        let operator = match self.peek_kind() {
            TokenKind::Operator("+") => BinaryOperator::Add,
            TokenKind::Operator("-") => BinaryOperator::Subtract,
            TokenKind::Operator("*") => BinaryOperator::Multiply,
            TokenKind::Operator("/") => BinaryOperator::Divide,
            TokenKind::Operator("%") => BinaryOperator::Modulo,
            _ => return Err(self.unexpected()),
        };
        let end = self.peek().end;
        let adjacent = self
            .tokens
            .get(self.at + 1)
            .is_some_and(|next| next.kind == TokenKind::Operator("=") && next.start == end);
        if !adjacent {
            return Err(self.unexpected());
        }
        self.at += 2;
        Ok(Some(operator))
    }

    pub(super) fn sqlserver_set_variable(&mut self) -> SqlResult<Statement> {
        let variable = self.variable_name()?;
        let operator = self.assignment_operator()?;
        let current = self.variable_reference(&variable.text, variable.position)?;
        let value = self.expression()?;
        let value = match operator {
            Some(operator) => Self::binary(current, operator, value),
            None => value,
        };
        Ok(Statement::SqlServer(SqlServerStatement::SetVariable {
            variable,
            value,
        }))
    }

    pub(super) fn select_assigns_variables(&self) -> bool {
        let mut ahead = 1;
        if self.is_word_at(ahead, "distinct") || self.is_word_at(ahead, "all") {
            ahead += 1;
        }
        if self.is_word_at(ahead, "top") {
            ahead += 1;
            if self.peek_kind_at(ahead) == &TokenKind::LeftParen {
                let mut depth = 0usize;
                loop {
                    match self.peek_kind_at(ahead) {
                        TokenKind::LeftParen => depth += 1,
                        TokenKind::RightParen => depth = depth.saturating_sub(1),
                        TokenKind::End => return false,
                        _ => {}
                    }
                    ahead += 1;
                    if depth == 0 {
                        break;
                    }
                }
            } else {
                ahead += 1;
            }
            if self.is_word_at(ahead, "percent") {
                ahead += 1;
            }
            if self.is_word_at(ahead, "with") && self.is_word_at(ahead + 1, "ties") {
                ahead += 2;
            }
        }
        self.at_variable(ahead)
            && (matches!(self.peek_kind_at(ahead + 1), TokenKind::Operator("="))
                || (matches!(
                    self.peek_kind_at(ahead + 1),
                    TokenKind::Operator("+" | "-" | "*" | "/" | "%")
                ) && matches!(self.peek_kind_at(ahead + 2), TokenKind::Operator("="))))
    }

    pub(super) fn sqlserver_select_assignment(&mut self) -> SqlResult<Statement> {
        let position = self.position();
        let outer = self.select_targets.replace(Vec::new());
        let parsed = self.query_statement();
        let targets = std::mem::replace(&mut self.select_targets, outer).unwrap_or_default();
        let query = parsed?;
        let (items, has_from) = match &query {
            Statement::Select(select) => (select.items.len(), select.from.is_some()),
            Statement::Query(query) => match &query.body {
                SetExpr::Select(select) => (select.items.len(), select.from.is_some()),
                _ => {
                    return Err(not_supported(
                        "A SELECT that assigns variables and combines queries with UNION, EXCEPT or INTERSECT",
                    )
                    .at(position));
                }
            },
            _ => return Err(mixed_assignment(position)),
        };
        if items != targets.len() {
            return Err(mixed_assignment(position));
        }
        for (index, (_, reads)) in targets.iter().enumerate() {
            let assigned_before = targets
                .get(..index)
                .unwrap_or_default()
                .iter()
                .any(|(target, _)| reads.contains(&target.variable.text));
            let assigned_per_row = has_from
                && targets
                    .iter()
                    .any(|(target, _)| reads.contains(&target.variable.text));
            if assigned_before || assigned_per_row {
                return Err(not_supported(
                    "A SELECT whose value reads a variable the same SELECT assigns (use SELECT @v += expression, or SET @v = (SELECT ...))",
                )
                .at(position));
            }
        }
        Ok(Statement::SqlServer(SqlServerStatement::SelectAssign {
            targets: targets.into_iter().map(|(target, _)| target).collect(),
            query: Box::new(query),
        }))
    }

    pub(super) fn sqlserver_assignment_item(&mut self) -> SqlResult<SelectItem> {
        let position = self.position();
        if !self.at_variable(0) {
            return Err(mixed_assignment(position));
        }
        let variable = self.variable_name()?;
        let operator = self.assignment_operator()?;
        let read_before = self.variables_read.len();
        self.variable_reference(&variable.text, variable.position)?;
        self.variables_read.truncate(read_before);
        let expr = self.expression()?;
        let reads = self
            .variables_read
            .get(read_before..)
            .unwrap_or_default()
            .to_vec();
        if let Some(targets) = &mut self.select_targets {
            targets.push((AssignmentTarget { variable, operator }, reads));
        }
        Ok(SelectItem::Expr {
            expr,
            alias: Some(String::new()),
        })
    }

    pub(super) fn sqlserver_print(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        let message = self.expression()?;
        Ok(Statement::SqlServer(SqlServerStatement::Print(message)))
    }

    pub(super) fn sqlserver_raiserror(&mut self) -> SqlResult<Statement> {
        let position = self.position();
        self.at += 1;
        if self.peek_kind() != &TokenKind::LeftParen {
            return Err(
                not_supported("RAISERROR without parentheses (a form SQL Server removed)")
                    .at(position),
            );
        }
        self.at += 1;
        let message = self.expression()?;
        self.expect(&TokenKind::Comma)?;
        let severity = self.expression()?;
        self.expect(&TokenKind::Comma)?;
        let state = self.expression()?;
        let mut arguments = Vec::new();
        while self.take(&TokenKind::Comma) {
            if arguments.len() == MAX_RAISERROR_ARGUMENTS {
                return Err(SqlError::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    "Too many substitution parameters for RAISERROR. Cannot exceed 20 substitution parameters.",
                )
                .with_sqlserver_code(2747)
                .at(self.position()));
            }
            arguments.push(self.expression()?);
        }
        self.expect(&TokenKind::RightParen)?;
        let mut set_error = false;
        if self.take_word("with") {
            loop {
                if self.take_word("seterror") {
                    set_error = true;
                } else if !(self.take_word("log") || self.take_word("nowait")) {
                    return Err(self.unexpected());
                }
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
        }
        Ok(Statement::SqlServer(SqlServerStatement::RaiseError(
            RaiseError {
                message,
                severity,
                state,
                arguments,
                set_error,
                position,
            },
        )))
    }

    pub(super) fn at_statement_end(&self) -> bool {
        match self.peek_kind() {
            TokenKind::Semicolon | TokenKind::End => true,
            TokenKind::Word(word) => starts_statement(word) || ends_a_block(word),
            _ => false,
        }
    }

    pub(super) fn sqlserver_throw(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        if self.at_statement_end() {
            return Ok(Statement::SqlServer(SqlServerStatement::Throw(None)));
        }
        let number = self.expression()?;
        self.expect(&TokenKind::Comma)?;
        let message = self.expression()?;
        self.expect(&TokenKind::Comma)?;
        let state = self.expression()?;
        Ok(Statement::SqlServer(SqlServerStatement::Throw(Some(
            ThrowArguments {
                number,
                message,
                state,
            },
        ))))
    }

    pub(crate) fn sqlserver_condition(&mut self) -> SqlResult<Statement> {
        let condition = self.expression()?;
        Ok(Statement::SqlServer(SqlServerStatement::Condition(
            condition,
        )))
    }

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
        if matches!(self.peek_kind(), TokenKind::Comma | TokenKind::RightParen) {
            return Ok(());
        }
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
