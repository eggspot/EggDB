mod packages;

use packages::AttachedPackage;

use super::super::frame::{FrameVariable, Slot, row_record};
use super::super::raise::{display_value, with_context_line};
use super::super::collections::CollectionShape;
use super::super::{
    Block, ExceptionCondition, Fragment, Handler, Program, RaisedException, Subprogram,
    TypeDeclaration, VariableType, line_of,
};
use super::{Flow, Run, coerce};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::plsql::{
    ANONYMOUS_BLOCK_NAME, DeclaredType, NORMAL_COMPLETION, NO_DATA_FOUND_CODE, USER_EXCEPTION_CODE,
    USER_EXCEPTION_MESSAGE,
    application_error_code, predefined_by_code, shown_object, sqlcode_of,
};
use crate::routines::{NamedRoutines, RoutineDefinition, RoutineKind};
use crate::types::{SqlType, Value};
use std::cell::{Cell, RefCell};
use std::sync::Arc;

const UNHANDLED_USER_EXCEPTION: &str = "PL/SQL: unhandled user-defined exception";
const UNMAPPED_ERROR_CODE: u16 = 600;
const TOO_MANY_ROWS_CODE: u16 = 1422;
const CASE_NOT_FOUND_CODE: u16 = 6592;
const INVALID_CURSOR_CODE: u16 = 1001;
const CURSOR_ALREADY_OPEN_CODE: u16 = 6511;
const VALUE_ERROR_CODE: u16 = 6502;
const APPLICATION_ERROR_RANGE_CODE: u16 = 21000;
const TRANSACTION_IN_QUERY_CODE: u16 = 14552;
const MAX_APPLICATION_MESSAGE: usize = 2048;
const MAX_LOCAL_ROUTINES: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransactionEnd {
    Commit,
    Rollback,
}

#[derive(Debug)]
pub(super) struct UnitLines {
    pub(super) label: String,
    pub(super) text: Arc<str>,
    pub(super) base: usize,
}

impl UnitLines {
    pub(super) fn of_routine(definition: &RoutineDefinition) -> Arc<Self> {
        Arc::new(Self {
            label: format!("\"{}\"", shown_object(&definition.name)),
            text: Arc::from(definition.body.as_str()),
            base: 0,
        })
    }

    pub(super) fn of_package(name: &str, text: &Arc<str>) -> Arc<Self> {
        Arc::new(Self {
            label: format!("\"{}\"", shown_object(name)),
            text: Arc::clone(text),
            base: 0,
        })
    }

    pub(super) fn of_anonymous_block(code: &str, base: usize) -> Arc<Self> {
        Arc::new(Self {
            label: ANONYMOUS_BLOCK_NAME.to_owned(),
            text: Arc::from(code),
            base,
        })
    }

    fn context_line(&self, position: Option<usize>) -> String {
        let anonymous = self.label == ANONYMOUS_BLOCK_NAME;
        match position {
            Some(position) => {
                let line = line_of(&self.text, position.saturating_sub(self.base));
                if anonymous {
                    format!("ORA-06512: at line {line}")
                } else {
                    format!("ORA-06512: at {}, line {line}", self.label)
                }
            }
            None if anonymous => "ORA-06512: at anonymous block".to_owned(),
            None => format!("ORA-06512: at {}", self.label),
        }
    }
}

struct LocalRoutine {
    name: String,
    definition: Arc<RoutineDefinition>,
    program: Arc<Program>,
    frame: usize,
    visible: usize,
    unit: Arc<UnitLines>,
    types: Vec<(String, TypeDeclaration)>,
    constructors: Vec<(String, CollectionShape)>,
    packages: Vec<AttachedPackage>,
}

enum Entry {
    Boundary,
    Routine(LocalRoutine),
}

#[derive(Default)]
struct Locals {
    entries: Vec<Entry>,
    frames: Vec<Vec<FrameVariable>>,
}

thread_local! {
    static LOCALS: RefCell<Locals> = RefCell::new(Locals::default());
    static TRANSACTION_END: Cell<Option<TransactionEnd>> = const { Cell::new(None) };
}

#[derive(Debug, Default)]
pub(super) struct PlsqlState {
    types: Vec<(String, TypeDeclaration)>,
    shared_frame: Option<usize>,
    mirror: Option<(usize, usize)>,
    entries_mark: usize,
    frames_mark: usize,
    pub(super) guards: usize,
    packages: Vec<AttachedPackage>,
    package_scope: bool,
}

pub(crate) fn local_routines(name: &str) -> Option<Arc<NamedRoutines>> {
    if crate::schemas::is_qualified(name) {
        return None;
    }
    LOCALS.with(|locals| {
        let locals = locals.try_borrow().ok()?;
        let mut found: Vec<Arc<RoutineDefinition>> = Vec::new();
        for entry in locals.entries.iter().rev() {
            let routine = match entry {
                Entry::Boundary => break,
                Entry::Routine(routine) => routine,
            };
            if routine.name != name {
                continue;
            }
            let identity = routine.definition.identity();
            if found.iter().all(|known| known.identity() != identity) {
                found.push(Arc::clone(&routine.definition));
            }
        }
        (!found.is_empty()).then(|| {
            Arc::new(NamedRoutines {
                name: name.to_owned(),
                routines: found,
            })
        })
    })
}

pub(crate) fn local_program(definition: &Arc<RoutineDefinition>) -> Option<Arc<Program>> {
    LOCALS.with(|locals| {
        let locals = locals.try_borrow().ok()?;
        locals.entries.iter().rev().find_map(|entry| match entry {
            Entry::Routine(routine) if Arc::ptr_eq(&routine.definition, definition) => {
                Some(Arc::clone(&routine.program))
            }
            _ => None,
        })
    })
}

pub(crate) fn take_transaction_end() -> Option<TransactionEnd> {
    TRANSACTION_END.with(Cell::take)
}

fn oracle_user(run: &Run<'_>) -> String {
    run.settings
        .get("session_authorization")
        .unwrap_or_default()
        .to_uppercase()
}

fn plsql_code(error: &SqlError) -> Option<(u16, &'static str)> {
    let message = error.message();
    Some(match error.state() {
        SqlState::NO_DATA_FOUND => (NO_DATA_FOUND_CODE, "no data found"),
        SqlState::TOO_MANY_ROWS => (
            TOO_MANY_ROWS_CODE,
            "exact fetch returns more than requested number of rows",
        ),
        SqlState::CASE_NOT_FOUND => (
            CASE_NOT_FOUND_CODE,
            "Case not found while executing CASE statement",
        ),
        SqlState::INVALID_CURSOR_NAME => (INVALID_CURSOR_CODE, "invalid cursor"),
        SqlState::DUPLICATE_CURSOR if message.contains("already in use") => {
            (CURSOR_ALREADY_OPEN_CODE, "PL/SQL: cursor already open")
        }
        SqlState::NULL_VALUE_NOT_ALLOWED if message.contains("declared NOT NULL") => (
            VALUE_ERROR_CODE,
            "PL/SQL: numeric or value error",
        ),
        _ => return None,
    })
}

fn oracle_error(error: SqlError, text: &str, user: &str) -> SqlError {
    if error.vendor_code().is_some() {
        return error;
    }
    if let Some((code, message)) = plsql_code(&error) {
        let context = error.context().map(str::to_owned);
        let translated = SqlError::new(error.state(), message).with_oracle_code(code);
        return match context {
            Some(context) => translated.with_context(context),
            None => translated,
        };
    }
    let context = error.context().map(str::to_owned);
    let translated = crate::oracle_errors::translate(error, text, user);
    match context {
        Some(context) if translated.context().is_none() => translated.with_context(context),
        _ => translated,
    }
}

fn assignment_error(error: SqlError) -> SqlError {
    let message = match error.state() {
        SqlState::STRING_DATA_RIGHT_TRUNCATION => {
            "PL/SQL: numeric or value error: character string buffer too small"
        }
        SqlState::INVALID_TEXT_REPRESENTATION => {
            "PL/SQL: numeric or value error: character to number conversion error"
        }
        SqlState::NUMERIC_VALUE_OUT_OF_RANGE => {
            "PL/SQL: numeric or value error: number precision too large"
        }
        _ => return error,
    };
    SqlError::new(error.state(), message).with_oracle_code(VALUE_ERROR_CODE)
}

fn sqlcode_and_message(error: &SqlError) -> (i32, String) {
    if error.declared_exception().is_some() {
        return (sqlcode_of(USER_EXCEPTION_CODE), USER_EXCEPTION_MESSAGE.to_owned());
    }
    match (error.vendor(), error.vendor_code()) {
        (Some(crate::Dialect::Oracle), Some(code)) => (sqlcode_of(code), error.to_string()),
        _ => (
            sqlcode_of(UNMAPPED_ERROR_CODE),
            format!("ORA-{UNMAPPED_ERROR_CODE:05}: {}", error.message()),
        ),
    }
}

fn raised_oracle(code: u16) -> SqlError {
    let state = match code {
        NO_DATA_FOUND_CODE => SqlState::NO_DATA_FOUND,
        TOO_MANY_ROWS_CODE => SqlState::TOO_MANY_ROWS,
        CASE_NOT_FOUND_CODE => SqlState::CASE_NOT_FOUND,
        _ => SqlState::RAISE_EXCEPTION,
    };
    let message = predefined_by_code(code).map_or("", |predefined| predefined.message);
    SqlError::new(state, message).with_oracle_code(code)
}

fn too_many_local_routines() -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        "too many nested PL/SQL subprograms are declared at once",
    )
    .with_hint(format!(
        "EggDB keeps at most {MAX_LOCAL_ROUTINES} local subprograms in scope."
    ))
}

impl Run<'_> {
    pub(super) fn begin_plsql(&mut self) {
        LOCALS.with(|locals| {
            let mut locals = locals.borrow_mut();
            self.plsql.frames_mark = locals.frames.len();
            if self.plsql.mirror.is_none() {
                locals.entries.push(Entry::Boundary);
                self.plsql.entries_mark = locals.entries.len() - 1;
            } else {
                self.plsql.entries_mark = locals.entries.len();
            }
        });
        if self.plsql.mirror.is_none() {
            let package = self
                .definition
                .plsql
                .as_ref()
                .and_then(|plsql| plsql.package.clone());
            crate::plsql::enter_package_scope(package);
            self.plsql.package_scope = true;
        }
    }

    pub(super) fn end_plsql(&mut self) {
        LOCALS.with(|locals| {
            let mut locals = locals.borrow_mut();
            locals.entries.truncate(self.plsql.entries_mark);
            locals.frames.truncate(self.plsql.frames_mark);
        });
        self.plsql.shared_frame = None;
        if std::mem::take(&mut self.plsql.package_scope) {
            crate::plsql::leave_package_scope();
        }
    }

    pub(super) fn attach_local(&mut self, definition: &Arc<RoutineDefinition>) {
        let attached = LOCALS.with(|locals| {
            let locals = locals.borrow();
            locals.entries.iter().rev().find_map(|entry| match entry {
                Entry::Routine(routine) if Arc::ptr_eq(&routine.definition, definition) => {
                    let variables = locals
                        .frames
                        .get(routine.frame)
                        .map(|frame| frame[..routine.visible.min(frame.len())].to_vec())
                        .unwrap_or_default();
                    Some((
                        routine.frame,
                        variables,
                        Arc::clone(&routine.unit),
                        routine.types.clone(),
                        routine.constructors.clone(),
                        routine.packages.clone(),
                    ))
                }
                _ => None,
            })
        });
        if let Some((frame, variables, unit, types, constructors, packages)) = attached {
            self.plsql.mirror = Some((frame, variables.len()));
            self.frame.variables = variables;
            self.unit = Some(unit);
            self.plsql.types = types;
            if !constructors.is_empty() {
                self.plsql_frame_mut().constructors = constructors;
            }
            self.plsql.packages = packages;
            self.adopt_packages();
        }
    }

    pub(super) fn share_frame(&mut self) {
        self.share_locals();
        self.share_packages();
    }

    fn share_locals(&self) {
        if self.plsql.shared_frame.is_none() && self.plsql.mirror.is_none() {
            return;
        }
        let packaged = self.package_ranges();
        LOCALS.with(|locals| {
            let mut locals = locals.borrow_mut();
            if let Some((frame, count)) = self.plsql.mirror
                && let Some(shared) = locals.frames.get_mut(frame)
            {
                for (at, variable) in self.frame.variables.iter().take(count).enumerate() {
                    if packaged.iter().any(|range| range.contains(&at)) {
                        continue;
                    }
                    if let Some(slot) = shared.get_mut(at) {
                        slot.value = variable.value.clone();
                    }
                }
            }
            if let Some(frame) = self.plsql.shared_frame
                && let Some(shared) = locals.frames.get_mut(frame)
            {
                shared.clone_from(&self.frame.variables);
            }
        });
    }

    pub(super) fn adopt_frame(&mut self) {
        self.adopt_packages();
        self.adopt_locals();
    }

    fn adopt_locals(&mut self) {
        if self.plsql.shared_frame.is_none() && self.plsql.mirror.is_none() {
            return;
        }
        let packaged = self.package_ranges();
        LOCALS.with(|locals| {
            let locals = locals.borrow();
            let source = self
                .plsql
                .shared_frame
                .map(|frame| (frame, usize::MAX))
                .or(self.plsql.mirror);
            let Some((frame, count)) = source else {
                return;
            };
            let Some(shared) = locals.frames.get(frame) else {
                return;
            };
            for (at, (variable, adopted)) in self
                .frame
                .variables
                .iter_mut()
                .zip(shared)
                .take(count)
                .enumerate()
            {
                if packaged.iter().any(|range| range.contains(&at)) {
                    continue;
                }
                if variable.name == adopted.name {
                    variable.value = adopted.value.clone();
                }
            }
        });
    }

    pub(super) fn declare_subprogram(&mut self, name: &str, subprogram: &Subprogram) -> SqlResult<()> {
        let Some(program) = &subprogram.program else {
            return Ok(());
        };
        let unit = self
            .unit
            .clone()
            .unwrap_or_else(|| UnitLines::of_routine(self.definition));
        let mut definition = subprogram.definition.clone();
        definition.name = name.to_owned();
        let visible = self.frame.variables.len();
        let types = self.plsql.types.clone();
        let constructors = self
            .frame
            .plsql
            .as_ref()
            .map(|frame| frame.constructors.clone())
            .unwrap_or_default();
        let packages = self.inherited_packages();
        let frame = LOCALS.with(|locals| {
            let mut locals = locals.borrow_mut();
            if locals.entries.len() >= MAX_LOCAL_ROUTINES {
                return Err(too_many_local_routines());
            }
            let frame = match self.plsql.shared_frame {
                Some(frame) => frame,
                None => {
                    locals.frames.push(self.frame.variables.clone());
                    locals.frames.len() - 1
                }
            };
            locals.entries.push(Entry::Routine(LocalRoutine {
                name: name.to_owned(),
                definition: Arc::new(definition),
                program: Arc::new(program.clone()),
                frame,
                visible,
                unit,
                types,
                constructors,
                packages,
            }));
            Ok(frame)
        })?;
        self.plsql.shared_frame = Some(frame);
        Ok(())
    }

    pub(super) fn local_scope(&self) -> (usize, usize, usize) {
        let entries = LOCALS.with(|locals| locals.borrow().entries.len());
        let constructors = self
            .frame
            .plsql
            .as_ref()
            .map_or(0, |frame| frame.constructors.len());
        (self.plsql.types.len(), entries, constructors)
    }

    pub(super) fn leave_local_scope(&mut self, (types, entries, constructors): (usize, usize, usize)) {
        self.plsql.types.truncate(types);
        if let Some(frame) = self.frame.plsql.as_mut() {
            frame.constructors.truncate(constructors);
        }
        LOCALS.with(|locals| {
            let mut locals = locals.borrow_mut();
            if locals.entries.len() > entries {
                locals.entries.truncate(entries);
            }
        });
    }

    pub(super) fn declare_type(&mut self, name: &str, declared: &TypeDeclaration) -> SqlResult<()> {
        self.plsql.types.push((name.to_owned(), declared.clone()));
        if let Some(shape) = self.collection_shape(declared)? {
            self.plsql_frame_mut()
                .constructors
                .push((name.to_owned(), *shape));
        }
        Ok(())
    }

    fn declared_type_named(&self, parts: &[String]) -> Option<TypeDeclaration> {
        let named = |known: &str| match parts {
            [name] => known == name,
            [package, name] => known
                .strip_prefix(package.as_str())
                .and_then(|rest| rest.strip_prefix('.'))
                == Some(name.as_str()),
            _ => false,
        };
        self.plsql
            .types
            .iter()
            .rev()
            .find(|(known, _)| named(known))
            .map(|(_, declared)| declared.clone())
    }

    fn engine_type_of(&mut self, variable_type: &VariableType) -> SqlResult<SqlType> {
        Ok(match self.slot_of(variable_type)? {
            Slot::Typed(sql_type) => sql_type,
            Slot::Cursor(_) => SqlType::Text,
            Slot::Untyped | Slot::Record | Slot::Row(_) | Slot::Collection(_) => SqlType::Dynamic,
        })
    }

    pub(super) fn named_slot(&mut self, parts: &[String]) -> SqlResult<Slot> {
        let Some(declared) = self.declared_type_named(parts) else {
            return Err(SqlError::not_supported(format!(
                "the PL/SQL type {} declared outside this unit",
                parts.join(".")
            )));
        };
        match declared {
            TypeDeclaration::Record(fields) => {
                let mut columns = Vec::with_capacity(fields.len());
                for field in &fields {
                    columns.push((field.name.clone(), self.engine_type_of(&field.variable_type)?));
                }
                Ok(Slot::Row(columns))
            }
            TypeDeclaration::Subtype { base, .. } => self.slot_of(&base),
            TypeDeclaration::RefCursor => Ok(Slot::Cursor(None)),
            TypeDeclaration::Table { .. } | TypeDeclaration::Varray { .. } => {
                match self.collection_shape(&declared)? {
                    Some(shape) => Ok(Slot::Collection(shape)),
                    None => Ok(Slot::Untyped),
                }
            }
        }
    }

    pub(super) fn record_defaults(&mut self, parts: &[String]) -> SqlResult<Option<Value>> {
        let Some(TypeDeclaration::Record(fields)) = self.declared_type_named(parts) else {
            return Ok(None);
        };
        if fields.iter().all(|field| field.default.is_none()) {
            return Ok(None);
        }
        let mut columns = Vec::with_capacity(fields.len());
        let mut values = Vec::with_capacity(fields.len());
        for field in &fields {
            let slot = self.slot_of(&field.variable_type)?;
            let value = match &field.default {
                Some(default) => {
                    let value = self.value(default)?;
                    coerce(&slot, value)?
                }
                None => Value::Null,
            };
            columns.push((field.name.clone(), self.engine_type_of(&field.variable_type)?));
            values.push(value);
        }
        Ok(Some(row_record(&columns, values)))
    }

    pub(super) fn declared_slot(&mut self, declared: &DeclaredType) -> SqlResult<Option<Slot>> {
        Ok(Some(match declared {
            DeclaredType::Sql { .. } => return Ok(None),
            DeclaredType::ColumnType(parts) => self.copied_slot(parts)?,
            DeclaredType::RowType(parts) => match parts.as_slice() {
                [table] => Slot::Row(self.relation_columns(table)?),
                [schema, table] => {
                    Slot::Row(self.relation_columns(&crate::schemas::qualified(schema, table))?)
                }
                _ => Slot::Record,
            },
            DeclaredType::Named(_) if crate::plsql::is_ref_cursor(declared) => Slot::Cursor(None),
            DeclaredType::Named(parts) => match self.named_slot(parts) {
                Ok(slot) => slot,
                Err(_) => Slot::Untyped,
            },
        }))
    }

    pub(super) fn bind_declared_parameters(&mut self) -> SqlResult<()> {
        let definition = self.definition;
        let Some(plsql) = definition.plsql.as_deref() else {
            return Ok(());
        };
        for (at, declared) in plsql.declared.iter().enumerate() {
            if !declared.needs_resolution() {
                continue;
            }
            let Some(index) = self.frame.positional.get(at).copied() else {
                continue;
            };
            let Some(slot) = self.declared_slot(declared)? else {
                continue;
            };
            let value = std::mem::replace(&mut self.frame.variables[index].value, Value::Null);
            let value = coerce(&slot, value).map_err(assignment_error)?;
            let variable = &mut self.frame.variables[index];
            variable.slot = slot;
            variable.value = value;
        }
        Ok(())
    }

    pub(super) fn oracle_return(&mut self, value: Value) -> SqlResult<Value> {
        let declared = self
            .definition
            .plsql
            .as_deref()
            .and_then(|plsql| plsql.returns.clone());
        let Some(declared) = declared else {
            return Ok(value);
        };
        match self.declared_slot(&declared)? {
            Some(slot) => coerce(&slot, value).map_err(assignment_error),
            None => crate::types::convert(
                value,
                declared.engine_type(),
                crate::types::Conversion::Assignment,
            )
            .map_err(assignment_error),
        }
    }

    pub(super) fn assignment_error(&self, error: SqlError) -> SqlError {
        if self.oracle { assignment_error(error) } else { error }
    }

    pub(super) fn declare_oracle_builtins(&mut self) {
        let label = Some(crate::schemas::base_name(&self.definition.name).to_owned());
        let builtins = [
            ("sqlcode", Slot::Typed(SqlType::Integer), Value::Integer(0)),
            (
                "sqlerrm",
                Slot::Typed(SqlType::Text),
                Value::Text(NORMAL_COMPLETION.to_owned()),
            ),
        ];
        for (name, slot, value) in builtins {
            let mut variable = FrameVariable::new(name, slot, value);
            variable.label = label.clone();
            variable.constant = true;
            self.frame.variables.push(variable);
        }
    }

    pub(super) fn translated(&self, error: SqlError) -> SqlError {
        oracle_error(error, "", &oracle_user(self))
    }

    pub(super) fn statement_mark(&mut self) -> Option<usize> {
        (self.oracle && self.plsql.guards > 0).then(|| self.undo.begin())
    }

    pub(super) fn end_statement(&mut self, mark: Option<usize>, succeeded: bool) -> SqlResult<()> {
        match mark {
            None => Ok(()),
            Some(_) if succeeded => {
                self.undo.release();
                Ok(())
            }
            Some(mark) => self.undo.roll_back(&mut *self.writer, mark),
        }
    }

    pub(super) fn guarded_oracle(&mut self, block: &Block) -> SqlResult<Flow> {
        let cursors = self.cursors.len();
        self.plsql.guards += 1;
        let outcome = self.steps(&block.steps);
        self.plsql.guards -= 1;
        let error = match outcome {
            Ok(flow) => return Ok(flow),
            Err(error) => self.translated(error),
        };
        let Some(handler) = block
            .handlers
            .iter()
            .find(|handler| self.catches(handler, &error))
        else {
            return Err(error);
        };
        self.close_cursors_from(cursors);
        self.failure = None;
        self.handle(handler, error)
    }

    fn catches(&self, handler: &Handler, error: &SqlError) -> bool {
        if error.state() == SqlState::QUERY_CANCELED {
            return false;
        }
        let code = match error.vendor() {
            Some(crate::Dialect::Oracle) => error.vendor_code(),
            _ => None,
        };
        handler.catches_oracle(code, error.declared_exception())
            || (code.is_some()
                && handler.conditions.iter().any(|condition| {
                    matches!(condition, ExceptionCondition::Declared(identity)
                        if self.package_exception_code(identity) == code)
                }))
    }

    pub(super) fn push_error_variables(&mut self, error: &SqlError) {
        let (code, message) = sqlcode_and_message(error);
        for (name, slot, value) in [
            ("sqlcode", Slot::Typed(SqlType::Integer), Value::Integer(code)),
            ("sqlerrm", Slot::Typed(SqlType::Text), Value::Text(message)),
        ] {
            let mut variable = FrameVariable::new(name, slot, value);
            variable.constant = true;
            self.frame.variables.push(variable);
        }
    }

    pub(super) fn located_oracle(&mut self, error: SqlError) -> SqlError {
        let error = self.translated(error).without_position();
        let unit = self
            .unit
            .clone()
            .unwrap_or_else(|| UnitLines::of_routine(self.definition));
        let position = self.failure.take().map(|(position, _)| position);
        with_context_line(error, &unit.context_line(position))
    }

    pub(super) fn raise_exception(&mut self, raised: &RaisedException) -> SqlResult<Flow> {
        Err(match raised {
            RaisedException::Oracle(code) => raised_oracle(*code),
            RaisedException::Declared(identity) => match self.package_exception_code(identity) {
                Some(code) => raised_oracle(code),
                None => SqlError::new(SqlState::RAISE_EXCEPTION, UNHANDLED_USER_EXCEPTION)
                    .with_oracle_code(USER_EXCEPTION_CODE)
                    .with_declared_exception(identity.clone()),
            },
        })
    }

    pub(super) fn application_error(
        &mut self,
        number: &Fragment,
        message: &Fragment,
        keep: Option<&Fragment>,
    ) -> SqlResult<Flow> {
        let number = self.integer(number, "error number argument to raise_application_error is null")?;
        let Some(code) = application_error_code(number) else {
            return Err(SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("error number argument to raise_application_error of {number} is out of range"),
            )
            .with_oracle_code(APPLICATION_ERROR_RANGE_CODE));
        };
        let mut text = match self.value(message)? {
            Value::Null => String::new(),
            value => display_value(&value),
        };
        if text.len() > MAX_APPLICATION_MESSAGE {
            let mut end = MAX_APPLICATION_MESSAGE;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        let keep = match keep {
            Some(keep) => self.condition(keep)?,
            None => false,
        };
        let error = SqlError::new(SqlState::RAISE_EXCEPTION, text).with_oracle_code(code);
        Err(match (keep, self.handling.last()) {
            (true, Some(previous)) => error.with_context(previous.to_string()),
            _ => error,
        })
    }

    pub(super) fn transaction_end(&self, end: TransactionEnd) -> SqlResult<Flow> {
        if self.definition.kind == RoutineKind::Function {
            return Err(SqlError::new(
                SqlState::INVALID_TRANSACTION_TERMINATION,
                "cannot perform a DDL, commit or rollback inside a query or DML",
            )
            .with_oracle_code(TRANSACTION_IN_QUERY_CODE));
        }
        TRANSACTION_END.with(|pending| pending.set(Some(end)));
        Ok(Flow::Next)
    }
}
