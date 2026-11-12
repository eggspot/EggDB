use super::super::frame::RoutineFrame;
use super::super::statements::processed_rows;
use super::dbms_scheduler::split_arguments;
use super::{Flow, Run, Target, output_target, parsed};
use crate::ast::Statement;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::plsql::with_session;
use crate::types::Value;
use std::collections::BTreeMap;

const PACKAGE: &str = "dbms_sql";
const OWNER: &str = "sys";
const MOST_OPEN_CURSORS: usize = 300;
const INVALID_CURSOR: i32 = 29_471;
const FUNCTIONS: [&str; 6] = ["open_cursor", "execute", "fetch_rows", "execute_and_fetch", "is_open", "last_row_count"];

/// One `DBMS_SQL` cursor of a session: its statement, its bind values and the rows a query
/// returned.
#[derive(Debug, Clone, Default)]
pub(crate) struct DynamicCursor {
    statement: Option<String>,
    binds: BTreeMap<String, Value>,
    rows: Vec<Vec<Value>>,
    next_row: usize,
    current: Option<Vec<Value>>,
}

/// The session's `DBMS_SQL` cursors by number.
#[derive(Debug, Default)]
pub(crate) struct DynamicCursors {
    open: BTreeMap<i64, DynamicCursor>,
    next: i64,
    last_row_count: u64,
}

fn invalid_cursor() -> SqlError {
    SqlError::new(SqlState::INVALID_CURSOR_NAME, format!("ORA-{INVALID_CURSOR:05}: invalid cursor"))
        .with_oracle_code(INVALID_CURSOR)
}

fn wrong_arguments(member: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("PLS-00306: wrong number or types of arguments in call to '{}'", member.to_ascii_uppercase()),
    )
}

fn with_cursors<T>(use_them: impl FnOnce(&mut DynamicCursors) -> SqlResult<T>) -> SqlResult<T> {
    with_session(|session| use_them(&mut session.dynamic_cursors))
        .unwrap_or_else(|| Err(SqlError::not_supported("DBMS_SQL outside a session")))
}

fn cursor_number(value: &Value) -> SqlResult<i64> {
    value
        .to_text()
        .and_then(|text| text.trim().parse::<f64>().ok())
        .filter(|number| number.fract() == 0.0)
        .map(|number| number as i64)
        .ok_or_else(invalid_cursor)
}

fn literal_of(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) | Value::Numeric(_) | Value::Real(_) | Value::Double(_) => {
            value.to_text().unwrap_or_else(|| "NULL".to_owned())
        }
        other => {
            let text = other.to_text().unwrap_or_default();
            format!("'{}'", text.replace('\'', "''"))
        }
    }
}

fn bound_statement(statement: &str, binds: &BTreeMap<String, Value>) -> SqlResult<String> {
    let mut out = String::with_capacity(statement.len());
    let mut characters = statement.char_indices().peekable();
    let mut quoted = false;
    while let Some((_, character)) = characters.next() {
        if character == '\'' {
            quoted = !quoted;
            out.push(character);
            continue;
        }
        if character != ':' || quoted {
            out.push(character);
            continue;
        }
        if characters.peek().is_some_and(|(_, next)| *next == '=') {
            out.push(character);
            continue;
        }
        let mut name = String::new();
        while let Some(&(_, next)) = characters.peek() {
            if next.is_alphanumeric() || next == '_' {
                name.push(next);
                characters.next();
            } else {
                break;
            }
        }
        if name.is_empty() {
            out.push(character);
            continue;
        }
        let value = binds.get(&name.to_ascii_lowercase()).ok_or_else(|| {
            SqlError::new(SqlState::UNDEFINED_PARAMETER, "ORA-01008: not all variables bound").with_oracle_code(1008)
        })?;
        out.push_str(&literal_of(value));
    }
    Ok(out)
}

struct PackageCall {
    member: String,
    arguments: Vec<String>,
}

fn package_member(name: &str) -> Option<String> {
    let name = name.trim().replace('"', "").to_ascii_lowercase();
    let parts: Vec<&str> = name.split('.').map(str::trim).collect();
    match parts.as_slice() {
        [PACKAGE, member] | [OWNER, PACKAGE, member] => Some((*member).to_owned()),
        _ => None,
    }
}

impl PackageCall {
    fn of(text: &str) -> Option<Self> {
        let text = text.trim();
        if !text.get(..4)?.eq_ignore_ascii_case("call") {
            return None;
        }
        let rest = text.get(4..)?;
        let open = rest.find('(')?;
        let close = rest.rfind(')')?;
        Some(Self {
            member: package_member(rest.get(..open)?)?,
            arguments: split_arguments(rest.get(open + 1..close)?),
        })
    }
}

fn function_calls(text: &str) -> Vec<(usize, usize, String, Vec<String>)> {
    let lower = text.to_ascii_lowercase();
    let mut calls = Vec::new();
    let mut from = 0;
    while let Some(found) = lower.get(from..).and_then(|rest| rest.find("dbms_sql.")) {
        let start = from + found;
        let name_start = start + PACKAGE.len() + 1;
        let name_end = lower
            .get(name_start..)
            .and_then(|rest| rest.find(|character: char| !(character.is_alphanumeric() || character == '_')))
            .map_or(lower.len(), |at| name_start + at);
        let member = lower.get(name_start..name_end).unwrap_or_default().to_owned();
        let owner_start = if lower.get(..start).is_some_and(|before| before.ends_with("sys.")) { start - 4 } else { start };
        from = name_end;
        if !FUNCTIONS.contains(&member.as_str()) {
            continue;
        }
        let after = lower.get(name_end..).unwrap_or_default();
        let spaces = after.len() - after.trim_start().len();
        if after.trim_start().starts_with('(') {
            let open = name_end + spaces;
            let mut depth = 0usize;
            let mut close = None;
            let mut quoted = false;
            for (at, character) in text.get(open..).unwrap_or_default().char_indices() {
                match character {
                    '\'' => quoted = !quoted,
                    '(' if !quoted => depth += 1,
                    ')' if !quoted => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            close = Some(open + at);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let Some(close) = close else {
                continue;
            };
            let arguments = split_arguments(text.get(open + 1..close).unwrap_or_default());
            calls.push((owner_start, close + 1, member, arguments));
            from = close + 1;
        } else {
            calls.push((owner_start, name_end, member, Vec::new()));
        }
    }
    calls
}

impl Run<'_> {
    /// Replaces the `DBMS_SQL` functions in an Oracle expression with their results, running
    /// what they run (`EXECUTE`, `FETCH_ROWS`), before the expression is evaluated.
    pub(super) fn with_dbms_sql_results(&mut self, text: &str) -> SqlResult<String> {
        if !self.oracle || !text.to_ascii_lowercase().contains("dbms_sql.") {
            return Ok(text.to_owned());
        }
        let calls = function_calls(text);
        if calls.is_empty() {
            return Ok(text.to_owned());
        }
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        for (start, end, member, arguments) in calls {
            out.push_str(text.get(at..start).unwrap_or_default());
            let result = self.dbms_sql_function(&member, &arguments)?;
            out.push_str(&literal_of(&result));
            at = end;
        }
        out.push_str(text.get(at..).unwrap_or_default());
        Ok(out)
    }

    fn cursor_argument(&mut self, arguments: &[String], member: &str) -> SqlResult<i64> {
        let text = arguments.first().ok_or_else(|| wrong_arguments(member))?;
        let value = self.text_value(text)?;
        cursor_number(&value)
    }

    fn dbms_sql_function(&mut self, member: &str, arguments: &[String]) -> SqlResult<Value> {
        match member {
            "open_cursor" => with_cursors(|cursors| {
                if cursors.open.len() >= MOST_OPEN_CURSORS {
                    return Err(SqlError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, "ORA-01000: maximum open cursors exceeded")
                        .with_oracle_code(1000));
                }
                cursors.next += 1;
                let number = cursors.next;
                cursors.open.insert(number, DynamicCursor::default());
                Ok(Value::BigInt(number))
            }),
            "is_open" => {
                let number = self.cursor_argument(arguments, member)?;
                with_cursors(|cursors| Ok(Value::Boolean(cursors.open.contains_key(&number))))
            }
            "last_row_count" => with_cursors(|cursors| Ok(Value::BigInt(cursors.last_row_count as i64))),
            "execute" => {
                let number = self.cursor_argument(arguments, member)?;
                self.dbms_sql_execute(number).map(|count| Value::BigInt(count as i64))
            }
            "fetch_rows" => {
                let number = self.cursor_argument(arguments, member)?;
                Self::dbms_sql_fetch(number).map(|fetched| Value::BigInt(i64::from(fetched)))
            }
            _ => {
                let number = self.cursor_argument(arguments, member)?;
                self.dbms_sql_execute(number)?;
                Self::dbms_sql_fetch(number).map(|fetched| Value::BigInt(i64::from(fetched)))
            }
        }
    }

    fn dbms_sql_execute(&mut self, number: i64) -> SqlResult<u64> {
        let (statement, binds) = with_cursors(|cursors| {
            let cursor = cursors.open.get(&number).ok_or_else(invalid_cursor)?;
            let statement = cursor.statement.clone().ok_or_else(|| {
                SqlError::new(SqlState::INVALID_CURSOR_NAME, "ORA-01003: no statement parsed").with_oracle_code(1003)
            })?;
            Ok((statement, cursor.binds.clone()))
        })?;
        let text = bound_statement(&statement, &binds)?;
        let result = self.query_text(&text)?;
        let processed = processed_rows(&result);
        let is_query = result.columns.is_some();
        with_cursors(|cursors| {
            let cursor = cursors.open.get_mut(&number).ok_or_else(invalid_cursor)?;
            cursor.rows = if is_query { result.rows } else { Vec::new() };
            cursor.next_row = 0;
            cursor.current = None;
            cursors.last_row_count = if is_query { 0 } else { processed };
            Ok(if is_query { 0 } else { processed })
        })
    }

    fn dbms_sql_fetch(number: i64) -> SqlResult<bool> {
        with_cursors(|cursors| {
            let cursor = cursors.open.get_mut(&number).ok_or_else(invalid_cursor)?;
            let row = cursor.rows.get(cursor.next_row).cloned();
            let fetched = row.is_some();
            if fetched {
                cursor.next_row += 1;
            }
            cursor.current = row;
            if fetched {
                cursors.last_row_count += 1;
            }
            Ok(fetched)
        })
    }

    fn dbms_sql_targets(&self, text: &str, member: &str) -> SqlResult<Vec<Option<Target>>> {
        let Statement::Call(parsed_call) = parsed(text, &self.settings, &RoutineFrame::default(), self.dialect())? else {
            return Err(wrong_arguments(member));
        };
        Ok(parsed_call.arguments.iter().map(output_target).collect())
    }

    /// Runs a `DBMS_SQL` procedure call: `PARSE`, `BIND_VARIABLE`, `DEFINE_COLUMN`,
    /// `COLUMN_VALUE`, `VARIABLE_VALUE` and `CLOSE_CURSOR`.
    pub(super) fn dbms_sql_call(&mut self, text: &str) -> SqlResult<Option<Flow>> {
        let Some(call) = PackageCall::of(text) else {
            return Ok(None);
        };
        let member = call.member.as_str();
        let arguments = &call.arguments;
        match member {
            "parse" => {
                let number = self.cursor_argument(arguments, member)?;
                let statement = self.text_value(arguments.get(1).ok_or_else(|| wrong_arguments(member))?)?;
                let statement = statement.to_text().unwrap_or_default();
                let statement = statement.trim().trim_end_matches(';').to_owned();
                let ddl = ["create", "alter", "drop", "truncate", "grant", "revoke", "rename", "comment"]
                    .iter()
                    .any(|word| statement.to_ascii_lowercase().starts_with(word));
                if ddl {
                    self.query_text(&statement)?;
                }
                with_cursors(|cursors| {
                    let cursor = cursors.open.get_mut(&number).ok_or_else(invalid_cursor)?;
                    *cursor = DynamicCursor { statement: Some(statement), ..DynamicCursor::default() };
                    Ok(())
                })?;
            }
            "bind_variable" | "bind_array" => {
                let number = self.cursor_argument(arguments, member)?;
                let name = self
                    .text_value(arguments.get(1).ok_or_else(|| wrong_arguments(member))?)?
                    .to_text()
                    .unwrap_or_default()
                    .trim_start_matches(':')
                    .to_ascii_lowercase();
                let value = self.text_value(arguments.get(2).ok_or_else(|| wrong_arguments(member))?)?;
                with_cursors(|cursors| {
                    let cursor = cursors.open.get_mut(&number).ok_or_else(invalid_cursor)?;
                    cursor.binds.insert(name, value);
                    Ok(())
                })?;
            }
            "define_column" | "define_array" | "define_column_long" => {
                let number = self.cursor_argument(arguments, member)?;
                with_cursors(|cursors| cursors.open.get(&number).map(|_| ()).ok_or_else(invalid_cursor))?;
            }
            "column_value" | "column_value_long" => {
                let number = self.cursor_argument(arguments, member)?;
                let position = self.text_value(arguments.get(1).ok_or_else(|| wrong_arguments(member))?)?;
                let position = cursor_number(&position)?;
                let value = with_cursors(|cursors| {
                    let cursor = cursors.open.get(&number).ok_or_else(invalid_cursor)?;
                    let row = cursor.current.as_ref().ok_or_else(|| {
                        SqlError::new(SqlState::NO_DATA_FOUND, "ORA-01002: fetch out of sequence").with_oracle_code(1002)
                    })?;
                    usize::try_from(position - 1)
                        .ok()
                        .and_then(|at| row.get(at))
                        .cloned()
                        .ok_or_else(|| {
                            SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "ORA-01007: variable not in select list")
                                .with_oracle_code(1007)
                        })
                })?;
                let target = self.dbms_sql_targets(text, member)?.get(2).cloned().flatten().ok_or_else(|| wrong_arguments(member))?;
                self.assign(&target, value)?;
            }
            "variable_value" => {
                let number = self.cursor_argument(arguments, member)?;
                let name = self
                    .text_value(arguments.get(1).ok_or_else(|| wrong_arguments(member))?)?
                    .to_text()
                    .unwrap_or_default()
                    .trim_start_matches(':')
                    .to_ascii_lowercase();
                let value = with_cursors(|cursors| {
                    let cursor = cursors.open.get(&number).ok_or_else(invalid_cursor)?;
                    Ok(cursor.binds.get(&name).cloned().unwrap_or(Value::Null))
                })?;
                let target = self.dbms_sql_targets(text, member)?.get(2).cloned().flatten().ok_or_else(|| wrong_arguments(member))?;
                self.assign(&target, value)?;
            }
            "close_cursor" => {
                let number = self.cursor_argument(arguments, member)?;
                with_cursors(|cursors| cursors.open.remove(&number).map(|_| ()).ok_or_else(invalid_cursor))?;
                if let Some(target) = self.dbms_sql_targets(text, member)?.first().cloned().flatten() {
                    self.assign(&target, Value::Null)?;
                }
            }
            _ => return Ok(None),
        }
        Ok(Some(Flow::Next))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binds_fill_placeholders_outside_quotes_and_functions_are_found() {
        let mut binds = BTreeMap::new();
        binds.insert("id".to_owned(), Value::BigInt(7));
        binds.insert("name".to_owned(), Value::Text("O'Hara".to_owned()));
        assert_eq!(
            bound_statement("SELECT ':id' FROM t WHERE id = :id AND name = :NAME", &binds).unwrap(),
            "SELECT ':id' FROM t WHERE id = 7 AND name = 'O''Hara'"
        );
        assert!(bound_statement("SELECT :missing FROM dual", &binds).is_err());
        let calls = function_calls("WHILE DBMS_SQL.FETCH_ROWS(c) > 0 AND sys.dbms_sql.is_open(c)");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].2, "fetch_rows");
        assert_eq!(calls[1].0, "WHILE DBMS_SQL.FETCH_ROWS(c) > 0 AND ".len());
        assert_eq!(function_calls("c := DBMS_SQL.OPEN_CURSOR;")[0].2, "open_cursor");
    }
}
