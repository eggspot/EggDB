use super::UnitLines;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::TableCache;
use crate::plpgsql::frame::{FrameVariable, Slot};
use crate::plpgsql::run::{Run, enter_routine};
use crate::plpgsql::{Block, DeclarationKind, Program};
use crate::plsql::{CompiledPackage, shown_object, variable_shell, with_session};
use crate::routines::RoutineKind;
use crate::settings::Settings;
use crate::store::KeyValueWrite;
use crate::types::Value;
use std::cell::RefCell;
use std::ops::Range;
use std::sync::Arc;

const MAX_ATTACHED_PACKAGES: usize = 256;
const PROGRAM_UNIT_MISSING_CODE: u16 = 6508;

#[derive(Debug, Clone)]
pub(in crate::plpgsql::run) struct AttachedPackage {
    name: String,
    label: String,
    start: usize,
    count: usize,
    generation: u64,
    checked_out: bool,
    compiled: Arc<CompiledPackage>,
}

thread_local! {
    static INITIALIZING: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

struct Initializing;

impl Initializing {
    fn enter(name: &str) -> SqlResult<Self> {
        INITIALIZING.with(|names| {
            let mut names = names.borrow_mut();
            if names.iter().any(|known| known == name) {
                return Err(used_while_initializing(name));
            }
            names.push(name.to_owned());
            Ok(Self)
        })
    }
}

impl Drop for Initializing {
    fn drop(&mut self) {
        INITIALIZING.with(|names| {
            if let Ok(mut names) = names.try_borrow_mut() {
                names.pop();
            }
        });
    }
}

fn used_while_initializing(name: &str) -> SqlError {
    SqlError::new(
        SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
        format!(
            "PL/SQL: could not find program unit being called: \"{}\"",
            shown_object(name)
        ),
    )
    .with_oracle_code(PROGRAM_UNIT_MISSING_CODE)
    .with_detail("The package is used by its own declarations before its initialization finished.")
}

fn too_many_attached() -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("a PL/SQL unit uses at most {MAX_ATTACHED_PACKAGES} packages"),
    )
}

fn parameterless(compiled: &CompiledPackage, own: bool) -> Vec<(String, String)> {
    let mut functions = Vec::new();
    for member in &compiled.members {
        let definition = &member.definition;
        let callable = definition.kind == RoutineKind::Function
            && (own || member.public)
            && definition
                .parameters
                .iter()
                .all(|parameter| !parameter.mode.is_input() || parameter.default.is_some());
        if !callable {
            continue;
        }
        functions.push((
            format!("{}.{}", compiled.label, member.name),
            crate::schemas::written_qualified(&compiled.label, &member.name),
        ));
        if own {
            functions.push((member.name.clone(), member.name.clone()));
        }
    }
    functions
}

fn initialize(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    compiled: &Arc<CompiledPackage>,
) -> SqlResult<()> {
    let _depth = enter_routine()?;
    let _initializing = Initializing::enter(&compiled.name)?;
    let definition = compiled.initialization_definition();
    let label = format!("\"{}\"", shown_object(&compiled.name));
    let mut run = Run::new(writer, tables, settings, &definition, label);
    run.begin_plsql();
    let outcome = run.initialize_package(compiled);
    let outcome = outcome.map_err(|error| run.located(error));
    run.share_packages();
    run.end_plsql();
    if outcome.is_err() {
        with_session(|session| session.discard_package(&compiled.name));
    }
    outcome
}

impl Run<'_> {
    pub(in crate::plpgsql::run) fn attach_packages(&mut self, program: &Program) -> SqlResult<()> {
        if !self.oracle || self.plsql.mirror.is_some() {
            return Ok(());
        }
        let outcome = self.attach_referenced_packages(program);
        if outcome.is_err() {
            self.share_packages();
        }
        outcome
    }

    fn attach_referenced_packages(&mut self, program: &Program) -> SqlResult<()> {
        let own = self
            .definition
            .plsql
            .as_ref()
            .and_then(|plsql| plsql.package.clone());
        if let Some(own) = own
            && let Some(compiled) = crate::plsql::compiled_package(&*self.writer, self.tables, &own)?
        {
            self.attach_package(compiled, true)?;
        }
        for candidate in &program.package_candidates {
            if self
                .plsql
                .packages
                .iter()
                .any(|attached| &attached.label == candidate)
            {
                continue;
            }
            let Some(compiled) =
                crate::plsql::resolve_compiled_package(&*self.writer, self.tables, candidate)?
            else {
                continue;
            };
            if self
                .plsql
                .packages
                .iter()
                .any(|attached| attached.name == compiled.name)
            {
                continue;
            }
            self.attach_package(compiled, false)?;
        }
        self.adopt_packages();
        Ok(())
    }

    fn attach_package(&mut self, compiled: Arc<CompiledPackage>, own: bool) -> SqlResult<()> {
        if self.plsql.packages.len() >= MAX_ATTACHED_PACKAGES {
            return Err(too_many_attached());
        }
        let shells = match with_session(|session| session.package_shells(&compiled)).flatten() {
            Some(shells) => shells,
            None => {
                self.instantiate(&compiled)?;
                match with_session(|session| session.package_shells(&compiled)).flatten() {
                    Some(shells) => shells,
                    None => return Ok(()),
                }
            }
        };
        let label = compiled.label.clone();
        let start = self.frame.variables.len();
        let count = shells.variables.len();
        for (at, mut variable) in shells.variables.into_iter().enumerate() {
            variable.label = (own || at < shells.public).then(|| label.clone());
            variable.qualified_only = !own;
            self.frame.variables.push(variable);
        }
        self.declare_package_types(&compiled, own)?;
        let functions = parameterless(&compiled, own);
        if !functions.is_empty() {
            self.plsql_frame_mut().package_functions.extend(functions);
        }
        if own {
            self.unit = Some(UnitLines::of_package(&compiled.name, &compiled.source));
        }
        self.plsql.packages.push(AttachedPackage {
            name: compiled.name.clone(),
            label,
            start,
            count,
            generation: shells.generation,
            checked_out: false,
            compiled,
        });
        Ok(())
    }

    fn declare_package_types(&mut self, compiled: &CompiledPackage, own: bool) -> SqlResult<()> {
        for declaration in compiled.declarations(own) {
            let DeclarationKind::Type(declared) = &declaration.kind else {
                continue;
            };
            if own {
                self.declare_type(&declaration.name, declared)?;
            } else {
                self.plsql
                    .types
                    .push((declaration.name.clone(), declared.clone()));
            }
            let qualified = format!("{}.{}", compiled.label, declaration.name);
            self.declare_type(&qualified, declared)?;
        }
        Ok(())
    }

    fn instantiate(&mut self, compiled: &Arc<CompiledPackage>) -> SqlResult<()> {
        self.share_packages();
        let outcome = self.with_writer(|writer, tables, settings| {
            initialize(writer, tables, settings, compiled)
        });
        self.adopt_packages();
        outcome
    }

    fn initialize_package(&mut self, compiled: &Arc<CompiledPackage>) -> SqlResult<()> {
        let specification: Arc<str> = Arc::from(compiled.specification.text.as_str());
        self.unit = Some(UnitLines::of_package(&compiled.name, &specification));
        self.declare_package_items(&compiled.specification.block)?;
        let public = self.frame.variables.len();
        if let Some(body) = &compiled.body {
            self.unit = Some(UnitLines::of_package(&compiled.name, &compiled.source));
            self.declare_package_items(&body.block)?;
        }
        let count = self.frame.variables.len();
        for variable in &mut self.frame.variables {
            variable.label = Some(compiled.label.clone());
            if matches!(variable.slot, Slot::Cursor(Some(_))) {
                variable.value = Value::Text(format!("{}.{}", compiled.label, variable.name));
            }
        }
        let shells: Vec<FrameVariable> = self.frame.variables.iter().map(variable_shell).collect();
        let installed = with_session(|session| {
            session.install_package(Arc::clone(compiled), shells, public)
        });
        if let Some(generation) = installed.transpose()? {
            self.plsql.packages.push(AttachedPackage {
                name: compiled.name.clone(),
                label: compiled.label.clone(),
                start: 0,
                count,
                generation,
                checked_out: true,
                compiled: Arc::clone(compiled),
            });
        }
        let functions = parameterless(compiled, true);
        if !functions.is_empty() {
            self.plsql_frame_mut().package_functions.extend(functions);
        }
        self.declare_oracle_builtins();
        let Some(body) = &compiled.body else {
            return Ok(());
        };
        if body.block.steps.is_empty() {
            return Ok(());
        }
        let initialization = Block {
            label: None,
            declarations: Vec::new(),
            steps: body.block.steps.clone(),
            handlers: body.block.handlers.clone(),
            position: body.block.position,
        };
        self.block(&initialization).map(drop)
    }

    fn declare_package_items(&mut self, block: &Block) -> SqlResult<()> {
        for declaration in &block.declarations {
            if matches!(declaration.kind, DeclarationKind::Subprogram(_)) {
                continue;
            }
            if let Err(error) = self.declare_one(block, declaration) {
                self.fail_at(declaration.position, || {
                    "during package initialization".to_owned()
                });
                return Err(error);
            }
        }
        Ok(())
    }

    pub(in crate::plpgsql::run) fn share_packages(&mut self) {
        if !self.plsql.packages.iter().any(|attached| attached.checked_out) {
            return;
        }
        let variables = &mut self.frame.variables;
        let cursors = &mut self.cursors;
        let packages = &mut self.plsql.packages;
        with_session(|session| {
            for attached in packages.iter_mut().filter(|attached| attached.checked_out) {
                attached.checked_out = false;
                let Some(instance) = session.instance_mut(&attached.name, attached.generation)
                else {
                    continue;
                };
                let range = variables.iter_mut().skip(attached.start).take(attached.count);
                for (stored, variable) in instance.variables.iter_mut().zip(range) {
                    stored.value = std::mem::replace(&mut variable.value, Value::Null);
                }
                let prefix = format!("{}.", attached.label);
                if cursors.iter().any(|cursor| cursor.name.starts_with(&prefix)) {
                    let (owned, kept): (Vec<_>, Vec<_>) = std::mem::take(cursors)
                        .into_iter()
                        .partition(|cursor| cursor.name.starts_with(&prefix));
                    *cursors = kept;
                    instance.cursors.extend(owned);
                }
            }
        });
    }

    pub(in crate::plpgsql::run) fn adopt_packages(&mut self) {
        if self.plsql.packages.iter().all(|attached| attached.checked_out) {
            return;
        }
        let variables = &mut self.frame.variables;
        let cursors = &mut self.cursors;
        let packages = &mut self.plsql.packages;
        with_session(|session| {
            for attached in packages.iter_mut().filter(|attached| !attached.checked_out) {
                let Some(instance) = session.instance_mut(&attached.name, attached.generation)
                else {
                    continue;
                };
                attached.checked_out = true;
                let range = variables.iter_mut().skip(attached.start).take(attached.count);
                for (stored, variable) in instance.variables.iter_mut().zip(range) {
                    if stored.name == variable.name {
                        variable.value = std::mem::replace(&mut stored.value, Value::Null);
                    }
                }
                cursors.append(&mut instance.cursors);
            }
        });
    }

    pub(in crate::plpgsql::run) fn package_ranges(&self) -> Vec<Range<usize>> {
        self.plsql
            .packages
            .iter()
            .map(|attached| attached.start..attached.start + attached.count)
            .collect()
    }

    pub(in crate::plpgsql::run) fn inherited_packages(&self) -> Vec<AttachedPackage> {
        self.plsql
            .packages
            .iter()
            .map(|attached| AttachedPackage {
                checked_out: false,
                ..attached.clone()
            })
            .collect()
    }

    pub(in crate::plpgsql::run) fn close_cursors_from(&mut self, mark: usize) {
        if self.plsql.packages.is_empty() {
            self.cursors.truncate(mark);
            return;
        }
        let prefixes: Vec<String> = self
            .plsql
            .packages
            .iter()
            .map(|attached| format!("{}.", attached.label))
            .collect();
        let mut at = mark.min(self.cursors.len());
        while at < self.cursors.len() {
            let owned = prefixes
                .iter()
                .any(|prefix| self.cursors[at].name.starts_with(prefix.as_str()));
            if owned {
                at += 1;
            } else {
                self.cursors.remove(at);
            }
        }
    }

    pub(in crate::plpgsql::run) fn package_exception_code(&self, identity: &str) -> Option<u16> {
        let (label, name) = identity.split_once('.')?;
        self.plsql
            .packages
            .iter()
            .find(|attached| attached.label == label)
            .and_then(|attached| attached.compiled.exception_code(name))
    }
}
