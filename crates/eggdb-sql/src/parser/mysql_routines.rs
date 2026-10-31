use super::Parser;
use crate::ast::{CreateRoutine, DropRoutine, RoutineTarget, Statement};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::{TokenKind, tokenize_in};
use crate::plpgsql::{
    Block, Branch, CaseArm, Declaration, DeclarationKind, Fragment, IntoTargets, Loop, LoopKind, Program, Raise,
    RaiseCondition, RaiseItem, RaiseLevel, RaiseOption, Step, StepKind, Target, VariableConflict, VariableDeclaration,
    VariableType,
};
use crate::routines::{ParameterMode, ReturnType, RoutineDefinition, RoutineKind, RoutineLanguage, RoutineParameter, RoutineReturns, Volatility};

const MAX_NESTING: usize = 64;

fn not_supported_yet(what: &str, position: usize) -> SqlError {
    SqlError::not_supported(format!("{what} in MySQL stored programs")).at(position)
}

struct Compiler<'p, 't> {
    parser: &'p mut Parser<'t>,
    labels: Vec<String>,
    generated_labels: usize,
}

impl<'t> Parser<'t> {
    pub(super) fn creates_mysql_routine(&self) -> bool {
        let mut ahead = 1;
        if self.is_word_at(1, "definer") {
            while ahead < 8 && !self.is_word_at(ahead, "procedure") && !self.is_word_at(ahead, "function") {
                ahead += 1;
            }
        }
        self.is_word_at(ahead, "procedure") || self.is_word_at(ahead, "function")
    }

    fn skip_definer(&mut self) -> SqlResult<()> {
        if !self.take_word("definer") {
            return Ok(());
        }
        if !self.take_operator("=") {
            return Err(self.unexpected());
        }
        if self.take_word("current_user") {
            if self.take(&TokenKind::LeftParen) {
                self.expect(&TokenKind::RightParen)?;
            }
            return Ok(());
        }
        match self.peek_kind() {
            TokenKind::Word(_) | TokenKind::QuotedIdentifier(_) | TokenKind::String(_) => self.at += 1,
            _ => return Err(self.unexpected()),
        }
        if let TokenKind::Variable(_) = self.peek_kind() {
            self.at += 1;
        } else if self.is_operator("@") {
            self.at += 1;
            match self.peek_kind() {
                TokenKind::Word(_) | TokenKind::QuotedIdentifier(_) | TokenKind::String(_) => self.at += 1,
                _ => return Err(self.unexpected()),
            }
        }
        Ok(())
    }

    fn mysql_parameter(&mut self, function: bool) -> SqlResult<RoutineParameter> {
        let mode = if function {
            ParameterMode::In
        } else if self.take_word("inout") {
            ParameterMode::InOut
        } else if self.take_word("out") {
            ParameterMode::Out
        } else {
            self.take_word("in");
            ParameterMode::In
        };
        let name = self.name()?.text;
        let sql_type = self.mysql_type()?;
        Ok(RoutineParameter { name: Some(name), mode, sql_type, default: None, declared: None })
    }

    fn mysql_characteristics(&mut self) -> SqlResult<(Volatility, bool)> {
        let mut volatility = Volatility::Volatile;
        let mut security_definer = true;
        loop {
            if self.take_word("comment") {
                if !matches!(self.peek_kind(), TokenKind::String(_)) {
                    return Err(self.unexpected());
                }
                self.at += 1;
            } else if self.take_word("language") {
                self.expect_word("sql")?;
            } else if self.is_word("not") && self.is_word_at(1, "deterministic") {
                self.at += 2;
            } else if self.take_word("deterministic") {
                volatility = Volatility::Immutable;
            } else if self.is_word("contains") && self.is_word_at(1, "sql") || self.is_word("no") && self.is_word_at(1, "sql") {
                self.at += 2;
            } else if (self.is_word("reads") || self.is_word("modifies")) && self.is_word_at(1, "sql") && self.is_word_at(2, "data") {
                self.at += 3;
            } else if self.is_word("sql") && self.is_word_at(1, "security") {
                self.at += 2;
                security_definer = if self.take_word("invoker") {
                    false
                } else {
                    self.expect_word("definer")?;
                    true
                };
            } else {
                return Ok((volatility, security_definer));
            }
        }
    }

    pub(super) fn mysql_create_routine(&mut self) -> SqlResult<Statement> {
        let start = self.peek().start;
        self.at += 1;
        self.skip_definer()?;
        let kind = if self.take_word("function") {
            RoutineKind::Function
        } else {
            self.expect_word("procedure")?;
            RoutineKind::Procedure
        };
        let if_not_exists = self.is_word("if") && self.is_word_at(1, "not") && self.is_word_at(2, "exists");
        if if_not_exists {
            self.at += 3;
        }
        let name = self.table_name()?;
        self.expect(&TokenKind::LeftParen)?;
        let mut parameters = Vec::new();
        if !self.take(&TokenKind::RightParen) {
            loop {
                parameters.push(self.mysql_parameter(kind == RoutineKind::Function)?);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
        }
        let returns = if kind == RoutineKind::Function {
            self.expect_word("returns")?;
            RoutineReturns::One(ReturnType::Type(self.mysql_type()?))
        } else {
            RoutineReturns::Nothing
        };
        let (volatility, security_definer) = self.mysql_characteristics()?;
        let body_start = self.peek().start;
        let body_position = self.position();
        Compiler::new(self).body()?;
        let body = self.slice(body_start, self.previous_end());
        let text = self.slice(start, self.previous_end());
        let definition = RoutineDefinition {
            name: name.text.clone(),
            kind,
            dialect: Dialect::MySql,
            language: RoutineLanguage::MySql,
            parameters,
            returns,
            volatility,
            strict: false,
            security_definer,
            settings: Vec::new(),
            body,
            text,
            tsql: None,
            plsql: None,
        };
        Ok(Statement::CreateRoutine(CreateRoutine {
            name,
            replace: false,
            must_exist: false,
            definition,
            body_position: Some(body_position),
        }))
    }

    pub(super) fn mysql_drop_routine(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        let kind = if self.take_word("function") {
            RoutineKind::Function
        } else {
            self.expect_word("procedure")?;
            RoutineKind::Procedure
        };
        let if_exists = self.is_word("if") && self.is_word_at(1, "exists");
        if if_exists {
            self.at += 2;
        }
        let name = self.table_name()?;
        Ok(Statement::DropRoutine(DropRoutine {
            kind: Some(kind),
            targets: vec![RoutineTarget { name, arguments: None }],
            if_exists,
            cascade: false,
        }))
    }
}

/// Compiles the body of a MySQL procedure or function into the program the routine interpreter
/// runs: compound statements, local variables, flow control and SQL statements.
pub(crate) fn mysql_program(definition: &RoutineDefinition) -> SqlResult<Program> {
    let body = &definition.body;
    let tokens = tokenize_in(body, Dialect::MySql)?;
    let mut parser = Parser::new(body, &tokens).in_dialect(Dialect::MySql);
    let block = Compiler::new(&mut parser).body()?;
    if !parser.at_end() && !matches!(parser.peek_kind(), TokenKind::Semicolon) {
        return Err(parser.unexpected());
    }
    Ok(Program {
        variable_conflict: VariableConflict::UseVariable,
        print_strict_params: false,
        block,
        package_candidates: Vec::new(),
    })
}

impl<'p, 't> Compiler<'p, 't> {
    fn new(parser: &'p mut Parser<'t>) -> Self {
        Self { parser, labels: Vec::new(), generated_labels: 0 }
    }

    fn position(&self) -> usize {
        self.parser.position()
    }

    fn is_word(&self, word: &str) -> bool {
        self.parser.is_word(word)
    }

    fn take_word(&mut self, word: &str) -> bool {
        self.parser.take_word(word)
    }

    fn expect_word(&mut self, word: &str) -> SqlResult<()> {
        self.parser.expect_word(word)
    }

    fn body(&mut self) -> SqlResult<Block> {
        let position = self.position();
        let step = self.step(0)?;
        Ok(match step.kind {
            StepKind::Block(block) => block,
            other => Block { label: None, declarations: Vec::new(), steps: vec![Step { kind: other, position }], handlers: Vec::new(), position },
        })
    }

    fn label_ahead(&self) -> bool {
        matches!(self.parser.peek_kind(), TokenKind::Word(_) | TokenKind::QuotedIdentifier(_))
            && matches!(self.parser.peek_kind_at(1), TokenKind::Colon)
            && !self.is_word("begin")
    }

    fn generated_label(&mut self) -> String {
        self.generated_labels += 1;
        format!("mysql_loop_{}", self.generated_labels)
    }

    fn word_text(&self) -> Option<String> {
        match self.parser.peek_kind() {
            TokenKind::Word(word) => Some(word.to_string()),
            TokenKind::QuotedIdentifier(name) => Some(name.to_string()),
            _ => None,
        }
    }

    fn end_label(&mut self, label: Option<&str>) -> SqlResult<()> {
        if let Some(label) = label
            && self.word_text().as_deref() == Some(label)
        {
            self.parser.at += 1;
        }
        Ok(())
    }

    fn fragment(&mut self, stops: &[&str]) -> SqlResult<Fragment> {
        let first = self.parser.at;
        let mut parens = 0usize;
        let mut cases = 0usize;
        loop {
            let top = parens == 0 && cases == 0;
            match self.parser.peek_kind() {
                TokenKind::End => break,
                TokenKind::Semicolon if parens == 0 => break,
                TokenKind::Comma if top && stops.contains(&",") => break,
                TokenKind::LeftParen => parens += 1,
                TokenKind::RightParen => {
                    if parens == 0 {
                        break;
                    }
                    parens -= 1;
                }
                TokenKind::Word(word) if top && stops.contains(&word.as_ref()) => break,
                TokenKind::Word(word) if word == "case" => cases += 1,
                TokenKind::Word(word) if word == "end" => {
                    if top {
                        break;
                    }
                    cases = cases.saturating_sub(1);
                }
                _ => {}
            }
            self.parser.at += 1;
        }
        if self.parser.at <= first {
            return Err(self.parser.unexpected());
        }
        let start = self.parser.tokens[first].start;
        Ok(Fragment { text: self.parser.slice(start, self.parser.previous_end()), position: start })
    }

    fn end_statement(&mut self) -> SqlResult<()> {
        if self.parser.take(&TokenKind::Semicolon) || self.parser.at_end() {
            return Ok(());
        }
        Err(self.parser.unexpected())
    }

    fn steps(&mut self, depth: usize, stops: &[&str]) -> SqlResult<Vec<Step>> {
        let mut steps = Vec::new();
        while !stops.iter().any(|stop| self.is_word(stop)) {
            if self.parser.at_end() {
                return Err(self.parser.unexpected());
            }
            steps.push(self.step(depth)?);
            self.end_statement()?;
        }
        Ok(steps)
    }

    fn step(&mut self, depth: usize) -> SqlResult<Step> {
        let position = self.position();
        if depth > MAX_NESTING {
            return Err(SqlError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, "the stored program is nested too deeply").at(position));
        }
        let label = if self.label_ahead() {
            let label = self.word_text().unwrap_or_default();
            self.parser.at += 2;
            Some(label)
        } else {
            None
        };
        let kind = match self.word_text().as_deref() {
            Some("begin") => StepKind::Block(self.block(label, depth)?),
            Some("while") | Some("repeat") | Some("loop") => self.loop_step(label, depth)?,
            _ if label.is_some() => return Err(self.parser.unexpected()),
            Some("if") => self.if_step(depth)?,
            Some("case") => self.case_step(depth)?,
            Some("leave") | Some("iterate") => {
                let leave = self.take_word("leave");
                if !leave {
                    self.parser.at += 1;
                }
                let target = self.word_text().ok_or_else(|| self.parser.unexpected())?;
                if !self.labels.contains(&target) {
                    return Err(SqlError::new(SqlState::SYNTAX_ERROR, format!("{} with no matching label: {target}", if leave { "LEAVE" } else { "ITERATE" })).at(position));
                }
                self.parser.at += 1;
                if leave {
                    StepKind::Exit { label: Some(target), condition: None }
                } else {
                    StepKind::Continue { label: Some(target), condition: None }
                }
            }
            Some("return") => {
                self.parser.at += 1;
                StepKind::Return(Some(self.fragment(&[])?))
            }
            Some("declare") => return Err(SqlError::syntax("DECLARE is only allowed at the start of a BEGIN ... END block", position)),
            Some("signal") | Some("resignal") => self.signal_step()?,
            Some("open") | Some("fetch") | Some("close") => return Err(not_supported_yet("cursors", position)),
            Some("set") if self.sets_locals() => self.set_step()?,
            Some("select") => self.select_step()?,
            _ => StepKind::Sql { statement: self.fragment(&[])?, into: None },
        };
        Ok(Step { kind, position })
    }

    fn block(&mut self, label: Option<String>, depth: usize) -> SqlResult<Block> {
        let position = self.position();
        self.expect_word("begin")?;
        if self.is_word("not") && self.parser.is_word_at(1, "atomic") {
            self.parser.at += 2;
        }
        if let Some(label) = &label {
            self.labels.push(label.clone());
        }
        let mut declarations = Vec::new();
        while self.is_word("declare") {
            declarations.extend(self.declaration()?);
            self.end_statement()?;
        }
        let steps = self.steps(depth + 1, &["end"]);
        if label.is_some() {
            self.labels.pop();
        }
        let steps = steps?;
        self.expect_word("end")?;
        self.end_label(label.as_deref())?;
        Ok(Block { label, declarations, steps, handlers: Vec::new(), position })
    }

    fn declaration(&mut self) -> SqlResult<Vec<Declaration>> {
        let position = self.position();
        self.expect_word("declare")?;
        if self.parser.is_word_at(1, "cursor") || self.parser.is_word_at(1, "condition") {
            return Err(not_supported_yet("cursors and named conditions", position));
        }
        if ["continue", "exit", "undo"].iter().any(|word| self.is_word(word)) && self.parser.is_word_at(1, "handler") {
            return Err(not_supported_yet("handlers", position));
        }
        let mut names = vec![(self.parser.name()?.text, position)];
        while self.parser.take(&TokenKind::Comma) {
            let at = self.position();
            names.push((self.parser.name()?.text, at));
        }
        let sql_type = self.parser.mysql_type()?;
        let default = if self.take_word("default") { Some(self.fragment(&[])?) } else { None };
        Ok(names
            .into_iter()
            .map(|(name, position)| Declaration {
                name: name.clone(),
                position,
                kind: DeclarationKind::Variable(VariableDeclaration {
                    variable_type: VariableType::Sql(sql_type.clone()),
                    constant: false,
                    not_null: false,
                    default: default.clone(),
                }),
            })
            .collect())
    }

    fn if_step(&mut self, depth: usize) -> SqlResult<StepKind> {
        self.expect_word("if")?;
        let mut branches = Vec::new();
        loop {
            let condition = self.fragment(&["then"])?;
            self.expect_word("then")?;
            let steps = self.steps(depth + 1, &["elseif", "else", "end"])?;
            branches.push(Branch { condition, steps });
            if !self.take_word("elseif") {
                break;
            }
        }
        let otherwise = if self.take_word("else") { Some(self.steps(depth + 1, &["end"])?) } else { None };
        self.expect_word("end")?;
        self.expect_word("if")?;
        Ok(StepKind::If { branches, otherwise })
    }

    fn case_step(&mut self, depth: usize) -> SqlResult<StepKind> {
        self.expect_word("case")?;
        let subject = if self.is_word("when") { None } else { Some(self.fragment(&["when"])?) };
        let mut arms = Vec::new();
        let mut searched = Vec::new();
        while self.take_word("when") {
            let value = self.fragment(&["then"])?;
            self.expect_word("then")?;
            let steps = self.steps(depth + 1, &["when", "else", "end"])?;
            if subject.is_some() {
                arms.push(CaseArm { values: vec![value], steps });
            } else {
                searched.push(Branch { condition: value, steps });
            }
        }
        let otherwise = if self.take_word("else") { Some(self.steps(depth + 1, &["end"])?) } else { None };
        self.expect_word("end")?;
        self.expect_word("case")?;
        if subject.is_none() {
            let otherwise = otherwise.or_else(|| Some(vec![case_not_found(self.position())]));
            return Ok(StepKind::If { branches: searched, otherwise });
        }
        Ok(StepKind::Case { subject, arms, otherwise: otherwise.or_else(|| Some(vec![case_not_found(self.position())])) })
    }

    fn loop_step(&mut self, label: Option<String>, depth: usize) -> SqlResult<StepKind> {
        let written_label = label.clone();
        let label = label.unwrap_or_else(|| self.generated_label());
        self.labels.push(label.clone());
        let outcome = self.loop_contents(&label, depth);
        self.labels.pop();
        let (kind, body) = outcome?;
        self.end_label(written_label.as_deref())?;
        Ok(StepKind::Loop(Loop { label: Some(label), kind, body }))
    }

    fn loop_contents(&mut self, label: &str, depth: usize) -> SqlResult<(LoopKind, Vec<Step>)> {
        if self.take_word("while") {
            let condition = self.fragment(&["do"])?;
            self.expect_word("do")?;
            let body = self.steps(depth + 1, &["end"])?;
            self.expect_word("end")?;
            self.expect_word("while")?;
            return Ok((LoopKind::While(condition), body));
        }
        if self.take_word("repeat") {
            let mut body = self.steps(depth + 1, &["until"])?;
            let position = self.position();
            self.expect_word("until")?;
            let condition = self.fragment(&["end"])?;
            self.expect_word("end")?;
            self.expect_word("repeat")?;
            body.push(Step { kind: StepKind::Exit { label: Some(label.to_owned()), condition: Some(condition) }, position });
            return Ok((LoopKind::Plain, body));
        }
        self.expect_word("loop")?;
        let body = self.steps(depth + 1, &["end"])?;
        self.expect_word("end")?;
        self.expect_word("loop")?;
        Ok((LoopKind::Plain, body))
    }

    fn sets_locals(&self) -> bool {
        !matches!(self.parser.peek_kind_at(1), TokenKind::Variable(_))
            && !["global", "session", "local", "persist", "names", "character", "transaction", "password"]
                .iter()
                .any(|word| self.parser.is_word_at(1, word))
    }

    fn set_step(&mut self) -> SqlResult<StepKind> {
        self.expect_word("set")?;
        let mut assignments = Vec::new();
        loop {
            let position = self.position();
            let name = self.parser.name()?.text;
            if !self.parser.take_operator("=") && !self.parser.take_operator(":=") {
                return Err(self.parser.unexpected());
            }
            let value = self.fragment(&[","])?;
            assignments.push(Step { kind: StepKind::Assign { target: Target { name, path: Vec::new(), position }, value }, position });
            if !self.parser.take(&TokenKind::Comma) {
                break;
            }
        }
        Ok(match assignments.len() {
            1 => assignments.remove(0).kind,
            _ => StepKind::Block(Block { label: None, declarations: Vec::new(), steps: assignments, handlers: Vec::new(), position: self.position() }),
        })
    }

    fn select_step(&mut self) -> SqlResult<StepKind> {
        let first = self.parser.at;
        let mut parens = 0usize;
        let mut into: Option<(usize, usize, Vec<Target>)> = None;
        loop {
            match self.parser.peek_kind() {
                TokenKind::End => break,
                TokenKind::Semicolon if parens == 0 => break,
                TokenKind::LeftParen => parens += 1,
                TokenKind::RightParen => parens = parens.saturating_sub(1),
                TokenKind::Word(word) if parens == 0 && word == "into" && into.is_none() => {
                    let into_start = self.parser.peek().start;
                    self.parser.at += 1;
                    let mut targets = Vec::new();
                    loop {
                        let position = self.position();
                        let name = self.parser.name()?.text;
                        targets.push(Target { name, path: Vec::new(), position });
                        if !self.parser.take(&TokenKind::Comma) {
                            break;
                        }
                    }
                    into = Some((into_start, self.parser.previous_end(), targets));
                    continue;
                }
                _ => {}
            }
            self.parser.at += 1;
        }
        let start = self.parser.tokens[first].start;
        let end = self.parser.previous_end();
        let Some((into_start, into_end, targets)) = into else {
            return Ok(StepKind::Sql { statement: Fragment { text: self.parser.slice(start, end), position: start }, into: None });
        };
        let text = format!("{} {}", self.parser.slice(start, into_start), self.parser.slice(into_end, end));
        Ok(StepKind::Sql {
            statement: Fragment { text: text.trim().to_owned(), position: start },
            into: Some(IntoTargets { strict: false, bulk: false, targets }),
        })
    }

    fn signal_step(&mut self) -> SqlResult<StepKind> {
        let position = self.position();
        self.parser.at += 1;
        self.expect_word("sqlstate")?;
        self.take_word("value");
        let TokenKind::String(code) = self.parser.peek_kind() else {
            return Err(not_supported_yet("SIGNAL with a condition name", position));
        };
        let state = SqlState::from_text(code).filter(|_| code.len() == 5 && !code.starts_with("00")).ok_or_else(|| {
            SqlError::new(SqlState::SYNTAX_ERROR, format!("Bad SQLSTATE: '{code}'")).at(position)
        })?;
        self.parser.at += 1;
        let mut options = Vec::new();
        if self.take_word("set") {
            loop {
                let item_position = self.position();
                let item = self.word_text().ok_or_else(|| self.parser.unexpected())?;
                self.parser.at += 1;
                if !self.parser.take_operator("=") {
                    return Err(self.parser.unexpected());
                }
                let value = self.fragment(&[","])?;
                match item.as_str() {
                    "message_text" => options.push(RaiseOption { item: RaiseItem::Message, value }),
                    "mysql_errno" | "class_origin" | "subclass_origin" | "constraint_catalog" | "constraint_schema"
                    | "constraint_name" | "catalog_name" | "schema_name" | "table_name" | "column_name" | "cursor_name" => {}
                    _ => return Err(SqlError::syntax(format!("unknown SIGNAL item {item}"), item_position)),
                }
                if !self.parser.take(&TokenKind::Comma) {
                    break;
                }
            }
        }
        if options.is_empty() {
            options.push(RaiseOption {
                item: RaiseItem::Message,
                value: Fragment { text: "'Unhandled user-defined exception condition'".to_owned(), position },
            });
        }
        Ok(StepKind::Raise(Raise { level: RaiseLevel::Exception, condition: Some(RaiseCondition::State(state)), format: None, arguments: Vec::new(), options }))
    }
}

fn case_not_found(position: usize) -> Step {
    Step {
        kind: StepKind::Raise(Raise {
            level: RaiseLevel::Exception,
            condition: Some(RaiseCondition::State(SqlState::CASE_NOT_FOUND)),
            format: None,
            arguments: Vec::new(),
            options: vec![RaiseOption {
                item: RaiseItem::Message,
                value: Fragment { text: "'Case not found for CASE statement'".to_owned(), position },
            }],
        }),
        position,
    }
}
