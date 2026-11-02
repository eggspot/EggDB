use crate::ast::Expr;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::session::{QueryResult, Session};
use crate::types::Value;

const MICROS_PER_DAY: i64 = 86_400_000_000;

fn incorrect_time(text: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::INVALID_DATETIME_FORMAT,
        format!("Incorrect time syntax in time string '{text}' used with WAITFOR."),
    )
    .with_sqlserver_code(148)
    .at(position)
}

fn local_time_of_day() -> i64 {
    let now = crate::datetime::statement_now();
    let local = crate::datetime::to_local(now, crate::datetime::session_offset_at(now));
    crate::datetime::time_of_timestamp(local)
}

impl Session<'_> {
    pub(in crate::session) fn run_waitfor(
        &mut self,
        until_time: bool,
        time: &Expr,
    ) -> SqlResult<QueryResult> {
        let text = match self.evaluate(time)? {
            Value::Null => return Ok(QueryResult::command("WAITFOR")),
            Value::Text(text) => text,
            other => crate::sqlserver_conversion::text_of(&other, None, None)?,
        };
        let micros = crate::datetime::parse_time_of_day(text.trim(), false)
            .map(|(micros, _)| micros)
            .map_err(|_| incorrect_time(&text, time.position))?;
        let delay = if until_time {
            (micros - local_time_of_day()).rem_euclid(MICROS_PER_DAY)
        } else {
            micros
        };
        crate::cancel::sleep(delay as f64 / 1_000_000.0)?;
        Ok(QueryResult::command("WAITFOR"))
    }
}
