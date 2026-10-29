use super::Parser;
use super::sqlserver::not_supported;
use crate::ast::{SqlServerStatement, Statement};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;
use crate::session::OutputColumn;
use crate::tsql::{Declaration, Instruction, Program, TryBlock, chooses_dialect};

const MAX_CONTROL_NESTING: usize = 128;
const TRANSACTION_WORDS: [&str; 5] = ["tran", "transaction", "distributed", "dialog", "conversation"];
const UNSET: usize = usize::MAX;

struct Loop {
    start: usize,
    breaks: Vec<usize>,
}

#[derive(Default)]
struct Builder {
    instructions: Vec<Instruction>,
    tries: Vec<TryBlock>,
    tables: Vec<Declaration>,
    loops: Vec<Loop>,
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
                    self.at += 1;
                    if !self.at_statement_end() {
                        return Err(SqlError::new(
                            SqlState::SYNTAX_ERROR,
                            "A RETURN statement with a return value cannot be used in this context.",
                        )
                        .with_sqlserver_code(178)
                        .at(position));
                    }
                    builder.push(Instruction::Return);
                    self.take(&TokenKind::Semicolon);
                    return Ok(false);
                }
                "goto" => return Err(not_supported("GOTO").at(position)),
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
        let at = self.resume_point();
        let chooses = if self.literal_rows().is_none() {
            let statement = self.statement()?;
            if let Statement::SqlServer(SqlServerStatement::DeclareTable { name, columns }) =
                &statement
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

    fn throws_again(&self) -> bool {
        matches!(
            self.peek_kind_at(1),
            TokenKind::Semicolon | TokenKind::End
        ) || matches!(self.peek_kind_at(1), TokenKind::Word(word)
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
        for jump in enclosing.map(|enclosing| enclosing.breaks).unwrap_or_default() {
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
