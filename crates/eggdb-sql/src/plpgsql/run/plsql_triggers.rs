use super::plsql::UnitLines;
use super::{Run, enter_routine, with_context_line};
use crate::error::{SqlError, SqlResult};
use crate::execute::TableCache;
use crate::plpgsql::Program;
use crate::plpgsql::frame::{FrameVariable, Slot};
use crate::routines::RoutineDefinition;
use crate::schemas::base_name;
use crate::settings::Settings;
use crate::store::KeyValueWrite;
use crate::triggers::TriggerEvent;
use crate::types::{Conversion, SqlType, Value, convert};
use std::sync::Arc;

pub(crate) struct PlsqlTriggerCall<'c> {
    pub(crate) definition: &'c Arc<RoutineDefinition>,
    pub(crate) program: &'c Program,
    pub(crate) condition: Option<&'c str>,
    pub(crate) old: Value,
    pub(crate) new: Value,
    pub(crate) event: TriggerEvent,
    pub(crate) updated: &'c [String],
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PlsqlTriggerEnd {
    ConditionFalse,
    Finished { new: Value },
}

fn failed_trigger(error: SqlError, definition: &RoutineDefinition) -> SqlError {
    with_context_line(
        error,
        &format!(
            "ORA-04088: error during execution of trigger '{}'",
            crate::plsql::shown_object(&definition.name)
        ),
    )
}

pub(crate) fn run_plsql_trigger(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    call: PlsqlTriggerCall<'_>,
) -> SqlResult<PlsqlTriggerEnd> {
    let _depth = enter_routine()?;
    crate::cancel::check()?;
    let definition = call.definition;
    let label = format!("\"{}\"", crate::plsql::shown_object(&definition.name));
    let mut run = Run::new(writer, tables, settings, definition, label.clone());
    run.unit = Some(Arc::new(UnitLines {
        label,
        text: Arc::from(definition.body.as_str()),
        base: 0,
    }));
    let outcome = run.plsql_trigger(&call);
    run.share_packages();
    outcome.map_err(|error| failed_trigger(error, definition))
}

impl Run<'_> {
    fn plsql_trigger(&mut self, call: &PlsqlTriggerCall<'_>) -> SqlResult<PlsqlTriggerEnd> {
        self.attach_packages(call.program)?;
        self.bind_parameters(Vec::new());
        self.bind_plsql_trigger(call);
        if let Some(condition) = call.condition
            && !self.trigger_condition(condition)?
        {
            return Ok(PlsqlTriggerEnd::ConditionFalse);
        }
        self.program(call.program)?;
        let new = self
            .frame
            .index_of("new")
            .map_or(Value::Null, |index| self.frame.variables[index].value.clone());
        Ok(PlsqlTriggerEnd::Finished { new })
    }

    fn bind_plsql_trigger(&mut self, call: &PlsqlTriggerCall<'_>) {
        let label = Some(base_name(&self.definition.name).to_owned());
        let predicate = |event: TriggerEvent| Value::Boolean(call.event == event);
        let variables = [
            ("new", Slot::Record, call.new.clone(), false),
            ("old", Slot::Record, call.old.clone(), true),
            (
                "inserting",
                Slot::Typed(SqlType::Boolean),
                predicate(TriggerEvent::Insert),
                true,
            ),
            (
                "updating",
                Slot::Typed(SqlType::Boolean),
                predicate(TriggerEvent::Update),
                true,
            ),
            (
                "deleting",
                Slot::Typed(SqlType::Boolean),
                predicate(TriggerEvent::Delete),
                true,
            ),
        ];
        for (name, slot, value, constant) in variables {
            let mut variable = FrameVariable::new(name, slot, value);
            variable.label = label.clone();
            variable.constant = constant;
            self.frame.variables.push(variable);
        }
        self.plsql_frame_mut().updated_columns = Some(call.updated.to_vec());
    }

    fn trigger_condition(&mut self, condition: &str) -> SqlResult<bool> {
        Ok(match self.text_value(condition)? {
            Value::Null => false,
            Value::Boolean(value) => value,
            other => matches!(
                convert(other, SqlType::Boolean, Conversion::Explicit)?,
                Value::Boolean(true)
            ),
        })
    }
}
