use crate::ast::Statement;
use crate::error::{SqlError, SqlState};
use crate::session::OutputColumn;
use crate::types::Value;

pub(crate) const MAX_CURSORS: usize = 1_000;
pub(crate) const FETCH_SUCCEEDED: i32 = 0;
pub(crate) const FETCH_BEYOND: i32 = -1;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CursorState {
    pub(crate) name: String,
    pub(crate) global: bool,
    pub(crate) nest_level: u8,
    pub(crate) status: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OpenCursor {
    pub(crate) columns: Vec<OutputColumn>,
    pub(crate) rows: Vec<Vec<Value>>,
    position: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Cursor {
    pub(crate) name: String,
    pub(crate) global: bool,
    pub(crate) nest_level: u8,
    pub(crate) scroll: bool,
    pub(crate) dynamic: bool,
    pub(crate) query: Box<Statement>,
    pub(crate) open: Option<OpenCursor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CursorMove {
    Next,
    Prior,
    First,
    Last,
    Absolute(i64),
    Relative(i64),
}

impl CursorMove {
    pub(crate) fn shown(self) -> &'static str {
        match self {
            Self::Next => "NEXT",
            Self::Prior => "PRIOR",
            Self::First => "FIRST",
            Self::Last => "LAST",
            Self::Absolute(_) => "ABSOLUTE",
            Self::Relative(_) => "RELATIVE",
        }
    }
}

impl OpenCursor {
    pub(crate) fn new(columns: Vec<OutputColumn>, rows: Vec<Vec<Value>>) -> Self {
        Self {
            columns,
            rows,
            position: 0,
        }
    }

    fn length(&self) -> i64 {
        i64::try_from(self.rows.len()).unwrap_or(i64::MAX)
    }

    fn place(&mut self, target: i64) -> Option<&[Value]> {
        let after = self.length().saturating_add(1);
        let clamped = target.clamp(0, after);
        self.position = usize::try_from(clamped).unwrap_or(self.rows.len() + 1);
        if clamped == 0 || clamped == after {
            return None;
        }
        self.rows
            .get(self.position.saturating_sub(1))
            .map(Vec::as_slice)
    }

    pub(crate) fn fetch(&mut self, step: CursorMove) -> Option<&[Value]> {
        let current = i64::try_from(self.position).unwrap_or(i64::MAX);
        let length = self.length();
        let target = match step {
            CursorMove::Next => current.saturating_add(1),
            CursorMove::Prior => current.saturating_sub(1),
            CursorMove::First => 1,
            CursorMove::Last => length,
            CursorMove::Absolute(0) => 0,
            CursorMove::Absolute(offset) if offset > 0 => offset,
            CursorMove::Absolute(offset) => length.saturating_add(1).saturating_add(offset),
            CursorMove::Relative(offset) => current.saturating_add(offset),
        };
        self.place(target)
    }
}

impl Cursor {
    pub(crate) fn status(&self) -> i32 {
        match &self.open {
            None => -1,
            Some(_) if self.dynamic => 1,
            Some(open) if open.rows.is_empty() => 0,
            Some(_) => 1,
        }
    }

    pub(crate) fn state(&self) -> CursorState {
        CursorState {
            name: self.name.clone(),
            global: self.global,
            nest_level: self.nest_level,
            status: self.status(),
        }
    }

    pub(crate) fn named(&self, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name)
    }
}

pub(crate) fn cursor_status(
    states: &[CursorState],
    scope: &str,
    name: &str,
    nest_level: u8,
) -> Option<i32> {
    let found = match scope.trim().to_ascii_lowercase().as_str() {
        "local" => states.iter().find(|state| {
            !state.global && state.nest_level == nest_level && state.name.eq_ignore_ascii_case(name)
        }),
        "global" => states
            .iter()
            .find(|state| state.global && state.name.eq_ignore_ascii_case(name)),
        "variable" => None,
        _ => return None,
    };
    Some(found.map_or(-3, |state| state.status))
}

pub(crate) fn cursor_exists(name: &str) -> SqlError {
    SqlError::new(
        SqlState::DUPLICATE_CURSOR,
        format!("A cursor with the name '{name}' already exists."),
    )
    .with_sqlserver_code(16915)
}

pub(crate) fn no_such_cursor(name: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_CURSOR_NAME,
        format!("A cursor with the name '{name}' does not exist."),
    )
    .with_sqlserver_code(16916)
}

pub(crate) fn cursor_already_open() -> SqlError {
    SqlError::new(SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE, "The cursor is already open.")
        .with_sqlserver_code(16905)
}

pub(crate) fn cursor_not_open() -> SqlError {
    SqlError::new(SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE, "Cursor is not open.")
        .with_sqlserver_code(16917)
}

pub(crate) fn forward_only(step: CursorMove) -> SqlError {
    SqlError::new(
        SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
        format!(
            "fetch: The fetch type {} cannot be used with forward only cursors.",
            step.shown()
        ),
    )
    .with_sqlserver_code(16911)
}

pub(crate) fn into_count_mismatch() -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        "Cursorfetch: The number of variables declared in the INTO list must match that of selected columns.",
    )
    .with_sqlserver_code(16924)
}

pub(crate) fn too_many_cursors() -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("A connection can have at most {MAX_CURSORS} Transact-SQL cursors at a time."),
    )
}

pub(crate) fn invalid_cursor_scope(scope: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!(
            "The scope '{scope}' of CURSOR_STATUS is not valid: use 'local', 'global' or 'variable'."
        ),
    )
    .with_sqlserver_code(16956)
}
