mod collections;
mod conditions;
mod frame;
mod raise;
mod run;
mod statements;
mod writes;

#[cfg(test)]
mod manual_tests;
#[cfg(test)]
mod run_tests;
#[cfg(test)]
mod tests;

pub(crate) use collections::{CollectionShape, bulk_exceptions_shape, bulk_rowcount_shape};
pub(crate) use conditions::{condition_name, condition_states};
pub(crate) use frame::RoutineFrame;
pub(crate) use raise::take_raised_notices;
pub(crate) use run::{
    Call, Returned, TriggerCall, invoke, invoke_read_only, local_program, local_routines,
    run_anonymous_block, run_inline, take_transaction_end,
};
pub(crate) use writes::ReadOnlyWrite;

use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::tokenize_in;
use crate::routines::{ReturnType, RoutineDefinition, RoutineLanguage, RoutineReturns};
use crate::schemas::base_name;
use crate::types::SqlType;

pub(crate) const MAX_NESTING: usize = 64;
pub(crate) const MAX_STEPS: usize = 100_000;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Fragment {
    pub(crate) text: String,
    pub(crate) position: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VariableConflict {
    Error,
    UseVariable,
    UseColumn,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Program {
    pub(crate) variable_conflict: VariableConflict,
    pub(crate) print_strict_params: bool,
    pub(crate) block: Block,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Block {
    pub(crate) label: Option<String>,
    pub(crate) declarations: Vec<Declaration>,
    pub(crate) steps: Vec<Step>,
    pub(crate) handlers: Vec<Handler>,
    pub(crate) position: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Declaration {
    pub(crate) name: String,
    pub(crate) position: usize,
    pub(crate) kind: DeclarationKind,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DeclarationKind {
    Variable(VariableDeclaration),
    Alias(AliasTarget),
    Cursor(CursorDeclaration),
    Exception { code: Option<u16> },
    Subprogram(Box<Subprogram>),
    Type(TypeDeclaration),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Subprogram {
    pub(crate) definition: RoutineDefinition,
    pub(crate) program: Option<Program>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TypeDeclaration {
    Record(Vec<RecordField>),
    Table {
        element: VariableType,
        index: Option<VariableType>,
    },
    Varray {
        limit: u32,
        element: VariableType,
    },
    RefCursor,
    Subtype {
        base: VariableType,
        not_null: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RecordField {
    pub(crate) name: String,
    pub(crate) variable_type: VariableType,
    pub(crate) not_null: bool,
    pub(crate) default: Option<Fragment>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VariableDeclaration {
    pub(crate) variable_type: VariableType,
    pub(crate) constant: bool,
    pub(crate) not_null: bool,
    pub(crate) collation: Option<String>,
    pub(crate) default: Option<Fragment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AliasTarget {
    Parameter(usize),
    Name(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CursorDeclaration {
    pub(crate) scroll: Option<bool>,
    pub(crate) arguments: Vec<CursorArgument>,
    pub(crate) query: Fragment,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CursorArgument {
    pub(crate) name: String,
    pub(crate) variable_type: VariableType,
    pub(crate) default: Option<Fragment>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum VariableType {
    Sql(SqlType),
    Record,
    RefCursor,
    Row(String),
    TypeOf(Vec<String>),
    Named(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Target {
    pub(crate) name: String,
    pub(crate) path: Vec<TargetPart>,
    pub(crate) position: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TargetPart {
    Field(String),
    Index(Fragment),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IntoTargets {
    pub(crate) strict: bool,
    pub(crate) bulk: bool,
    pub(crate) targets: Vec<Target>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Step {
    pub(crate) kind: StepKind,
    pub(crate) position: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StepKind {
    Block(Block),
    Assign {
        target: Target,
        value: Fragment,
    },
    Sql {
        statement: Fragment,
        into: Option<IntoTargets>,
    },
    Perform(Fragment),
    Call(Fragment),
    Execute(DynamicSql),
    GetDiagnostics {
        stacked: bool,
        items: Vec<(Target, DiagnosticsItem)>,
    },
    If {
        branches: Vec<Branch>,
        otherwise: Option<Vec<Step>>,
    },
    Case {
        subject: Option<Fragment>,
        arms: Vec<CaseArm>,
        otherwise: Option<Vec<Step>>,
    },
    Loop(Loop),
    Exit {
        label: Option<String>,
        condition: Option<Fragment>,
    },
    Continue {
        label: Option<String>,
        condition: Option<Fragment>,
    },
    Return(Option<Fragment>),
    ReturnNext(Option<Fragment>),
    ReturnQuery(QuerySource),
    Raise(Raise),
    Reraise,
    RaiseException(RaisedException),
    ApplicationError {
        number: Fragment,
        message: Fragment,
        keep: Option<Fragment>,
    },
    Forall(Forall),
    CollectionMethod {
        target: Target,
        method: CollectionMethod,
        arguments: Vec<Fragment>,
    },
    Assert {
        condition: Fragment,
        message: Option<Fragment>,
    },
    Open {
        cursor: String,
        scroll: Option<bool>,
        source: OpenSource,
    },
    Fetch {
        cursor: String,
        direction: FetchDirection,
        into: IntoTargets,
        limit: Option<Fragment>,
    },
    Move {
        cursor: String,
        direction: FetchDirection,
    },
    Close(String),
    Commit {
        chain: bool,
    },
    Rollback {
        chain: bool,
    },
    Null,
}

impl StepKind {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Block(_) => "statement block",
            Self::Assign { .. } => "assignment",
            Self::Sql { .. } => "SQL statement",
            Self::Perform(_) => "PERFORM",
            Self::Call(_) => "CALL",
            Self::Execute(_) => "EXECUTE",
            Self::GetDiagnostics { .. } => "GET DIAGNOSTICS",
            Self::If { .. } => "IF",
            Self::Case { .. } => "CASE",
            Self::Loop(found) => found.kind.name(),
            Self::Exit { .. } => "EXIT",
            Self::Continue { .. } => "CONTINUE",
            Self::Return(_) => "RETURN",
            Self::ReturnNext(_) => "RETURN NEXT",
            Self::ReturnQuery(_) => "RETURN QUERY",
            Self::Raise(_) | Self::Reraise | Self::RaiseException(_) => "RAISE",
            Self::ApplicationError { .. } => "RAISE_APPLICATION_ERROR",
            Self::Forall(_) => "FORALL",
            Self::CollectionMethod { method, .. } => method.name(),
            Self::Assert { .. } => "ASSERT",
            Self::Open { .. } => "OPEN",
            Self::Fetch { .. } => "FETCH",
            Self::Move { .. } => "MOVE",
            Self::Close(_) => "CLOSE",
            Self::Commit { .. } => "COMMIT",
            Self::Rollback { .. } => "ROLLBACK",
            Self::Null => "NULL",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RaisedException {
    Oracle(u16),
    Declared(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CollectionMethod {
    Extend,
    Delete,
    Trim,
}

impl CollectionMethod {
    pub(crate) fn of_word(word: &str) -> Option<Self> {
        Some(match word {
            "extend" => Self::Extend,
            "delete" => Self::Delete,
            "trim" => Self::Trim,
            _ => return None,
        })
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Extend => "EXTEND",
            Self::Delete => "DELETE",
            Self::Trim => "TRIM",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Forall {
    pub(crate) variable: String,
    pub(crate) bounds: ForallBounds,
    pub(crate) save_exceptions: bool,
    pub(crate) statement: Box<Step>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ForallBounds {
    Range { lower: Fragment, upper: Fragment },
    Indices {
        collection: Target,
        between: Option<(Fragment, Fragment)>,
    },
    Values(Target),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DynamicSql {
    pub(crate) command: Fragment,
    pub(crate) into: Option<IntoTargets>,
    pub(crate) using: Vec<Fragment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiagnosticsItem {
    RowCount,
    PgContext,
    PgRoutineOid,
    ReturnedSqlstate,
    ColumnName,
    ConstraintName,
    PgDatatypeName,
    MessageText,
    TableName,
    SchemaName,
    PgExceptionDetail,
    PgExceptionHint,
    PgExceptionContext,
}

const DIAGNOSTICS_ITEMS: [(&str, DiagnosticsItem); 13] = [
    ("row_count", DiagnosticsItem::RowCount),
    ("pg_context", DiagnosticsItem::PgContext),
    ("pg_routine_oid", DiagnosticsItem::PgRoutineOid),
    ("returned_sqlstate", DiagnosticsItem::ReturnedSqlstate),
    ("column_name", DiagnosticsItem::ColumnName),
    ("constraint_name", DiagnosticsItem::ConstraintName),
    ("pg_datatype_name", DiagnosticsItem::PgDatatypeName),
    ("message_text", DiagnosticsItem::MessageText),
    ("table_name", DiagnosticsItem::TableName),
    ("schema_name", DiagnosticsItem::SchemaName),
    ("pg_exception_detail", DiagnosticsItem::PgExceptionDetail),
    ("pg_exception_hint", DiagnosticsItem::PgExceptionHint),
    ("pg_exception_context", DiagnosticsItem::PgExceptionContext),
];

impl DiagnosticsItem {
    pub(crate) fn of_name(name: &str) -> Option<Self> {
        DIAGNOSTICS_ITEMS
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, item)| *item)
    }

    pub(crate) fn name(self) -> &'static str {
        DIAGNOSTICS_ITEMS
            .iter()
            .find(|(_, item)| *item == self)
            .map_or("", |(name, _)| name)
    }

    pub(crate) fn is_current(self) -> bool {
        matches!(self, Self::RowCount | Self::PgContext | Self::PgRoutineOid)
    }

    pub(crate) fn is_stacked(self) -> bool {
        !self.is_current()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Branch {
    pub(crate) condition: Fragment,
    pub(crate) steps: Vec<Step>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CaseArm {
    pub(crate) values: Vec<Fragment>,
    pub(crate) steps: Vec<Step>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Loop {
    pub(crate) label: Option<String>,
    pub(crate) kind: LoopKind,
    pub(crate) body: Vec<Step>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LoopKind {
    Plain,
    While(Fragment),
    Range {
        variable: String,
        reverse: bool,
        lower: Fragment,
        upper: Fragment,
        step: Option<Fragment>,
    },
    Query {
        targets: Vec<Target>,
        source: QuerySource,
    },
    Cursor {
        record: String,
        cursor: String,
        arguments: Vec<CursorValue>,
    },
    Foreach {
        targets: Vec<Target>,
        slice: usize,
        array: Fragment,
    },
}

impl LoopKind {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Plain => "LOOP",
            Self::While(_) => "WHILE",
            Self::Range { .. } => "FOR with integer loop variable",
            Self::Query {
                source: QuerySource::Static(_),
                ..
            } => "FOR over SELECT rows",
            Self::Query {
                source: QuerySource::Dynamic { .. },
                ..
            } => "FOR over EXECUTE statement",
            Self::Cursor { .. } => "FOR over cursor",
            Self::Foreach { .. } => "FOREACH over array",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum QuerySource {
    Static(Fragment),
    Dynamic {
        command: Fragment,
        using: Vec<Fragment>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum OpenSource {
    Bound(Vec<CursorValue>),
    Query(QuerySource),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CursorValue {
    pub(crate) name: Option<String>,
    pub(crate) value: Fragment,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FetchDirection {
    Next,
    Prior,
    First,
    Last,
    Absolute(Fragment),
    Relative(Fragment),
    Forward(Option<Fragment>),
    Backward(Option<Fragment>),
    ForwardAll,
    BackwardAll,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Raise {
    pub(crate) level: RaiseLevel,
    pub(crate) condition: Option<RaiseCondition>,
    pub(crate) format: Option<String>,
    pub(crate) arguments: Vec<Fragment>,
    pub(crate) options: Vec<RaiseOption>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RaiseLevel {
    Debug,
    Log,
    Info,
    Notice,
    Warning,
    Exception,
}

impl RaiseLevel {
    pub(crate) fn of_word(word: &str) -> Option<Self> {
        Some(match word {
            "debug" => Self::Debug,
            "log" => Self::Log,
            "info" => Self::Info,
            "notice" => Self::Notice,
            "warning" => Self::Warning,
            "exception" => Self::Exception,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RaiseCondition {
    Name { name: String, state: SqlState },
    State(SqlState),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RaiseOption {
    pub(crate) item: RaiseItem,
    pub(crate) value: Fragment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RaiseItem {
    Message,
    Detail,
    Hint,
    Errcode,
    Column,
    Constraint,
    Datatype,
    Table,
    Schema,
}

const RAISE_ITEMS: [(&str, RaiseItem); 9] = [
    ("message", RaiseItem::Message),
    ("detail", RaiseItem::Detail),
    ("hint", RaiseItem::Hint),
    ("errcode", RaiseItem::Errcode),
    ("column", RaiseItem::Column),
    ("constraint", RaiseItem::Constraint),
    ("datatype", RaiseItem::Datatype),
    ("table", RaiseItem::Table),
    ("schema", RaiseItem::Schema),
];

impl RaiseItem {
    pub(crate) fn of_word(word: &str) -> Option<Self> {
        RAISE_ITEMS
            .iter()
            .find(|(known, _)| *known == word)
            .map(|(_, item)| *item)
    }

    pub(crate) fn keyword(self) -> String {
        RAISE_ITEMS
            .iter()
            .find(|(_, item)| *item == self)
            .map_or_else(String::new, |(name, _)| name.to_ascii_uppercase())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Handler {
    pub(crate) conditions: Vec<ExceptionCondition>,
    pub(crate) steps: Vec<Step>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExceptionCondition {
    Others,
    State(SqlState),
    Oracle(u16),
    Declared(String),
}

impl ExceptionCondition {
    pub(crate) fn catches(&self, state: SqlState) -> bool {
        match self {
            Self::Others => state != SqlState::QUERY_CANCELED && state != SqlState::ASSERT_FAILURE,
            Self::Oracle(_) | Self::Declared(_) => false,
            Self::State(caught) => {
                let caught = caught.code();
                let found = state.code();
                if caught.ends_with("000") {
                    caught.get(..2) == found.get(..2)
                } else {
                    caught == found
                }
            }
        }
    }

    pub(crate) fn catches_oracle(&self, code: Option<u16>, declared: Option<&str>) -> bool {
        match self {
            Self::Others => true,
            Self::Oracle(caught) => code == Some(*caught),
            Self::Declared(identity) => declared == Some(identity.as_str()),
            Self::State(_) => false,
        }
    }
}

impl Handler {
    pub(crate) fn catches(&self, state: SqlState) -> bool {
        self.conditions
            .iter()
            .any(|condition| condition.catches(state))
    }

    pub(crate) fn catches_oracle(&self, code: Option<u16>, declared: Option<&str>) -> bool {
        self.conditions
            .iter()
            .any(|condition| condition.catches_oracle(code, declared))
    }
}

pub(crate) fn format_placeholders(format: &str) -> usize {
    let mut count = 0;
    let mut characters = format.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '%' {
            continue;
        }
        if characters.peek() == Some(&'%') {
            characters.next();
        } else {
            count += 1;
        }
    }
    count
}

pub(crate) fn has_output_parameters(definition: &RoutineDefinition) -> bool {
    matches!(definition.returns, RoutineReturns::Table(_))
        || definition
            .parameters
            .iter()
            .any(|parameter| parameter.mode.is_output())
}

pub(crate) fn is_trigger_function(definition: &RoutineDefinition) -> bool {
    definition.returns == RoutineReturns::One(ReturnType::Trigger)
}

pub(crate) fn compile(definition: &RoutineDefinition) -> SqlResult<Program> {
    let tokens = tokenize_in(&definition.body, Dialect::PostgreSql)?;
    crate::parser::plpgsql_program(&definition.body, &tokens, definition)
}

pub(crate) fn compile_for_run(definition: &RoutineDefinition) -> SqlResult<Program> {
    if definition.language == RoutineLanguage::PlSql {
        return crate::plsql::compile(definition);
    }
    compile(definition).map_err(|error| compilation_error(definition, None, error))
}

pub(crate) fn line_of(body: &str, position: usize) -> usize {
    body.get(..position)
        .unwrap_or(body)
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

pub(crate) fn check(definition: &RoutineDefinition, body_position: Option<usize>) -> SqlResult<()> {
    if is_trigger_function(definition) && !definition.parameters.is_empty() {
        return Err(SqlError::new(
            SqlState::INVALID_FUNCTION_DEFINITION,
            "trigger functions cannot have declared arguments",
        )
        .with_hint("The arguments of the trigger can be accessed through TG_NARGS and TG_ARGV instead."));
    }
    let compiled = compile(definition).and_then(|program| check_expressions(&program));
    compiled.map_err(|error| compilation_error(definition, body_position, error))
}

fn compilation_error(
    definition: &RoutineDefinition,
    body_position: Option<usize>,
    error: SqlError,
) -> SqlError {
    let Some(position) = error.position() else {
        return error.with_context(format!(
            "compilation of PL/pgSQL function \"{}\"",
            base_name(&definition.name)
        ));
    };
    let context = format!(
        "compilation of PL/pgSQL function \"{}\" near line {}",
        base_name(&definition.name),
        line_of(&definition.body, position)
    );
    let error = match body_position {
        Some(start) => error.at(start + position),
        None => error.without_position(),
    };
    error.with_context(context)
}

fn check_expressions(program: &Program) -> SqlResult<()> {
    let mut expressions = Vec::new();
    block_expressions(&program.block, &mut expressions);
    for fragment in expressions {
        check_expression(fragment)?;
    }
    Ok(())
}

fn check_expression(fragment: &Fragment) -> SqlResult<()> {
    let shifted = |error: SqlError| match error.position() {
        Some(position) => error.at(fragment.position + position),
        None => error.at(fragment.position),
    };
    let tokens = tokenize_in(&fragment.text, Dialect::PostgreSql).map_err(shifted)?;
    let mut parser = crate::parser::Parser::new(&fragment.text, &tokens).in_dialect(Dialect::PostgreSql);
    let parsed = parser.expression().and_then(|_| {
        if parser.at_end() {
            Ok(())
        } else {
            Err(parser.unexpected())
        }
    });
    match parsed {
        Err(error) if error.state() == SqlState::SYNTAX_ERROR => Err(shifted(error)),
        _ => Ok(()),
    }
}

fn block_expressions<'p>(block: &'p Block, found: &mut Vec<&'p Fragment>) {
    for declaration in &block.declarations {
        if let DeclarationKind::Variable(variable) = &declaration.kind
            && let Some(default) = &variable.default
        {
            found.push(default);
        }
    }
    steps_expressions(&block.steps, found);
    for handler in &block.handlers {
        steps_expressions(&handler.steps, found);
    }
}

fn steps_expressions<'p>(steps: &'p [Step], found: &mut Vec<&'p Fragment>) {
    for step in steps {
        step_expressions(&step.kind, found);
    }
}

fn step_expressions<'p>(kind: &'p StepKind, found: &mut Vec<&'p Fragment>) {
    match kind {
        StepKind::Block(block) => block_expressions(block, found),
        StepKind::Assign { value, .. } => found.push(value),
        StepKind::Execute(dynamic) => {
            found.push(&dynamic.command);
            found.extend(&dynamic.using);
        }
        StepKind::If {
            branches,
            otherwise,
        } => {
            for branch in branches {
                found.push(&branch.condition);
                steps_expressions(&branch.steps, found);
            }
            if let Some(otherwise) = otherwise {
                steps_expressions(otherwise, found);
            }
        }
        StepKind::Case {
            subject,
            arms,
            otherwise,
        } => {
            found.extend(subject);
            for arm in arms {
                found.extend(&arm.values);
                steps_expressions(&arm.steps, found);
            }
            if let Some(otherwise) = otherwise {
                steps_expressions(otherwise, found);
            }
        }
        StepKind::Loop(found_loop) => {
            match &found_loop.kind {
                LoopKind::While(condition) => found.push(condition),
                LoopKind::Range {
                    lower, upper, step, ..
                } => {
                    found.push(lower);
                    found.push(upper);
                    found.extend(step);
                }
                LoopKind::Foreach { array, .. } => found.push(array),
                LoopKind::Cursor { arguments, .. } => {
                    found.extend(arguments.iter().map(|argument| &argument.value));
                }
                LoopKind::Query { .. } | LoopKind::Plain => {}
            }
            steps_expressions(&found_loop.body, found);
        }
        StepKind::Exit { condition, .. } | StepKind::Continue { condition, .. } => {
            found.extend(condition);
        }
        StepKind::Return(value) | StepKind::ReturnNext(value) => found.extend(value),
        StepKind::Raise(raise) => {
            found.extend(&raise.arguments);
            found.extend(raise.options.iter().map(|option| &option.value));
        }
        StepKind::ApplicationError {
            number,
            message,
            keep,
        } => {
            found.push(number);
            found.push(message);
            found.extend(keep);
        }
        StepKind::Forall(forall) => {
            if let ForallBounds::Range { lower, upper } = &forall.bounds {
                found.push(lower);
                found.push(upper);
            }
            if let ForallBounds::Indices {
                between: Some((lower, upper)),
                ..
            } = &forall.bounds
            {
                found.push(lower);
                found.push(upper);
            }
            step_expressions(&forall.statement.kind, found);
        }
        StepKind::CollectionMethod { arguments, .. } => found.extend(arguments),
        StepKind::Assert { condition, message } => {
            found.push(condition);
            found.extend(message);
        }
        StepKind::Sql { .. }
        | StepKind::Perform(_)
        | StepKind::Call(_)
        | StepKind::GetDiagnostics { .. }
        | StepKind::ReturnQuery(_)
        | StepKind::Reraise
        | StepKind::RaiseException(_)
        | StepKind::Open { .. }
        | StepKind::Fetch { .. }
        | StepKind::Move { .. }
        | StepKind::Close(_)
        | StepKind::Commit { .. }
        | StepKind::Rollback { .. }
        | StepKind::Null => {}
    }
}
