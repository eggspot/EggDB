use crate::ast::{Expr, ProcedureCall};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::session::{QueryResult, Session};
use crate::types::Value;

pub(in crate::session) const PROCEDURE: &str = "sp_sequence_get_range";

const PARAMETERS: [&str; 8] = [
    "@sequence_name",
    "@range_size",
    "@range_first_value",
    "@range_last_value",
    "@range_cycle_count",
    "@sequence_increment",
    "@sequence_min_value",
    "@sequence_max_value",
];

fn raised(number: u32, message: String) -> SqlError {
    SqlError::raised_in_sqlserver(number, 16, 1, message, false)
}

fn bracketed(text: &str) -> String {
    format!("[{}]", text.replace(']', "]]"))
}

impl Session<'_> {
    fn sequence_range_arguments(
        &mut self,
        call: &ProcedureCall,
    ) -> SqlResult<(Vec<Option<Value>>, Vec<Option<String>>)> {
        let mut values: Vec<Option<Expr>> = vec![None; PARAMETERS.len()];
        let mut outputs: Vec<Option<String>> = vec![None; PARAMETERS.len()];
        for (ordinal, argument) in call.arguments.iter().enumerate() {
            let index = match &argument.name {
                None => Some(ordinal),
                Some(name) => {
                    let name = name.trim_start_matches('@');
                    PARAMETERS
                        .iter()
                        .position(|known| known.trim_start_matches('@').eq_ignore_ascii_case(name))
                }
            };
            let Some(index) = index.filter(|index| *index < PARAMETERS.len()) else {
                return Err(raised(
                    8145,
                    format!(
                        "{} is not a parameter for procedure {PROCEDURE}.",
                        argument.name.as_deref().unwrap_or("?")
                    ),
                ));
            };
            values[index] = argument.value.clone();
            outputs[index] = argument.output_variable.clone();
        }
        let given: Vec<Expr> = values.iter().flatten().cloned().collect();
        let mut evaluated = if given.is_empty() {
            Vec::new()
        } else {
            self.evaluate_row(given)?
        }
        .into_iter();
        Ok((
            values
                .iter()
                .map(|expr| expr.as_ref().and_then(|_| evaluated.next()))
                .collect(),
            outputs,
        ))
    }

    /// SQL Server's `sp_sequence_get_range`: reserves `@range_size` values of a sequence at once
    /// and returns the first and last through output parameters.
    pub(in crate::session) fn run_sequence_get_range(
        &mut self,
        call: &ProcedureCall,
    ) -> SqlResult<QueryResult> {
        let (values, outputs) = self.sequence_range_arguments(call)?;
        let Some(text) = values[0]
            .as_ref()
            .and_then(Value::character_text)
            .map(str::to_owned)
        else {
            return Err(raised(
                201,
                format!("Procedure or function '{PROCEDURE}' expects parameter '@sequence_name', which was not supplied."),
            ));
        };
        let size = match values[1].as_ref().and_then(Value::as_integer) {
            Some(size) if size > 0 => size,
            _ => {
                return Err(raised(
                    11733,
                    "The range size must be a positive number.".to_owned(),
                ));
            }
        };
        let settings = self.settings.clone();
        let name = crate::parser::dialect_table_name(&text, &settings, crate::Dialect::SqlServer)?;
        let Some(sequence) = self.read(|reader, _| crate::sequences::read(reader, &name.text))? else {
            return Err(raised(208, format!("Invalid object name '{text}'.")));
        };
        let schema = crate::schemas::schema_of(&name.text);
        let base = crate::schemas::base_name(&name.text);
        let quoted = if schema.is_empty() || schema == crate::schemas::DEFAULT_SCHEMA {
            bracketed(base)
        } else {
            format!("{}.{}", bracketed(schema), bracketed(base))
        };
        let mut first = None;
        self.execute(&format!("SELECT NEXT VALUE FOR {quoted}"), &mut |result| {
            first = result
                .rows
                .first()
                .and_then(|row| row.first())
                .and_then(Value::as_integer);
        })?;
        let first = first.ok_or_else(|| raised(11732, format!("The requested range for sequence object '{text}' exceeds the maximum or minimum limit. Retry with a smaller range.")))?;
        let exceeded = || {
            raised(
                11732,
                format!("The requested range for sequence object '{text}' exceeds the maximum or minimum limit. Retry with a smaller range."),
            )
        };
        let last = (size - 1)
            .checked_mul(sequence.increment)
            .and_then(|span| first.checked_add(span))
            .ok_or_else(exceeded)?;
        if last > sequence.max || last < sequence.min {
            return Err(exceeded());
        }
        if size > 1
            && let Some(next) = last.checked_add(sequence.increment)
            && (sequence.min..=sequence.max).contains(&next)
        {
            self.execute(
                &format!("ALTER SEQUENCE {quoted} RESTART WITH {next}"),
                &mut |_| {},
            )?;
        }
        let returned = [
            None,
            None,
            Some(Value::BigInt(first)),
            Some(Value::BigInt(last)),
            Some(Value::Integer(0)),
            Some(Value::BigInt(sequence.increment)),
            Some(Value::BigInt(sequence.min)),
            Some(Value::BigInt(sequence.max)),
        ];
        for (output, value) in outputs.iter().zip(returned) {
            if let (Some(variable), Some(value)) = (output, value) {
                self.assign_variable(variable, value)?;
            }
        }
        if let Some(variable) = &call.status_variable {
            self.assign_variable(&variable.text, Value::Integer(0))
                .map_err(|error| error.at_if_unplaced(variable.position))?;
        }
        Ok(QueryResult::command("EXECUTE"))
    }
}
