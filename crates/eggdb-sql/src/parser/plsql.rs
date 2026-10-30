use super::Parser;
use crate::ast::Name;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::{Token, TokenKind, swap_uniform_case};
use crate::plpgsql::{
    Block, Branch, CaseArm, CollectionMethod, CursorArgument, CursorDeclaration, CursorValue,
    Declaration, DeclarationKind, DynamicSql, ExceptionCondition, FetchDirection, Forall,
    ForallBounds, Fragment, Handler, IntoTargets, Loop, LoopKind, MAX_NESTING, MAX_STEPS,
    OpenSource, Program, QuerySource, RaisedException, RecordField, Step, StepKind, Subprogram,
    Target, TargetPart, TypeDeclaration, VariableConflict, VariableDeclaration, VariableType,
};
use crate::plsql::{
    DeclaredType, NO_DATA_FOUND_CODE, PlsqlPackage, PlsqlRoutine, PlsqlUnit, TriggerRecords,
    compilation_error, is_ref_cursor, predefined_by_name,
};
use crate::routines::{
    ParameterMode, ReturnType, RoutineDefinition, RoutineKind, RoutineLanguage, RoutineParameter,
    RoutineReturns, Volatility,
};
use crate::schemas::qualified;
use crate::types::SqlType;

const STATEMENTS_WITH_INTO: [&str; 6] = ["select", "with", "insert", "update", "delete", "merge"];

const SQL_STATEMENT_WORDS: [&str; 9] = [
    "select", "with", "insert", "update", "delete", "merge", "lock", "savepoint", "set",
];

const FORALL_STATEMENT_WORDS: [&str; 5] = ["insert", "update", "delete", "merge", "execute"];

const FORALL_BOUND_STOPS: [Stop; 6] = [
    Stop::Word("insert"),
    Stop::Word("update"),
    Stop::Word("delete"),
    Stop::Word("merge"),
    Stop::Word("execute"),
    Stop::Word("save"),
];

const INTEGER_TYPES: [&str; 8] = [
    "pls_integer",
    "binary_integer",
    "simple_integer",
    "natural",
    "naturaln",
    "positive",
    "positiven",
    "signtype",
];

const IGNORED_PRAGMAS: [&str; 7] = [
    "serially_reusable",
    "restrict_references",
    "inline",
    "udf",
    "deprecated",
    "coverage",
    "suppresses_warning_6009",
];

const TRIGGER_PREDICATES: [&str; 3] = ["inserting", "updating", "deleting"];

const MAX_STRING_LENGTH: u32 = 32767;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Comma,
    Range,
    Assignment,
    Word(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Binding {
    Value { constant: bool },
    Cursor,
    Exception { identity: String, code: Option<u16> },
    Type { ref_cursor: bool },
    Subprogram,
}

#[derive(Debug, Clone)]
struct Variable {
    name: String,
    binding: Binding,
}

impl Variable {
    fn new(name: &str, binding: Binding) -> Self {
        Self {
            name: name.to_owned(),
            binding,
        }
    }

    fn value(name: &str) -> Self {
        Self::new(name, Binding::Value { constant: false })
    }

    fn constant(name: &str) -> Self {
        Self::new(name, Binding::Value { constant: true })
    }
}

#[derive(Debug, Clone)]
struct Scope {
    label: Option<String>,
    is_loop: bool,
    variables: Vec<Variable>,
}

impl Scope {
    fn new(label: Option<String>, is_loop: bool, variables: Vec<Variable>) -> Self {
        Self {
            label,
            is_loop,
            variables,
        }
    }
}

#[derive(Debug, Clone)]
struct Unit {
    kind: RoutineKind,
    pipelined: bool,
    floor: usize,
    name: String,
    package: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
struct SubprogramOptions {
    deterministic: bool,
    pipelined: bool,
    current_user: bool,
}

#[derive(Debug, Clone)]
struct Heading {
    kind: RoutineKind,
    name: Name,
    local_name: String,
    parameters: Vec<RoutineParameter>,
    declared: Vec<DeclaredType>,
    returns: Option<DeclaredType>,
    options: SubprogramOptions,
}

impl Heading {
    fn definition(&self, body: String) -> RoutineDefinition {
        let returns = match (&self.returns, self.options.pipelined) {
            (None, _) => RoutineReturns::Nothing,
            (Some(declared), true) => RoutineReturns::Set(ReturnType::Type(declared.engine_type())),
            (Some(declared), false) => RoutineReturns::One(ReturnType::Type(declared.engine_type())),
        };
        RoutineDefinition {
            name: self.name.text.clone(),
            kind: self.kind,
            dialect: Dialect::Oracle,
            language: RoutineLanguage::PlSql,
            parameters: self.parameters.clone(),
            returns,
            volatility: if self.options.deterministic {
                Volatility::Immutable
            } else {
                Volatility::Volatile
            },
            strict: false,
            security_definer: !self.options.current_user,
            settings: Vec::new(),
            body,
            text: String::new(),
            tsql: None,
            plsql: Some(Box::new(PlsqlRoutine {
                declared: self.declared.clone(),
                returns: self.returns.clone(),
                package: None,
                deterministic: self.options.deterministic,
                pipelined: self.options.pipelined,
            })),
        }
    }

    fn variables(&self) -> Vec<Variable> {
        self.parameters
            .iter()
            .zip(&self.declared)
            .filter_map(|(parameter, declared)| {
                let name = parameter.name.as_deref()?;
                let binding = if is_ref_cursor(declared) {
                    Binding::Cursor
                } else {
                    Binding::Value {
                        constant: parameter.mode == ParameterMode::In,
                    }
                };
                Some(Variable::new(name, binding))
            })
            .collect()
    }
}

struct CompiledSubprogram {
    heading: Heading,
    definition: RoutineDefinition,
    program: Option<Program>,
}

fn pls_error(state: SqlState, code: u16, message: &str, position: usize) -> SqlError {
    SqlError::new(state, format!("PLS-{code:05}: {message}")).at(position)
}

fn shown(name: &str) -> String {
    swap_uniform_case(name)
}

fn undeclared(name: &str, position: usize) -> SqlError {
    pls_error(
        SqlState::UNDEFINED_OBJECT,
        201,
        &format!("identifier '{}' must be declared", shown(name)),
        position,
    )
}

fn nested_too_deeply(position: usize) -> SqlError {
    SqlError::new(
        SqlState::STATEMENT_TOO_COMPLEX,
        "the PL/SQL unit is nested too deeply",
    )
    .with_hint(format!(
        "A PL/SQL unit nests blocks, subprograms, IF, CASE and loops at most {MAX_NESTING} deep."
    ))
    .at(position)
}

fn too_many_steps(position: usize) -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("a PL/SQL unit has at most {MAX_STEPS} statements and declarations"),
    )
    .at(position)
}

fn not_supported(what: &str, position: usize) -> SqlError {
    SqlError::not_supported(format!("{what} is not supported yet")).at(position)
}

fn assignable(variable: &Variable, position: usize) -> SqlResult<()> {
    match &variable.binding {
        Binding::Value { constant: false } | Binding::Cursor => Ok(()),
        Binding::Value { constant: true } => Err(pls_error(
            SqlState::ERROR_IN_ASSIGNMENT,
            363,
            &format!(
                "expression '{}' cannot be used as an assignment target",
                shown(&variable.name)
            ),
            position,
        )),
        Binding::Exception { .. } | Binding::Type { .. } | Binding::Subprogram => Err(pls_error(
            SqlState::ERROR_IN_ASSIGNMENT,
            321,
            &format!(
                "expression '{}' is inappropriate as the left hand side of an assignment statement",
                shown(&variable.name)
            ),
            position,
        )),
    }
}

fn package_exception_identity(parts: &[String]) -> String {
    match parts {
        [.., package, name] => format!("{package}.{name}"),
        _ => parts.join("."),
    }
}

fn spec_variable(package: &str, declaration: &Declaration) -> Variable {
    let binding = match &declaration.kind {
        DeclarationKind::Variable(variable) => {
            if variable.variable_type == VariableType::RefCursor {
                Binding::Cursor
            } else {
                Binding::Value {
                    constant: variable.constant,
                }
            }
        }
        DeclarationKind::Alias(_) => Binding::Value { constant: false },
        DeclarationKind::Cursor(_) => Binding::Cursor,
        DeclarationKind::Exception { code } => Binding::Exception {
            identity: format!("{package}.{}", declaration.name),
            code: *code,
        },
        DeclarationKind::Type(declared) => Binding::Type {
            ref_cursor: *declared == TypeDeclaration::RefCursor,
        },
        DeclarationKind::Subprogram(_) => Binding::Subprogram,
    };
    Variable::new(&declaration.name, binding)
}

fn program_of(block: Block) -> Program {
    Program {
        variable_conflict: VariableConflict::UseColumn,
        print_strict_params: false,
        block,
    }
}

pub(crate) fn plsql_program<'t>(
    text: &'t str,
    tokens: &'t [Token<'t>],
    definition: &RoutineDefinition,
    trigger: Option<&TriggerRecords>,
) -> SqlResult<Program> {
    let mut parser = Parser::new(text, tokens).in_dialect(Dialect::Oracle);
    let lenient = definition
        .plsql
        .as_ref()
        .is_some_and(|plsql| plsql.package.is_some());
    let mut compiler = Compiler::new(&mut parser, trigger.cloned(), lenient);
    let program = if compiler.is_word("procedure") || compiler.is_word("function") {
        let position = compiler.position();
        let compiled = compiler.subprogram(1)?;
        compiled
            .program
            .ok_or_else(|| SqlError::syntax("syntax error at or near \";\"", position))?
    } else {
        compiler.anonymous_block()?
    };
    compiler.finish()?;
    Ok(program)
}

pub(crate) fn plsql_package_program<'t>(
    text: &'t str,
    tokens: &'t [Token<'t>],
    spec: Option<&Block>,
) -> SqlResult<PlsqlPackage> {
    let mut parser = Parser::new(text, tokens).in_dialect(Dialect::Oracle);
    let mut compiler = Compiler::new(&mut parser, None, false);
    let package = compiler.package(spec)?;
    compiler.finish()?;
    Ok(package)
}

impl<'t> Parser<'t> {
    fn plsql_compilation_error(&self, start: usize, error: SqlError) -> SqlError {
        let unit = self.text.get(start..).unwrap_or_default();
        let error = match error.position() {
            Some(position) if position >= start => error.at(position - start),
            _ => error,
        };
        compilation_error(unit, error)
    }

    pub(crate) fn plsql_subprogram(&mut self) -> SqlResult<PlsqlUnit> {
        let start = self.position();
        let compiled = Compiler::new(self, None, false).subprogram(1);
        let compiled = compiled.map_err(|error| self.plsql_compilation_error(start, error))?;
        let Some(program) = compiled.program else {
            let error = SqlError::syntax("syntax error at or near \";\"", self.position());
            return Err(self.plsql_compilation_error(start, error));
        };
        Ok(PlsqlUnit {
            name: compiled.heading.name,
            definition: compiled.definition,
            program,
        })
    }

    pub(crate) fn plsql_package(&mut self, spec: Option<&Block>) -> SqlResult<PlsqlPackage> {
        let start = self.position();
        let package = Compiler::new(self, None, false).package(spec);
        package.map_err(|error| self.plsql_compilation_error(start, error))
    }

    pub(crate) fn plsql_anonymous_block(&mut self) -> SqlResult<(Program, String)> {
        let start = self.position();
        let program = Compiler::new(self, None, false).anonymous_block();
        let program = program.map_err(|error| self.plsql_compilation_error(start, error))?;
        Ok((program, self.slice(start, self.previous_end())))
    }
}

struct Compiler<'p, 't> {
    parser: &'p mut Parser<'t>,
    scopes: Vec<Scope>,
    units: Vec<Unit>,
    trigger: Option<TriggerRecords>,
    lenient: bool,
    handling: usize,
    counted: usize,
}

impl<'p, 't> Compiler<'p, 't> {
    fn new(parser: &'p mut Parser<'t>, trigger: Option<TriggerRecords>, lenient: bool) -> Self {
        Self {
            parser,
            scopes: Vec::new(),
            units: Vec::new(),
            trigger,
            lenient,
            handling: 0,
            counted: 0,
        }
    }

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

    fn is_word_at(&self, ahead: usize, word: &str) -> bool {
        self.parser.is_word_at(ahead, word)
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

    fn expect(&mut self, kind: &TokenKind<'_>) -> SqlResult<()> {
        self.parser.expect(kind)
    }

    fn expect_semicolon(&mut self) -> SqlResult<()> {
        self.parser.expect(&TokenKind::Semicolon)
    }

    fn take_not_null(&mut self) -> bool {
        if self.is_word("not") && self.is_word_at(1, "null") {
            self.parser.at += 2;
            true
        } else {
            false
        }
    }

    fn unexpected(&self) -> SqlError {
        self.parser.unexpected()
    }

    fn finish(&mut self) -> SqlResult<()> {
        while self.take(&TokenKind::Semicolon) {}
        if self.parser.is_operator("/") {
            self.advance();
        }
        if self.parser.at_end() {
            Ok(())
        } else {
            Err(self.unexpected())
        }
    }

    fn is_operator_at(&self, ahead: usize, symbol: &str) -> bool {
        matches!(self.kind_at(ahead), TokenKind::Operator(found) if *found == symbol)
    }

    fn assignment_operator_at(&self, ahead: usize) -> usize {
        if self.is_operator_at(ahead, ":=") {
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

    fn dotted_name(&mut self) -> SqlResult<Vec<String>> {
        let mut parts = vec![self.any_word()?];
        while self.kind() == &TokenKind::Dot
            && matches!(
                self.kind_at(1),
                TokenKind::Word(_) | TokenKind::QuotedIdentifier(_)
            )
        {
            self.advance();
            parts.push(self.any_word()?);
        }
        Ok(parts)
    }

    fn previous_word(&self) -> String {
        let token = self
            .parser
            .at
            .checked_sub(1)
            .and_then(|index| self.parser.tokens.get(index));
        match token.map(|token| &token.kind) {
            Some(TokenKind::Word(word)) => word.as_ref().to_owned(),
            Some(TokenKind::QuotedIdentifier(name)) => name.clone(),
            _ => String::new(),
        }
    }

    fn positive_integer(&mut self) -> SqlResult<u32> {
        let TokenKind::Integer(digits) = self.kind() else {
            return Err(self.unexpected());
        };
        let Some(number) = digits.parse::<u32>().ok().filter(|number| *number > 0) else {
            return Err(self.unexpected());
        };
        self.advance();
        Ok(number)
    }

    fn skip_parenthesized(&mut self) -> SqlResult<()> {
        self.expect(&TokenKind::LeftParen)?;
        let mut depth = 1usize;
        while depth > 0 {
            match self.kind() {
                TokenKind::End => return Err(self.unexpected()),
                TokenKind::LeftParen => depth += 1,
                TokenKind::RightParen => depth -= 1,
                _ => {}
            }
            self.advance();
        }
        Ok(())
    }

    fn skip_to_semicolon(&mut self) {
        while !matches!(self.kind(), TokenKind::Semicolon | TokenKind::End) {
            self.advance();
        }
    }

    fn count_step(&mut self) -> SqlResult<()> {
        self.counted += 1;
        if self.counted > MAX_STEPS {
            return Err(too_many_steps(self.position()));
        }
        Ok(())
    }

    fn unit(&self) -> Option<&Unit> {
        self.units.last()
    }

    fn unit_floor(&self) -> usize {
        self.unit().map_or(0, |unit| unit.floor)
    }

    fn trigger_record(&self, token: &Token<'_>) -> Option<&'static str> {
        let records = self.trigger.as_ref()?;
        if !matches!(token.kind, TokenKind::NumberedParameter(_)) {
            return None;
        }
        let written = self.parser.text.get(token.start + 1..token.end)?;
        if written.eq_ignore_ascii_case(&records.new) {
            Some("new")
        } else if written.eq_ignore_ascii_case(&records.old) {
            Some("old")
        } else {
            None
        }
    }

    fn trigger_record_at(&self, ahead: usize) -> Option<&'static str> {
        let token = self.parser.tokens.get(self.parser.at + ahead)?;
        self.trigger_record(token)
    }

    fn source(&self, start: usize, end: usize) -> String {
        if self.trigger.is_none() {
            return self.parser.slice(start, end);
        }
        let tokens = self.parser.tokens;
        let first = tokens.partition_point(|token| token.start < start);
        let mut text = String::new();
        let mut copied = start;
        for token in tokens.iter().skip(first) {
            if token.end > end {
                break;
            }
            if let Some(record) = self.trigger_record(token) {
                text.push_str(&self.parser.slice(copied, token.start));
                text.push_str(record);
                copied = token.end;
            }
        }
        text.push_str(&self.parser.slice(copied, end));
        text
    }

    fn label(&mut self) -> SqlResult<Option<String>> {
        if !(self.is_operator_at(0, "<") && self.is_operator_at(1, "<")) {
            return Ok(None);
        }
        self.parser.at += 2;
        let label = self.parser.name()?.text;
        if !(self.is_operator_at(0, ">") && self.is_operator_at(1, ">")) {
            return Err(self.unexpected());
        }
        self.parser.at += 2;
        Ok(Some(label))
    }

    fn end_name(&mut self, expected: Option<&str>) -> SqlResult<()> {
        let position = self.position();
        let written = match self.kind() {
            TokenKind::Word(_) | TokenKind::QuotedIdentifier(_) => self.any_word()?,
            _ => return Ok(()),
        };
        match expected {
            Some(expected) if expected != written => Err(pls_error(
                SqlState::SYNTAX_ERROR,
                113,
                &format!(
                    "END identifier '{}' must match '{}'",
                    shown(&written),
                    shown(expected)
                ),
                position,
            )),
            _ => Ok(()),
        }
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
            .and_then(|scope| {
                scope
                    .variables
                    .iter()
                    .rev()
                    .find(|variable| variable.name == name)
            })
    }

    fn is_cursor(&self, name: &str) -> bool {
        self.variable(name)
            .is_some_and(|variable| variable.binding == Binding::Cursor)
    }

    fn is_cursor_at(&self, ahead: usize) -> bool {
        match self.kind_at(ahead) {
            TokenKind::Word(word) => self.is_cursor(word),
            TokenKind::QuotedIdentifier(name) => self.is_cursor(name),
            _ => false,
        }
    }

    fn is_ref_cursor_type(&self, declared: &DeclaredType) -> bool {
        if is_ref_cursor(declared) {
            return true;
        }
        let DeclaredType::Named(parts) = declared else {
            return false;
        };
        let [name] = parts.as_slice() else {
            return false;
        };
        self.variable(name).is_some_and(|variable| {
            variable.binding == Binding::Type { ref_cursor: true }
        })
    }

    fn exception_identity(&self, name: &str, position: usize) -> String {
        match self.unit() {
            Some(Unit {
                package: Some(package),
                ..
            }) => format!("{package}.{name}"),
            Some(unit) => format!("{}.{name}@{position}", unit.name),
            None => format!("{name}@{position}"),
        }
    }

    fn check_target(&self, target: &Target) -> SqlResult<()> {
        if let Some(TargetPart::Field(field)) = target.path.first()
            && let Some(variable) = self.labelled_variable(&target.name, field)
        {
            return assignable(variable, target.position);
        }
        match self.variable(&target.name) {
            Some(variable) => assignable(variable, target.position),
            None if !target.path.is_empty() || self.lenient => Ok(()),
            None => Err(undeclared(&target.name, target.position)),
        }
    }

    fn check_targets(&self, targets: &[Target]) -> SqlResult<()> {
        targets
            .iter()
            .try_for_each(|target| self.check_target(target))
    }

    fn require_known(&self, name: &str, position: usize) -> SqlResult<()> {
        if self.lenient || self.variable(name).is_some() {
            Ok(())
        } else {
            Err(undeclared(name, position))
        }
    }

    fn root_scope_variables(&self) -> Vec<Variable> {
        if self.trigger.is_none() {
            return Vec::new();
        }
        let mut variables = vec![Variable::value("new"), Variable::constant("old")];
        variables.extend(TRIGGER_PREDICATES.into_iter().map(Variable::constant));
        variables
    }

    fn anonymous_block(&mut self) -> SqlResult<Program> {
        let label = self.label()?;
        if !(self.is_word("declare") || self.is_word("begin")) {
            return Err(self.unexpected());
        }
        self.units.push(Unit {
            kind: RoutineKind::Procedure,
            pipelined: false,
            floor: self.scopes.len(),
            name: crate::plsql::ANONYMOUS_BLOCK_NAME.to_owned(),
            package: None,
        });
        let variables = self.root_scope_variables();
        self.scopes.push(Scope::new(None, false, variables));
        let block = self.block(label, 1);
        self.scopes.pop();
        self.units.pop();
        let block = block?;
        self.take(&TokenKind::Semicolon);
        Ok(program_of(block))
    }

    fn subprogram(&mut self, depth: usize) -> SqlResult<CompiledSubprogram> {
        let start = self.position();
        if depth > MAX_NESTING {
            return Err(nested_too_deeply(start));
        }
        let heading = self.heading()?;
        if self.take(&TokenKind::Semicolon) {
            let definition = heading.definition(self.parser.slice(start, self.parser.previous_end()));
            return Ok(CompiledSubprogram {
                heading,
                definition,
                program: None,
            });
        }
        if !(self.take_word("is") || self.take_word("as")) {
            return Err(self.unexpected());
        }
        if self.is_word("language") || self.is_word("external") {
            return Err(not_supported(
                "a PL/SQL call specification (LANGUAGE or EXTERNAL)",
                self.position(),
            ));
        }
        self.units.push(Unit {
            kind: heading.kind,
            pipelined: heading.options.pipelined,
            floor: self.scopes.len(),
            name: heading.local_name.clone(),
            package: None,
        });
        self.scopes.push(Scope::new(
            Some(heading.local_name.clone()),
            false,
            heading.variables(),
        ));
        let block = self.subprogram_block(&heading.local_name, depth);
        self.scopes.pop();
        self.units.pop();
        let block = block?;
        let definition = heading.definition(self.parser.slice(start, self.parser.previous_end()));
        Ok(CompiledSubprogram {
            heading,
            definition,
            program: Some(program_of(block)),
        })
    }

    fn heading(&mut self) -> SqlResult<Heading> {
        let kind = if self.take_word("procedure") {
            RoutineKind::Procedure
        } else {
            self.expect_word("function")?;
            RoutineKind::Function
        };
        let name = self.parser.table_name()?;
        let local_name = self.previous_word();
        let (parameters, declared) = self.parameters()?;
        let returns = if kind == RoutineKind::Function {
            self.expect_word("return")?;
            Some(self.declared_type(true)?)
        } else {
            None
        };
        let options = self.subprogram_options()?;
        if options.pipelined && kind == RoutineKind::Procedure {
            return Err(self.unexpected());
        }
        Ok(Heading {
            kind,
            name,
            local_name,
            parameters,
            declared,
            returns,
            options,
        })
    }

    fn parameters(&mut self) -> SqlResult<(Vec<RoutineParameter>, Vec<DeclaredType>)> {
        let mut parameters = Vec::new();
        let mut declared = Vec::new();
        if !self.take(&TokenKind::LeftParen) {
            return Ok((parameters, declared));
        }
        loop {
            self.count_step()?;
            let (parameter, declared_type) = self.parameter(&parameters)?;
            parameters.push(parameter);
            declared.push(declared_type);
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen)?;
        Ok((parameters, declared))
    }

    fn parameter(
        &mut self,
        earlier: &[RoutineParameter],
    ) -> SqlResult<(RoutineParameter, DeclaredType)> {
        let position = self.position();
        let name = self.parser.name()?.text;
        if earlier
            .iter()
            .any(|parameter| parameter.name.as_deref() == Some(name.as_str()))
        {
            return Err(pls_error(
                SqlState::DUPLICATE_OBJECT,
                410,
                "duplicate fields in RECORD,TABLE or argument list are not permitted",
                position,
            ));
        }
        let mode = if self.take_word("in") {
            if self.take_word("out") {
                ParameterMode::InOut
            } else {
                ParameterMode::In
            }
        } else if self.take_word("out") {
            ParameterMode::Out
        } else {
            ParameterMode::In
        };
        self.take_word("nocopy");
        let declared = self.declared_type(true)?;
        let default_position = self.position();
        let default = if self.take_word("default") || self.take_assignment_operator() {
            Some(self.fragment(&[Stop::Comma])?.text)
        } else {
            None
        };
        if default.is_some() && mode != ParameterMode::In {
            return Err(pls_error(
                SqlState::INVALID_FUNCTION_DEFINITION,
                230,
                "OUT and IN OUT formal parameters may not have default expressions",
                default_position,
            ));
        }
        let parameter = RoutineParameter {
            name: Some(name),
            mode,
            sql_type: declared.engine_type(),
            default,
            declared: None,
        };
        Ok((parameter, declared))
    }

    fn subprogram_options(&mut self) -> SqlResult<SubprogramOptions> {
        let mut options = SubprogramOptions::default();
        loop {
            if self.take_word("deterministic") {
                options.deterministic = true;
            } else if self.take_word("pipelined") {
                if self.is_word("using") {
                    return Err(not_supported(
                        "a PIPELINED USING implementation type",
                        self.position(),
                    ));
                }
                options.pipelined = true;
            } else if self.take_word("parallel_enable") || self.take_word("sql_macro") {
                if self.kind() == &TokenKind::LeftParen {
                    self.skip_parenthesized()?;
                }
            } else if self.take_word("result_cache") {
                if self.take_word("relies_on") {
                    self.skip_parenthesized()?;
                }
            } else if self.take_word("authid") {
                options.current_user = if self.take_word("current_user") {
                    true
                } else {
                    self.expect_word("definer")?;
                    false
                };
            } else if self.take_word("accessible") {
                self.expect_word("by")?;
                self.skip_parenthesized()?;
            } else if self.is_word("sharing") && self.is_operator_at(1, "=") {
                self.parser.at += 2;
                self.any_word()?;
            } else if self.is_word("default") && self.is_word_at(1, "collation") {
                self.parser.at += 2;
                self.any_word()?;
            } else if self.is_word("aggregate") {
                return Err(not_supported(
                    "a user-defined aggregate function (AGGREGATE USING)",
                    self.position(),
                ));
            } else {
                return Ok(options);
            }
        }
    }

    fn subprogram_block(&mut self, name: &str, depth: usize) -> SqlResult<Block> {
        let position = self.position();
        self.scopes.push(Scope::new(None, false, Vec::new()));
        let block = self.subprogram_block_contents(name, position, depth);
        self.scopes.pop();
        block
    }

    fn subprogram_block_contents(
        &mut self,
        name: &str,
        position: usize,
        depth: usize,
    ) -> SqlResult<Block> {
        let declarations = self.declarations(&["begin"], depth)?;
        self.expect_word("begin")?;
        let steps = self.steps(depth, &["exception", "end"])?;
        let handlers = self.handlers(depth)?;
        self.expect_word("end")?;
        self.end_name(Some(name))?;
        self.expect_semicolon()?;
        Ok(Block {
            label: None,
            declarations,
            steps,
            handlers,
            position,
        })
    }

    fn package(&mut self, spec: Option<&Block>) -> SqlResult<PlsqlPackage> {
        let start = self.position();
        self.expect_word("package")?;
        let body = self.take_word("body");
        let name = self.parser.table_name()?;
        let package = self.previous_word();
        if !body {
            self.subprogram_options()?;
        }
        if !(self.take_word("is") || self.take_word("as")) {
            return Err(self.unexpected());
        }
        let was_lenient = self.lenient;
        self.lenient = was_lenient || (body && spec.is_none());
        let variables = spec.map_or_else(Vec::new, |spec| {
            spec.declarations
                .iter()
                .map(|declaration| spec_variable(&package, declaration))
                .collect()
        });
        self.units.push(Unit {
            kind: RoutineKind::Procedure,
            pipelined: false,
            floor: self.scopes.len(),
            name: package.clone(),
            package: Some(package.clone()),
        });
        self.scopes
            .push(Scope::new(Some(package.clone()), false, variables));
        let block = self.package_block(body, &package);
        self.scopes.pop();
        self.units.pop();
        self.lenient = was_lenient;
        let block = block?;
        Ok(PlsqlPackage {
            name,
            body,
            text: self.parser.slice(start, self.parser.previous_end()),
            block,
        })
    }

    fn package_block(&mut self, body: bool, package: &str) -> SqlResult<Block> {
        let position = self.position();
        let declarations = self.declarations(&["begin", "end"], 1)?;
        if !body
            && let Some(defined) = declarations.iter().find(|declaration| {
                matches!(&declaration.kind, DeclarationKind::Subprogram(subprogram) if subprogram.program.is_some())
            })
        {
            return Err(pls_error(
                SqlState::INVALID_FUNCTION_DEFINITION,
                103,
                &format!(
                    "Encountered the symbol \"IS\" in the specification of '{}'",
                    shown(&defined.name)
                ),
                defined.position,
            ));
        }
        let (steps, handlers) = if body && self.take_word("begin") {
            let steps = self.steps(1, &["exception", "end"])?;
            (steps, self.handlers(1)?)
        } else {
            (Vec::new(), Vec::new())
        };
        self.expect_word("end")?;
        self.end_name(Some(package))?;
        self.expect_semicolon()?;
        Ok(Block {
            label: None,
            declarations,
            steps,
            handlers,
            position,
        })
    }

    fn declarations(&mut self, ends: &[&str], depth: usize) -> SqlResult<Vec<Declaration>> {
        let mut declarations = Vec::new();
        while !ends.iter().any(|end| self.is_word(end)) {
            if self.parser.at_end() {
                return Err(self.unexpected());
            }
            self.count_step()?;
            if let Some(declaration) = self.declaration(&mut declarations, depth)? {
                declarations.push(declaration);
            }
        }
        Ok(declarations)
    }

    fn declaration(
        &mut self,
        earlier: &mut [Declaration],
        depth: usize,
    ) -> SqlResult<Option<Declaration>> {
        let position = self.position();
        if self.is_word("pragma") {
            self.pragma(earlier)?;
            return Ok(None);
        }
        if self.is_word("procedure") || self.is_word("function") {
            let compiled = self.subprogram(depth + 1)?;
            let name = compiled.heading.local_name.clone();
            self.declare(Variable::new(&name, Binding::Subprogram));
            return Ok(Some(Declaration {
                name,
                position,
                kind: DeclarationKind::Subprogram(Box::new(Subprogram {
                    definition: compiled.definition,
                    program: compiled.program,
                })),
            }));
        }
        if self.is_word("type") && self.is_word_at(2, "is") {
            return self.type_declaration().map(Some);
        }
        if self.is_word("subtype") && self.is_word_at(2, "is") {
            return self.subtype_declaration().map(Some);
        }
        if self.is_word("cursor") {
            return self.cursor_declaration();
        }
        let name = self.parser.name()?.text;
        if self.take_word("exception") {
            self.expect_semicolon()?;
            let identity = self.exception_identity(&name, position);
            self.declare(Variable::new(
                &name,
                Binding::Exception {
                    identity,
                    code: None,
                },
            ));
            return Ok(Some(Declaration {
                name,
                position,
                kind: DeclarationKind::Exception { code: None },
            }));
        }
        let constant = self.take_word("constant");
        let declared = self.declared_type(false)?;
        let variable_type = self.variable_type_of(&declared);
        let not_null = self.take_not_null();
        let default = if self.take_word("default") || self.take_assignment_operator() {
            Some(self.fragment(&[])?)
        } else {
            None
        };
        if constant && default.is_none() {
            return Err(pls_error(
                SqlState::SYNTAX_ERROR,
                322,
                &format!(
                    "declaration of a constant '{}' must contain an initialization assignment",
                    shown(&name)
                ),
                position,
            ));
        }
        if not_null && default.is_none() {
            return Err(pls_error(
                SqlState::SYNTAX_ERROR,
                218,
                "a variable declared NOT NULL must have an initialization assignment",
                position,
            ));
        }
        self.expect_semicolon()?;
        let binding = if variable_type == VariableType::RefCursor {
            Binding::Cursor
        } else {
            Binding::Value { constant }
        };
        self.declare(Variable::new(&name, binding));
        Ok(Some(Declaration {
            name,
            position,
            kind: DeclarationKind::Variable(VariableDeclaration {
                variable_type,
                constant,
                not_null,
                collation: None,
                default,
            }),
        }))
    }

    fn pragma(&mut self, earlier: &mut [Declaration]) -> SqlResult<()> {
        self.advance();
        let position = self.position();
        let word = self.any_word()?;
        match word.as_str() {
            "exception_init" => self.exception_init(earlier),
            "autonomous_transaction" => Err(not_supported(
                "PRAGMA AUTONOMOUS_TRANSACTION",
                position,
            )),
            other if IGNORED_PRAGMAS.contains(&other) => {
                self.skip_to_semicolon();
                self.expect_semicolon()
            }
            _ => Err(pls_error(
                SqlState::SYNTAX_ERROR,
                127,
                &format!("Pragma {} is not a supported pragma", shown(&word)),
                position,
            )),
        }
    }

    fn exception_init(&mut self, earlier: &mut [Declaration]) -> SqlResult<()> {
        self.expect(&TokenKind::LeftParen)?;
        let name_position = self.position();
        let name = self.any_word()?;
        self.expect(&TokenKind::Comma)?;
        let code = self.error_number()?;
        self.expect(&TokenKind::RightParen)?;
        self.expect_semicolon()?;
        let Some(declaration) = earlier.iter_mut().rev().find(|declaration| {
            declaration.name == name
                && matches!(declaration.kind, DeclarationKind::Exception { .. })
        }) else {
            return Err(pls_error(
                SqlState::UNDEFINED_OBJECT,
                109,
                &format!(
                    "unknown exception name '{}' in PRAGMA EXCEPTION_INIT",
                    shown(&name)
                ),
                name_position,
            ));
        };
        declaration.kind = DeclarationKind::Exception { code: Some(code) };
        if let Some(scope) = self.scopes.last_mut()
            && let Some(variable) = scope
                .variables
                .iter_mut()
                .rev()
                .find(|variable| variable.name == name)
            && let Binding::Exception { code: bound, .. } = &mut variable.binding
        {
            *bound = Some(code);
        }
        Ok(())
    }

    fn error_number(&mut self) -> SqlResult<u16> {
        let position = self.position();
        let negative = self.parser.take_operator("-");
        let TokenKind::Integer(digits) = self.kind() else {
            return Err(self.unexpected());
        };
        let number = digits.parse::<u64>().unwrap_or(u64::MAX);
        self.advance();
        let code = match (negative, number) {
            (false, 100) => Some(NO_DATA_FOUND_CODE),
            (true, 1403) | (false, _) => None,
            (true, number) => u16::try_from(number).ok().filter(|code| *code > 0),
        };
        code.ok_or_else(|| {
            pls_error(
                SqlState::INVALID_PARAMETER_VALUE,
                701,
                &format!(
                    "illegal ORACLE error number {}{number} for PRAGMA EXCEPTION_INIT",
                    if negative { "-" } else { "" }
                ),
                position,
            )
        })
    }

    fn type_declaration(&mut self) -> SqlResult<Declaration> {
        let position = self.position();
        self.advance();
        let name = self.parser.name()?.text;
        self.expect_word("is")?;
        let declared = if self.take_word("record") {
            TypeDeclaration::Record(self.record_fields()?)
        } else if self.take_word("table") {
            self.expect_word("of")?;
            let element = self.element_type()?;
            let index = if self.take_word("index") {
                self.expect_word("by")?;
                let index = self.declared_type(false)?;
                Some(self.variable_type_of(&index))
            } else {
                None
            };
            TypeDeclaration::Table { element, index }
        } else if self.is_word("varray") || self.is_word("varying") {
            if !self.take_word("varray") {
                self.advance();
                self.expect_word("array")?;
            }
            self.expect(&TokenKind::LeftParen)?;
            let limit = self.positive_integer()?;
            self.expect(&TokenKind::RightParen)?;
            self.expect_word("of")?;
            let element = self.element_type()?;
            TypeDeclaration::Varray { limit, element }
        } else if self.take_word("ref") {
            self.expect_word("cursor")?;
            if self.take_word("return") {
                self.declared_type(false)?;
            }
            TypeDeclaration::RefCursor
        } else {
            return Err(self.unexpected());
        };
        self.expect_semicolon()?;
        self.declare(Variable::new(
            &name,
            Binding::Type {
                ref_cursor: declared == TypeDeclaration::RefCursor,
            },
        ));
        Ok(Declaration {
            name,
            position,
            kind: DeclarationKind::Type(declared),
        })
    }

    fn subtype_declaration(&mut self) -> SqlResult<Declaration> {
        let position = self.position();
        self.advance();
        let name = self.parser.name()?.text;
        self.expect_word("is")?;
        let declared = self.declared_type(false)?;
        let ref_cursor = self.is_ref_cursor_type(&declared);
        let base = self.variable_type_of(&declared);
        let not_null = self.take_not_null();
        self.expect_semicolon()?;
        self.declare(Variable::new(&name, Binding::Type { ref_cursor }));
        Ok(Declaration {
            name,
            position,
            kind: DeclarationKind::Type(TypeDeclaration::Subtype { base, not_null }),
        })
    }

    fn element_type(&mut self) -> SqlResult<VariableType> {
        let declared = self.declared_type(false)?;
        let element = self.variable_type_of(&declared);
        self.take_not_null();
        Ok(element)
    }

    fn record_fields(&mut self) -> SqlResult<Vec<RecordField>> {
        self.expect(&TokenKind::LeftParen)?;
        let mut fields: Vec<RecordField> = Vec::new();
        loop {
            let position = self.position();
            let name = self.parser.name()?.text;
            if fields.iter().any(|field| field.name == name) {
                return Err(pls_error(
                    SqlState::DUPLICATE_COLUMN,
                    410,
                    "duplicate fields in RECORD,TABLE or argument list are not permitted",
                    position,
                ));
            }
            let declared = self.declared_type(false)?;
            let variable_type = self.variable_type_of(&declared);
            let not_null = self.take_not_null();
            let default = if self.take_word("default") || self.take_assignment_operator() {
                Some(self.fragment(&[Stop::Comma])?)
            } else {
                None
            };
            if not_null && default.is_none() {
                return Err(pls_error(
                    SqlState::SYNTAX_ERROR,
                    218,
                    "a variable declared NOT NULL must have an initialization assignment",
                    position,
                ));
            }
            fields.push(RecordField {
                name,
                variable_type,
                not_null,
                default,
            });
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(fields)
    }

    fn cursor_declaration(&mut self) -> SqlResult<Option<Declaration>> {
        let position = self.position();
        self.advance();
        let name = self.parser.name()?.text;
        let mut arguments = Vec::new();
        if self.take(&TokenKind::LeftParen) {
            loop {
                let argument = self.parser.name()?.text;
                self.take_word("in");
                let declared = self.declared_type(true)?;
                let variable_type = self.variable_type_of(&declared);
                let default = if self.take_word("default") || self.take_assignment_operator() {
                    Some(self.fragment(&[Stop::Comma])?)
                } else {
                    None
                };
                arguments.push(CursorArgument {
                    name: argument,
                    variable_type,
                    default,
                });
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
        }
        if self.take_word("return") {
            self.declared_type(false)?;
        }
        self.declare(Variable::new(&name, Binding::Cursor));
        if self.take(&TokenKind::Semicolon) {
            return Ok(None);
        }
        self.expect_word("is")?;
        let query = self.fragment(&[])?;
        self.expect_semicolon()?;
        Ok(Some(Declaration {
            name,
            position,
            kind: DeclarationKind::Cursor(CursorDeclaration {
                scroll: None,
                arguments,
                query,
            }),
        }))
    }

    fn declared_type(&mut self, parameter: bool) -> SqlResult<DeclaredType> {
        let start = self.parser.at;
        let first_position = self.position();
        if let Some(copied) = self.copied_type()? {
            return Ok(copied);
        }
        self.parser.at = start;
        let word = match self.kind() {
            TokenKind::Word(word) => word.as_ref().to_owned(),
            TokenKind::QuotedIdentifier(_) => return self.named_type(parameter),
            _ => return Err(self.unexpected()),
        };
        let unconstrained = self.kind_at(1) != &TokenKind::LeftParen;
        let plsql_type = match word.as_str() {
            known if INTEGER_TYPES.contains(&known) => {
                self.advance();
                Some(SqlType::BigInt)
            }
            "string" => {
                self.advance();
                Some(SqlType::Varchar(self.string_length()?))
            }
            "varchar2" | "varchar" | "nvarchar2" if unconstrained => {
                self.advance();
                Some(SqlType::Varchar(None))
            }
            "char" | "nchar" if unconstrained && parameter => {
                self.advance();
                Some(SqlType::Text)
            }
            "raw" if unconstrained => {
                self.advance();
                Some(SqlType::Bytea)
            }
            _ => None,
        };
        let sql_type = match plsql_type {
            Some(sql_type) => sql_type,
            None => match self.parser.oracle_type() {
                Ok(sql_type) => sql_type,
                Err(error) if error.state() == SqlState::UNDEFINED_OBJECT => {
                    self.parser.at = start;
                    return self.named_type(parameter);
                }
                Err(error) => return Err(error),
            },
        };
        let written = self.parser.slice(first_position, self.parser.previous_end());
        self.skip_type_constraint()?;
        Ok(DeclaredType::sql(sql_type, &written))
    }

    fn string_length(&mut self) -> SqlResult<Option<u32>> {
        if !self.take(&TokenKind::LeftParen) {
            return Ok(None);
        }
        let position = self.position();
        let TokenKind::Integer(digits) = self.kind() else {
            return Err(self.unexpected());
        };
        let Some(length) = digits
            .parse::<u32>()
            .ok()
            .filter(|length| (1..=MAX_STRING_LENGTH).contains(length))
        else {
            return Err(pls_error(
                SqlState::INVALID_PARAMETER_VALUE,
                215,
                &format!("String length constraints must be in range (1 .. {MAX_STRING_LENGTH})"),
                position,
            ));
        };
        self.advance();
        let _ = self.take_word("char") || self.take_word("byte");
        self.expect(&TokenKind::RightParen)?;
        Ok(Some(length))
    }

    fn skip_type_constraint(&mut self) -> SqlResult<()> {
        if self.take_word("range") {
            self.fragment(&[Stop::Range])?;
            if !self.range_ahead() {
                return Err(self.unexpected());
            }
            self.parser.at += 2;
            self.fragment(&[Stop::Word("not"), Stop::Word("default"), Stop::Assignment])?;
        }
        if self.is_word("character") && self.is_word_at(1, "set") {
            self.parser.at += 2;
            self.dotted_name()?;
            if self.is_operator_at(0, "%") && self.is_word_at(1, "charset") {
                self.parser.at += 2;
            }
        }
        Ok(())
    }

    fn named_type(&mut self, parameter: bool) -> SqlResult<DeclaredType> {
        let position = self.position();
        let parts = self.dotted_name()?;
        if let [single] = parts.as_slice()
            && single != crate::plsql::REF_CURSOR_TYPE
            && !parameter
            && !self.lenient
            && !self
                .variable(single)
                .is_some_and(|variable| matches!(variable.binding, Binding::Type { .. }))
        {
            return Err(undeclared(single, position));
        }
        Ok(DeclaredType::Named(parts))
    }

    fn copied_type(&mut self) -> SqlResult<Option<DeclaredType>> {
        let position = self.position();
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
            if let [single] = parts.as_slice() {
                self.require_known(single, position)?;
            }
            return Ok(Some(DeclaredType::ColumnType(parts)));
        }
        if self.take_word("rowtype") {
            if parts.len() > 2 {
                return Err(SqlError::syntax(
                    "syntax error at or near \"%\"",
                    position,
                ));
            }
            return Ok(Some(DeclaredType::RowType(parts)));
        }
        Err(self.unexpected())
    }

    fn variable_type_of(&self, declared: &DeclaredType) -> VariableType {
        if self.is_ref_cursor_type(declared) {
            return VariableType::RefCursor;
        }
        match declared {
            DeclaredType::Sql { sql_type, .. } => VariableType::Sql(*sql_type),
            DeclaredType::ColumnType(parts) => VariableType::TypeOf(parts.clone()),
            DeclaredType::RowType(parts) => match parts.as_slice() {
                [name] if self.is_cursor(name) => VariableType::Record,
                [table] => VariableType::Row(table.clone()),
                [schema, table] => VariableType::Row(qualified(schema, table)),
                _ => VariableType::Record,
            },
            DeclaredType::Named(parts) => VariableType::Named(parts.clone()),
        }
    }

    fn range_ahead(&self) -> bool {
        let at = self.parser.at;
        let (Some(dot), Some(next)) = (self.parser.tokens.get(at), self.parser.tokens.get(at + 1))
        else {
            return false;
        };
        dot.kind == TokenKind::Dot && next.kind == TokenKind::Dot && next.start == dot.end
    }

    fn stops_here(&self, stops: &[Stop]) -> bool {
        stops.iter().any(|stop| match stop {
            Stop::Comma => self.kind() == &TokenKind::Comma,
            Stop::Range => self.range_ahead(),
            Stop::Assignment => self.assignment_operator_at(0) > 0,
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
            text: self.source(start, end),
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

    fn argument_list(&mut self) -> SqlResult<Vec<Fragment>> {
        self.expect(&TokenKind::LeftParen)?;
        if self.take(&TokenKind::RightParen) {
            return Ok(Vec::new());
        }
        let arguments = self.fragment_list(&[])?;
        self.expect(&TokenKind::RightParen)?;
        Ok(arguments)
    }

    fn target(&mut self) -> SqlResult<Target> {
        let position = self.position();
        let name = match self.trigger_record_at(0) {
            Some(record) => {
                self.advance();
                record.to_owned()
            }
            None => self.parser.name()?.text,
        };
        let mut path = Vec::new();
        loop {
            match self.kind() {
                TokenKind::Dot => {
                    self.advance();
                    path.push(TargetPart::Field(self.any_word()?));
                }
                TokenKind::LeftParen => {
                    self.advance();
                    let index = self.fragment(&[])?;
                    self.expect(&TokenKind::RightParen)?;
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
        self.check_targets(&targets)?;
        Ok(targets)
    }

    fn assignment_ahead(&self) -> bool {
        let starts = self.trigger_record_at(0).is_some()
            || matches!(
                self.kind(),
                TokenKind::Word(_) | TokenKind::QuotedIdentifier(_)
            );
        if !starts {
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
                TokenKind::LeftParen => {
                    let mut depth = 0usize;
                    loop {
                        match self.kind_at(ahead) {
                            TokenKind::LeftParen => depth += 1,
                            TokenKind::RightParen => {
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

    fn steps(&mut self, depth: usize, stops: &[&str]) -> SqlResult<Vec<Step>> {
        let mut steps = Vec::new();
        loop {
            if stops.iter().any(|stop| self.is_word(stop)) {
                if steps.is_empty() {
                    return Err(self.unexpected());
                }
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
        let kind = match label {
            Some(label) => self.labelled_step(label, depth)?,
            None => self.unlabelled_step(depth)?,
        };
        Ok(Step { kind, position })
    }

    fn unlabelled_step(&mut self, depth: usize) -> SqlResult<StepKind> {
        if self.assignment_ahead() {
            self.assignment()
        } else {
            self.keyword_step(depth)
        }
    }

    fn labelled_step(&mut self, label: String, depth: usize) -> SqlResult<StepKind> {
        if self.is_word("declare") || self.is_word("begin") {
            return self.nested_block(Some(label), depth);
        }
        if self.is_word("loop") || self.is_word("while") || self.is_word("for") {
            return self.loop_step(Some(label), depth);
        }
        if self.label()?.is_some() {
            return Err(self.unexpected());
        }
        self.unlabelled_step(depth)
    }

    fn nested_block(&mut self, label: Option<String>, depth: usize) -> SqlResult<StepKind> {
        let block = self.block(label, depth + 1)?;
        self.expect_semicolon()?;
        Ok(StepKind::Block(block))
    }

    fn block(&mut self, label: Option<String>, depth: usize) -> SqlResult<Block> {
        if depth > MAX_NESTING {
            return Err(nested_too_deeply(self.position()));
        }
        self.scopes.push(Scope::new(label.clone(), false, Vec::new()));
        let block = self.block_contents(label, depth);
        self.scopes.pop();
        block
    }

    fn block_contents(&mut self, label: Option<String>, depth: usize) -> SqlResult<Block> {
        let position = self.position();
        let declarations = if self.take_word("declare") {
            self.declarations(&["begin"], depth)?
        } else {
            Vec::new()
        };
        self.expect_word("begin")?;
        let steps = self.steps(depth, &["exception", "end"])?;
        let handlers = self.handlers(depth)?;
        self.expect_word("end")?;
        self.end_name(label.as_deref())?;
        Ok(Block {
            label,
            declarations,
            steps,
            handlers,
            position,
        })
    }

    fn handlers(&mut self, depth: usize) -> SqlResult<Vec<Handler>> {
        let mut handlers = Vec::new();
        if self.take_word("exception") {
            while self.is_word("when") {
                handlers.push(self.handler(depth)?);
            }
            if handlers.is_empty() {
                return Err(self.unexpected());
            }
        }
        Ok(handlers)
    }

    fn handler(&mut self, depth: usize) -> SqlResult<Handler> {
        self.expect_word("when")?;
        let mut conditions = Vec::new();
        loop {
            conditions.push(self.exception_condition()?);
            if !self.take_word("or") {
                break;
            }
        }
        self.expect_word("then")?;
        self.handling += 1;
        let steps = self.steps(depth + 1, &["when", "end"]);
        self.handling -= 1;
        Ok(Handler {
            conditions,
            steps: steps?,
        })
    }

    fn exception_condition(&mut self) -> SqlResult<ExceptionCondition> {
        let position = self.position();
        let parts = self.dotted_name()?;
        if let [name] = parts.as_slice() {
            if name == "others" {
                return Ok(ExceptionCondition::Others);
            }
            if let Some(Variable {
                binding: Binding::Exception { identity, code },
                ..
            }) = self.variable(name)
            {
                return Ok(match code {
                    Some(code) => ExceptionCondition::Oracle(*code),
                    None => ExceptionCondition::Declared(identity.clone()),
                });
            }
            if let Some(predefined) = predefined_by_name(name) {
                return Ok(ExceptionCondition::Oracle(predefined.code));
            }
            if self.lenient {
                return Ok(ExceptionCondition::Declared(name.clone()));
            }
            return Err(undeclared(name, position));
        }
        if let [standard, name] = parts.as_slice()
            && standard == "standard"
            && let Some(predefined) = predefined_by_name(name)
        {
            return Ok(ExceptionCondition::Oracle(predefined.code));
        }
        Ok(ExceptionCondition::Declared(package_exception_identity(
            &parts,
        )))
    }

    fn keyword_step(&mut self, depth: usize) -> SqlResult<StepKind> {
        let TokenKind::Word(word) = self.kind() else {
            return self.call_step();
        };
        match word.as_ref() {
            "declare" | "begin" => self.nested_block(None, depth),
            "if" => self.if_step(depth),
            "case" => self.case_step(depth),
            "loop" | "while" | "for" => self.loop_step(None, depth),
            "forall" => self.forall_step(depth),
            "exit" | "continue" => self.exit_step(),
            "return" => self.return_step(),
            "pipe" => self.pipe_step(),
            "raise" => self.raise_step(),
            "execute" => self.execute_step(),
            "open" => self.open_step(),
            "fetch" => self.fetch_step(),
            "close" => self.close_step(),
            "null" => {
                self.advance();
                self.expect_semicolon()?;
                Ok(StepKind::Null)
            }
            "commit" => {
                self.skip_to_semicolon();
                self.expect_semicolon()?;
                Ok(StepKind::Commit { chain: false })
            }
            "rollback" if !(self.is_word_at(1, "to") || self.is_word_at(2, "to")) => {
                self.skip_to_semicolon();
                self.expect_semicolon()?;
                Ok(StepKind::Rollback { chain: false })
            }
            "rollback" => self.sql_step(),
            "goto" => Err(not_supported("GOTO", self.position())),
            "end" | "else" | "elsif" | "when" | "then" | "exception" | "call" => {
                Err(self.unexpected())
            }
            known if SQL_STATEMENT_WORDS.contains(&known) => self.sql_step(),
            _ => self.call_step(),
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

    fn starts_into(&self) -> Option<bool> {
        if self.is_word("into") && !self.follows_into_keyword() {
            return Some(false);
        }
        if self.is_word("bulk") && self.is_word_at(1, "collect") && self.is_word_at(2, "into") {
            return Some(true);
        }
        None
    }

    fn sql_step(&mut self) -> SqlResult<StepKind> {
        let first = self.parser.at;
        let queries = self.is_word("select") || self.is_word("with");
        let finds_into = STATEMENTS_WITH_INTO.iter().any(|word| self.is_word(word));
        let mut parens = 0usize;
        let mut into: Option<(usize, usize, IntoTargets)> = None;
        loop {
            match self.kind() {
                TokenKind::End => break,
                TokenKind::Semicolon if parens == 0 => break,
                TokenKind::LeftParen | TokenKind::LeftBracket => parens += 1,
                TokenKind::RightParen | TokenKind::RightBracket => {
                    parens = parens.saturating_sub(1);
                }
                TokenKind::Word(_) if parens == 0 && finds_into && into.is_none() => {
                    if let Some(bulk) = self.starts_into() {
                        let into_start = self.position();
                        self.parser.at += if bulk { 3 } else { 1 };
                        let targets = self.target_list()?;
                        let found = IntoTargets {
                            strict: queries && !bulk,
                            bulk,
                            targets,
                        };
                        into = Some((into_start, self.parser.previous_end(), found));
                        continue;
                    }
                }
                _ => {}
            }
            self.advance();
        }
        let whole = self.fragment_from(first)?;
        let statement = match &into {
            Some((into_start, into_end, _)) => {
                let end = self.parser.previous_end();
                let before = self.source(whole.position, *into_start);
                let after = self.source(*into_end, end);
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

    fn call_step(&mut self) -> SqlResult<StepKind> {
        let position = self.position();
        let parts = self.dotted_name()?;
        if let [name] = parts.as_slice() {
            if name == "raise_application_error" {
                return self.application_error(position);
            }
            if let Some(variable) = self.variable(name)
                && variable.binding != Binding::Subprogram
            {
                return Err(pls_error(
                    SqlState::WRONG_OBJECT_TYPE,
                    221,
                    &format!("'{}' is not a procedure or is undefined", shown(name)),
                    position,
                ));
            }
        }
        if let [variable, fields @ .., method] = parts.as_slice()
            && let Some(method) = CollectionMethod::of_word(method)
            && self
                .variable(variable)
                .is_some_and(|found| matches!(found.binding, Binding::Value { .. }))
        {
            let arguments = if self.kind() == &TokenKind::LeftParen {
                self.argument_list()?
            } else {
                Vec::new()
            };
            self.expect_semicolon()?;
            let target = Target {
                name: variable.clone(),
                path: fields
                    .iter()
                    .map(|field| TargetPart::Field(field.clone()))
                    .collect(),
                position,
            };
            return Ok(StepKind::CollectionMethod {
                target,
                method,
                arguments,
            });
        }
        let has_arguments = self.kind() == &TokenKind::LeftParen;
        if has_arguments {
            self.skip_parenthesized()?;
        }
        let text = self.source(position, self.parser.previous_end());
        self.expect_semicolon()?;
        let text = if has_arguments {
            format!("CALL {text}")
        } else {
            format!("CALL {text}()")
        };
        Ok(StepKind::Call(Fragment { text, position }))
    }

    fn application_error(&mut self, position: usize) -> SqlResult<StepKind> {
        let arguments = self.argument_list()?;
        self.expect_semicolon()?;
        let mut arguments = arguments.into_iter();
        let (Some(number), Some(message), keep, None) = (
            arguments.next(),
            arguments.next(),
            arguments.next(),
            arguments.next(),
        ) else {
            return Err(pls_error(
                SqlState::INVALID_PARAMETER_VALUE,
                306,
                "wrong number or types of arguments in call to 'RAISE_APPLICATION_ERROR'",
                position,
            ));
        };
        Ok(StepKind::ApplicationError {
            number,
            message,
            keep,
        })
    }

    fn if_step(&mut self, depth: usize) -> SqlResult<StepKind> {
        self.advance();
        let mut branches = Vec::new();
        loop {
            let condition = self.fragment(&[Stop::Word("then")])?;
            self.expect_word("then")?;
            let steps = self.steps(depth + 1, &["elsif", "else", "end"])?;
            branches.push(Branch { condition, steps });
            if !self.take_word("elsif") {
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
            let value = self.fragment(&[Stop::Word("then")])?;
            self.expect_word("then")?;
            let steps = self.steps(depth + 1, &["when", "else", "end"])?;
            arms.push(CaseArm {
                values: vec![value],
                steps,
            });
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
        self.end_name(None)?;
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
            _ => return Err(self.unexpected()),
        };
        if opening != "loop" {
            self.expect_word("loop")?;
        }
        self.scopes
            .push(Scope::new(label.clone(), true, loop_variables));
        let body = self.steps(depth + 1, &["end"]);
        self.scopes.pop();
        let body = body?;
        self.expect_word("end")?;
        self.expect_word("loop")?;
        self.end_name(label.as_deref())?;
        self.expect_semicolon()?;
        Ok(StepKind::Loop(Loop { label, kind, body }))
    }

    fn starts_parenthesized_query(&self) -> bool {
        self.kind() == &TokenKind::LeftParen && (self.is_word_at(1, "select") || self.is_word_at(1, "with"))
    }

    fn for_head(&mut self, loop_variables: &mut Vec<Variable>) -> SqlResult<LoopKind> {
        let position = self.position();
        let variable = self.parser.name()?.text;
        self.expect_word("in")?;
        if self.starts_parenthesized_query() {
            self.advance();
            let query = self.fragment(&[])?;
            self.expect(&TokenKind::RightParen)?;
            loop_variables.push(Variable::value(&variable));
            return Ok(LoopKind::Query {
                targets: vec![Target {
                    name: variable,
                    path: Vec::new(),
                    position,
                }],
                source: QuerySource::Static(query),
            });
        }
        if self.is_cursor_at(0) && (self.kind_at(1) == &TokenKind::LeftParen || self.is_word_at(1, "loop")) {
            let cursor = self.any_word()?;
            let arguments = if self.kind() == &TokenKind::LeftParen {
                self.cursor_values()?
            } else {
                Vec::new()
            };
            loop_variables.push(Variable::value(&variable));
            return Ok(LoopKind::Cursor {
                record: variable,
                cursor,
                arguments,
            });
        }
        let reverse = self.take_word("reverse");
        let lower = self.fragment(&[Stop::Range, Stop::Word("loop")])?;
        if !self.range_ahead() {
            if !reverse && self.is_word("loop") {
                loop_variables.push(Variable::value(&variable));
                return Ok(LoopKind::Cursor {
                    record: variable,
                    cursor: lower.text,
                    arguments: Vec::new(),
                });
            }
            return Err(self.unexpected());
        }
        self.parser.at += 2;
        let upper = self.fragment(&[Stop::Word("loop")])?;
        loop_variables.push(Variable::constant(&variable));
        let (lower, upper) = if reverse {
            (upper, lower)
        } else {
            (lower, upper)
        };
        Ok(LoopKind::Range {
            variable,
            reverse,
            lower,
            upper,
            step: None,
        })
    }

    fn cursor_values(&mut self) -> SqlResult<Vec<CursorValue>> {
        self.expect(&TokenKind::LeftParen)?;
        let mut values = Vec::new();
        if self.take(&TokenKind::RightParen) {
            return Ok(values);
        }
        loop {
            let named = matches!(
                self.kind(),
                TokenKind::Word(_) | TokenKind::QuotedIdentifier(_)
            ) && self.is_operator_at(1, "=>");
            let name = if named {
                let name = self.any_word()?;
                self.advance();
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
        self.expect(&TokenKind::RightParen)?;
        Ok(values)
    }

    fn forall_step(&mut self, depth: usize) -> SqlResult<StepKind> {
        self.advance();
        let variable = self.parser.name()?.text;
        self.expect_word("in")?;
        let bounds = if self.take_word("indices") {
            self.expect_word("of")?;
            let collection = self.collection_target()?;
            let between = if self.take_word("between") {
                let lower = self.fragment(&[Stop::Word("and")])?;
                self.expect_word("and")?;
                let upper = self.fragment(&FORALL_BOUND_STOPS)?;
                Some((lower, upper))
            } else {
                None
            };
            ForallBounds::Indices {
                collection,
                between,
            }
        } else if self.take_word("values") {
            self.expect_word("of")?;
            ForallBounds::Values(self.collection_target()?)
        } else {
            let lower = self.fragment(&[Stop::Range])?;
            if !self.range_ahead() {
                return Err(self.unexpected());
            }
            self.parser.at += 2;
            let upper = self.fragment(&FORALL_BOUND_STOPS)?;
            ForallBounds::Range { lower, upper }
        };
        let save_exceptions = if self.take_word("save") {
            self.expect_word("exceptions")?;
            true
        } else {
            false
        };
        if !FORALL_STATEMENT_WORDS.iter().any(|word| self.is_word(word)) {
            return Err(self.unexpected());
        }
        self.scopes
            .push(Scope::new(None, false, vec![Variable::constant(&variable)]));
        let statement = self.step(depth + 1);
        self.scopes.pop();
        Ok(StepKind::Forall(Forall {
            variable,
            bounds,
            save_exceptions,
            statement: Box::new(statement?),
        }))
    }

    fn collection_target(&mut self) -> SqlResult<Target> {
        let target = self.target()?;
        if target.path.is_empty() {
            self.require_known(&target.name, target.position)?;
        }
        Ok(target)
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
        self.check_exit(label.as_deref(), position)?;
        Ok(if exits {
            StepKind::Exit { label, condition }
        } else {
            StepKind::Continue { label, condition }
        })
    }

    fn check_exit(&self, label: Option<&str>, position: usize) -> SqlResult<()> {
        let own = self.scopes.get(self.unit_floor()..).unwrap_or_default();
        let found = match label {
            None => own.iter().any(|scope| scope.is_loop),
            Some(label) => own
                .iter()
                .rev()
                .find(|scope| scope.label.as_deref() == Some(label))
                .is_some_and(|scope| scope.is_loop),
        };
        if found {
            return Ok(());
        }
        Err(match label {
            None => pls_error(
                SqlState::SYNTAX_ERROR,
                376,
                "illegal EXIT/CONTINUE statement; it must appear inside a loop",
                position,
            ),
            Some(label) => pls_error(
                SqlState::SYNTAX_ERROR,
                373,
                &format!("EXIT/CONTINUE label '{}' must label a LOOP statement", shown(label)),
                position,
            ),
        })
    }

    fn return_step(&mut self) -> SqlResult<StepKind> {
        let position = self.position();
        self.advance();
        let value = if self.kind() == &TokenKind::Semicolon {
            None
        } else {
            Some(self.fragment(&[])?)
        };
        self.expect_semicolon()?;
        let (kind, pipelined) = self
            .unit()
            .map_or((RoutineKind::Procedure, false), |unit| (unit.kind, unit.pipelined));
        match (kind, pipelined, value.is_some()) {
            (RoutineKind::Procedure, _, true) => Err(pls_error(
                SqlState::DATATYPE_MISMATCH,
                372,
                "In a procedure, RETURN statement cannot contain an expression",
                position,
            )),
            (RoutineKind::Function, true, true) => Err(pls_error(
                SqlState::DATATYPE_MISMATCH,
                633,
                "RETURN statement in a pipelined function cannot contain an expression",
                position,
            )),
            (RoutineKind::Function, false, false) => Err(pls_error(
                SqlState::DATATYPE_MISMATCH,
                503,
                "RETURN <value> statement required for this return from function",
                position,
            )),
            _ => Ok(StepKind::Return(value)),
        }
    }

    fn pipe_step(&mut self) -> SqlResult<StepKind> {
        let position = self.position();
        self.advance();
        self.expect_word("row")?;
        self.expect(&TokenKind::LeftParen)?;
        let value = self.fragment(&[])?;
        self.expect(&TokenKind::RightParen)?;
        self.expect_semicolon()?;
        if !self.unit().is_some_and(|unit| unit.pipelined) {
            return Err(pls_error(
                SqlState::DATATYPE_MISMATCH,
                629,
                "PIPE statement cannot be used in non-pipelined functions",
                position,
            ));
        }
        Ok(StepKind::ReturnNext(Some(value)))
    }

    fn raise_step(&mut self) -> SqlResult<StepKind> {
        let position = self.position();
        self.advance();
        if self.take(&TokenKind::Semicolon) {
            if self.handling == 0 {
                return Err(pls_error(
                    SqlState::SYNTAX_ERROR,
                    367,
                    "a RAISE statement with no exception name must be inside an exception handler",
                    position,
                ));
            }
            return Ok(StepKind::Reraise);
        }
        let name_position = self.position();
        let raised = match self.exception_condition()? {
            ExceptionCondition::Oracle(code) => RaisedException::Oracle(code),
            ExceptionCondition::Declared(identity) => RaisedException::Declared(identity),
            ExceptionCondition::Others | ExceptionCondition::State(_) => {
                return Err(SqlError::syntax(
                    "syntax error at or near \"others\"",
                    name_position,
                ));
            }
        };
        self.expect_semicolon()?;
        Ok(StepKind::RaiseException(raised))
    }

    fn using_list(&mut self) -> SqlResult<Vec<Fragment>> {
        let stops = [
            Stop::Comma,
            Stop::Word("into"),
            Stop::Word("bulk"),
            Stop::Word("returning"),
            Stop::Word("return"),
        ];
        let mut values = Vec::new();
        loop {
            let position = self.position();
            self.take_word("in");
            if self.is_word("out") {
                return Err(not_supported(
                    "an OUT or IN OUT bind argument in USING",
                    position,
                ));
            }
            values.push(self.fragment(&stops)?);
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        Ok(values)
    }

    fn into_clause_ahead(&self) -> bool {
        self.is_word("into")
            || (self.is_word("bulk") && self.is_word_at(1, "collect"))
            || ((self.is_word("returning") || self.is_word("return"))
                && (self.is_word_at(1, "into") || self.is_word_at(1, "bulk")))
    }

    fn execute_step(&mut self) -> SqlResult<StepKind> {
        self.advance();
        self.expect_word("immediate")?;
        let command = self.fragment(&[
            Stop::Word("into"),
            Stop::Word("bulk"),
            Stop::Word("using"),
            Stop::Word("returning"),
            Stop::Word("return"),
        ])?;
        let mut into = None;
        let mut using = None;
        loop {
            let position = self.position();
            if self.into_clause_ahead() {
                if into.is_some() {
                    return Err(SqlError::syntax("INTO specified more than once", position));
                }
                let returning = self.take_word("returning") || self.take_word("return");
                let bulk = if self.take_word("bulk") {
                    self.expect_word("collect")?;
                    true
                } else {
                    false
                };
                self.expect_word("into")?;
                let targets = self.target_list()?;
                into = Some(IntoTargets {
                    strict: !bulk && !returning,
                    bulk,
                    targets,
                });
            } else if self.take_word("using") {
                if using.is_some() {
                    return Err(SqlError::syntax("USING specified more than once", position));
                }
                using = Some(self.using_list()?);
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

    fn cursor_name(&mut self) -> SqlResult<String> {
        let position = self.position();
        let parts = self.dotted_name()?;
        if let [single] = parts.as_slice() {
            self.require_known(single, position)?;
        }
        Ok(parts.join("."))
    }

    fn open_step(&mut self) -> SqlResult<StepKind> {
        self.advance();
        let cursor = self.cursor_name()?;
        let source = if self.take_word("for") {
            if self.is_word("select") || self.is_word("with") || self.starts_parenthesized_query() {
                OpenSource::Query(QuerySource::Static(self.fragment(&[])?))
            } else {
                let command = self.fragment(&[Stop::Word("using")])?;
                let using = if self.take_word("using") {
                    self.using_list()?
                } else {
                    Vec::new()
                };
                OpenSource::Query(QuerySource::Dynamic { command, using })
            }
        } else if self.kind() == &TokenKind::LeftParen {
            OpenSource::Bound(self.cursor_values()?)
        } else {
            OpenSource::Bound(Vec::new())
        };
        self.expect_semicolon()?;
        Ok(StepKind::Open {
            cursor,
            scroll: None,
            source,
        })
    }

    fn fetch_step(&mut self) -> SqlResult<StepKind> {
        self.advance();
        let cursor = self.cursor_name()?;
        let bulk = if self.take_word("bulk") {
            self.expect_word("collect")?;
            true
        } else {
            false
        };
        self.expect_word("into")?;
        let targets = self.target_list()?;
        let limit_position = self.position();
        let limit = if self.take_word("limit") {
            if !bulk {
                return Err(SqlError::syntax(
                    "syntax error at or near \"limit\"",
                    limit_position,
                ));
            }
            Some(self.fragment(&[])?)
        } else {
            None
        };
        self.expect_semicolon()?;
        Ok(StepKind::Fetch {
            cursor,
            direction: FetchDirection::Next,
            into: IntoTargets {
                strict: false,
                bulk,
                targets,
            },
            limit,
        })
    }

    fn close_step(&mut self) -> SqlResult<StepKind> {
        self.advance();
        let cursor = self.cursor_name()?;
        self.expect_semicolon()?;
        Ok(StepKind::Close(cursor))
    }
}
