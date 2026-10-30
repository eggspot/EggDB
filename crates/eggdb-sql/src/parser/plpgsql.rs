use super::Parser;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::{Token, TokenKind};
use crate::plpgsql::{
    AliasTarget, Block, Branch, CaseArm, CursorArgument, CursorDeclaration, CursorValue,
    Declaration, DeclarationKind, DiagnosticsItem, DynamicSql, ExceptionCondition, FetchDirection,
    Fragment, Handler, IntoTargets, Loop, LoopKind, MAX_NESTING, MAX_STEPS, OpenSource, Program,
    QuerySource, Raise, RaiseCondition, RaiseItem, RaiseLevel, RaiseOption, Step, StepKind, Target,
    TargetPart, VariableConflict, VariableDeclaration, VariableType, condition_states,
    format_placeholders, has_output_parameters, is_trigger_function,
};
use crate::routines::{ReturnType, RoutineDefinition, RoutineKind, RoutineReturns};
use crate::schemas::{base_name, qualified};

const TRIGGER_VARIABLES: [&str; 12] = [
    "new",
    "old",
    "tg_name",
    "tg_when",
    "tg_level",
    "tg_op",
    "tg_relid",
    "tg_relname",
    "tg_table_name",
    "tg_table_schema",
    "tg_nargs",
    "tg_argv",
];

const STATEMENTS_WITH_INTO: [&str; 7] = ["select", "with", "insert", "update", "delete", "merge", "values"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Comma,
    Range,
    Word(&'static str),
}

#[derive(Debug, Clone)]
struct Variable {
    name: String,
    constant: bool,
    cursor: bool,
}

impl Variable {
    fn plain(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            constant: false,
            cursor: false,
        }
    }
}

#[derive(Debug, Clone)]
struct Scope {
    label: Option<String>,
    is_loop: bool,
    variables: Vec<Variable>,
}

fn nested_too_deeply(position: usize) -> SqlError {
    SqlError::new(
        SqlState::STATEMENT_TOO_COMPLEX,
        "the PL/pgSQL body is nested too deeply",
    )
    .with_hint(format!(
        "A PL/pgSQL body nests blocks, IF, CASE and loops at most {MAX_NESTING} deep."
    ))
    .at(position)
}

fn too_many_steps(position: usize) -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("a PL/pgSQL body has at most {MAX_STEPS} statements and declarations"),
    )
    .at(position)
}

fn not_a_variable(name: &str, position: usize) -> SqlError {
    SqlError::syntax(format!("\"{name}\" is not a known variable"), position)
}

fn mismatch(message: &str, position: usize) -> SqlError {
    SqlError::new(SqlState::DATATYPE_MISMATCH, message).at(position)
}

fn invalid_sqlstate(position: usize) -> SqlError {
    SqlError::syntax("invalid SQLSTATE code", position)
}

fn unrecognized_condition(name: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_OBJECT,
        format!("unrecognized exception condition \"{name}\""),
    )
    .at(position)
}

fn unsupported_transaction_command(position: usize) -> SqlError {
    SqlError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        "unsupported transaction command in PL/pgSQL",
    )
    .at(position)
}

pub(crate) fn plpgsql_program<'t>(
    text: &'t str,
    tokens: &'t [Token<'t>],
    definition: &RoutineDefinition,
) -> SqlResult<Program> {
    let parser = Parser::new(text, tokens).in_dialect(Dialect::PostgreSql);
    let mut compiler = Compiler {
        parser,
        definition,
        scopes: vec![root_scope(definition)],
        counted: 0,
    };
    compiler.program()
}

fn root_scope(definition: &RoutineDefinition) -> Scope {
    let mut variables: Vec<Variable> = definition
        .parameters
        .iter()
        .filter_map(|parameter| parameter.name.as_deref())
        .map(Variable::plain)
        .collect();
    if let RoutineReturns::Table(columns) = &definition.returns {
        variables.extend(columns.iter().map(|(name, _)| Variable::plain(name)));
    }
    variables.push(Variable::plain("found"));
    if is_trigger_function(definition) {
        variables.extend(TRIGGER_VARIABLES.into_iter().map(Variable::plain));
    }
    Scope {
        label: Some(base_name(&definition.name).to_owned()),
        is_loop: false,
        variables,
    }
}

struct Compiler<'t, 'd> {
    parser: Parser<'t>,
    definition: &'d RoutineDefinition,
    scopes: Vec<Scope>,
    counted: usize,
}

impl<'t> Compiler<'t, '_> {
    fn kind(&self) -> &'t TokenKind<'t> {
        self.parser.peek_kind()
    }

    fn kind_at(&self, ahead: usize) -> &'t TokenKind<'t> {
        self.parser.peek_kind_at(ahead)
    }

    fn position(&self) -> usize {
        self.parser.position()
    }

    fn advance(&mut self) {
        self.parser.at += 1;
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

    fn take(&mut self, kind: &TokenKind<'_>) -> bool {
        self.parser.take(kind)
    }

    fn expect_semicolon(&mut self) -> SqlResult<()> {
        self.parser.expect(&TokenKind::Semicolon)
    }

    fn unexpected(&self) -> SqlError {
        self.parser.unexpected()
    }

    fn is_operator_at(&self, ahead: usize, symbol: &str) -> bool {
        matches!(self.kind_at(ahead), TokenKind::Operator(found) if *found == symbol)
    }

    fn assignment_operator_at(&self, ahead: usize) -> usize {
        if self.is_operator_at(ahead, ":=") || self.is_operator_at(ahead, "=") {
            return 1;
        }
        if self.kind_at(ahead) == &TokenKind::Colon && self.is_operator_at(ahead + 1, "=") {
            return 2;
        }
        0
    }

    fn take_assignment_operator(&mut self) -> bool {
        let length = self.assignment_operator_at(0);
        self.parser.at += length;
        length > 0
    }

    fn expect_assignment_operator(&mut self) -> SqlResult<()> {
        if self.take_assignment_operator() {
            Ok(())
        } else {
            Err(self.unexpected())
        }
    }

    fn any_word(&mut self) -> SqlResult<String> {
        let text = match self.kind() {
            TokenKind::Word(word) => word.as_ref().to_owned(),
            TokenKind::QuotedIdentifier(name) => name.clone(),
            _ => return Err(self.unexpected()),
        };
        self.advance();
        Ok(text)
    }

    fn string_literal(&mut self) -> SqlResult<String> {
        let TokenKind::String(text) = self.kind() else {
            return Err(self.unexpected());
        };
        let text = text.as_ref().to_owned();
        self.advance();
        Ok(text)
    }

    fn count_step(&mut self) -> SqlResult<()> {
        self.counted += 1;
        if self.counted > MAX_STEPS {
            return Err(too_many_steps(self.position()));
        }
        Ok(())
    }

    fn program(&mut self) -> SqlResult<Program> {
        let mut variable_conflict = VariableConflict::Error;
        let mut print_strict_params = false;
        while self.is_operator_at(0, "#") {
            self.advance();
            let position = self.position();
            match self.any_word()?.as_str() {
                "variable_conflict" => {
                    variable_conflict = match self.any_word()?.as_str() {
                        "error" => VariableConflict::Error,
                        "use_variable" => VariableConflict::UseVariable,
                        "use_column" => VariableConflict::UseColumn,
                        _ => return Err(self.parser_error_before()),
                    };
                }
                "print_strict_params" => {
                    print_strict_params = match self.any_word()?.as_str() {
                        "on" | "true" => true,
                        "off" | "false" => false,
                        _ => return Err(self.parser_error_before()),
                    };
                }
                "option" => self.expect_word("dump")?,
                other => {
                    return Err(SqlError::syntax(
                        format!("unrecognized PL/pgSQL compiler option \"{other}\""),
                        position,
                    ));
                }
            }
        }
        let label = self.label()?;
        if !(self.is_word("declare") || self.is_word("begin")) {
            return Err(self.unexpected());
        }
        let block = self.block(label, 1)?;
        self.take(&TokenKind::Semicolon);
        if !self.parser.at_end() {
            return Err(self.unexpected());
        }
        Ok(Program {
            variable_conflict,
            print_strict_params,
            block,
        })
    }

    fn parser_error_before(&self) -> SqlError {
        let token = self
            .parser
            .at
            .checked_sub(1)
            .and_then(|index| self.parser.tokens.get(index));
        match token {
            Some(token) => SqlError::syntax(
                format!(
                    "syntax error at or near \"{}\"",
                    self.parser.slice(token.start, token.end)
                ),
                token.start,
            ),
            None => self.unexpected(),
        }
    }

    fn label(&mut self) -> SqlResult<Option<String>> {
        if !self.is_operator_at(0, "<<") {
            return Ok(None);
        }
        self.advance();
        let label = self.parser.name()?.text;
        if !self.parser.take_operator(">>") {
            return Err(self.unexpected());
        }
        Ok(Some(label))
    }

    fn end_label(&mut self, label: Option<&str>) -> SqlResult<()> {
        let position = self.position();
        let written = match self.kind() {
            TokenKind::Word(_) | TokenKind::QuotedIdentifier(_) => self.any_word()?,
            _ => return Ok(()),
        };
        match label {
            Some(label) if label == written => Ok(()),
            Some(label) => Err(SqlError::syntax(
                format!("end label \"{written}\" differs from block's label \"{label}\""),
                position,
            )),
            None => Err(SqlError::syntax(
                format!("end label \"{written}\" specified for unlabeled block"),
                position,
            )),
        }
    }

    fn block(&mut self, label: Option<String>, depth: usize) -> SqlResult<Block> {
        if depth > MAX_NESTING {
            return Err(nested_too_deeply(self.position()));
        }
        self.scopes.push(Scope {
            label: label.clone(),
            is_loop: false,
            variables: Vec::new(),
        });
        let block = self.block_contents(label, depth);
        self.scopes.pop();
        block
    }

    fn block_contents(&mut self, label: Option<String>, depth: usize) -> SqlResult<Block> {
        let position = self.position();
        let mut declarations = Vec::new();
        if self.take_word("declare") {
            while !self.is_word("begin") {
                if self.take_word("declare") {
                    continue;
                }
                if self.parser.at_end() {
                    return Err(self.unexpected());
                }
                self.count_step()?;
                declarations.push(self.declaration()?);
            }
        }
        self.expect_word("begin")?;
        let steps = self.steps(depth, &["exception", "end"])?;
        let mut handlers = Vec::new();
        if self.take_word("exception") {
            while self.is_word("when") {
                handlers.push(self.handler(depth)?);
            }
            if handlers.is_empty() {
                return Err(self.unexpected());
            }
        }
        self.expect_word("end")?;
        self.end_label(label.as_deref())?;
        Ok(Block {
            label,
            declarations,
            steps,
            handlers,
            position,
        })
    }

    fn handler(&mut self, depth: usize) -> SqlResult<Handler> {
        self.expect_word("when")?;
        let mut conditions = Vec::new();
        loop {
            conditions.extend(self.exception_condition()?);
            if !self.take_word("or") {
                break;
            }
        }
        self.expect_word("then")?;
        self.scopes.push(Scope {
            label: None,
            is_loop: false,
            variables: vec![Variable::plain("sqlstate"), Variable::plain("sqlerrm")],
        });
        let steps = self.steps(depth + 1, &["when", "end"]);
        self.scopes.pop();
        Ok(Handler {
            conditions,
            steps: steps?,
        })
    }

    fn exception_condition(&mut self) -> SqlResult<Vec<ExceptionCondition>> {
        let position = self.position();
        if self.take_word("sqlstate") {
            let code = self.string_literal()?;
            let state = SqlState::from_text(&code).ok_or_else(|| invalid_sqlstate(position))?;
            return Ok(vec![ExceptionCondition::State(state)]);
        }
        let name = self.any_word()?;
        if name == "others" {
            return Ok(vec![ExceptionCondition::Others]);
        }
        let states = condition_states(&name);
        if states.is_empty() {
            return Err(unrecognized_condition(&name, position));
        }
        Ok(states.into_iter().map(ExceptionCondition::State).collect())
    }

    fn declaration(&mut self) -> SqlResult<Declaration> {
        let position = self.position();
        let name = self.parser.name()?.text;
        if self.take_word("alias") {
            self.expect_word("for")?;
            let target = self.alias_target()?;
            self.expect_semicolon()?;
            self.declare(Variable::plain(&name));
            return Ok(Declaration {
                name,
                position,
                kind: DeclarationKind::Alias(target),
            });
        }
        let scroll = if self.is_word("no") && self.parser.is_word_at(1, "scroll") {
            self.parser.at += 2;
            Some(false)
        } else if self.is_word("scroll") && self.parser.is_word_at(1, "cursor") {
            self.advance();
            Some(true)
        } else {
            None
        };
        if self.take_word("cursor") {
            let cursor = self.cursor_declaration(scroll)?;
            self.declare(Variable {
                name: name.clone(),
                constant: false,
                cursor: true,
            });
            return Ok(Declaration {
                name,
                position,
                kind: DeclarationKind::Cursor(cursor),
            });
        }
        if scroll.is_some() {
            return Err(self.unexpected());
        }
        let constant = self.take_word("constant");
        let variable_type = self.variable_type()?;
        let collation = if self.take_word("collate") {
            Some(self.parser.table_name()?.text)
        } else {
            None
        };
        let not_null = if self.take_word("not") {
            self.expect_word("null")?;
            true
        } else {
            false
        };
        let default = if self.take_word("default") || self.take_assignment_operator() {
            Some(self.fragment(&[])?)
        } else {
            None
        };
        if not_null && default.is_none() {
            return Err(SqlError::syntax(
                format!("variable \"{name}\" must have a default value, since it's declared NOT NULL"),
                position,
            ));
        }
        self.expect_semicolon()?;
        self.declare(Variable {
            name: name.clone(),
            constant,
            cursor: false,
        });
        Ok(Declaration {
            name,
            position,
            kind: DeclarationKind::Variable(VariableDeclaration {
                variable_type,
                constant,
                not_null,
                collation,
                default,
            }),
        })
    }

    fn alias_target(&mut self) -> SqlResult<AliasTarget> {
        let position = self.position();
        if let TokenKind::Parameter(digits) = self.kind() {
            let number = digits.parse::<usize>().unwrap_or(0);
            if number == 0 || number > self.definition.parameters.len() {
                return Err(not_a_variable(&format!("${digits}"), position));
            }
            self.advance();
            return Ok(AliasTarget::Parameter(number));
        }
        let name = self.parser.name()?.text;
        if self.variable(&name).is_none() {
            return Err(SqlError::new(
                SqlState::UNDEFINED_OBJECT,
                format!("variable \"{name}\" does not exist"),
            )
            .at(position));
        }
        Ok(AliasTarget::Name(name))
    }

    fn cursor_declaration(&mut self, scroll: Option<bool>) -> SqlResult<CursorDeclaration> {
        let mut arguments = Vec::new();
        if self.take(&TokenKind::LeftParen) {
            loop {
                let name = self.parser.name()?.text;
                let variable_type = self.variable_type()?;
                arguments.push(CursorArgument {
                    name,
                    variable_type,
                    default: None,
                });
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.parser.expect(&TokenKind::RightParen)?;
        }
        if !(self.take_word("for") || self.take_word("is")) {
            return Err(self.unexpected());
        }
        let query = self.fragment(&[])?;
        self.expect_semicolon()?;
        Ok(CursorDeclaration {
            scroll,
            arguments,
            query,
        })
    }

    fn variable_type(&mut self) -> SqlResult<VariableType> {
        let start = self.parser.at;
        if let Some(copied) = self.copied_type()? {
            return Ok(copied);
        }
        self.parser.at = start;
        if self.take_word("record") {
            return Ok(VariableType::Record);
        }
        if self.take_word("refcursor") {
            return Ok(VariableType::RefCursor);
        }
        match self.parser.sql_type() {
            Ok(sql_type) => Ok(VariableType::Sql(sql_type)),
            Err(error) => {
                self.parser.at = start;
                match self.parser.table_name() {
                    Ok(name) if self.ends_declared_type() => Ok(VariableType::Row(name.text)),
                    _ => Err(error),
                }
            }
        }
    }

    fn copied_type(&mut self) -> SqlResult<Option<VariableType>> {
        let mut parts = Vec::new();
        loop {
            let part = match self.kind() {
                TokenKind::Word(word) => word.as_ref().to_owned(),
                TokenKind::QuotedIdentifier(name) => name.clone(),
                _ => return Ok(None),
            };
            parts.push(part);
            self.advance();
            if self.kind() != &TokenKind::Dot {
                break;
            }
            self.advance();
        }
        if !self.is_operator_at(0, "%") {
            return Ok(None);
        }
        self.advance();
        if self.take_word("type") {
            return Ok(Some(VariableType::TypeOf(parts)));
        }
        if self.take_word("rowtype") {
            return match parts.as_slice() {
                [table] => Ok(Some(VariableType::Row(table.clone()))),
                [schema, table] => Ok(Some(VariableType::Row(qualified(schema, table)))),
                _ => Err(self.parser_error_before()),
            };
        }
        Err(self.unexpected())
    }

    fn ends_declared_type(&self) -> bool {
        matches!(
            self.kind(),
            TokenKind::Semicolon | TokenKind::Comma | TokenKind::RightParen | TokenKind::Colon
        ) || self.is_word("collate")
            || self.is_word("not")
            || self.is_word("default")
            || self.assignment_operator_at(0) > 0
    }

    fn declare(&mut self, variable: Variable) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.variables.push(variable);
        }
    }

    fn variable(&self, name: &str) -> Option<&Variable> {
        self.scopes
            .iter()
            .rev()
            .flat_map(|scope| scope.variables.iter().rev())
            .find(|variable| variable.name == name)
    }

    fn labelled_variable(&self, label: &str, name: &str) -> Option<&Variable> {
        self.scopes
            .iter()
            .rev()
            .find(|scope| scope.label.as_deref() == Some(label))
            .and_then(|scope| scope.variables.iter().rev().find(|variable| variable.name == name))
    }

    fn is_label(&self, name: &str) -> bool {
        self.scopes
            .iter()
            .any(|scope| scope.label.as_deref() == Some(name))
    }

    fn check_target(&self, target: &Target) -> SqlResult<()> {
        if let Some(digits) = target.name.strip_prefix('$') {
            let number = digits.parse::<usize>().unwrap_or(0);
            if number == 0 || number > self.definition.parameters.len() {
                return Err(not_a_variable(&target.name, target.position));
            }
            return Ok(());
        }
        if let Some(TargetPart::Field(field)) = target.path.first()
            && self.is_label(&target.name)
            && let Some(variable) = self.labelled_variable(&target.name, field)
        {
            return check_constant(variable, target.path.len() == 1, target.position);
        }
        match self.variable(&target.name) {
            Some(variable) => check_constant(variable, target.path.is_empty(), target.position),
            None => Err(not_a_variable(&target.name, target.position)),
        }
    }

    fn check_targets(&self, targets: &[Target]) -> SqlResult<()> {
        targets.iter().try_for_each(|target| self.check_target(target))
    }

    fn require_variable(&self, name: &str, position: usize) -> SqlResult<()> {
        if self.variable(name).is_some() {
            Ok(())
        } else {
            Err(not_a_variable(name, position))
        }
    }

    fn is_cursor_at(&self, ahead: usize) -> bool {
        let TokenKind::Word(word) = self.kind_at(ahead) else {
            return false;
        };
        self.variable(word).is_some_and(|variable| variable.cursor)
    }

    fn target(&mut self) -> SqlResult<Target> {
        let position = self.position();
        let name = match self.kind() {
            TokenKind::Parameter(digits) => {
                let name = format!("${digits}");
                self.advance();
                name
            }
            _ => self.parser.name()?.text,
        };
        let mut path = Vec::new();
        loop {
            match self.kind() {
                TokenKind::Dot => {
                    self.advance();
                    path.push(TargetPart::Field(self.any_word()?));
                }
                TokenKind::LeftBracket => {
                    self.advance();
                    let index = self.fragment(&[])?;
                    self.parser.expect(&TokenKind::RightBracket)?;
                    path.push(TargetPart::Index(index));
                }
                _ => break,
            }
        }
        Ok(Target {
            name,
            path,
            position,
        })
    }

    fn target_list(&mut self) -> SqlResult<Vec<Target>> {
        let mut targets = vec![self.target()?];
        while self.take(&TokenKind::Comma) {
            targets.push(self.target()?);
        }
        Ok(targets)
    }

    fn into_targets(&mut self) -> SqlResult<IntoTargets> {
        let strict = self.take_word("strict");
        let targets = self.target_list()?;
        self.check_targets(&targets)?;
        Ok(IntoTargets {
            strict,
            bulk: false,
            targets,
        })
    }

    fn assignment_ahead(&self) -> bool {
        if !matches!(
            self.kind(),
            TokenKind::Word(_) | TokenKind::QuotedIdentifier(_) | TokenKind::Parameter(_)
        ) {
            return false;
        }
        let mut ahead = 1;
        loop {
            match self.kind_at(ahead) {
                TokenKind::Dot
                    if matches!(
                        self.kind_at(ahead + 1),
                        TokenKind::Word(_) | TokenKind::QuotedIdentifier(_)
                    ) =>
                {
                    ahead += 2;
                }
                TokenKind::LeftBracket => {
                    let mut depth = 0usize;
                    loop {
                        match self.kind_at(ahead) {
                            TokenKind::LeftBracket => depth += 1,
                            TokenKind::RightBracket => {
                                depth = depth.saturating_sub(1);
                                if depth == 0 {
                                    ahead += 1;
                                    break;
                                }
                            }
                            TokenKind::End | TokenKind::Semicolon => return false,
                            _ => {}
                        }
                        ahead += 1;
                    }
                }
                _ => break,
            }
        }
        self.assignment_operator_at(ahead) > 0
    }

    fn range_ahead(&self) -> bool {
        let at = self.parser.at;
        let (Some(dot), Some(next)) = (self.parser.tokens.get(at), self.parser.tokens.get(at + 1))
        else {
            return false;
        };
        dot.kind == TokenKind::Dot
            && next.start == dot.end
            && match &next.kind {
                TokenKind::Dot => true,
                TokenKind::Decimal(text) => text.starts_with('.'),
                _ => false,
            }
    }

    fn stops_here(&self, stops: &[Stop]) -> bool {
        stops.iter().any(|stop| match stop {
            Stop::Comma => self.kind() == &TokenKind::Comma,
            Stop::Range => self.range_ahead(),
            Stop::Word(word) => self.is_word(word),
        })
    }

    fn fragment(&mut self, stops: &[Stop]) -> SqlResult<Fragment> {
        let first = self.parser.at;
        let mut parens = 0usize;
        let mut cases = 0usize;
        loop {
            let top = parens == 0 && cases == 0;
            match self.kind() {
                TokenKind::End => break,
                TokenKind::Semicolon if parens == 0 => break,
                _ if top && self.stops_here(stops) => break,
                TokenKind::LeftParen | TokenKind::LeftBracket => parens += 1,
                TokenKind::RightParen | TokenKind::RightBracket => {
                    if parens == 0 {
                        break;
                    }
                    parens -= 1;
                }
                TokenKind::Word(word) if word == "case" => cases += 1,
                TokenKind::Word(word) if word == "end" => {
                    if top {
                        break;
                    }
                    cases = cases.saturating_sub(1);
                }
                _ => {}
            }
            self.advance();
        }
        self.fragment_from(first)
    }

    fn fragment_from(&self, first: usize) -> SqlResult<Fragment> {
        if self.parser.at <= first {
            return Err(self.unexpected());
        }
        let start = self.parser.tokens.get(first).map_or(0, |token| token.start);
        let end = self.parser.previous_end();
        Ok(Fragment {
            text: self.parser.slice(start, end),
            position: start,
        })
    }

    fn fragment_list(&mut self, stops: &[Stop]) -> SqlResult<Vec<Fragment>> {
        let mut all = stops.to_vec();
        all.push(Stop::Comma);
        let mut list = vec![self.fragment(&all)?];
        while self.take(&TokenKind::Comma) {
            list.push(self.fragment(&all)?);
        }
        Ok(list)
    }

    fn steps(&mut self, depth: usize, stops: &[&str]) -> SqlResult<Vec<Step>> {
        let mut steps = Vec::new();
        loop {
            while self.take(&TokenKind::Semicolon) {}
            if stops.iter().any(|stop| self.is_word(stop)) {
                return Ok(steps);
            }
            if self.parser.at_end() {
                return Err(self.unexpected());
            }
            self.count_step()?;
            steps.push(self.step(depth)?);
        }
    }

    fn step(&mut self, depth: usize) -> SqlResult<Step> {
        let position = self.position();
        if depth > MAX_NESTING {
            return Err(nested_too_deeply(position));
        }
        let label = self.label()?;
        let kind = if label.is_some() {
            self.labelled_step(label, depth)?
        } else if self.assignment_ahead() {
            self.assignment()?
        } else {
            self.keyword_step(depth)?
        };
        Ok(Step { kind, position })
    }

    fn labelled_step(&mut self, label: Option<String>, depth: usize) -> SqlResult<StepKind> {
        let TokenKind::Word(word) = self.kind() else {
            return Err(self.unexpected());
        };
        match word.as_ref() {
            "declare" | "begin" => self.nested_block(label, depth),
            "loop" | "while" | "for" | "foreach" => self.loop_step(label, depth),
            _ => Err(self.unexpected()),
        }
    }

    fn nested_block(&mut self, label: Option<String>, depth: usize) -> SqlResult<StepKind> {
        let block = self.block(label, depth + 1)?;
        self.expect_semicolon()?;
        Ok(StepKind::Block(block))
    }

    fn keyword_step(&mut self, depth: usize) -> SqlResult<StepKind> {
        let TokenKind::Word(word) = self.kind() else {
            return self.sql_step();
        };
        match word.as_ref() {
            "declare" | "begin" => self.nested_block(None, depth),
            "if" => self.if_step(depth),
            "case" => self.case_step(depth),
            "loop" | "while" | "for" | "foreach" => self.loop_step(None, depth),
            "exit" | "continue" => self.exit_step(),
            "return" => self.return_step(),
            "raise" => self.raise_step(),
            "assert" => self.assert_step(),
            "perform" => self.perform_step(),
            "execute" => self.execute_step(),
            "get" => self.diagnostics_step(),
            "open" => self.open_step(),
            "fetch" => self.fetch_step(false),
            "move" => self.fetch_step(true),
            "close" => self.close_step(),
            "null" => {
                self.advance();
                self.expect_semicolon()?;
                Ok(StepKind::Null)
            }
            "commit" | "rollback" => self.transaction_step(),
            "start" | "savepoint" | "release" => Err(unsupported_transaction_command(self.position())),
            "end" | "else" | "elsif" | "elseif" | "when" | "then" | "exception" | "loop" => {
                Err(self.unexpected())
            }
            "call" => {
                let call = self.fragment(&[])?;
                self.expect_semicolon()?;
                Ok(StepKind::Call(call))
            }
            _ => self.sql_step(),
        }
    }

    fn assignment(&mut self) -> SqlResult<StepKind> {
        let target = self.target()?;
        self.check_target(&target)?;
        self.expect_assignment_operator()?;
        let value = self.fragment(&[])?;
        self.expect_semicolon()?;
        Ok(StepKind::Assign { target, value })
    }

    fn follows_into_keyword(&self) -> bool {
        self.parser
            .at
            .checked_sub(1)
            .and_then(|index| self.parser.tokens.get(index))
            .is_some_and(|token| matches!(&token.kind, TokenKind::Word(word) if word == "insert" || word == "merge"))
    }

    fn sql_step(&mut self) -> SqlResult<StepKind> {
        let first = self.parser.at;
        let finds_into = match self.kind() {
            TokenKind::Word(word) => STATEMENTS_WITH_INTO
                .iter()
                .any(|known| word == known),
            TokenKind::LeftParen => true,
            _ => false,
        };
        let mut parens = 0usize;
        let mut into: Option<(usize, usize, IntoTargets)> = None;
        loop {
            match self.kind() {
                TokenKind::End => break,
                TokenKind::Semicolon if parens == 0 => break,
                TokenKind::LeftParen | TokenKind::LeftBracket => parens += 1,
                TokenKind::RightParen | TokenKind::RightBracket => parens = parens.saturating_sub(1),
                TokenKind::Word(word)
                    if word == "into"
                        && parens == 0
                        && finds_into
                        && into.is_none()
                        && !self.follows_into_keyword() =>
                {
                    let into_start = self.position();
                    self.advance();
                    let targets = self.into_targets()?;
                    into = Some((into_start, self.parser.previous_end(), targets));
                    continue;
                }
                _ => {}
            }
            self.advance();
        }
        let whole = self.fragment_from(first)?;
        let statement = match &into {
            Some((into_start, into_end, _)) => {
                let end = whole.position + whole.text.len();
                let before = self.parser.slice(whole.position, *into_start);
                let after = self.parser.slice(*into_end, end);
                Fragment {
                    text: format!("{} {}", before.trim_end(), after.trim_start())
                        .trim()
                        .to_owned(),
                    position: whole.position,
                }
            }
            None => whole,
        };
        self.expect_semicolon()?;
        Ok(StepKind::Sql {
            statement,
            into: into.map(|(_, _, targets)| targets),
        })
    }

    fn perform_step(&mut self) -> SqlResult<StepKind> {
        self.advance();
        let query = self.fragment(&[])?;
        self.expect_semicolon()?;
        Ok(StepKind::Perform(Fragment {
            text: format!("SELECT {}", query.text),
            position: query.position,
        }))
    }

    fn dynamic_source(&mut self, stops: &[Stop]) -> SqlResult<QuerySource> {
        let mut command_stops = stops.to_vec();
        command_stops.push(Stop::Word("using"));
        let command = self.fragment(&command_stops)?;
        let using = if self.take_word("using") {
            self.fragment_list(stops)?
        } else {
            Vec::new()
        };
        Ok(QuerySource::Dynamic { command, using })
    }

    fn execute_step(&mut self) -> SqlResult<StepKind> {
        self.advance();
        let command = self.fragment(&[Stop::Word("into"), Stop::Word("using")])?;
        let mut into = None;
        let mut using = None;
        loop {
            let position = self.position();
            if self.take_word("into") {
                if into.is_some() {
                    return Err(SqlError::syntax("INTO specified more than once", position));
                }
                into = Some(self.into_targets()?);
            } else if self.take_word("using") {
                if using.is_some() {
                    return Err(SqlError::syntax("USING specified more than once", position));
                }
                using = Some(self.fragment_list(&[Stop::Word("into")])?);
            } else {
                break;
            }
        }
        self.expect_semicolon()?;
        Ok(StepKind::Execute(DynamicSql {
            command,
            into,
            using: using.unwrap_or_default(),
        }))
    }

    fn diagnostics_step(&mut self) -> SqlResult<StepKind> {
        self.advance();
        let stacked = if self.take_word("stacked") {
            true
        } else {
            self.take_word("current");
            false
        };
        self.expect_word("diagnostics")?;
        let mut items = Vec::new();
        loop {
            let target = self.target()?;
            self.check_target(&target)?;
            self.expect_assignment_operator()?;
            let position = self.position();
            let word = self.any_word()?;
            let Some(item) = DiagnosticsItem::of_name(&word) else {
                return Err(SqlError::syntax("unrecognized GET DIAGNOSTICS item", position));
            };
            if item.is_stacked() != stacked {
                return Err(SqlError::syntax(
                    format!(
                        "diagnostics item {} is not allowed in GET {} DIAGNOSTICS",
                        item.name().to_ascii_uppercase(),
                        if stacked { "STACKED" } else { "CURRENT" }
                    ),
                    position,
                ));
            }
            items.push((target, item));
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.expect_semicolon()?;
        Ok(StepKind::GetDiagnostics { stacked, items })
    }

    fn if_step(&mut self, depth: usize) -> SqlResult<StepKind> {
        self.advance();
        let mut branches = Vec::new();
        loop {
            let condition = self.fragment(&[Stop::Word("then")])?;
            self.expect_word("then")?;
            let steps = self.steps(depth + 1, &["elsif", "elseif", "else", "end"])?;
            branches.push(Branch { condition, steps });
            if !(self.take_word("elsif") || self.take_word("elseif")) {
                break;
            }
        }
        let otherwise = if self.take_word("else") {
            Some(self.steps(depth + 1, &["end"])?)
        } else {
            None
        };
        self.expect_word("end")?;
        self.expect_word("if")?;
        self.expect_semicolon()?;
        Ok(StepKind::If {
            branches,
            otherwise,
        })
    }

    fn case_step(&mut self, depth: usize) -> SqlResult<StepKind> {
        self.advance();
        let subject = if self.is_word("when") {
            None
        } else {
            Some(self.fragment(&[Stop::Word("when")])?)
        };
        let mut arms = Vec::new();
        while self.take_word("when") {
            let values = if subject.is_some() {
                self.fragment_list(&[Stop::Word("then")])?
            } else {
                vec![self.fragment(&[Stop::Word("then")])?]
            };
            self.expect_word("then")?;
            let steps = self.steps(depth + 1, &["when", "else", "end"])?;
            arms.push(CaseArm { values, steps });
        }
        if arms.is_empty() {
            return Err(self.unexpected());
        }
        let otherwise = if self.take_word("else") {
            Some(self.steps(depth + 1, &["end"])?)
        } else {
            None
        };
        self.expect_word("end")?;
        self.expect_word("case")?;
        self.expect_semicolon()?;
        Ok(StepKind::Case {
            subject,
            arms,
            otherwise,
        })
    }

    fn loop_step(&mut self, label: Option<String>, depth: usize) -> SqlResult<StepKind> {
        let opening = self.any_word()?;
        let mut loop_variables = Vec::new();
        let kind = match opening.as_str() {
            "loop" => LoopKind::Plain,
            "while" => LoopKind::While(self.fragment(&[Stop::Word("loop")])?),
            "for" => self.for_head(&mut loop_variables)?,
            "foreach" => self.foreach_head()?,
            _ => return Err(self.parser_error_before()),
        };
        if opening != "loop" {
            self.expect_word("loop")?;
        }
        self.scopes.push(Scope {
            label: label.clone(),
            is_loop: true,
            variables: loop_variables,
        });
        let body = self.steps(depth + 1, &["end"]);
        self.scopes.pop();
        let body = body?;
        self.expect_word("end")?;
        self.expect_word("loop")?;
        self.end_label(label.as_deref())?;
        self.expect_semicolon()?;
        Ok(StepKind::Loop(Loop { label, kind, body }))
    }

    fn for_head(&mut self, loop_variables: &mut Vec<Variable>) -> SqlResult<LoopKind> {
        let targets = self.target_list()?;
        self.expect_word("in")?;
        if self.take_word("execute") {
            self.check_targets(&targets)?;
            let source = self.dynamic_source(&[Stop::Word("loop")])?;
            return Ok(LoopKind::Query { targets, source });
        }
        let single = match targets.as_slice() {
            [single] if single.path.is_empty() && !single.name.starts_with('$') => Some(single.name.clone()),
            _ => None,
        };
        if let Some(record) = &single
            && self.is_cursor_at(0)
            && (self.kind_at(1) == &TokenKind::LeftParen || self.parser.is_word_at(1, "loop"))
        {
            let cursor = self.any_word()?;
            let arguments = if self.kind() == &TokenKind::LeftParen {
                self.cursor_values()?
            } else {
                Vec::new()
            };
            loop_variables.push(Variable::plain(record));
            return Ok(LoopKind::Cursor {
                record: record.clone(),
                cursor,
                arguments,
            });
        }
        let reverse_position = self.position();
        let reverse = self.take_word("reverse");
        let lower = self.fragment(&[Stop::Range, Stop::Word("loop")])?;
        if self.range_ahead() {
            let Some(variable) = single else {
                return Err(SqlError::syntax(
                    "integer FOR loop must have only one target variable",
                    reverse_position,
                ));
            };
            let upper = self.range_upper()?;
            let step = if self.take_word("by") {
                Some(self.fragment(&[Stop::Word("loop")])?)
            } else {
                None
            };
            loop_variables.push(Variable::plain(&variable));
            return Ok(LoopKind::Range {
                variable,
                reverse,
                lower,
                upper,
                step,
            });
        }
        if reverse {
            return Err(SqlError::syntax(
                "syntax error at or near \"reverse\"",
                reverse_position,
            ));
        }
        self.check_targets(&targets)?;
        Ok(LoopKind::Query {
            targets,
            source: QuerySource::Static(lower),
        })
    }

    fn range_upper(&mut self) -> SqlResult<Fragment> {
        let joined_with_number = matches!(
            self.parser.tokens.get(self.parser.at + 1).map(|token| &token.kind),
            Some(TokenKind::Decimal(_))
        );
        self.parser.at += if joined_with_number { 1 } else { 2 };
        let upper = self.fragment(&[Stop::Word("by"), Stop::Word("loop")])?;
        if !joined_with_number {
            return Ok(upper);
        }
        Ok(Fragment {
            text: upper.text.get(1..).unwrap_or_default().to_owned(),
            position: upper.position + 1,
        })
    }

    fn foreach_head(&mut self) -> SqlResult<LoopKind> {
        let targets = self.target_list()?;
        self.check_targets(&targets)?;
        let slice = if self.take_word("slice") {
            let TokenKind::Integer(digits) = self.kind() else {
                return Err(self.unexpected());
            };
            let Ok(slice) = digits.parse::<usize>() else {
                return Err(self.unexpected());
            };
            self.advance();
            slice
        } else {
            0
        };
        self.expect_word("in")?;
        self.expect_word("array")?;
        let array = self.fragment(&[Stop::Word("loop")])?;
        Ok(LoopKind::Foreach {
            targets,
            slice,
            array,
        })
    }

    fn cursor_values(&mut self) -> SqlResult<Vec<CursorValue>> {
        self.parser.expect(&TokenKind::LeftParen)?;
        let mut values = Vec::new();
        loop {
            let named = matches!(self.kind(), TokenKind::Word(_) | TokenKind::QuotedIdentifier(_))
                && (self.assignment_operator_at(1) > 0 || self.is_operator_at(1, "=>"));
            let name = if named {
                let name = self.any_word()?;
                if !self.parser.take_operator("=>") {
                    self.expect_assignment_operator()?;
                }
                Some(name)
            } else {
                None
            };
            let value = self.fragment(&[Stop::Comma])?;
            values.push(CursorValue { name, value });
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.parser.expect(&TokenKind::RightParen)?;
        Ok(values)
    }

    fn exit_step(&mut self) -> SqlResult<StepKind> {
        let position = self.position();
        let exits = self.is_word("exit");
        self.advance();
        let label = match self.kind() {
            TokenKind::Word(word) if word != "when" => Some(self.any_word()?),
            TokenKind::QuotedIdentifier(_) => Some(self.any_word()?),
            _ => None,
        };
        let condition = if self.take_word("when") {
            Some(self.fragment(&[])?)
        } else {
            None
        };
        self.expect_semicolon()?;
        self.check_exit(exits, label.as_deref(), position)?;
        Ok(if exits {
            StepKind::Exit { label, condition }
        } else {
            StepKind::Continue { label, condition }
        })
    }

    fn check_exit(&self, exits: bool, label: Option<&str>, position: usize) -> SqlResult<()> {
        let Some(label) = label else {
            if self.scopes.iter().any(|scope| scope.is_loop) {
                return Ok(());
            }
            let message = if exits {
                "EXIT cannot be used outside a loop, unless it has a label"
            } else {
                "CONTINUE cannot be used outside a loop"
            };
            return Err(SqlError::syntax(message, position));
        };
        match self
            .scopes
            .iter()
            .rev()
            .find(|scope| scope.label.as_deref() == Some(label))
        {
            None => Err(SqlError::syntax(
                format!("there is no label \"{label}\" attached to any block or loop enclosing this statement"),
                position,
            )),
            Some(scope) if !exits && !scope.is_loop => Err(SqlError::syntax(
                format!("block label \"{label}\" cannot be used in CONTINUE"),
                position,
            )),
            Some(_) => Ok(()),
        }
    }

    fn optional_value(&mut self) -> SqlResult<Option<Fragment>> {
        let value = if self.kind() == &TokenKind::Semicolon {
            None
        } else {
            Some(self.fragment(&[])?)
        };
        self.expect_semicolon()?;
        Ok(value)
    }

    fn return_step(&mut self) -> SqlResult<StepKind> {
        let position = self.position();
        self.advance();
        if self.take_word("next") {
            let value = self.optional_value()?;
            self.check_return_next(value.is_some(), position)?;
            return Ok(StepKind::ReturnNext(value));
        }
        if self.take_word("query") {
            if !self.definition.returns.is_set() {
                return Err(mismatch("cannot use RETURN QUERY in a non-SETOF function", position));
            }
            let source = if self.take_word("execute") {
                self.dynamic_source(&[])?
            } else {
                QuerySource::Static(self.fragment(&[])?)
            };
            self.expect_semicolon()?;
            return Ok(StepKind::ReturnQuery(source));
        }
        let value = self.optional_value()?;
        self.check_return(value.is_some(), position)?;
        Ok(StepKind::Return(value))
    }

    fn check_return(&self, has_value: bool, position: usize) -> SqlResult<()> {
        let definition = self.definition;
        if definition.kind == RoutineKind::Procedure {
            return if has_value {
                Err(mismatch("RETURN cannot have a parameter in a procedure", position))
            } else {
                Ok(())
            };
        }
        if definition.returns.is_set() {
            return if has_value {
                Err(mismatch("RETURN cannot have a parameter in function returning set", position)
                    .with_hint("Use RETURN NEXT or RETURN QUERY."))
            } else {
                Ok(())
            };
        }
        if definition.returns == RoutineReturns::One(ReturnType::Void) {
            return if has_value {
                Err(mismatch("RETURN cannot have a parameter in function returning void", position))
            } else {
                Ok(())
            };
        }
        if has_output_parameters(definition) {
            return if has_value {
                Err(mismatch("RETURN cannot have a parameter in function with OUT parameters", position))
            } else {
                Ok(())
            };
        }
        if has_value {
            Ok(())
        } else {
            Err(SqlError::syntax("missing expression at or near \";\"", position))
        }
    }

    fn check_return_next(&self, has_value: bool, position: usize) -> SqlResult<()> {
        if !self.definition.returns.is_set() {
            return Err(mismatch("cannot use RETURN NEXT in a non-SETOF function", position));
        }
        match (has_output_parameters(self.definition), has_value) {
            (true, true) => Err(mismatch(
                "RETURN NEXT cannot have a parameter in function with OUT parameters",
                position,
            )),
            (false, false) => Err(SqlError::syntax("RETURN NEXT must have a parameter", position)),
            _ => Ok(()),
        }
    }

    fn raise_step(&mut self) -> SqlResult<StepKind> {
        self.advance();
        if self.take(&TokenKind::Semicolon) {
            return Ok(StepKind::Reraise);
        }
        let level = match self.kind() {
            TokenKind::Word(word) => RaiseLevel::of_word(word),
            _ => None,
        };
        if level.is_some() {
            self.advance();
        }
        let mut raise = Raise {
            level: level.unwrap_or(RaiseLevel::Exception),
            condition: None,
            format: None,
            arguments: Vec::new(),
            options: Vec::new(),
        };
        let position = self.position();
        match self.kind() {
            TokenKind::String(_) => {
                let format = self.string_literal()?;
                while self.take(&TokenKind::Comma) {
                    raise
                        .arguments
                        .push(self.fragment(&[Stop::Comma, Stop::Word("using")])?);
                }
                let wanted = format_placeholders(&format);
                if wanted > raise.arguments.len() {
                    return Err(SqlError::syntax("too few parameters specified for RAISE", position));
                }
                if wanted < raise.arguments.len() {
                    return Err(SqlError::syntax("too many parameters specified for RAISE", position));
                }
                raise.format = Some(format);
            }
            TokenKind::Word(word) if word == "using" => {}
            TokenKind::Word(word) if word == "sqlstate" => {
                self.advance();
                let code = self.string_literal()?;
                let state = SqlState::from_text(&code).ok_or_else(|| invalid_sqlstate(position))?;
                raise.condition = Some(RaiseCondition::State(state));
            }
            TokenKind::Word(_) | TokenKind::QuotedIdentifier(_) => {
                let name = self.any_word()?;
                let Some(state) = condition_states(&name).first().copied() else {
                    return Err(unrecognized_condition(&name, position));
                };
                raise.condition = Some(RaiseCondition::Name { name, state });
            }
            _ => return Err(self.unexpected()),
        }
        if self.take_word("using") {
            loop {
                let option_position = self.position();
                let word = self.any_word()?;
                let Some(item) = RaiseItem::of_word(&word) else {
                    return Err(SqlError::syntax(
                        format!("unrecognized RAISE statement option \"{word}\""),
                        option_position,
                    ));
                };
                let repeated = raise.options.iter().any(|option| option.item == item)
                    || (item == RaiseItem::Message && raise.format.is_some())
                    || (item == RaiseItem::Errcode && raise.condition.is_some());
                if repeated {
                    return Err(SqlError::syntax(
                        format!("RAISE option already specified: {}", item.keyword()),
                        option_position,
                    ));
                }
                self.expect_assignment_operator()?;
                let value = self.fragment(&[Stop::Comma])?;
                raise.options.push(RaiseOption { item, value });
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
        } else if raise.format.is_none() && raise.condition.is_none() {
            return Err(self.unexpected());
        }
        self.expect_semicolon()?;
        Ok(StepKind::Raise(raise))
    }

    fn assert_step(&mut self) -> SqlResult<StepKind> {
        self.advance();
        let condition = self.fragment(&[Stop::Comma])?;
        let message = if self.take(&TokenKind::Comma) {
            Some(self.fragment(&[])?)
        } else {
            None
        };
        self.expect_semicolon()?;
        Ok(StepKind::Assert { condition, message })
    }

    fn cursor_name(&mut self) -> SqlResult<String> {
        let position = self.position();
        let name = self.parser.name()?.text;
        self.require_variable(&name, position)?;
        Ok(name)
    }

    fn open_step(&mut self) -> SqlResult<StepKind> {
        self.advance();
        let cursor = self.cursor_name()?;
        let scroll = if self.take_word("no") {
            self.expect_word("scroll")?;
            Some(false)
        } else if self.take_word("scroll") {
            Some(true)
        } else {
            None
        };
        let source = if self.take_word("for") {
            if self.take_word("execute") {
                OpenSource::Query(self.dynamic_source(&[])?)
            } else {
                OpenSource::Query(QuerySource::Static(self.fragment(&[])?))
            }
        } else if self.kind() == &TokenKind::LeftParen {
            OpenSource::Bound(self.cursor_values()?)
        } else {
            OpenSource::Bound(Vec::new())
        };
        self.expect_semicolon()?;
        Ok(StepKind::Open {
            cursor,
            scroll,
            source,
        })
    }

    fn fetch_step(&mut self, moves: bool) -> SqlResult<StepKind> {
        self.advance();
        let direction = self.fetch_direction(moves)?;
        let cursor = self.cursor_name()?;
        if moves {
            self.expect_semicolon()?;
            return Ok(StepKind::Move { cursor, direction });
        }
        self.expect_word("into")?;
        let into = self.into_targets()?;
        self.expect_semicolon()?;
        Ok(StepKind::Fetch {
            cursor,
            direction,
            into,
            limit: None,
        })
    }

    fn fetch_direction(&mut self, moves: bool) -> SqlResult<FetchDirection> {
        if self.take_word("from") || self.take_word("in") {
            return Ok(FetchDirection::Next);
        }
        let TokenKind::Word(word) = self.kind() else {
            return Ok(FetchDirection::Next);
        };
        let names_cursor = matches!(self.kind_at(1), TokenKind::Semicolon | TokenKind::End)
            || self.parser.is_word_at(1, "into");
        let word = word.as_ref().to_owned();
        if names_cursor {
            return Ok(FetchDirection::Next);
        }
        let position = self.position();
        let counted = [Stop::Word("from"), Stop::Word("in")];
        let direction = match word.as_str() {
            "next" => {
                self.advance();
                FetchDirection::Next
            }
            "prior" => {
                self.advance();
                FetchDirection::Prior
            }
            "first" => {
                self.advance();
                FetchDirection::First
            }
            "last" => {
                self.advance();
                FetchDirection::Last
            }
            "absolute" => {
                self.advance();
                FetchDirection::Absolute(self.fragment(&counted)?)
            }
            "relative" => {
                self.advance();
                FetchDirection::Relative(self.fragment(&counted)?)
            }
            "forward" | "backward" => {
                self.advance();
                let forward = word == "forward";
                let direction = if self.take_word("all") {
                    if forward {
                        FetchDirection::ForwardAll
                    } else {
                        FetchDirection::BackwardAll
                    }
                } else if self.is_word("from") || self.is_word("in") {
                    if forward {
                        FetchDirection::Forward(None)
                    } else {
                        FetchDirection::Backward(None)
                    }
                } else {
                    let count = Some(self.fragment(&counted)?);
                    if forward {
                        FetchDirection::Forward(count)
                    } else {
                        FetchDirection::Backward(count)
                    }
                };
                let multiple = matches!(
                    direction,
                    FetchDirection::ForwardAll
                        | FetchDirection::BackwardAll
                        | FetchDirection::Forward(Some(_))
                        | FetchDirection::Backward(Some(_))
                );
                if multiple && !moves {
                    return Err(SqlError::syntax("FETCH statement cannot return multiple rows", position));
                }
                direction
            }
            _ => return Ok(FetchDirection::Next),
        };
        if !(self.take_word("from") || self.take_word("in")) {
            return Err(self.unexpected());
        }
        Ok(direction)
    }

    fn close_step(&mut self) -> SqlResult<StepKind> {
        self.advance();
        let cursor = self.cursor_name()?;
        self.expect_semicolon()?;
        Ok(StepKind::Close(cursor))
    }

    fn transaction_step(&mut self) -> SqlResult<StepKind> {
        let position = self.position();
        let commits = self.is_word("commit");
        self.advance();
        if self.is_word("to") || self.is_word("prepared") {
            return Err(unsupported_transaction_command(position));
        }
        if !self.take_word("work") {
            self.take_word("transaction");
        }
        let chain = if self.take_word("and") {
            let no = self.take_word("no");
            self.expect_word("chain")?;
            !no
        } else {
            false
        };
        self.expect_semicolon()?;
        Ok(if commits {
            StepKind::Commit { chain }
        } else {
            StepKind::Rollback { chain }
        })
    }
}

fn check_constant(variable: &Variable, whole: bool, position: usize) -> SqlResult<()> {
    if variable.constant && whole {
        return Err(SqlError::new(
            SqlState::ERROR_IN_ASSIGNMENT,
            format!("variable \"{}\" is declared CONSTANT", variable.name),
        )
        .at(position));
    }
    Ok(())
}

impl<'t> Parser<'t> {
    pub(crate) fn with_routine_frame(mut self, frame: &'t crate::plpgsql::RoutineFrame) -> Self {
        self.routine_frame = Some(frame);
        self
    }

    pub(super) fn routine_primary(&mut self) -> SqlResult<Option<crate::ast::Expr>> {
        let Some(frame) = self.routine_frame else {
            return Ok(None);
        };
        let position = self.position();
        let word = match self.peek_kind() {
            TokenKind::Parameter(digits) => {
                let Some(expr) = frame.positional_expr(digits, position) else {
                    return Ok(None);
                };
                self.at += 1;
                return Ok(Some(expr));
            }
            TokenKind::Word(word) => word.as_ref(),
            TokenKind::QuotedIdentifier(name) => name.as_str(),
            _ => return Ok(None),
        };
        match self.peek_kind_at(1) {
            TokenKind::LeftParen => Ok(None),
            TokenKind::Dot => {
                let field = match self.peek_kind_at(2) {
                    TokenKind::Word(field) => field.as_ref(),
                    TokenKind::QuotedIdentifier(field) => field.as_str(),
                    _ => return Ok(None),
                };
                if self.peek_kind_at(3) == &TokenKind::LeftParen {
                    return Ok(None);
                }
                let Some(expr) = frame.qualified_expr(word, field, position)? else {
                    return Ok(None);
                };
                self.at += 3;
                Ok(Some(expr))
            }
            _ => {
                let Some(expr) = frame.variable_expr(word, position) else {
                    return Ok(None);
                };
                self.at += 1;
                Ok(Some(expr))
            }
        }
    }
}
