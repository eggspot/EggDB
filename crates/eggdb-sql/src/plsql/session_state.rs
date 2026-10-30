use super::CompiledPackage;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::plpgsql::{FrameVariable, OpenCursor};
use crate::types::Value;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

pub(crate) const DEFAULT_OUTPUT_BUFFER_BYTES: usize = 20_000;
pub(crate) const UNLIMITED_OUTPUT_BUFFER_BYTES: usize = 1 << 20;
const MIN_OUTPUT_BUFFER_BYTES: usize = 2_000;
const MAX_OUTPUT_LINE_BYTES: usize = 32_767;
const PACKAGE_ERROR_CODE: u16 = 20_000;
const MAX_PACKAGE_INSTANCES: usize = 1_000;

pub(crate) type SharedPlsqlSession = Arc<Mutex<PlsqlSession>>;

#[derive(Debug, Default)]
pub(crate) struct PlsqlSession {
    pub(crate) output: ServerOutput,
    packages: Vec<PackageInstance>,
    generations: u64,
}

#[derive(Debug)]
pub(crate) struct PackageInstance {
    compiled: Arc<CompiledPackage>,
    generation: u64,
    public: usize,
    pub(crate) variables: Vec<FrameVariable>,
    pub(crate) cursors: Vec<OpenCursor>,
}

pub(crate) struct PackageShells {
    pub(crate) generation: u64,
    pub(crate) variables: Vec<FrameVariable>,
    pub(crate) public: usize,
}

pub(crate) fn variable_shell(variable: &FrameVariable) -> FrameVariable {
    let mut shell = FrameVariable::new(variable.name.clone(), variable.slot.clone(), Value::Null);
    shell.constant = variable.constant;
    shell.not_null = variable.not_null;
    shell
}

fn too_many_packages() -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        "too many PL/SQL packages are in use in this session",
    )
    .with_hint(format!(
        "EggDB keeps the state of at most {MAX_PACKAGE_INSTANCES} packages per session."
    ))
}

impl PlsqlSession {
    pub(crate) fn package_shells(&mut self, compiled: &Arc<CompiledPackage>) -> Option<PackageShells> {
        let at = self
            .packages
            .iter()
            .position(|instance| instance.compiled.name == compiled.name)?;
        if !Arc::ptr_eq(&self.packages[at].compiled, compiled) {
            if self.packages[at].compiled.stored != compiled.stored {
                self.packages.remove(at);
                return None;
            }
            self.packages[at].compiled = Arc::clone(compiled);
        }
        let instance = &self.packages[at];
        Some(PackageShells {
            generation: instance.generation,
            variables: instance.variables.iter().map(variable_shell).collect(),
            public: instance.public,
        })
    }

    pub(crate) fn install_package(
        &mut self,
        compiled: Arc<CompiledPackage>,
        variables: Vec<FrameVariable>,
        public: usize,
    ) -> SqlResult<u64> {
        self.discard_package(&compiled.name);
        if self.packages.len() >= MAX_PACKAGE_INSTANCES {
            return Err(too_many_packages());
        }
        self.generations += 1;
        self.packages.push(PackageInstance {
            compiled,
            generation: self.generations,
            public,
            variables,
            cursors: Vec::new(),
        });
        Ok(self.generations)
    }

    pub(crate) fn discard_package(&mut self, name: &str) {
        self.packages
            .retain(|instance| instance.compiled.name != name);
    }

    pub(crate) fn instance_mut(&mut self, name: &str, generation: u64) -> Option<&mut PackageInstance> {
        self.packages
            .iter_mut()
            .find(|instance| instance.generation == generation && instance.compiled.name == name)
    }
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
