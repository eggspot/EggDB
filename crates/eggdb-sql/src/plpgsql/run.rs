use super::frame::{
    FrameVariable, RoutineFrame, Slot, is_composite_type, missing_field, record_of, row_record,
    unassigned_record,
};
use super::raise::{
    RaisedFields, display_value, errcode, format_message, push_notice, push_statement_notices,
    with_context_line,
};
use super::statements::{processed_rows, run_sql};
use super::writes::{ReadOnlyWrite, UndoLog, UndoWrite};
use super::{
    AliasTarget, Block, CaseArm, CursorDeclaration, CursorValue, Declaration, DeclarationKind,
    DiagnosticsItem, DynamicSql, FetchDirection, Fragment, Handler, IntoTargets, Loop, LoopKind,
    OpenSource, Program, QuerySource, Raise, RaiseCondition, RaiseItem, RaiseLevel, Step,
    StepKind, Target, TargetPart, VariableConflict, VariableType, is_trigger_function, line_of,
};
use crate::arrays::{ArrayElement, ArrayValue};
use crate::ast::Statement;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{Relation, TableCache};
use crate::lexer::tokenize_in;
use crate::parser::{Parameters, Parser};
use crate::records::{self, RECORD_OID};
use crate::routines::{
    ReturnType, RoutineDefinition, RoutineKind, RoutineLanguage, RoutineReturns, Volatility,
};
use crate::schemas::{base_name, display};
use crate::session::QueryResult;
use crate::settings::Settings;
use crate::store::{KeyValueRead, KeyValueWrite};
use crate::types::{Conversion, SqlType, Value, compare_values, convert};
use std::cell::Cell;
use std::cmp::Ordering;
use std::sync::Arc;

pub(crate) const MAX_ROUTINE_DEPTH: usize = 100;
const MAX_OPEN_CURSORS: usize = 1_000;
const MAX_ARRAY_GROWTH: i64 = 1 << 20;
const INLINE_NAME: &str = "inline_code_block";

thread_local! {
    static DEPTH: Cell<usize> = const { Cell::new(0) };
}

struct Depth;

impl Drop for Depth {
    fn drop(&mut self) {
        DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

fn enter_routine() -> SqlResult<Depth> {
    DEPTH.with(|depth| {
        if depth.get() >= MAX_ROUTINE_DEPTH {
            return Err(SqlError::new(
                SqlState::STATEMENT_TOO_COMPLEX,
                "stack depth limit exceeded",
            )
            .with_hint(format!(
                "EggDB nests function and procedure calls at most {MAX_ROUTINE_DEPTH} deep."
            )));
        }
        depth.set(depth.get() + 1);
        Ok(Depth)
    })
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TriggerCall {
    pub(crate) new: Option<Value>,
    pub(crate) old: Option<Value>,
    pub(crate) name: String,
    pub(crate) when: &'static str,
    pub(crate) level: &'static str,
    pub(crate) operation: &'static str,
    pub(crate) relation_oid: u32,
    pub(crate) table_name: String,
    pub(crate) table_schema: String,
    pub(crate) arguments: Vec<String>,
}

pub(crate) struct Call<'c> {
    pub(crate) definition: &'c Arc<RoutineDefinition>,
    pub(crate) arguments: Vec<Value>,
    pub(crate) trigger: Option<TriggerCall>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Returned {
    Value(Value),
    Set(Vec<Value>),
}

pub(crate) fn invoke(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    call: Call<'_>,
) -> SqlResult<Returned> {
    let _depth = enter_routine()?;
    crate::cancel::check()?;
    let definition = Arc::clone(call.definition);
    if is_trigger_function(&definition) && call.trigger.is_none() {
        return Err(SqlError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "trigger functions can only be called as triggers",
        ));
    }
    let label = match definition.language {
        RoutineLanguage::PlPgSql => format!("PL/pgSQL function {}", definition.signature()),
        RoutineLanguage::Sql => format!("SQL function \"{}\"", base_name(&definition.name)),
    };
    let mut run = Run::new(writer, tables, settings, &definition, label);
    let arguments = run.complete_arguments(call.arguments)?;
    if definition.strict && arguments.iter().any(|argument| *argument == Value::Null) {
        return Ok(if definition.returns.is_set() {
            Returned::Set(Vec::new())
        } else {
            Returned::Value(Value::Null)
        });
    }
    run.bind_parameters(arguments);
    match definition.language {
        RoutineLanguage::Sql => run.sql_body(),
        RoutineLanguage::PlPgSql => {
            let program = crate::routines::compiled_program(run.tables, &definition)?;
            if let Some(trigger) = call.trigger {
                run.bind_trigger(trigger);
            }
            run.program(&program)
        }
    }
}

pub(crate) fn invoke_read_only(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    settings: &Settings,
    call: Call<'_>,
) -> SqlResult<Returned> {
    let mut writer = ReadOnlyWrite { inner: reader };
    invoke(&mut writer, tables, settings, call)
}

fn inline_definition(code: &str) -> RoutineDefinition {
    RoutineDefinition {
        name: INLINE_NAME.to_owned(),
        kind: RoutineKind::Function,
        dialect: Dialect::PostgreSql,
        language: RoutineLanguage::PlPgSql,
        parameters: Vec::new(),
        returns: RoutineReturns::One(ReturnType::Void),
        volatility: Volatility::Volatile,
        strict: false,
        security_definer: false,
        settings: Vec::new(),
        body: code.to_owned(),
        text: String::new(),
    }
}

pub(crate) fn run_inline(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    code: &str,
) -> SqlResult<()> {
    let _depth = enter_routine()?;
    let definition = inline_definition(code);
    let program = super::compile_for_run(&definition)?;
    let label = format!("PL/pgSQL function {INLINE_NAME}");
    let mut run = Run::new(writer, tables, settings, &definition, label);
    run.bind_parameters(Vec::new());
    run.program(&program).map(drop)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Flow {
    Next,
    Exit(Option<String>),
    Continue(Option<String>),
    Return,
}

enum Shape {
    Scalar(Option<SqlType>),
    Columns(Vec<(String, SqlType)>),
    Record,
}

struct OpenCursor {
    name: String,
    names: Vec<String>,
    rows: Vec<Vec<Value>>,
    position: usize,
    scroll: Option<bool>,
}

fn column_names(result: &QueryResult) -> Vec<String> {
    result
        .columns
        .as_ref()
        .map(|columns| columns.iter().map(|column| column.name.clone()).collect())
        .unwrap_or_default()
}

fn parsed(text: &str, settings: &Settings, frame: &RoutineFrame) -> SqlResult<Statement> {
    let tokens = tokenize_in(text, Dialect::PostgreSql)?;
    let mut parser = Parser::new(text, &tokens)
        .with_context(Parameters::Absent, settings)
        .in_dialect(Dialect::PostgreSql)
        .with_routine_frame(frame);
    let statement = parser.statement()?;
    if !parser.at_end() {
        return Err(parser.unexpected());
    }
    Ok(statement)
}

fn output_target(argument: &crate::ast::Expr) -> Option<Target> {
    match &argument.kind {
        crate::ast::ExprKind::Column {
            qualifier: None,
            name,
        } => Some(Target {
            name: name.clone(),
            path: Vec::new(),
            position: argument.position,
        }),
        _ => None,
    }
}

fn single_value(result: QueryResult, query: &str) -> SqlResult<Value> {
    let width = result.columns.as_ref().map_or(0, Vec::len);
    if width != 1 {
        return Err(SqlError::new(
            SqlState::SYNTAX_ERROR,
            format!("query \"{query}\" returned {width} columns"),
        ));
    }
    if result.rows.len() > 1 {
        return Err(SqlError::new(
            SqlState::CARDINALITY_VIOLATION,
            format!("query \"{query}\" returned more than one row"),
        ));
    }
    Ok(result
        .rows
        .into_iter()
        .next()
        .and_then(|row| row.into_iter().next())
        .unwrap_or(Value::Null))
}

fn unknown_variable(name: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_OBJECT,
        format!("variable \"{name}\" does not exist"),
    )
}

fn null_into_not_null(name: &str) -> SqlError {
    SqlError::new(
        SqlState::NULL_VALUE_NOT_ALLOWED,
        format!("null value cannot be assigned to variable \"{name}\" declared NOT NULL"),
    )
}

fn non_composite(kind: &str) -> SqlError {
    SqlError::new(
        SqlState::DATATYPE_MISMATCH,
        format!("cannot assign non-composite value to a {kind} variable"),
    )
}

fn coerce(slot: &Slot, value: Value) -> SqlResult<Value> {
    match slot {
        Slot::Typed(sql_type) => convert(value, *sql_type, Conversion::Assignment),
        Slot::Untyped => Ok(value),
        Slot::Cursor(_) => Ok(match value {
            Value::Null => Value::Null,
            other => Value::Text(display_value(&other)),
        }),
        Slot::Record => match value {
            Value::Null => Ok(Value::Null),
            other if record_of(&other).is_some() => Ok(other),
            _ => Err(non_composite("record")),
        },
        Slot::Row(columns) => {
            if value == Value::Null {
                return Ok(Value::Null);
            }
            let Some(record) = record_of(&value) else {
                return Err(non_composite("row"));
            };
            let mut fields = Vec::with_capacity(columns.len());
            for (at, (_, sql_type)) in columns.iter().enumerate() {
                let field = record.fields.get(at).cloned().unwrap_or(Value::Null);
                fields.push(convert(field, *sql_type, Conversion::Assignment)?);
            }
            Ok(row_record(columns, fields))
        }
    }
}

fn takes_collation(sql_type: SqlType) -> bool {
    matches!(
        sql_type,
        SqlType::Text | SqlType::Varchar(_) | SqlType::Bpchar(_)
    )
}

struct Run<'r> {
    writer: &'r mut dyn KeyValueWrite,
    tables: &'r mut TableCache,
    settings: &'r Settings,
    definition: &'r RoutineDefinition,
    label: String,
    frame: RoutineFrame,
    outputs: Vec<usize>,
    output_columns: Vec<(String, SqlType)>,
    found: Option<usize>,
    row_count: u64,
    undo: UndoLog,
    handling: Vec<SqlError>,
    failure: Option<(usize, String)>,
    result: Option<Value>,
    rows: Vec<Value>,
    cursors: Vec<OpenCursor>,
    portals: usize,
    print_strict_params: bool,
}

impl<'r> Run<'r> {
    fn new(
        writer: &'r mut dyn KeyValueWrite,
        tables: &'r mut TableCache,
        settings: &'r Settings,
        definition: &'r RoutineDefinition,
        label: String,
    ) -> Self {
        Self {
            writer,
            tables,
            settings,
            definition,
            label,
            frame: RoutineFrame::default(),
            outputs: Vec::new(),
            output_columns: Vec::new(),
            found: None,
            row_count: 0,
            undo: UndoLog::default(),
            handling: Vec::new(),
            failure: None,
            result: None,
            rows: Vec::new(),
            cursors: Vec::new(),
            portals: 0,
            print_strict_params: false,
        }
    }

    fn complete_arguments(&mut self, arguments: Vec<Value>) -> SqlResult<Vec<Value>> {
        let definition = self.definition;
        let inputs: Vec<_> = definition
            .parameters
            .iter()
            .filter(|parameter| parameter.mode.is_input())
            .collect();
        if arguments.len() > inputs.len() {
            return Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!(
                    "{} {} takes at most {} arguments",
                    definition.kind.noun(),
                    definition.signature(),
                    inputs.len()
                ),
            ));
        }
        let mut given = arguments.into_iter();
        let mut values = Vec::with_capacity(inputs.len());
        for parameter in inputs {
            let value = match given.next() {
                Some(value) => value,
                None => match &parameter.default {
                    Some(default) => self.text_value(default)?,
                    None => {
                        return Err(SqlError::new(
                            SqlState::UNDEFINED_FUNCTION,
                            format!(
                                "{} {} needs more arguments",
                                definition.kind.noun(),
                                definition.signature()
                            ),
                        ));
                    }
                },
            };
            values.push(convert(value, parameter.sql_type, Conversion::Assignment)?);
        }
        Ok(values)
    }

    fn declare_parameter(
        &mut self,
        name: String,
        slot: Slot,
        value: Value,
        label: Option<String>,
        output: bool,
    ) {
        let index = self.frame.variables.len();
        let mut variable = FrameVariable::new(name, slot, value);
        variable.label = label;
        self.frame.variables.push(variable);
        self.frame.positional.push(index);
        if output {
            self.outputs.push(index);
        }
    }

    fn bind_parameters(&mut self, arguments: Vec<Value>) {
        let definition = self.definition;
        let procedural = definition.language == RoutineLanguage::PlPgSql;
        let label = Some(base_name(&definition.name).to_owned());
        let mut inputs = arguments.into_iter();
        for (at, parameter) in definition.parameters.iter().enumerate() {
            let output = parameter.mode.is_output();
            if output {
                let name = parameter
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("column{}", at + 1));
                self.output_columns.push((name, parameter.sql_type));
            }
            if !procedural && !parameter.mode.is_input() {
                continue;
            }
            let value = if parameter.mode.is_input() {
                inputs.next().unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            let name = parameter
                .name
                .clone()
                .unwrap_or_else(|| format!("${}", at + 1));
            self.declare_parameter(
                name,
                Slot::Typed(parameter.sql_type),
                value,
                label.clone(),
                output,
            );
        }
        if let RoutineReturns::Table(columns) = &definition.returns {
            for (name, sql_type) in columns {
                self.output_columns.push((name.clone(), *sql_type));
                if procedural {
                    self.declare_parameter(
                        name.clone(),
                        Slot::Typed(*sql_type),
                        Value::Null,
                        label.clone(),
                        true,
                    );
                }
            }
        }
        if procedural {
            let mut found =
                FrameVariable::new("found", Slot::Typed(SqlType::Boolean), Value::Boolean(false));
            found.label = label;
            self.found = Some(self.frame.variables.len());
            self.frame.variables.push(found);
        }
    }

    fn bind_trigger(&mut self, trigger: TriggerCall) {
        let arguments = trigger.arguments.len();
        let argv = Value::Array(Box::new(ArrayValue {
            element: ArrayElement::Text,
            lower_bound: 0,
            items: trigger.arguments.into_iter().map(Value::Text).collect(),
        }));
        let variables = [
            ("new", Slot::Record, trigger.new.unwrap_or(Value::Null)),
            ("old", Slot::Record, trigger.old.unwrap_or(Value::Null)),
            ("tg_name", Slot::Typed(SqlType::Text), Value::Text(trigger.name)),
            ("tg_when", Slot::Typed(SqlType::Text), Value::Text(trigger.when.to_owned())),
            ("tg_level", Slot::Typed(SqlType::Text), Value::Text(trigger.level.to_owned())),
            ("tg_op", Slot::Typed(SqlType::Text), Value::Text(trigger.operation.to_owned())),
            ("tg_relid", Slot::Untyped, Value::Oid(trigger.relation_oid)),
            ("tg_relname", Slot::Typed(SqlType::Text), Value::Text(trigger.table_name.clone())),
            ("tg_table_name", Slot::Typed(SqlType::Text), Value::Text(trigger.table_name)),
            ("tg_table_schema", Slot::Typed(SqlType::Text), Value::Text(trigger.table_schema)),
            (
                "tg_nargs",
                Slot::Typed(SqlType::Integer),
                Value::Integer(i32::try_from(arguments).unwrap_or(i32::MAX)),
            ),
            ("tg_argv", Slot::Typed(SqlType::Array(ArrayElement::Text)), argv),
        ];
        let label = Some(base_name(&self.definition.name).to_owned());
        for (name, slot, value) in variables {
            let mut variable = FrameVariable::new(name, slot, value);
            variable.label = label.clone();
            self.frame.variables.push(variable);
        }
    }

    fn program(&mut self, program: &Program) -> SqlResult<Returned> {
        if program.variable_conflict == VariableConflict::UseColumn {
            return Err(SqlError::not_supported("#variable_conflict use_column").with_hint(
                "EggDB resolves a name that is both a PL/pgSQL variable and a column to the variable.",
            ));
        }
        self.print_strict_params = program.print_strict_params;
        match self.block(&program.block) {
            Ok(_) => self.finish().map_err(|error| self.located(error)),
            Err(error) => Err(self.located(error)),
        }
    }

    fn located(&mut self, error: SqlError) -> SqlError {
        let error = error.without_position();
        let line = match self.failure.take() {
            Some((position, statement)) => format!(
                "{} line {} {}",
                self.label,
                line_of(&self.definition.body, position),
                statement
            ),
            None => self.label.clone(),
        };
        with_context_line(error, &line)
    }

    fn fail_at(&mut self, position: usize, statement: impl FnOnce() -> String) {
        if self.failure.is_none() {
            self.failure = Some((position, statement()));
        }
    }

    fn finish(&mut self) -> SqlResult<Returned> {
        let definition = self.definition;
        if definition.kind == RoutineKind::Procedure {
            return Ok(Returned::Value(self.procedure_outputs()));
        }
        if definition.returns.is_set() {
            return Ok(Returned::Set(std::mem::take(&mut self.rows)));
        }
        match &definition.returns {
            RoutineReturns::One(ReturnType::Void) => Ok(Returned::Value(Value::Null)),
            RoutineReturns::One(ReturnType::Trigger) => match self.result.take() {
                Some(value) => Ok(Returned::Value(value)),
                None => Err(SqlError::new(
                    SqlState::FUNCTION_EXECUTED_NO_RETURN_STATEMENT,
                    "control reached end of trigger procedure without RETURN",
                )),
            },
            _ if !self.outputs.is_empty() => Ok(Returned::Value(self.outputs_value())),
            _ => match self.result.take() {
                Some(value) => Ok(Returned::Value(value)),
                None => Err(SqlError::new(
                    SqlState::FUNCTION_EXECUTED_NO_RETURN_STATEMENT,
                    "control reached end of function without RETURN",
                )),
            },
        }
    }

    fn output_record(&self) -> Value {
        let names = self
            .outputs
            .iter()
            .map(|index| self.frame.variables[*index].name.clone())
            .collect();
        let fields = self
            .outputs
            .iter()
            .map(|index| self.frame.variables[*index].value.clone())
            .collect();
        records::record_value(RECORD_OID, fields, names)
    }

    fn outputs_value(&self) -> Value {
        match self.outputs.as_slice() {
            [single] => self.frame.variables[*single].value.clone(),
            _ => self.output_record(),
        }
    }

    fn procedure_outputs(&self) -> Value {
        if self.outputs.is_empty() {
            Value::Null
        } else {
            self.output_record()
        }
    }

    fn shape(&self) -> Shape {
        match &self.definition.returns {
            RoutineReturns::Table(columns) if columns.len() == 1 => {
                Shape::Scalar(Some(columns[0].1))
            }
            RoutineReturns::Table(columns) => Shape::Columns(columns.clone()),
            RoutineReturns::One(ReturnType::Type(sql_type))
            | RoutineReturns::Set(ReturnType::Type(sql_type)) => {
                if is_composite_type(*sql_type) {
                    Shape::Record
                } else {
                    Shape::Scalar(Some(*sql_type))
                }
            }
            _ if self.output_columns.len() == 1 => Shape::Scalar(Some(self.output_columns[0].1)),
            _ if self.output_columns.len() > 1 => Shape::Columns(self.output_columns.clone()),
            _ => Shape::Record,
        }
    }

    fn element_value(&self, value: Value) -> SqlResult<Value> {
        match self.shape() {
            Shape::Scalar(Some(sql_type)) => convert(value, sql_type, Conversion::Assignment),
            _ => Ok(value),
        }
    }

    fn row_element(&self, row: Vec<Value>, names: &[String]) -> SqlResult<Value> {
        match self.shape() {
            Shape::Scalar(sql_type) => {
                let first = row.into_iter().next().unwrap_or(Value::Null);
                match sql_type {
                    Some(sql_type) => convert(first, sql_type, Conversion::Assignment),
                    None => Ok(first),
                }
            }
            Shape::Columns(columns) => {
                if row.len() != columns.len() {
                    return Err(SqlError::new(
                        SqlState::DATATYPE_MISMATCH,
                        "structure of query does not match function result type",
                    )
                    .with_detail(format!(
                        "Number of returned columns ({}) does not match expected column count ({}).",
                        row.len(),
                        columns.len()
                    )));
                }
                let mut fields = Vec::with_capacity(columns.len());
                for (value, (_, sql_type)) in row.into_iter().zip(&columns) {
                    fields.push(convert(value, *sql_type, Conversion::Assignment)?);
                }
                Ok(row_record(&columns, fields))
            }
            Shape::Record => Ok(match row.as_slice() {
                [single] if record_of(single).is_some() => single.clone(),
                _ => records::record_value(RECORD_OID, row, names.to_vec()),
            }),
        }
    }

    fn with_writer<T>(
        &mut self,
        body: impl FnOnce(&mut dyn KeyValueWrite, &mut TableCache, &Settings) -> SqlResult<T>,
    ) -> SqlResult<T> {
        if self.undo.is_recording() {
            let mut writer = UndoWrite {
                inner: &mut *self.writer,
                log: &mut self.undo,
            };
            body(&mut writer, self.tables, self.settings)
        } else {
            body(&mut *self.writer, self.tables, self.settings)
        }
    }

    fn execute(&mut self, statement: Statement) -> SqlResult<QueryResult> {
        let mut result = self.with_writer(|writer, tables, settings| {
            run_sql(writer, tables, settings, statement)
        })?;
        push_statement_notices(std::mem::take(&mut result.notices));
        Ok(result)
    }

    fn call_step(&mut self, text: &str) -> SqlResult<Flow> {
        let Statement::Call(call) = parsed(text, self.settings, &self.frame)? else {
            self.query_text(text)?;
            return Ok(Flow::Next);
        };
        let written = parsed(text, self.settings, &RoutineFrame::default()).ok();
        let (mut result, sources) = self.with_writer(|writer, tables, settings| {
            let binding = crate::name_resolution::Binding {
                settings,
                walk_expressions: true,
            };
            let mut call = call;
            for argument in &mut call.arguments {
                binding.condition(&*writer, tables, argument)?;
            }
            crate::routines::call_procedure_outputs(writer, tables, settings, &call)
        })?;
        push_statement_notices(std::mem::take(&mut result.notices));
        let (Some(Statement::Call(written)), Some(row)) = (written, result.rows.pop()) else {
            return Ok(Flow::Next);
        };
        for (value, source) in row.into_iter().zip(sources) {
            let Some(argument) = source.and_then(|at| written.arguments.get(at)) else {
                continue;
            };
            if let Some(target) = output_target(argument) {
                self.assign(&target, value)?;
            }
        }
        Ok(Flow::Next)
    }

    fn query_text(&mut self, text: &str) -> SqlResult<QueryResult> {
        let statement = parsed(text, self.settings, &self.frame)?;
        self.execute(statement)
    }

    fn text_value(&mut self, text: &str) -> SqlResult<Value> {
        let query = format!("SELECT {text}");
        let result = self.query_text(&query)?;
        single_value(result, &query)
    }

    fn value(&mut self, fragment: &Fragment) -> SqlResult<Value> {
        self.text_value(&fragment.text)
    }

    fn condition(&mut self, fragment: &Fragment) -> SqlResult<bool> {
        match self.value(fragment)? {
            Value::Null => Ok(false),
            Value::Boolean(value) => Ok(value),
            other => Ok(matches!(
                convert(other, SqlType::Boolean, Conversion::Explicit)?,
                Value::Boolean(true)
            )),
        }
    }

    fn integer(&mut self, fragment: &Fragment, null_message: &str) -> SqlResult<i64> {
        match convert(self.value(fragment)?, SqlType::Integer, Conversion::Assignment)? {
            Value::Integer(value) => Ok(i64::from(value)),
            _ => Err(SqlError::new(SqlState::NULL_VALUE_NOT_ALLOWED, null_message)),
        }
    }

    fn set_found(&mut self, rows: u64) {
        self.row_count = rows;
        self.set_found_flag(rows > 0);
    }

    fn set_found_flag(&mut self, found: bool) {
        if let Some(index) = self.found {
            self.frame.variables[index].value = Value::Boolean(found);
        }
    }

    fn relation_columns(&mut self, name: &str) -> SqlResult<Vec<(String, SqlType)>> {
        let internal = crate::schemas::resolve_relation(&*self.writer, name)?;
        match self.tables.relation(&*self.writer, &internal)? {
            Some(Relation::Table(table)) => Ok(table
                .columns
                .iter()
                .map(|column| (column.name.clone(), column.sql_type))
                .collect()),
            Some(Relation::View(view)) => Ok(view
                .columns
                .iter()
                .map(|column| (column.name.clone(), column.sql_type.unwrap_or(SqlType::Text)))
                .collect()),
            None => Err(SqlError::new(
                SqlState::UNDEFINED_TABLE,
                format!("relation \"{}\" does not exist", display(name)),
            )),
        }
    }

    fn copied_slot(&mut self, parts: &[String]) -> SqlResult<Slot> {
        if let [name] = parts
            && let Some(index) = self.frame.index_of(name)
        {
            return Ok(match &self.frame.variables[index].slot {
                Slot::Cursor(_) => Slot::Cursor(None),
                slot => slot.clone(),
            });
        }
        if let [record, field] = parts
            && let Some(index) = self.frame.index_of(record)
            && let Slot::Row(columns) = &self.frame.variables[index].slot
            && let Some((_, sql_type)) = columns.iter().find(|(name, _)| name == field)
        {
            return Ok(Slot::Typed(*sql_type));
        }
        let (relation, column) = match parts {
            [table, column] => (table.clone(), column),
            [schema, table, column] => (crate::schemas::qualified(schema, table), column),
            _ => return Err(unknown_variable(&parts.join("."))),
        };
        self.relation_columns(&relation)?
            .into_iter()
            .find(|(name, _)| name == column)
            .map(|(_, sql_type)| Slot::Typed(sql_type))
            .ok_or_else(|| {
                SqlError::new(
                    SqlState::UNDEFINED_COLUMN,
                    format!(
                        "column \"{column}\" of relation \"{}\" does not exist",
                        display(&relation)
                    ),
                )
            })
    }

    fn slot_of(&mut self, variable_type: &VariableType) -> SqlResult<Slot> {
        Ok(match variable_type {
            VariableType::Sql(sql_type) => Slot::Typed(*sql_type),
            VariableType::Record => Slot::Record,
            VariableType::RefCursor => Slot::Cursor(None),
            VariableType::Row(name) => Slot::Row(self.relation_columns(name)?),
            VariableType::TypeOf(parts) => self.copied_slot(parts)?,
        })
    }

    fn declare(&mut self, block: &Block) -> SqlResult<()> {
        for declaration in &block.declarations {
            if let Err(error) = self.declare_one(block, declaration) {
                self.fail_at(declaration.position, || {
                    "during statement block local variable initialization".to_owned()
                });
                return Err(error);
            }
        }
        Ok(())
    }

    fn declare_one(&mut self, block: &Block, declaration: &Declaration) -> SqlResult<()> {
        let mut variable = match &declaration.kind {
            DeclarationKind::Variable(declared) => {
                let slot = self.slot_of(&declared.variable_type)?;
                if declared.collation.is_some()
                    && let Slot::Typed(sql_type) = &slot
                    && !takes_collation(*sql_type)
                {
                    return Err(SqlError::new(
                        SqlState::DATATYPE_MISMATCH,
                        format!("collations are not supported by type {}", sql_type.name()),
                    ));
                }
                let value = match &declared.default {
                    Some(default) => {
                        let value = self.value(default)?;
                        coerce(&slot, value)?
                    }
                    None => Value::Null,
                };
                if declared.not_null && value == Value::Null {
                    return Err(null_into_not_null(&declaration.name));
                }
                let mut variable = FrameVariable::new(declaration.name.clone(), slot, value);
                variable.constant = declared.constant;
                variable.not_null = declared.not_null;
                variable
            }
            DeclarationKind::Alias(target) => {
                let index = match target {
                    AliasTarget::Parameter(number) => self.frame.positional_index(*number),
                    AliasTarget::Name(name) => self.frame.index_of(name),
                };
                let Some(index) = index else {
                    return Err(unknown_variable(&declaration.name));
                };
                let mut variable =
                    FrameVariable::new(declaration.name.clone(), Slot::Untyped, Value::Null);
                variable.alias = Some(index);
                variable
            }
            DeclarationKind::Cursor(cursor) => FrameVariable::new(
                declaration.name.clone(),
                Slot::Cursor(Some(Box::new(cursor.clone()))),
                Value::Null,
            ),
        };
        variable.label = block.label.clone();
        self.frame.variables.push(variable);
        Ok(())
    }

    fn block(&mut self, block: &Block) -> SqlResult<Flow> {
        let scope = self.frame.variables.len();
        let outcome = self.declare(block).and_then(|()| {
            if block.handlers.is_empty() {
                self.steps(&block.steps)
            } else {
                self.guarded(block)
            }
        });
        self.frame.variables.truncate(scope);
        match outcome? {
            Flow::Exit(Some(label)) if block.label.as_deref() == Some(label.as_str()) => {
                Ok(Flow::Next)
            }
            flow => Ok(flow),
        }
    }

    fn guarded(&mut self, block: &Block) -> SqlResult<Flow> {
        let mark = self.undo.begin();
        let cursors = self.cursors.len();
        let error = match self.steps(&block.steps) {
            Ok(flow) => {
                self.undo.release();
                return Ok(flow);
            }
            Err(error) => error,
        };
        let Some(handler) = block
            .handlers
            .iter()
            .find(|handler| handler.catches(error.state()))
        else {
            self.undo.release();
            return Err(error);
        };
        self.undo.roll_back(&mut *self.writer, mark)?;
        self.cursors.truncate(cursors);
        self.failure = None;
        self.handle(handler, error)
    }

    fn handle(&mut self, handler: &Handler, error: SqlError) -> SqlResult<Flow> {
        let scope = self.frame.variables.len();
        self.frame.variables.push(FrameVariable::new(
            "sqlstate",
            Slot::Typed(SqlType::Text),
            Value::Text(error.state().code().to_owned()),
        ));
        self.frame.variables.push(FrameVariable::new(
            "sqlerrm",
            Slot::Typed(SqlType::Text),
            Value::Text(error.message().to_owned()),
        ));
        self.handling.push(error);
        let outcome = self.steps(&handler.steps);
        self.handling.pop();
        self.frame.variables.truncate(scope);
        outcome
    }

    fn steps(&mut self, steps: &[Step]) -> SqlResult<Flow> {
        for step in steps {
            let flow = self.step(step)?;
            if flow != Flow::Next {
                return Ok(flow);
            }
        }
        Ok(Flow::Next)
    }

    fn step(&mut self, step: &Step) -> SqlResult<Flow> {
        let outcome = self.step_kind(&step.kind);
        if outcome.is_err() {
            self.fail_at(step.position, || format!("at {}", step.kind.name()));
        }
        outcome
    }

    fn step_kind(&mut self, kind: &StepKind) -> SqlResult<Flow> {
        match kind {
            StepKind::Block(block) => self.block(block),
            StepKind::Assign { target, value } => {
                let value = self.value(value)?;
                self.assign(target, value)?;
                Ok(Flow::Next)
            }
            StepKind::Sql { statement, into } => self.sql(statement, into.as_ref()),
            StepKind::Perform(query) => {
                let result = self.query_text(&query.text)?;
                self.set_found(processed_rows(&result));
                Ok(Flow::Next)
            }
            StepKind::Call(call) => self.call_step(&call.text),
            StepKind::Execute(dynamic) => self.execute_dynamic(dynamic),
            StepKind::GetDiagnostics { stacked, items } => self.diagnostics(*stacked, items),
            StepKind::If {
                branches,
                otherwise,
            } => {
                for branch in branches {
                    if self.condition(&branch.condition)? {
                        return self.steps(&branch.steps);
                    }
                }
                match otherwise {
                    Some(steps) => self.steps(steps),
                    None => Ok(Flow::Next),
                }
            }
            StepKind::Case {
                subject,
                arms,
                otherwise,
            } => self.case(subject.as_ref(), arms, otherwise.as_deref()),
            StepKind::Loop(found) => self.run_loop(found),
            StepKind::Exit { label, condition } => self.leave(condition.as_ref(), || {
                Flow::Exit(label.clone())
            }),
            StepKind::Continue { label, condition } => self.leave(condition.as_ref(), || {
                Flow::Continue(label.clone())
            }),
            StepKind::Return(value) => self.return_value(value.as_ref()),
            StepKind::ReturnNext(value) => self.return_next(value.as_ref()),
            StepKind::ReturnQuery(source) => self.return_query(source),
            StepKind::Raise(raise) => self.raise(raise),
            StepKind::Reraise => match self.handling.last() {
                Some(error) => Err(error.clone()),
                None => Err(SqlError::new(
                    SqlState::STACKED_DIAGNOSTICS_ACCESSED_WITHOUT_ACTIVE_HANDLER,
                    "RAISE without parameters cannot be used outside an exception handler",
                )),
            },
            StepKind::Assert { condition, message } => self.assert(condition, message.as_ref()),
            StepKind::Open {
                cursor,
                scroll,
                source,
            } => self.open(cursor, *scroll, source),
            StepKind::Fetch {
                cursor,
                direction,
                into,
            } => self.fetch(cursor, direction, into),
            StepKind::Move { cursor, direction } => self.move_cursor(cursor, direction),
            StepKind::Close(cursor) => {
                let at = self.open_cursor(cursor)?;
                self.cursors.remove(at);
                Ok(Flow::Next)
            }
            StepKind::Commit { chain } => Err(self.transaction_command("COMMIT", *chain)),
            StepKind::Rollback { chain } => Err(self.transaction_command("ROLLBACK", *chain)),
            StepKind::Null => Ok(Flow::Next),
        }
    }

    fn leave(
        &mut self,
        condition: Option<&Fragment>,
        flow: impl FnOnce() -> Flow,
    ) -> SqlResult<Flow> {
        match condition {
            Some(condition) if !self.condition(condition)? => Ok(Flow::Next),
            _ => Ok(flow()),
        }
    }

    fn transaction_command(&self, command: &str, chain: bool) -> SqlError {
        if self.definition.kind == RoutineKind::Function {
            return SqlError::new(
                SqlState::INVALID_TRANSACTION_TERMINATION,
                "invalid transaction termination",
            );
        }
        let written = if chain {
            format!("{command} AND CHAIN")
        } else {
            command.to_owned()
        };
        SqlError::not_supported(format!("{written} inside a procedure")).with_hint(
            "A procedure's changes commit or roll back with the statement that called it.",
        )
    }

    fn sql(&mut self, statement: &Fragment, into: Option<&IntoTargets>) -> SqlResult<Flow> {
        let result = self.query_text(&statement.text)?;
        self.set_found(processed_rows(&result));
        match into {
            Some(into) => self.store_into(into, result)?,
            None if result.columns.is_some() => {
                return Err(SqlError::new(
                    SqlState::SYNTAX_ERROR,
                    "query has no destination for result data",
                )
                .with_hint("If you want to discard the results of a SELECT, use PERFORM instead."));
            }
            None => {}
        }
        Ok(Flow::Next)
    }

    fn strict_error(&self, state: SqlState, message: &str) -> SqlError {
        let error = SqlError::new(state, message);
        if !self.print_strict_params || self.frame.positional.is_empty() {
            return error;
        }
        let shown: Vec<String> = self
            .frame
            .positional
            .iter()
            .enumerate()
            .map(|(at, index)| match &self.frame.variables[*index].value {
                Value::Null => format!("${} = NULL", at + 1),
                value => format!("${} = '{}'", at + 1, display_value(value)),
            })
            .collect();
        error.with_detail(format!("parameters: {}", shown.join(", ")))
    }

    fn store_into(&mut self, into: &IntoTargets, result: QueryResult) -> SqlResult<()> {
        let names = column_names(&result);
        let selected = result.tag.starts_with("SELECT");
        let mut rows = result.rows.into_iter();
        let first = rows.next();
        let more = rows.next().is_some();
        if into.strict && first.is_none() {
            return Err(self.strict_error(SqlState::NO_DATA_FOUND, "query returned no rows"));
        }
        if more && (into.strict || !selected) {
            return Err(self
                .strict_error(SqlState::TOO_MANY_ROWS, "query returned more than one row")
                .with_hint("Make sure the query returns a single row, or use LIMIT 1."));
        }
        self.assign_row(&into.targets, first, &names)
    }

    fn assign_row(
        &mut self,
        targets: &[Target],
        row: Option<Vec<Value>>,
        names: &[String],
    ) -> SqlResult<()> {
        if let [target] = targets
            && target.path.is_empty()
            && let Some(index) = self.frame.target_index(&target.name)
            && self.frame.variables[index].is_composite()
        {
            let fields = row.unwrap_or_else(|| vec![Value::Null; names.len()]);
            let value = match fields.as_slice() {
                [single] if record_of(single).is_some() => single.clone(),
                _ => records::record_value(RECORD_OID, fields, names.to_vec()),
            };
            return self.store(index, value);
        }
        let mut values = row.unwrap_or_default().into_iter();
        for target in targets {
            let value = values.next().unwrap_or(Value::Null);
            self.assign(target, value)?;
        }
        Ok(())
    }

    fn writable(&self, index: usize) -> SqlResult<()> {
        let variable = &self.frame.variables[index];
        if variable.constant {
            return Err(SqlError::new(
                SqlState::ERROR_IN_ASSIGNMENT,
                format!("variable \"{}\" is declared CONSTANT", variable.name),
            ));
        }
        Ok(())
    }

    fn store(&mut self, index: usize, value: Value) -> SqlResult<()> {
        self.writable(index)?;
        let variable = &self.frame.variables[index];
        let value = coerce(&variable.slot, value)?;
        if variable.not_null && value == Value::Null {
            return Err(null_into_not_null(&variable.name));
        }
        self.frame.variables[index].value = value;
        Ok(())
    }

    fn assign(&mut self, target: &Target, value: Value) -> SqlResult<()> {
        let Some(index) = self.frame.target_index(&target.name) else {
            return Err(unknown_variable(&target.name));
        };
        match target.path.as_slice() {
            [] => self.store(index, value),
            [TargetPart::Field(field)] => self.assign_field(index, field, value),
            [TargetPart::Index(subscript)] => {
                let subscript = self.integer(
                    subscript,
                    "array subscript in assignment must not be null",
                )?;
                self.assign_element(index, subscript, value)
            }
            _ => Err(SqlError::not_supported(
                "assigning to a nested field or element of a PL/pgSQL variable",
            )),
        }
    }

    fn assign_field(&mut self, index: usize, field: &str, value: Value) -> SqlResult<()> {
        self.writable(index)?;
        let variable = &self.frame.variables[index];
        let current = match (&variable.slot, &variable.value) {
            (Slot::Record, Value::Null) => return Err(unassigned_record(&variable.name)),
            (Slot::Row(columns), Value::Null) => {
                row_record(columns, vec![Value::Null; columns.len()])
            }
            (_, current) => current.clone(),
        };
        let Some(record) = record_of(&current) else {
            return Err(SqlError::new(
                SqlState::DATATYPE_MISMATCH,
                format!("variable \"{}\" is not a record", variable.name),
            ));
        };
        let names = records::field_names(record);
        let Some(at) = names.iter().position(|name| name == field) else {
            return Err(missing_field(&variable.name, field));
        };
        let field_type = match &variable.slot {
            Slot::Row(columns) => columns.get(at).map(|(_, sql_type)| *sql_type),
            _ => records::field_type(current.sql_type(), field)
                .or_else(|| record.fields.get(at).and_then(Value::sql_type)),
        };
        let mut fields = record.fields.clone();
        fields[at] = match field_type {
            Some(sql_type) => convert(value, sql_type, Conversion::Assignment)?,
            None => value,
        };
        let updated = records::record_value(record.type_oid, fields, record.names.clone());
        self.frame.variables[index].value = updated;
        Ok(())
    }

    fn assign_element(&mut self, index: usize, subscript: i64, value: Value) -> SqlResult<()> {
        self.writable(index)?;
        let variable = &self.frame.variables[index];
        let element = match (&variable.slot, &variable.value) {
            (_, Value::Array(array)) => array.element,
            (Slot::Typed(SqlType::Array(element)), _) => *element,
            _ => {
                return Err(SqlError::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!("variable \"{}\" is not an array", variable.name),
                ));
            }
        };
        let mut array = match &variable.value {
            Value::Array(array) => (**array).clone(),
            _ => ArrayValue {
                element,
                lower_bound: i32::try_from(subscript).unwrap_or(1),
                items: Vec::new(),
            },
        };
        let value = convert(value, element.sql_type(), Conversion::Assignment)?;
        let lower = i64::from(array.lower_bound);
        let length = array.items.len() as i64;
        let growth = if subscript < lower {
            lower - subscript
        } else {
            (subscript - lower + 1 - length).max(0)
        };
        if growth > MAX_ARRAY_GROWTH {
            return Err(SqlError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                "array size exceeds the maximum allowed",
            ));
        }
        if subscript < lower {
            let mut items = vec![Value::Null; usize::try_from(lower - subscript).unwrap_or(0)];
            items.append(&mut array.items);
            array.items = items;
            array.lower_bound = i32::try_from(subscript).map_err(|_| {
                SqlError::new(SqlState::ARRAY_SUBSCRIPT_ERROR, "array subscript out of range")
            })?;
        }
        let offset = usize::try_from(subscript - i64::from(array.lower_bound)).unwrap_or(0);
        if offset >= array.items.len() {
            array.items.resize(offset + 1, Value::Null);
        }
        array.items[offset] = value;
        self.frame.variables[index].value = Value::Array(Box::new(array));
        Ok(())
    }

    fn case(
        &mut self,
        subject: Option<&Fragment>,
        arms: &[CaseArm],
        otherwise: Option<&[Step]>,
    ) -> SqlResult<Flow> {
        let subject = match subject {
            Some(subject) => Some(self.value(subject)?),
            None => None,
        };
        for arm in arms {
            for candidate in &arm.values {
                let matched = match &subject {
                    Some(subject) => {
                        let candidate = self.value(candidate)?;
                        *subject != Value::Null
                            && candidate != Value::Null
                            && compare_values(subject, &candidate) == Ordering::Equal
                    }
                    None => self.condition(candidate)?,
                };
                if matched {
                    return self.steps(&arm.steps);
                }
            }
        }
        match otherwise {
            Some(steps) => self.steps(steps),
            None => Err(SqlError::new(SqlState::CASE_NOT_FOUND, "case not found")
                .with_hint("CASE statement is missing ELSE part.")),
        }
    }

    fn run_loop(&mut self, found: &Loop) -> SqlResult<Flow> {
        let scope = self.frame.variables.len();
        let outcome = self.loop_kind(found);
        self.frame.variables.truncate(scope);
        outcome
    }

    fn iteration(&mut self, found: &Loop) -> SqlResult<Option<Flow>> {
        crate::cancel::check()?;
        let own = |label: &Option<String>| match label {
            None => true,
            Some(label) => found.label.as_deref() == Some(label.as_str()),
        };
        Ok(match self.steps(&found.body)? {
            Flow::Next => None,
            Flow::Continue(label) if own(&label) => None,
            Flow::Exit(label) if own(&label) => Some(Flow::Next),
            flow => Some(flow),
        })
    }

    fn loop_kind(&mut self, found: &Loop) -> SqlResult<Flow> {
        match &found.kind {
            LoopKind::Plain => loop {
                if let Some(flow) = self.iteration(found)? {
                    return Ok(flow);
                }
            },
            LoopKind::While(condition) => {
                while self.condition(condition)? {
                    if let Some(flow) = self.iteration(found)? {
                        return Ok(flow);
                    }
                }
                Ok(Flow::Next)
            }
            LoopKind::Range {
                variable,
                reverse,
                lower,
                upper,
                step,
            } => self.range_loop(found, variable, *reverse, (lower, upper), step.as_ref()),
            LoopKind::Query { targets, source } => {
                let (names, rows) = self.source_rows(source)?;
                self.rows_loop(found, targets, &names, rows)
            }
            LoopKind::Cursor {
                record,
                cursor,
                arguments,
            } => self.cursor_loop(found, record, cursor, arguments),
            LoopKind::Foreach {
                targets,
                slice,
                array,
            } => self.foreach_loop(found, targets, *slice, array),
        }
    }

    fn range_loop(
        &mut self,
        found: &Loop,
        variable: &str,
        reverse: bool,
        (lower, upper): (&Fragment, &Fragment),
        step: Option<&Fragment>,
    ) -> SqlResult<Flow> {
        let lower = self.integer(lower, "lower bound of FOR loop cannot be null")?;
        let upper = self.integer(upper, "upper bound of FOR loop cannot be null")?;
        let step = match step {
            Some(step) => self.integer(step, "BY value of FOR loop cannot be null")?,
            None => 1,
        };
        if step <= 0 {
            return Err(SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                "BY value of FOR loop must be greater than zero",
            ));
        }
        let index = self.frame.variables.len();
        let mut counter = FrameVariable::new(variable, Slot::Typed(SqlType::Integer), Value::Null);
        counter.label = found.label.clone();
        self.frame.variables.push(counter);
        let mut current = lower;
        let mut iterated = false;
        loop {
            let within = if reverse {
                current >= upper
            } else {
                current <= upper
            };
            if !within {
                break;
            }
            self.frame.variables[index].value =
                Value::Integer(i32::try_from(current).unwrap_or_default());
            iterated = true;
            if let Some(flow) = self.iteration(found)? {
                self.set_found_flag(true);
                return Ok(flow);
            }
            current = if reverse { current - step } else { current + step };
        }
        self.set_found_flag(iterated);
        Ok(Flow::Next)
    }

    fn rows_loop(
        &mut self,
        found: &Loop,
        targets: &[Target],
        names: &[String],
        rows: Vec<Vec<Value>>,
    ) -> SqlResult<Flow> {
        let mut iterated = false;
        for row in rows {
            iterated = true;
            self.assign_row(targets, Some(row), names)?;
            if let Some(flow) = self.iteration(found)? {
                self.set_found_flag(true);
                return Ok(flow);
            }
        }
        self.set_found_flag(iterated);
        Ok(Flow::Next)
    }

    fn cursor_loop(
        &mut self,
        found: &Loop,
        record: &str,
        cursor: &str,
        arguments: &[CursorValue],
    ) -> SqlResult<Flow> {
        self.open(cursor, None, &OpenSource::Bound(arguments.to_vec()))?;
        let at = self.open_cursor(cursor)?;
        let opened = self.cursors.remove(at);
        let mut variable = FrameVariable::new(record, Slot::Record, Value::Null);
        variable.label = found.label.clone();
        self.frame.variables.push(variable);
        let target = Target {
            name: record.to_owned(),
            path: Vec::new(),
            position: 0,
        };
        self.rows_loop(found, std::slice::from_ref(&target), &opened.names, opened.rows)
    }

    fn foreach_loop(
        &mut self,
        found: &Loop,
        targets: &[Target],
        slice: usize,
        array: &Fragment,
    ) -> SqlResult<Flow> {
        let array = match self.value(array)? {
            Value::Null => {
                return Err(SqlError::new(
                    SqlState::NULL_VALUE_NOT_ALLOWED,
                    "FOREACH expression must not be null",
                ));
            }
            Value::Array(array) => *array,
            other => {
                return Err(SqlError::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!("FOREACH expression must yield an array, not type {}", other.type_name()),
                ));
            }
        };
        if slice > 1 {
            return Err(SqlError::new(
                SqlState::ARRAY_SUBSCRIPT_ERROR,
                format!("slice dimension ({slice}) is out of the valid range 0..1"),
            ));
        }
        let items = if slice == 1 {
            vec![Value::Array(Box::new(array))]
        } else {
            array.items
        };
        let mut iterated = false;
        for item in items {
            iterated = true;
            match targets {
                [single] => self.assign(single, item)?,
                several => {
                    let fields = record_of(&item)
                        .map(|record| record.fields.clone())
                        .unwrap_or_default();
                    self.assign_row(several, Some(fields), &[])?;
                }
            }
            if let Some(flow) = self.iteration(found)? {
                self.set_found_flag(true);
                return Ok(flow);
            }
        }
        self.set_found_flag(iterated);
        Ok(Flow::Next)
    }

    fn dynamic(&mut self, command: &Fragment, using: &[Fragment]) -> SqlResult<QueryResult> {
        let text = match self.value(command)? {
            Value::Null => {
                return Err(SqlError::new(
                    SqlState::NULL_VALUE_NOT_ALLOWED,
                    "query string argument of EXECUTE is null",
                ));
            }
            value => display_value(&value),
        };
        let mut frame = RoutineFrame::default();
        for (at, fragment) in using.iter().enumerate() {
            let value = self.value(fragment)?;
            frame
                .variables
                .push(FrameVariable::new(format!("${}", at + 1), Slot::Untyped, value));
            frame.positional.push(at);
        }
        let statement = parsed(&text, self.settings, &frame)?;
        self.execute(statement)
    }

    fn execute_dynamic(&mut self, dynamic: &DynamicSql) -> SqlResult<Flow> {
        let result = self.dynamic(&dynamic.command, &dynamic.using)?;
        self.row_count = processed_rows(&result);
        if let Some(into) = &dynamic.into {
            self.store_into(into, result)?;
        }
        Ok(Flow::Next)
    }

    fn source_rows(&mut self, source: &QuerySource) -> SqlResult<(Vec<String>, Vec<Vec<Value>>)> {
        let result = match source {
            QuerySource::Static(query) => self.query_text(&query.text)?,
            QuerySource::Dynamic { command, using } => self.dynamic(command, using)?,
        };
        Ok((column_names(&result), result.rows))
    }

    fn return_value(&mut self, value: Option<&Fragment>) -> SqlResult<Flow> {
        if let Some(fragment) = value {
            let value = self.value(fragment)?;
            let value = match &self.definition.returns {
                RoutineReturns::One(ReturnType::Type(sql_type)) => {
                    convert(value, *sql_type, Conversion::Assignment)?
                }
                RoutineReturns::One(ReturnType::Trigger)
                    if value != Value::Null && record_of(&value).is_none() =>
                {
                    return Err(SqlError::new(
                        SqlState::DATATYPE_MISMATCH,
                        "returned row structure does not match the structure of the triggering table",
                    ));
                }
                _ => value,
            };
            self.result = Some(value);
        }
        Ok(Flow::Return)
    }

    fn return_next(&mut self, value: Option<&Fragment>) -> SqlResult<Flow> {
        crate::cancel::check()?;
        let element = match value {
            Some(fragment) => {
                let value = self.value(fragment)?;
                self.element_value(value)?
            }
            None => self.outputs_value(),
        };
        self.rows.push(element);
        Ok(Flow::Next)
    }

    fn return_query(&mut self, source: &QuerySource) -> SqlResult<Flow> {
        let (names, rows) = self.source_rows(source)?;
        let count = rows.len() as u64;
        for row in rows {
            let element = self.row_element(row, &names)?;
            self.rows.push(element);
        }
        self.set_found(count);
        Ok(Flow::Next)
    }

    fn raise(&mut self, raise: &Raise) -> SqlResult<Flow> {
        let mut fields = RaisedFields::default();
        match &raise.condition {
            Some(RaiseCondition::Name { name, state }) => {
                fields.state = Some(*state);
                fields.message = Some(name.clone());
            }
            Some(RaiseCondition::State(state)) => fields.state = Some(*state),
            None => {}
        }
        if let Some(format) = &raise.format {
            let mut arguments = Vec::with_capacity(raise.arguments.len());
            for argument in &raise.arguments {
                arguments.push(self.value(argument)?);
            }
            fields.message = Some(format_message(format, &arguments));
        }
        for option in &raise.options {
            let text = match self.value(&option.value)? {
                Value::Null => {
                    return Err(SqlError::new(
                        SqlState::NULL_VALUE_NOT_ALLOWED,
                        "RAISE statement option cannot be null",
                    ));
                }
                value => display_value(&value),
            };
            match option.item {
                RaiseItem::Message => fields.message = Some(text),
                RaiseItem::Detail => fields.detail = Some(text),
                RaiseItem::Hint => fields.hint = Some(text),
                RaiseItem::Errcode => fields.state = Some(errcode(&text)?),
                RaiseItem::Column
                | RaiseItem::Constraint
                | RaiseItem::Datatype
                | RaiseItem::Table
                | RaiseItem::Schema => {}
            }
        }
        if raise.level == RaiseLevel::Exception {
            return Err(fields.into_error());
        }
        push_notice(raise.level, &fields.notice_message(), self.settings);
        Ok(Flow::Next)
    }

    fn assert(&mut self, condition: &Fragment, message: Option<&Fragment>) -> SqlResult<Flow> {
        let checked = !matches!(
            self.settings
                .get("plpgsql.check_asserts")
                .unwrap_or_default()
                .to_ascii_lowercase()
                .as_str(),
            "off" | "false" | "0" | "no"
        );
        if !checked || self.condition(condition)? {
            return Ok(Flow::Next);
        }
        let message = match message {
            Some(message) => match self.value(message)? {
                Value::Null => "assertion failed".to_owned(),
                value => display_value(&value),
            },
            None => "assertion failed".to_owned(),
        };
        Err(SqlError::new(SqlState::ASSERT_FAILURE, message))
    }

    fn diagnostics(
        &mut self,
        stacked: bool,
        items: &[(Target, DiagnosticsItem)],
    ) -> SqlResult<Flow> {
        let error = match (stacked, self.handling.last()) {
            (false, _) => None,
            (true, Some(error)) => Some(error.clone()),
            (true, None) => {
                return Err(SqlError::new(
                    SqlState::STACKED_DIAGNOSTICS_ACCESSED_WITHOUT_ACTIVE_HANDLER,
                    "GET STACKED DIAGNOSTICS cannot be used outside an exception handler",
                ));
            }
        };
        for (target, item) in items {
            let value = match (item, &error) {
                (DiagnosticsItem::RowCount, _) => {
                    Value::BigInt(i64::try_from(self.row_count).unwrap_or(i64::MAX))
                }
                (DiagnosticsItem::PgContext, _) => Value::Text(self.label.clone()),
                (DiagnosticsItem::PgRoutineOid, _) => Value::Oid(0),
                (item, Some(error)) => Value::Text(stacked_item(*item, error)),
                (_, None) => Value::Null,
            };
            self.assign(target, value)?;
        }
        Ok(Flow::Next)
    }

    fn cursor_variable(&self, variable: &str) -> SqlResult<usize> {
        let Some(index) = self.frame.index_of(variable) else {
            return Err(unknown_variable(variable));
        };
        match &self.frame.variables[index].slot {
            Slot::Cursor(_) => Ok(index),
            _ => Err(SqlError::new(
                SqlState::DATATYPE_MISMATCH,
                format!("variable \"{variable}\" must be of type cursor or refcursor"),
            )),
        }
    }

    fn open(
        &mut self,
        variable: &str,
        scroll: Option<bool>,
        source: &OpenSource,
    ) -> SqlResult<Flow> {
        let index = self.cursor_variable(variable)?;
        let declaration = match &self.frame.variables[index].slot {
            Slot::Cursor(declaration) => declaration.clone(),
            _ => None,
        };
        let portal = match &self.frame.variables[index].value {
            Value::Text(name) => name.clone(),
            _ if declaration.is_some() => variable.to_owned(),
            _ => {
                self.portals += 1;
                format!("<unnamed portal {}>", self.portals)
            }
        };
        if self.cursors.iter().any(|cursor| cursor.name == portal) {
            return Err(SqlError::new(
                SqlState::DUPLICATE_CURSOR,
                format!("cursor \"{portal}\" already in use"),
            ));
        }
        if self.cursors.len() >= MAX_OPEN_CURSORS {
            return Err(SqlError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!("a PL/pgSQL function keeps at most {MAX_OPEN_CURSORS} cursors open"),
            ));
        }
        let (names, rows) = match source {
            OpenSource::Query(source) => self.source_rows(source)?,
            OpenSource::Bound(values) => {
                let Some(declaration) = &declaration else {
                    return Err(SqlError::new(
                        SqlState::SYNTAX_ERROR,
                        format!("cursor \"{variable}\" has no query; open it with OPEN {variable} FOR query"),
                    ));
                };
                self.bound_rows(variable, declaration, values)?
            }
        };
        let scroll = scroll.or_else(|| declaration.as_ref().and_then(|declared| declared.scroll));
        self.cursors.push(OpenCursor {
            name: portal.clone(),
            names,
            rows,
            position: 0,
            scroll,
        });
        self.frame.variables[index].value = Value::Text(portal);
        Ok(Flow::Next)
    }

    fn bound_rows(
        &mut self,
        variable: &str,
        declaration: &CursorDeclaration,
        values: &[CursorValue],
    ) -> SqlResult<(Vec<String>, Vec<Vec<Value>>)> {
        if values.len() != declaration.arguments.len() {
            return Err(SqlError::new(
                SqlState::SYNTAX_ERROR,
                format!("wrong number of arguments for cursor \"{variable}\""),
            ));
        }
        let mut assigned: Vec<Option<Value>> = vec![None; values.len()];
        for (at, value) in values.iter().enumerate() {
            let slot = match &value.name {
                Some(name) => declaration
                    .arguments
                    .iter()
                    .position(|argument| &argument.name == name)
                    .ok_or_else(|| {
                        SqlError::new(
                            SqlState::SYNTAX_ERROR,
                            format!("cursor \"{variable}\" has no argument named \"{name}\""),
                        )
                    })?,
                None => at,
            };
            assigned[slot] = Some(self.value(&value.value)?);
        }
        let mut arguments = Vec::with_capacity(assigned.len());
        for (argument, value) in declaration.arguments.iter().zip(assigned) {
            let slot = self.slot_of(&argument.variable_type)?;
            let value = coerce(&slot, value.unwrap_or(Value::Null))?;
            arguments.push(FrameVariable::new(argument.name.clone(), slot, value));
        }
        let scope = self.frame.variables.len();
        self.frame.variables.extend(arguments);
        let outcome = self.query_text(&declaration.query.text);
        self.frame.variables.truncate(scope);
        let result = outcome?;
        Ok((column_names(&result), result.rows))
    }

    fn open_cursor(&self, variable: &str) -> SqlResult<usize> {
        let index = self.cursor_variable(variable)?;
        let Value::Text(portal) = &self.frame.variables[index].value else {
            return Err(SqlError::new(
                SqlState::INVALID_CURSOR_NAME,
                format!("cursor variable \"{variable}\" is null"),
            ));
        };
        self.cursors
            .iter()
            .position(|cursor| &cursor.name == portal)
            .ok_or_else(|| {
                SqlError::new(
                    SqlState::INVALID_CURSOR_NAME,
                    format!("cursor \"{portal}\" does not exist"),
                )
            })
    }

    fn cursor_target(&mut self, at: usize, direction: &FetchDirection) -> SqlResult<usize> {
        let position = self.cursors[at].position as i64;
        let length = self.cursors[at].rows.len() as i64;
        let count = "FETCH or MOVE count cannot be null";
        let target = match direction {
            FetchDirection::Next | FetchDirection::Forward(None) => position + 1,
            FetchDirection::Prior | FetchDirection::Backward(None) => position - 1,
            FetchDirection::First => 1,
            FetchDirection::Last => length,
            FetchDirection::Absolute(fragment) => {
                let number = self.integer(fragment, count)?;
                if number >= 0 { number } else { length + 1 + number }
            }
            FetchDirection::Relative(fragment) | FetchDirection::Forward(Some(fragment)) => {
                position + self.integer(fragment, count)?
            }
            FetchDirection::Backward(Some(fragment)) => position - self.integer(fragment, count)?,
            FetchDirection::ForwardAll => length + 1,
            FetchDirection::BackwardAll => 0,
        };
        if target < position && self.cursors[at].scroll == Some(false) {
            return Err(SqlError::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "cursor can only scan forward",
            )
            .with_hint("Declare it with SCROLL option to enable backward scan."));
        }
        Ok(usize::try_from(target.clamp(0, length + 1)).unwrap_or(0))
    }

    fn fetch(
        &mut self,
        variable: &str,
        direction: &FetchDirection,
        into: &IntoTargets,
    ) -> SqlResult<Flow> {
        let at = self.open_cursor(variable)?;
        let target = self.cursor_target(at, direction)?;
        let cursor = &mut self.cursors[at];
        cursor.position = target;
        let row = target
            .checked_sub(1)
            .and_then(|offset| cursor.rows.get(offset))
            .cloned();
        let names = cursor.names.clone();
        let found = row.is_some();
        self.assign_row(&into.targets, row, &names)?;
        self.set_found(u64::from(found));
        Ok(Flow::Next)
    }

    fn move_cursor(&mut self, variable: &str, direction: &FetchDirection) -> SqlResult<Flow> {
        let at = self.open_cursor(variable)?;
        let target = self.cursor_target(at, direction)?;
        let cursor = &mut self.cursors[at];
        cursor.position = target;
        let found = (1..=cursor.rows.len()).contains(&target);
        self.set_found(u64::from(found));
        Ok(Flow::Next)
    }

    fn sql_located(&self, error: SqlError, statement: usize) -> SqlError {
        with_context_line(
            error.without_position(),
            &format!("{} statement {statement}", self.label),
        )
    }

    fn sql_body(&mut self) -> SqlResult<Returned> {
        let definition = self.definition;
        let text = definition.body.as_str();
        let tokens = tokenize_in(text, Dialect::PostgreSql).map_err(|error| self.sql_located(error, 1))?;
        let mut resume = (0, 0);
        let mut last = None;
        let mut number = 0;
        loop {
            number += 1;
            let statement = {
                let mut parser = Parser::new(text, &tokens)
                    .with_context(Parameters::Absent, self.settings)
                    .in_dialect(Dialect::PostgreSql)
                    .with_routine_frame(&self.frame)
                    .resuming_at(resume);
                parser.skip_empty_statements();
                if parser.at_end() {
                    break;
                }
                let statement = parser.statement();
                resume = parser.resume_point();
                statement
            };
            let statement = statement.map_err(|error| self.sql_located(error, number))?;
            let result = self
                .execute(statement)
                .map_err(|error| self.sql_located(error, number))?;
            last = Some(result);
        }
        self.sql_result(last)
    }

    fn sql_result(&mut self, last: Option<QueryResult>) -> SqlResult<Returned> {
        let definition = self.definition;
        if definition.kind == RoutineKind::Procedure {
            if self.output_columns.is_empty() {
                return Ok(Returned::Value(Value::Null));
            }
            let row = last.and_then(|result| result.rows.into_iter().next());
            let names = self
                .output_columns
                .iter()
                .map(|(name, _)| name.clone())
                .collect();
            return Ok(Returned::Value(match row {
                Some(row) => records::record_value(RECORD_OID, row, names),
                None => Value::Null,
            }));
        }
        let set = definition.returns.is_set();
        let Some(result) = last else {
            return Ok(if set {
                Returned::Set(Vec::new())
            } else {
                Returned::Value(Value::Null)
            });
        };
        let names = column_names(&result);
        if set {
            let mut rows = Vec::with_capacity(result.rows.len());
            for row in result.rows {
                rows.push(self.row_element(row, &names)?);
            }
            return Ok(Returned::Set(rows));
        }
        if definition.returns == RoutineReturns::One(ReturnType::Void) {
            return Ok(Returned::Value(Value::Null));
        }
        match result.rows.into_iter().next() {
            Some(row) => Ok(Returned::Value(self.row_element(row, &names)?)),
            None => Ok(Returned::Value(Value::Null)),
        }
    }
}

fn stacked_item(item: DiagnosticsItem, error: &SqlError) -> String {
    match item {
        DiagnosticsItem::ReturnedSqlstate => error.state().code().to_owned(),
        DiagnosticsItem::MessageText => error.message().to_owned(),
        DiagnosticsItem::PgExceptionDetail => error.detail().unwrap_or_default().to_owned(),
        DiagnosticsItem::PgExceptionHint => error.hint().unwrap_or_default().to_owned(),
        DiagnosticsItem::PgExceptionContext => error.context().unwrap_or_default().to_owned(),
        DiagnosticsItem::TableName => error.table().map(base_name).unwrap_or_default().to_owned(),
        DiagnosticsItem::SchemaName => error
            .table()
            .map(crate::schemas::schema_of)
            .unwrap_or_default()
            .to_owned(),
        DiagnosticsItem::RowCount
        | DiagnosticsItem::PgContext
        | DiagnosticsItem::PgRoutineOid
        | DiagnosticsItem::ColumnName
        | DiagnosticsItem::ConstraintName
        | DiagnosticsItem::PgDatatypeName => String::new(),
    }
}
