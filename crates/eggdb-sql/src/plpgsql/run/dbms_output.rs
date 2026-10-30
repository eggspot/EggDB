use super::super::frame::RoutineFrame;
use super::{Flow, Run, output_target, parsed};
use crate::ast::Statement;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::plsql::{DEFAULT_OUTPUT_BUFFER_BYTES, ServerOutput, with_session};
use crate::types::Value;

const PACKAGE: &str = "dbms_output";
const OWNER: &str = "sys";

struct OutputCall<'t> {
    member: String,
    arguments: &'t str,
}

impl<'t> OutputCall<'t> {
    fn of(text: &'t str) -> Option<Self> {
        let text = text.trim();
        if !text.get(..4)?.eq_ignore_ascii_case("call") {
            return None;
        }
        let rest = text.get(4..)?;
        let open = rest.find('(')?;
        let close = rest.rfind(')')?;
        let name = rest.get(..open)?.trim().replace('"', "").to_ascii_lowercase();
        let parts: Vec<&str> = name.split('.').map(str::trim).collect();
        let member = match parts.as_slice() {
            [PACKAGE, member] | [OWNER, PACKAGE, member] => (*member).to_owned(),
            _ => return None,
        };
        let arguments = rest.get(open + 1..close)?;
        Some(Self { member, arguments })
    }
}

impl Run<'_> {
    pub(super) fn dbms_output_call(&mut self, text: &str) -> SqlResult<Option<Flow>> {
        let Some(call) = OutputCall::of(text) else {
            return Ok(None);
        };
        match call.member.as_str() {
            "put_line" => {
                let line = self.output_text(call.arguments, "PUT_LINE")?;
                change_output(|output| {
                    output.put(&line)?;
                    output.new_line();
                    Ok(())
                })?;
            }
            "put" => {
                let item = self.output_text(call.arguments, "PUT")?;
                change_output(|output| output.put(&item))?;
            }
            "new_line" => change_output(|output| {
                output.new_line();
                Ok(())
            })?,
            "enable" => {
                let bytes = match call.arguments.trim() {
                    "" => Some(DEFAULT_OUTPUT_BUFFER_BYTES),
                    arguments => match self.text_value(arguments)? {
                        Value::Null => None,
                        value => Some(buffer_bytes(&value)?),
                    },
                };
                change_output(|output| {
                    output.enable(bytes);
                    Ok(())
                })?;
            }
            "disable" => change_output(|output| {
                output.disable();
                Ok(())
            })?,
            "get_line" => self.get_output_line(text)?,
            "get_lines" => return Err(SqlError::not_supported("DBMS_OUTPUT.GET_LINES")),
            other => return Err(unknown_member(other)),
        }
        Ok(Some(Flow::Next))
    }

    fn output_text(&mut self, arguments: &str, member: &str) -> SqlResult<String> {
        if arguments.trim().is_empty() {
            return Err(wrong_arguments(member));
        }
        Ok(self.text_value(arguments)?.to_text().unwrap_or_default())
    }

    fn get_output_line(&mut self, text: &str) -> SqlResult<()> {
        let Statement::Call(call) =
            parsed(text, self.settings, &RoutineFrame::default(), self.dialect())?
        else {
            return Err(wrong_arguments("GET_LINE"));
        };
        let [line, status] = call.arguments.as_slice() else {
            return Err(wrong_arguments("GET_LINE"));
        };
        let (Some(line), Some(status)) = (output_target(line), output_target(status)) else {
            return Err(wrong_arguments("GET_LINE"));
        };
        let next = with_session(|state| state.output.next_line()).flatten();
        let missing = next.is_none();
        self.assign(&line, next.map_or(Value::Null, Value::Text))?;
        self.assign(&status, Value::Integer(i32::from(missing)))
    }
}

fn change_output(change: impl FnOnce(&mut ServerOutput) -> SqlResult<()>) -> SqlResult<()> {
    with_session(|state| change(&mut state.output)).unwrap_or(Ok(()))
}

fn buffer_bytes(value: &Value) -> SqlResult<usize> {
    let text = value.to_text().unwrap_or_default();
    let number: f64 = text
        .trim()
        .parse()
        .map_err(|_| wrong_arguments("ENABLE"))?;
    if !number.is_finite() || number <= 0.0 {
        return Ok(0);
    }
    Ok(number.min(u32::MAX.into()) as usize)
}

fn wrong_arguments(member: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("PLS-00306: wrong number or types of arguments in call to '{member}'"),
    )
}

fn unknown_member(member: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_OBJECT,
        format!(
            "PLS-00302: component '{}' must be declared",
            member.to_ascii_uppercase()
        ),
    )
}
