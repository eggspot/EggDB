use super::{TriggerAction, TriggerDefinition, TriggerTiming, errors};
use crate::ast::{Expr, ExprKind, Select, SelectItem, Statement};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{self, TableCache};
use crate::lexer::{Token, tokenize_in};
use crate::name_resolution::Binding;
use crate::parser::{Parameters, Parser};
use crate::session::OutputColumn;
use crate::settings::Settings;
use crate::store::KeyValueWrite;
use crate::types::{Conversion, SqlType, Value, convert};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BodyStep {
    Statement(usize),
    Declare {
        names: Vec<String>,
        sql_type: SqlType,
        default: Option<usize>,
    },
    Assign(Vec<(StepTarget, usize)>),
    If {
        branches: Vec<ConditionalBranch>,
        otherwise: Vec<BodyStep>,
    },
    While {
        condition: usize,
        body: Vec<BodyStep>,
    },
    Signal(SignalStep),
    Block(Vec<BodyStep>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ConditionalBranch {
    pub(crate) condition: usize,
    pub(crate) steps: Vec<BodyStep>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StepTarget {
    New(String),
    Old(String),
    Local(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SignalStep {
    pub(crate) state: String,
    pub(crate) message: Option<usize>,
    pub(crate) code: Option<u16>,
}

#[derive(Debug, Clone, PartialEq)]
struct LocalVariable {
    name: String,
    sql_type: SqlType,
    value: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TriggerFrame {
    columns: Arc<Vec<OutputColumn>>,
    old: Option<Vec<Value>>,
    new: Option<Vec<Value>>,
    locals: Vec<LocalVariable>,
    dialect: Dialect,
}

fn constant(value: Value, sql_type: Option<SqlType>, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Constant {
            sql_type: sql_type.or_else(|| value.sql_type()),
            value,
        },
        position,
    }
}

impl TriggerFrame {
    fn event(&self) -> &'static str {
        match (&self.old, &self.new) {
            (None, _) => "INSERT",
            (Some(_), Some(_)) => "UPDATE",
            (Some(_), None) => "DELETE",
        }
    }

    pub(crate) fn reference(&self, qualifier: &str, column: &str, position: usize) -> SqlResult<Expr> {
        let row = if qualifier == "new" { &self.new } else { &self.old };
        let Some(row) = row else {
            return Err(errors::missing_row(qualifier, column, self.dialect, self.event()).at(position));
        };
        let Some(index) = self
            .columns
            .iter()
            .position(|known| known.name.eq_ignore_ascii_case(column))
        else {
            return Err(errors::missing_column(qualifier, column, self.dialect).at(position));
        };
        let value = row.get(index).cloned().unwrap_or(Value::Null);
        Ok(constant(value, self.columns[index].sql_type, position))
    }

    pub(crate) fn local(&self, name: &str, position: usize) -> Option<Expr> {
        self.locals
            .iter()
            .rev()
            .find(|local| local.name.eq_ignore_ascii_case(name))
            .map(|local| constant(local.value.clone(), Some(local.sql_type), position))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyEnd {
    Finished,
    Ignored,
}

fn truth(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Boolean(value) => *value,
        Value::SmallInt(value) => *value != 0,
        Value::Integer(value) => *value != 0,
        Value::BigInt(value) => *value != 0,
        other => other
            .to_text()
            .and_then(|text| text.trim().parse::<f64>().ok())
            .is_some_and(|number| number != 0.0),
    }
}

fn single_value_select(expr: Expr) -> Select {
    Select {
        distinct: false,
        items: vec![SelectItem::Expr { expr, alias: None }],
        from: None,
        filter: None,
        group_by: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        source_row_limit: None,
    }
}

pub(super) fn run_row_trigger(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    table: &str,
    trigger: &TriggerDefinition,
    columns: &Arc<Vec<OutputColumn>>,
    old: Option<&[Value]>,
    new: Option<&mut Vec<Value>>,
) -> SqlResult<BodyEnd> {
    let TriggerAction::Statements(text) = &trigger.action else {
        return Err(SqlError::not_supported("a PostgreSQL trigger function").with_hint(
            "PostgreSQL triggers run PL/pgSQL functions from the PL/pgSQL milestone on (docs/roadmap.md).",
        ));
    };
    let _active = super::enter_active(table, trigger)?;
    let _dialect = crate::constraints::enter_with_dialect(trigger.dialect);
    crate::cancel::check()?;
    let settings = super::current_settings();
    let frame = TriggerFrame {
        columns: Arc::clone(columns),
        old: old.map(<[Value]>::to_vec),
        new: new.as_deref().cloned(),
        locals: Vec::new(),
        dialect: trigger.dialect,
    };
    let mut run = Run {
        writer,
        tables,
        settings: &settings,
        dialect: trigger.dialect,
        timing: trigger.timing,
        frame,
    };
    match run.whole(trigger.condition.as_deref(), text) {
        Ok(()) => {}
        Err(error) if errors::is_ignored_row(&error) => return Ok(BodyEnd::Ignored),
        Err(error) => return Err(error),
    }
    if let Some(new) = new
        && let Some(changed) = run.frame.new.take()
    {
        *new = changed;
    }
    Ok(BodyEnd::Finished)
}

struct Run<'r> {
    writer: &'r mut dyn KeyValueWrite,
    tables: &'r mut TableCache,
    settings: &'r Settings,
    dialect: Dialect,
    timing: TriggerTiming,
    frame: TriggerFrame,
}

impl Run<'_> {
    fn whole(&mut self, condition: Option<&str>, text: &str) -> SqlResult<()> {
        if let Some(condition) = condition {
            let tokens = tokenize_in(condition, self.dialect)?;
            let value = self.value(condition, &tokens, 0)?;
            if !truth(&value) {
                return Ok(());
            }
        }
        let tokens = tokenize_in(text, self.dialect)?;
        let steps = Parser::new(text, &tokens)
            .with_context(Parameters::Absent, self.settings)
            .in_dialect(self.dialect)
            .trigger_body()?;
        self.steps(text, &tokens, &steps)
    }

    fn steps(&mut self, text: &str, tokens: &[Token<'_>], steps: &[BodyStep]) -> SqlResult<()> {
        for step in steps {
            self.step(text, tokens, step)?;
        }
        Ok(())
    }

    fn step(&mut self, text: &str, tokens: &[Token<'_>], step: &BodyStep) -> SqlResult<()> {
        match step {
            BodyStep::Statement(at) => self.statement(text, tokens, *at),
            BodyStep::Declare {
                names,
                sql_type,
                default,
            } => {
                let value = match default {
                    Some(at) => convert(self.value(text, tokens, *at)?, *sql_type, Conversion::Assignment)?,
                    None => Value::Null,
                };
                for name in names {
                    self.frame.locals.push(LocalVariable {
                        name: name.clone(),
                        sql_type: *sql_type,
                        value: value.clone(),
                    });
                }
                Ok(())
            }
            BodyStep::Assign(targets) => {
                for (target, at) in targets {
                    let value = self.value(text, tokens, *at)?;
                    self.assign(target, value)?;
                }
                Ok(())
            }
            BodyStep::If {
                branches,
                otherwise,
            } => {
                for branch in branches {
                    if truth(&self.value(text, tokens, branch.condition)?) {
                        return self.block(text, tokens, &branch.steps);
                    }
                }
                self.block(text, tokens, otherwise)
            }
            BodyStep::While { condition, body } => {
                while truth(&self.value(text, tokens, *condition)?) {
                    crate::cancel::check()?;
                    self.block(text, tokens, body)?;
                }
                Ok(())
            }
            BodyStep::Signal(signal) => self.signal(text, tokens, signal),
            BodyStep::Block(steps) => self.block(text, tokens, steps),
        }
    }

    fn block(&mut self, text: &str, tokens: &[Token<'_>], steps: &[BodyStep]) -> SqlResult<()> {
        let declared = self.frame.locals.len();
        let outcome = self.steps(text, tokens, steps);
        self.frame.locals.truncate(declared);
        outcome
    }

    fn signal(&mut self, text: &str, tokens: &[Token<'_>], signal: &SignalStep) -> SqlResult<()> {
        if signal.state.starts_with("00") {
            return Err(errors::bad_signal_state(&signal.state));
        }
        if signal.state.starts_with("01") {
            return Ok(());
        }
        let state =
            SqlState::from_text(&signal.state).ok_or_else(|| errors::bad_signal_state(&signal.state))?;
        let message = match signal.message {
            Some(at) => self.value(text, tokens, at)?.to_text(),
            None => None,
        };
        Err(errors::signal(state, message, signal.code))
    }

    fn assign(&mut self, target: &StepTarget, value: Value) -> SqlResult<()> {
        match target {
            StepTarget::Old(_) => Err(errors::old_row_read_only(self.frame.event())),
            StepTarget::New(_) if self.timing != TriggerTiming::Before => {
                Err(errors::new_row_after())
            }
            StepTarget::New(column) => {
                let event = self.frame.event();
                let dialect = self.dialect;
                let Some(index) = self
                    .frame
                    .columns
                    .iter()
                    .position(|known| known.name.eq_ignore_ascii_case(column))
                else {
                    return Err(errors::missing_column("new", column, dialect));
                };
                let sql_type = self.frame.columns[index].sql_type;
                let Some(row) = self.frame.new.as_mut() else {
                    return Err(errors::missing_row("new", column, dialect, event));
                };
                let value = match sql_type {
                    Some(sql_type) => convert(value, sql_type, Conversion::Assignment)?,
                    None => value,
                };
                if let Some(slot) = row.get_mut(index) {
                    *slot = value;
                }
                Ok(())
            }
            StepTarget::Local(name) => {
                let Some(local) = self
                    .frame
                    .locals
                    .iter_mut()
                    .rev()
                    .find(|local| local.name.eq_ignore_ascii_case(name))
                else {
                    return Err(SqlError::new(
                        SqlState::MYSQL_GENERAL,
                        format!("Unknown system variable '{name}'"),
                    )
                    .with_vendor_code(1193));
                };
                local.value = convert(value, local.sql_type, Conversion::Assignment)?;
                Ok(())
            }
        }
    }

    fn value(&mut self, text: &str, tokens: &[Token<'_>], at: usize) -> SqlResult<Value> {
        let expr = Parser::new(text, tokens)
            .with_context(Parameters::Absent, self.settings)
            .in_dialect(self.dialect)
            .with_trigger_frame(&self.frame)
            .resuming_at((at, 0))
            .expression()?;
        let mut select = single_value_select(expr);
        let binding = Binding {
            settings: self.settings,
            walk_expressions: true,
        };
        binding.select(&*self.writer, self.tables, &mut select)?;
        let result = execute::select(&*self.writer, self.tables, &select, false)?;
        Ok(result
            .rows
            .into_iter()
            .next()
            .and_then(|row| row.into_iter().next())
            .unwrap_or(Value::Null))
    }

    fn statement(&mut self, text: &str, tokens: &[Token<'_>], at: usize) -> SqlResult<()> {
        crate::cancel::check()?;
        let statement = Parser::new(text, tokens)
            .with_context(Parameters::Absent, self.settings)
            .in_dialect(self.dialect)
            .with_trigger_frame(&self.frame)
            .resuming_at((at, 0))
            .statement()?;
        run_statement(self.writer, self.tables, self.settings, statement, self.dialect)
    }
}

fn run_statement(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    statement: Statement,
    dialect: Dialect,
) -> SqlResult<()> {
    let binding = Binding {
        settings,
        walk_expressions: true,
    };
    match statement {
        Statement::Insert(mut insert) => {
            binding.insert(&*writer, tables, &mut insert)?;
            execute::insert(writer, tables, &insert).map(drop)
        }
        Statement::Update(mut update) => {
            binding.update(&*writer, tables, &mut update)?;
            execute::update(writer, tables, &update).map(drop)
        }
        Statement::Delete(mut delete) => {
            binding.delete(&*writer, tables, &mut delete)?;
            execute::delete(writer, tables, &delete).map(drop)
        }
        Statement::Select(mut select) => {
            binding.select(&*writer, tables, &mut select)?;
            execute::select(&*writer, tables, &select, false).map(drop)
        }
        Statement::Query(mut query) => {
            binding.query(&*writer, tables, &mut query)?;
            execute::query(&*writer, tables, &query, false).map(drop)
        }
        Statement::With(with) => crate::with_statement::run(writer, tables, &with, &binding).map(drop),
        _ => Err(errors::not_in_trigger("this statement", dialect)),
    }
}
