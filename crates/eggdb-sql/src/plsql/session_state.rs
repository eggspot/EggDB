use crate::error::{SqlError, SqlState};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

pub(crate) const DEFAULT_OUTPUT_BUFFER_BYTES: usize = 20_000;
pub(crate) const UNLIMITED_OUTPUT_BUFFER_BYTES: usize = 1 << 20;
const MIN_OUTPUT_BUFFER_BYTES: usize = 2_000;
const MAX_OUTPUT_LINE_BYTES: usize = 32_767;
const PACKAGE_ERROR_CODE: u16 = 20_000;

pub(crate) type SharedPlsqlSession = Arc<Mutex<PlsqlSession>>;

#[derive(Debug, Default)]
pub(crate) struct PlsqlSession {
    pub(crate) output: ServerOutput,
}

#[derive(Debug, Default)]
pub(crate) struct ServerOutput {
    limit: Option<usize>,
    lines: VecDeque<String>,
    partial: String,
    bytes: usize,
}

impl ServerOutput {
    pub(crate) fn enable(&mut self, buffer_bytes: Option<usize>) {
        let limit = buffer_bytes.map_or(UNLIMITED_OUTPUT_BUFFER_BYTES, |bytes| {
            bytes.clamp(MIN_OUTPUT_BUFFER_BYTES, UNLIMITED_OUTPUT_BUFFER_BYTES)
        });
        self.limit = Some(limit);
    }

    pub(crate) fn disable(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn put(&mut self, text: &str) -> Result<(), SqlError> {
        let Some(limit) = self.limit else {
            return Ok(());
        };
        if self.partial.len() + text.len() > MAX_OUTPUT_LINE_BYTES {
            return Err(package_error(format!(
                "ORU-10028: line length overflow, limit of {MAX_OUTPUT_LINE_BYTES} bytes per line"
            )));
        }
        if self.bytes + text.len() > limit {
            return Err(package_error(format!(
                "ORU-10027: buffer overflow, limit of {limit} bytes"
            )));
        }
        self.partial.push_str(text);
        self.bytes += text.len();
        Ok(())
    }

    pub(crate) fn new_line(&mut self) {
        if self.limit.is_some() {
            self.lines.push_back(std::mem::take(&mut self.partial));
        }
    }

    pub(crate) fn next_line(&mut self) -> Option<String> {
        let line = self.lines.pop_front()?;
        self.bytes = self.bytes.saturating_sub(line.len());
        Some(line)
    }

    pub(crate) fn take_lines(&mut self) -> Vec<String> {
        self.bytes = self.partial.len();
        self.lines.drain(..).collect()
    }
}

fn package_error(message: String) -> SqlError {
    SqlError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, message).with_oracle_code(PACKAGE_ERROR_CODE)
}

thread_local! {
    static CURRENT: RefCell<Option<SharedPlsqlSession>> = const { RefCell::new(None) };
}

pub(crate) struct EnteredPlsqlSession(Option<SharedPlsqlSession>);

impl Drop for EnteredPlsqlSession {
    fn drop(&mut self) {
        let previous = self.0.take();
        CURRENT.with(|current| {
            if let Ok(mut current) = current.try_borrow_mut() {
                *current = previous;
            }
        });
    }
}

pub(crate) fn enter_session(session: &SharedPlsqlSession) -> EnteredPlsqlSession {
    let previous = CURRENT.with(|current| match current.try_borrow_mut() {
        Ok(mut current) => current.replace(Arc::clone(session)),
        Err(_) => None,
    });
    EnteredPlsqlSession(previous)
}

pub(crate) fn with_session<T>(use_it: impl FnOnce(&mut PlsqlSession) -> T) -> Option<T> {
    let session = CURRENT.with(|current| current.try_borrow().ok().and_then(|current| current.clone()))?;
    let mut state = session.lock().unwrap_or_else(PoisonError::into_inner);
    Some(use_it(&mut state))
}
