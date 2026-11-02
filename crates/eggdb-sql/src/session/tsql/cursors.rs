use crate::ast::{CursorDeclaration, CursorName, Expr, FetchOrientation, Name};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::session::{QueryResult, Session};
use crate::tsql::{
    Cursor, CursorMove, CursorState, FETCH_BEYOND, FETCH_SUCCEEDED, MAX_CURSORS, OpenCursor,
    cursor_already_open, cursor_exists, cursor_not_open, forward_only, into_count_mismatch,
    no_such_cursor, too_many_cursors,
};
use crate::types::{SqlType, Value};
use std::sync::Arc;

fn invalid_fetch_offset(position: usize) -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        "The offset of FETCH ABSOLUTE or RELATIVE must be an integer.",
    )
    .with_sqlserver_code(16902)
    .at(position)
}

impl Session<'_> {
    fn cursor_nest_level(&self) -> u8 {
        self.batch_context.nest_level
    }

    fn cursor_index(&self, cursor: &CursorName) -> Option<usize> {
        let level = self.cursor_nest_level();
        let name = &cursor.name.text;
        let local = (!cursor.global)
            .then(|| {
                self.tsql_cursors.iter().position(|known| {
                    !known.global && known.nest_level == level && known.named(name)
                })
            })
            .flatten();
        local.or_else(|| {
            self.tsql_cursors
                .iter()
                .position(|known| known.global && known.named(name))
        })
    }

    fn found_cursor(&self, cursor: &CursorName) -> SqlResult<usize> {
        self.cursor_index(cursor)
            .ok_or_else(|| no_such_cursor(&cursor.name.text).at(cursor.name.position))
    }

    fn publish_cursors(&mut self) {
        let states: Vec<CursorState> = self.tsql_cursors.iter().map(Cursor::state).collect();
        if self.settings.sqlserver.cursors != states {
            Arc::make_mut(&mut self.settings).sqlserver.cursors = states;
        }
    }

    pub(in crate::session) fn drop_local_cursors(&mut self, nest_level: u8) {
        let before = self.tsql_cursors.len();
        self.tsql_cursors
            .retain(|cursor| cursor.global || cursor.nest_level != nest_level);
        if self.tsql_cursors.len() != before {
            self.publish_cursors();
        }
    }

    pub(in crate::session) fn run_declare_cursor(
        &mut self,
        declaration: &CursorDeclaration,
    ) -> SqlResult<QueryResult> {
        let level = self.cursor_nest_level();
        let name = &declaration.name.text;
        let exists = self.tsql_cursors.iter().any(|known| {
            known.named(name)
                && known.global == declaration.global
                && (known.global || known.nest_level == level)
        });
        if exists {
            return Err(cursor_exists(name).at(declaration.name.position));
        }
        if self.tsql_cursors.len() >= MAX_CURSORS {
            return Err(too_many_cursors().at(declaration.name.position));
        }
        self.tsql_cursors.push(Cursor {
            name: name.clone(),
            global: declaration.global,
            nest_level: if declaration.global { 0 } else { level },
            scroll: declaration.scroll,
            dynamic: declaration.dynamic,
            query: declaration.query.clone(),
            open: None,
        });
        self.publish_cursors();
        Ok(QueryResult::command("DECLARE CURSOR"))
    }

    pub(in crate::session) fn run_open_cursor(
        &mut self,
        cursor: &CursorName,
    ) -> SqlResult<QueryResult> {
        let index = self.found_cursor(cursor)?;
        let Some(known) = self.tsql_cursors.get(index) else {
            return Err(no_such_cursor(&cursor.name.text));
        };
        if known.open.is_some() {
            return Err(cursor_already_open().at(cursor.name.position));
        }
        let dynamic = known.dynamic;
        let mut query = (*known.query).clone();
        let result = self.run_query_statement(&mut query)?;
        let rows = i32::try_from(result.rows.len()).unwrap_or(i32::MAX);
        if let Some(known) = self.tsql_cursors.get_mut(index) {
            known.open = Some(OpenCursor::new(
                result.columns.unwrap_or_default(),
                result.rows,
            ));
        }
        Arc::make_mut(&mut self.settings).sqlserver.cursor_rows = if dynamic { -1 } else { rows };
        self.publish_cursors();
        Ok(QueryResult::command("OPEN CURSOR"))
    }

    fn fetch_offset(&mut self, offset: &Expr) -> SqlResult<i64> {
        let value = self.evaluate(offset)?;
        match crate::sqlserver_conversion::implicit(value, SqlType::BigInt) {
            Ok(Value::BigInt(offset)) => Ok(offset),
            _ => Err(invalid_fetch_offset(offset.position)),
        }
    }

    fn cursor_move(&mut self, orientation: &FetchOrientation) -> SqlResult<CursorMove> {
        Ok(match orientation {
            FetchOrientation::Next => CursorMove::Next,
            FetchOrientation::Prior => CursorMove::Prior,
            FetchOrientation::First => CursorMove::First,
            FetchOrientation::Last => CursorMove::Last,
            FetchOrientation::Absolute(offset) => CursorMove::Absolute(self.fetch_offset(offset)?),
            FetchOrientation::Relative(offset) => CursorMove::Relative(self.fetch_offset(offset)?),
        })
    }

    pub(in crate::session) fn run_fetch_cursor(
        &mut self,
        cursor: &CursorName,
        orientation: &FetchOrientation,
        into: &[Name],
    ) -> SqlResult<QueryResult> {
        let index = self.found_cursor(cursor)?;
        let step = self.cursor_move(orientation)?;
        let Some(known) = self.tsql_cursors.get_mut(index) else {
            return Err(no_such_cursor(&cursor.name.text));
        };
        let scroll = known.scroll;
        let Some(open) = known.open.as_mut() else {
            return Err(cursor_not_open().at(cursor.name.position));
        };
        if !scroll && step != CursorMove::Next {
            return Err(forward_only(step).at(cursor.name.position));
        }
        if !into.is_empty() && into.len() != open.columns.len() {
            return Err(into_count_mismatch().at(cursor.name.position));
        }
        let row = open.fetch(step).map(<[_]>::to_vec);
        let columns = if into.is_empty() {
            open.columns.clone()
        } else {
            Vec::new()
        };
        let status = if row.is_some() {
            FETCH_SUCCEEDED
        } else {
            FETCH_BEYOND
        };
        Arc::make_mut(&mut self.settings).sqlserver.fetch_status = Some(status);
        if into.is_empty() {
            let rows: Vec<Vec<Value>> = row.into_iter().collect();
            return Ok(QueryResult::rows(
                format!("SELECT {}", rows.len()),
                columns,
                rows,
            ));
        }
        if let Some(row) = row {
            for (variable, value) in into.iter().zip(row) {
                self.assign_variable(&variable.text, value)
                    .map_err(|error| error.at_if_unplaced(variable.position))?;
            }
        }
        Ok(QueryResult::command("FETCH"))
    }

    pub(in crate::session) fn run_close_cursor(
        &mut self,
        cursor: &CursorName,
    ) -> SqlResult<QueryResult> {
        let index = self.found_cursor(cursor)?;
        let closed = self
            .tsql_cursors
            .get_mut(index)
            .and_then(|known| known.open.take());
        if closed.is_none() {
            return Err(cursor_not_open().at(cursor.name.position));
        }
        self.publish_cursors();
        Ok(QueryResult::command("CLOSE CURSOR"))
    }

    pub(in crate::session) fn run_deallocate_cursor(
        &mut self,
        cursor: &CursorName,
    ) -> SqlResult<QueryResult> {
        let index = self.found_cursor(cursor)?;
        self.tsql_cursors.remove(index);
        self.publish_cursors();
        Ok(QueryResult::command("DEALLOCATE CURSOR"))
    }
}
