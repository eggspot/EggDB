use crate::native::{Cell, Failure, Outcome, Session};

pub(crate) const SQL_C_DEFAULT: i16 = 99;

#[derive(Debug, Default)]
pub(crate) struct Diagnostics {
    pub(crate) records: Vec<Failure>,
}

impl Diagnostics {
    pub(crate) fn clear(&mut self) {
        self.records.clear();
    }

    pub(crate) fn push(&mut self, failure: Failure) {
        self.records.push(failure);
    }
}

#[derive(Debug, Default)]
pub(crate) struct Environment {
    pub(crate) odbc_version: i32,
    pub(crate) diagnostics: Diagnostics,
}

#[derive(Debug)]
pub(crate) struct ConnectionHandle {
    pub(crate) session: Option<Session>,
    pub(crate) autocommit: bool,
    pub(crate) in_transaction: bool,
    pub(crate) database: String,
    pub(crate) diagnostics: Diagnostics,
}

impl ConnectionHandle {
    pub(crate) fn new() -> Self {
        Self {
            session: None,
            autocommit: true,
            in_transaction: false,
            database: String::new(),
            diagnostics: Diagnostics::default(),
        }
    }

    pub(crate) fn run(&mut self, text: &str, parameters: &[Option<String>]) -> Result<Vec<Outcome>, Failure> {
        let autocommit = self.autocommit;
        let begin = !autocommit && !self.in_transaction;
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| Failure::new("08003", "the connection is not open"))?;
        if begin {
            session.run("BEGIN", &[])?;
            self.in_transaction = true;
        }
        session.run(text, parameters)
    }

    pub(crate) fn run_catalog(&mut self, text: &str, parameters: &[Option<String>]) -> Result<Vec<Outcome>, Failure> {
        self.session
            .as_mut()
            .ok_or_else(|| Failure::new("08003", "the connection is not open"))?
            .run_catalog(text, parameters)
    }

    pub(crate) fn end_transaction(&mut self, commit: bool) -> Result<(), Failure> {
        if !self.in_transaction {
            return Ok(());
        }
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| Failure::new("08003", "the connection is not open"))?;
        self.in_transaction = false;
        session.run(if commit { "COMMIT" } else { "ROLLBACK" }, &[]).map(|_| ())
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct BoundColumn {
    pub(crate) target_type: i16,
    pub(crate) target: *mut u8,
    pub(crate) buffer_length: isize,
    pub(crate) indicator: *mut isize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct BoundParameter {
    pub(crate) value_type: i16,
    pub(crate) value: *const u8,
    pub(crate) buffer_length: isize,
    pub(crate) indicator: *const isize,
}

#[derive(Debug)]
pub(crate) struct StatementHandle {
    pub(crate) connection: *mut ConnectionHandle,
    pub(crate) prepared: Option<String>,
    pub(crate) columns: Vec<String>,
    pub(crate) rows: Vec<Vec<Cell>>,
    pub(crate) next_row: usize,
    pub(crate) current: Option<Vec<Cell>>,
    pub(crate) read_offsets: Vec<usize>,
    pub(crate) row_count: isize,
    pub(crate) bound_columns: Vec<Option<BoundColumn>>,
    pub(crate) parameters: Vec<Option<BoundParameter>>,
    pub(crate) diagnostics: Diagnostics,
}

impl StatementHandle {
    pub(crate) fn new(connection: *mut ConnectionHandle) -> Self {
        Self {
            connection,
            prepared: None,
            columns: Vec::new(),
            rows: Vec::new(),
            next_row: 0,
            current: None,
            read_offsets: Vec::new(),
            row_count: -1,
            bound_columns: Vec::new(),
            parameters: Vec::new(),
            diagnostics: Diagnostics::default(),
        }
    }

    pub(crate) fn close_cursor(&mut self) {
        self.columns.clear();
        self.rows.clear();
        self.next_row = 0;
        self.current = None;
        self.read_offsets.clear();
    }

    pub(crate) fn take_outcomes(&mut self, outcomes: Vec<Outcome>) {
        self.close_cursor();
        self.row_count = outcomes
            .iter()
            .map(|outcome| changed_rows(&outcome.tag))
            .sum::<isize>();
        if let Some(last) = outcomes.into_iter().rev().find(|outcome| !outcome.columns.is_empty()) {
            self.row_count = isize::try_from(last.rows.len()).unwrap_or(isize::MAX);
            self.read_offsets = vec![0; last.columns.len()];
            self.columns = last.columns;
            self.rows = last.rows;
        }
    }

    pub(crate) fn sql_type_of(&self, column: usize) -> i16 {
        for row in &self.rows {
            match row.get(column) {
                Some(Cell::Integer(_)) => return -5,
                Some(Cell::Real(_)) => return 8,
                Some(Cell::Text(_)) => return 12,
                Some(Cell::Blob(_)) => return -3,
                _ => {}
            }
        }
        12
    }
}

fn changed_rows(tag: &str) -> isize {
    let words: Vec<&str> = tag.split(' ').collect();
    match words.first() {
        Some(&("INSERT" | "UPDATE" | "DELETE" | "MERGE")) => words
            .last()
            .and_then(|count| count.parse().ok())
            .unwrap_or(0),
        _ => 0,
    }
}
