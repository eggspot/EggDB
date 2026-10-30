use super::{RaiseLevel, condition_name, condition_states};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::settings::Settings;
use crate::types::Value;
use std::cell::RefCell;

const MAX_RAISED_NOTICES: usize = 10_000;

thread_local! {
    static RAISED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn take_raised_notices() -> Vec<String> {
    RAISED.with(|raised| std::mem::take(&mut *raised.borrow_mut()))
}

pub(crate) fn push_statement_notices(notices: Vec<String>) {
    RAISED.with(|raised| {
        let mut raised = raised.borrow_mut();
        let room = MAX_RAISED_NOTICES.saturating_sub(raised.len());
        raised.extend(notices.into_iter().take(room));
    });
}

fn minimum_level(settings: &Settings) -> RaiseLevel {
    match settings
        .get("client_min_messages")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "debug" | "debug1" | "debug2" | "debug3" | "debug4" | "debug5" => RaiseLevel::Debug,
        "log" => RaiseLevel::Log,
        "warning" => RaiseLevel::Warning,
        "error" => RaiseLevel::Exception,
        _ => RaiseLevel::Notice,
    }
}

fn shown(level: RaiseLevel, settings: &Settings) -> bool {
    level == RaiseLevel::Info || level >= minimum_level(settings)
}

pub(crate) fn notice_text(level: RaiseLevel, message: &str) -> String {
    let severity = match level {
        RaiseLevel::Debug => "DEBUG",
        RaiseLevel::Log => "LOG",
        RaiseLevel::Info => "INFO",
        RaiseLevel::Notice => "NOTICE",
        RaiseLevel::Warning | RaiseLevel::Exception => "WARNING",
    };
    format!("{severity}:  {message}")
}

pub(crate) fn push_notice(level: RaiseLevel, message: &str, settings: &Settings) {
    if !shown(level, settings) {
        return;
    }
    let text = notice_text(level, message);
    RAISED.with(|raised| {
        let mut raised = raised.borrow_mut();
        if raised.len() < MAX_RAISED_NOTICES {
            raised.push(text);
        }
    });
}

pub(crate) fn display_value(value: &Value) -> String {
    value.to_text().unwrap_or_else(|| "<NULL>".to_owned())
}

pub(crate) fn format_message(format: &str, arguments: &[Value]) -> String {
    let mut message = String::with_capacity(format.len());
    let mut arguments = arguments.iter();
    let mut characters = format.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '%' {
            message.push(character);
            continue;
        }
        if characters.peek() == Some(&'%') {
            characters.next();
            message.push('%');
            continue;
        }
        match arguments.next() {
            Some(value) => message.push_str(&display_value(value)),
            None => message.push('%'),
        }
    }
    message
}

pub(crate) fn errcode(text: &str) -> SqlResult<SqlState> {
    if text.len() == 5
        && let Some(state) = SqlState::from_text(text)
    {
        return Ok(state);
    }
    condition_states(&text.to_ascii_lowercase())
        .into_iter()
        .next()
        .ok_or_else(|| {
            SqlError::new(
                SqlState::UNDEFINED_OBJECT,
                format!("unrecognized exception condition \"{text}\""),
            )
        })
}

pub(crate) fn default_message(state: SqlState) -> String {
    match condition_name(state) {
        Some(name) => name.to_owned(),
        None => state.code().to_owned(),
    }
}

pub(crate) fn with_context_line(error: SqlError, line: &str) -> SqlError {
    let context = match error.context() {
        Some(existing) if !existing.is_empty() => format!("{existing}\n{line}"),
        _ => line.to_owned(),
    };
    error.with_context(context)
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RaisedFields {
    pub(crate) message: Option<String>,
    pub(crate) detail: Option<String>,
    pub(crate) hint: Option<String>,
    pub(crate) state: Option<SqlState>,
}

impl RaisedFields {
    pub(crate) fn into_error(self) -> SqlError {
        let state = self.state.unwrap_or(SqlState::RAISE_EXCEPTION);
        let message = self.message.unwrap_or_else(|| default_message(state));
        let mut error = SqlError::new(state, message);
        if let Some(detail) = self.detail {
            error = error.with_detail(detail);
        }
        if let Some(hint) = self.hint {
            error = error.with_hint(hint);
        }
        error
    }

    pub(crate) fn notice_message(&self) -> String {
        let state = self.state.unwrap_or(SqlState::RAISE_EXCEPTION);
        let mut message = self
            .message
            .clone()
            .unwrap_or_else(|| default_message(state));
        if let Some(detail) = &self.detail {
            message.push_str("\nDETAIL:  ");
            message.push_str(detail);
        }
        if let Some(hint) = &self.hint {
            message.push_str("\nHINT:  ");
            message.push_str(hint);
        }
        message
    }
}

#[cfg(test)]
mod tests {
    use super::{errcode, format_message, notice_text, with_context_line};
    use crate::error::{SqlError, SqlState};
    use crate::plpgsql::RaiseLevel;
    use crate::types::Value;

    #[test]
    fn messages_fill_placeholders_like_postgresql() {
        let arguments = [Value::Integer(7), Value::Null];
        assert_eq!(format_message("a % b % c %%", &arguments), "a 7 b <NULL> c %");
        assert_eq!(format_message("100%%", &[]), "100%");
    }

    #[test]
    fn error_codes_read_states_and_condition_names() {
        assert_eq!(errcode("22012").map(|state| state.code().to_owned()), Ok("22012".to_owned()));
        assert_eq!(
            errcode("unique_violation").map(|state| state.code().to_owned()),
            Ok("23505".to_owned())
        );
        assert!(errcode("no_such_condition").is_err());
    }

    #[test]
    fn notices_carry_their_severity_and_contexts_stack() {
        assert_eq!(notice_text(RaiseLevel::Notice, "hi"), "NOTICE:  hi");
        assert_eq!(notice_text(RaiseLevel::Warning, "w"), "WARNING:  w");
        let error = with_context_line(SqlError::new(SqlState::RAISE_EXCEPTION, "x"), "inner");
        let error = with_context_line(error, "outer");
        assert_eq!(error.context(), Some("inner\nouter"));
    }
}
