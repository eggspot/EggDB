use super::Parser;
use super::sqlserver::not_supported;
use crate::ast::{SqlServerStatement, Statement};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;
use crate::session::OutputColumn;
use crate::tsql::{Declaration, Instruction, Program, TryBlock, chooses_dialect};

const MAX_CONTROL_NESTING: usize = 128;
const TRANSACTION_WORDS: [&str; 5] = [
    "tran",
    "transaction",
    "distributed",
    "dialog",
    "conversation",
];
const UNSET: usize = usize::MAX;

struct Loop {
    start: usize,
    breaks: Vec<usize>,
}

struct Label {
    name: String,
    at: usize,
}

struct Goto {
    label: String,
    jump: usize,
    position: usize,
}

#[derive(Default)]
struct Builder {
    instructions: Vec<Instruction>,
    tries: Vec<TryBlock>,
    tables: Vec<Declaration>,
    loops: Vec<Loop>,
    labels: Vec<Label>,
    gotos: Vec<Goto>,
    catch_depth: usize,
    depth: usize,
}

impl Builder {
    fn push(&mut self, instruction: Instruction) -> usize {
        self.instructions.push(instruction);
        self.instructions.len() - 1
    }

    fn next(&self) -> usize {
        self.instructions.len()
    }

    fn point_here(&mut self, at: usize) {
        let here = self.next();
        match self.instructions.get_mut(at) {
            Some(Instruction::Test { otherwise, .. }) => *otherwise = here,
            Some(Instruction::Jump { to }) => *to = here,
            _ => {}
        }
    }

    fn end_here(&mut self, test: usize) {
        let here = self.next();
        if let Some(Instruction::Test { end, .. }) = self.instructions.get_mut(test) {
            *end = here;
        }
    }

    fn enters_a_try_scope(&self, jump: usize, target: usize) -> bool {
        self.tries.iter().any(|block| {
            (block.body.contains(&target) && !block.body.contains(&jump))
                || (block.handler.contains(&target) && !block.handler.contains(&jump))
        })
    }

    fn resolve_gotos(&mut self) -> SqlResult<()> {
        for goto in std::mem::take(&mut self.gotos) {
            let Some(target) = self
                .labels
                .iter()
                .find(|label| label.name.eq_ignore_ascii_case(&goto.label))
                .map(|label| label.at)
            else {
                return Err(undeclared_label(&goto.label, goto.position));
            };
            if self.enters_a_try_scope(goto.jump, target) {
                return Err(SqlError::new(
                    SqlState::SYNTAX_ERROR,
                    "GOTO cannot be used to jump into a TRY or CATCH scope.",
                )
                .with_sqlserver_code(1026)
                .at(goto.position));
            }
            if let Some(Instruction::Jump { to }) = self.instructions.get_mut(goto.jump) {
                *to = target;
            }
        }
        Ok(())
    }
}

fn undeclared_label(label: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        format!(
            "A GOTO statement references the label '{label}' but the label has not been declared."
        ),
    )
    .with_sqlserver_code(133)
    .at(position)
}

fn duplicate_label(label: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        format!("The label '{label}' has already been declared. Label names must be unique within a query batch or stored procedure."),
    )
    .with_sqlserver_code(132)
    .at(position)
}

fn outside_loop(statement: &str, code: u16, position: usize) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        format!("Cannot use a {statement} statement outside the scope of a WHILE statement."),
    )
    .with_sqlserver_code(code)
    .at(position)
}

fn nested_too_deeply(position: usize) -> SqlError {
    SqlError::new(
        SqlState::STATEMENT_TOO_COMPLEX,
        "Some part of your SQL statement is nested too deeply. Rewrite the query or break it up into smaller queries.",
    )
    .with_sqlserver_code(191)
    .at(position)
}

impl Parser<'_> {
    pub(crate) fn compile_batch(&mut self, nested: bool) -> SqlResult<Program> {
        let mut builder = Builder::default();
        loop {
            self.skip_empty_statements();
            if self.at_end() {
                break;
            }
            if self.sqlserver() && (self.is_word("end") || self.is_word("else")) {
                return Err(self.unexpected());
            }
            if self.compile_statement(&mut builder, nested)? {
                break;
            }
        }
        builder.resolve_gotos()?;
        let mut declarations: Vec<Declaration> = std::mem::take(&mut self.declared_variables)
            .into_iter()
            .map(Declaration::Scalar)
            .collect();
        declarations.append(&mut builder.tables);
        Ok(Program {
            instructions: builder.instructions,
            tries: builder.tries,
            declarations,
        })
    }

    fn compile_statement(&mut self, builder: &mut Builder, nested: bool) -> SqlResult<bool> {
        let position = self.position();
        if builder.depth > MAX_CONTROL_NESTING {
            return Err(nested_too_deeply(position));
        }
        if self.sqlserver()
            && let TokenKind::Word(word) = self.peek_kind()
        {
            match word.as_ref() {
                "if" => {
                    self.compile_if(builder, nested)?;
                    return Ok(false);
                }
                "while" => {
                    self.compile_while(builder, nested)?;
                    return Ok(false);
                }
                "begin" if self.is_word_at(1, "try") => {
                    self.compile_try(builder, nested)?;
                    return Ok(false);
                }
                "begin"
                    if !matches!(self.peek_kind_at(1), TokenKind::Word(next)
                        if TRANSACTION_WORDS.iter().any(|candidate| next == candidate)) =>
                {
                    self.compile_block(builder, nested)?;
                    return Ok(false);
                }
                "break" => {
                    self.at += 1;
                    let jump = builder.next();
                    let Some(enclosing) = builder.loops.last_mut() else {
                        return Err(outside_loop("BREAK", 135, position));
                    };
                    enclosing.breaks.push(jump);
                    builder.push(Instruction::Jump { to: UNSET });
                    self.take(&TokenKind::Semicolon);
                    return Ok(false);
                }
                "continue" => {
                    self.at += 1;
                    let Some(start) = builder.loops.last().map(|enclosing| enclosing.start) else {
                        return Err(outside_loop("CONTINUE", 136, position));
                    };
                    builder.push(Instruction::Jump { to: start });
                    self.take(&TokenKind::Semicolon);
                    return Ok(false);
                }
                "return" => {
                    let at = self.resume_point();
                    self.at += 1;
                    if !self.at_statement_end() {
                        if !self.returns_values {
                            return Err(SqlError::new(
                                SqlState::SYNTAX_ERROR,
                                "A RETURN statement with a return value cannot be used in this context.",
                            )
                            .with_sqlserver_code(178)
                            .at(position));
                        }
                        self.expression()?;
                        builder.push(Instruction::Run { at });
                    }
                    builder.push(Instruction::Return);
                    self.take(&TokenKind::Semicolon);
                    return Ok(false);
                }
                "goto" => {
                    self.at += 1;
                    let label = self.label_name()?;
                    let jump = builder.push(Instruction::Jump { to: UNSET });
                    builder.gotos.push(Goto {
                        label,
                        jump,
                        position,
                    });
                    self.take(&TokenKind::Semicolon);
                    return Ok(false);
                }
                _ if self.at_label() => {
                    let label = self.label_name()?;
                    self.at += 1;
                    if builder
                        .labels
                        .iter()
                        .any(|known| known.name.eq_ignore_ascii_case(&label))
                    {
                        return Err(duplicate_label(&label, position));
                    }
                    let at = builder.next();
                    builder.labels.push(Label { name: label, at });
                    return Ok(false);
                }
                "throw" if builder.catch_depth == 0 && self.throws_again() => {
                    return Err(SqlError::new(
                        SqlState::SYNTAX_ERROR,
                        "To rethrow an error, a THROW statement must be used inside a CATCH block. Insert the THROW statement inside a CATCH block, or add error parameters to the THROW statement.",
                    )
                    .with_sqlserver_code(10704)
                    .at(position));
                }
                _ => {}
            }
        }
        self.queries = 0;
        let at = self.resume_point();
        let first_word = self.first_word_upper();
        let chooses = if self.literal_rows().is_none() {
            let statement = self.statement()?;
            if self.function_body
                && let Some(refused) =
                    crate::tsql::refused_in_function(&statement, &first_word, position)
            {
                return Err(refused);
            }
            if let Statement::SqlServer(SqlServerStatement::DeclareTable {
                name,
                columns,
                rules,
            }) = &statement
            {
                builder.tables.push(Declaration::Table {
                    name: name.text.clone(),
                    columns: columns
                        .iter()
                        .map(|(column, sql_type)| OutputColumn {
                            name: column.text.clone(),
                            sql_type: Some(*sql_type),
                        })
                        .collect(),
                    rules: rules.clone(),
                });
            }
            chooses_dialect(&statement)
        } else {
            false
        };
        builder.push(Instruction::Run { at });
        if chooses && (nested || builder.depth > 0) {
            return Err(not_supported(
                "SET eggdb.dialect inside IF, WHILE, BEGIN ... END, TRY ... CATCH or dynamic SQL",
            )
            .at(position));
        }
        Ok(chooses)
    }

    pub(super) fn at_label(&self) -> bool {
        self.sqlserver()
            && matches!(self.peek_kind(), TokenKind::Word(word)
                if !super::sqlserver::is_reserved(word))
            && self.peek_kind_at(1) == &TokenKind::Colon
    }

    fn label_name(&mut self) -> SqlResult<String> {
        match self.peek_kind() {
            TokenKind::Word(word) if !super::sqlserver::is_reserved(word) => {
                let name = word.as_ref().to_owned();
                self.at += 1;
                Ok(name)
            }
            _ => Err(self.unexpected()),
        }
    }

    fn first_word_upper(&self) -> String {
        if !self.function_body {
            return String::new();
        }
        match self.peek_kind() {
            TokenKind::Word(word) => word.to_uppercase(),
            _ => String::new(),
        }
    }

    fn throws_again(&self) -> bool {
        matches!(self.peek_kind_at(1), TokenKind::Semicolon | TokenKind::End)
            || matches!(self.peek_kind_at(1), TokenKind::Word(word)
            if super::sqlserver::starts_statement(word) || super::sqlserver::ends_a_block(word))
    }

    fn compile_body(&mut self, builder: &mut Builder, nested: bool) -> SqlResult<()> {
        builder.depth += 1;
        let compiled = self.compile_statement(builder, nested);
        builder.depth -= 1;
        compiled.map(drop)
    }

    fn compile_if(&mut self, builder: &mut Builder, nested: bool) -> SqlResult<()> {
        self.at += 1;
        let condition = self.resume_point();
        self.expression()?;
        let test = builder.push(Instruction::Test {
            condition,
            otherwise: UNSET,
            end: UNSET,
        });
        self.compile_body(builder, nested)?;
        if self.peek_kind() == &TokenKind::Semicolon && self.is_word_at(1, "else") {
            self.at += 1;
        }
        if self.take_word("else") {
            let jump = builder.push(Instruction::Jump { to: UNSET });
            builder.point_here(test);
            self.compile_body(builder, nested)?;
            builder.point_here(jump);
        } else {
            builder.point_here(test);
        }
        builder.end_here(test);
        Ok(())
    }

    fn compile_while(&mut self, builder: &mut Builder, nested: bool) -> SqlResult<()> {
        self.at += 1;
        let condition = self.resume_point();
        self.expression()?;
        let test = builder.push(Instruction::Test {
            condition,
            otherwise: UNSET,
            end: UNSET,
        });
        builder.loops.push(Loop {
            start: test,
            breaks: Vec::new(),
        });
        let body = self.compile_body(builder, nested);
        let enclosing = builder.loops.pop();
        body?;
        builder.push(Instruction::Jump { to: test });
        builder.point_here(test);
        builder.end_here(test);
        for jump in enclosing
            .map(|enclosing| enclosing.breaks)
            .unwrap_or_default()
        {
            builder.point_here(jump);
        }
        Ok(())
    }

    fn compile_until_end(
        &mut self,
        builder: &mut Builder,
        nested: bool,
        empty_allowed: bool,
    ) -> SqlResult<()> {
        let mut statements = 0usize;
        loop {
            self.skip_empty_statements();
            if self.is_word("end") {
                break;
            }
            if self.at_end() || self.is_word("else") {
                return Err(self.unexpected());
            }
            self.compile_body(builder, nested)?;
            statements += 1;
        }
        if statements == 0 && !empty_allowed {
            return Err(self.unexpected());
        }
        Ok(())
    }

    fn compile_block(&mut self, builder: &mut Builder, nested: bool) -> SqlResult<()> {
        self.at += 1;
        self.compile_until_end(builder, nested, false)?;
        self.expect_word("end")
    }

    fn compile_try(&mut self, builder: &mut Builder, nested: bool) -> SqlResult<()> {
        if self.function_body {
            return Err(crate::tsql::side_effecting("BEGIN TRY", self.position()));
        }
        self.at += 2;
        let index = builder.tries.len();
        builder.tries.push(TryBlock {
            body: 0..0,
            handler: 0..0,
        });
        let body_start = builder.next();
        self.compile_until_end(builder, nested, false)?;
        self.expect_word("end")?;
        self.expect_word("try")?;
        let body_end = builder.next();
        let jump = builder.push(Instruction::Jump { to: UNSET });
        self.skip_empty_statements();
        self.expect_word("begin")?;
        self.expect_word("catch")?;
        let handler_start = builder.next();
        builder.catch_depth += 1;
        let handler = self.compile_until_end(builder, nested, true);
        builder.catch_depth -= 1;
        handler?;
        self.expect_word("end")?;
        self.expect_word("catch")?;
        let handler_end = builder.next();
        builder.point_here(jump);
        if let Some(block) = builder.tries.get_mut(index) {
            *block = TryBlock {
                body: body_start..body_end,
                handler: handler_start..handler_end,
            };
        }
        Ok(())
    }
}
