use super::{
    PlsqlPackage, StoredPackage, anonymous_block_definition, compile_package, package_candidates,
    parse_package, shown_object,
};
use crate::dialect::Dialect;
use crate::encoding::{decode_package, package_key};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::TableCache;
use crate::lexer::tokenize_in;
use crate::plpgsql::{Declaration, DeclarationKind, Program};
use crate::routines::{NamedRoutines, RoutineDefinition};
use crate::schemas::{base_name, is_qualified, normalized, qualified, schema_of};
use crate::store::KeyValueRead;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

const CACHED_PACKAGES: usize = 1_024;
const BODY_MISSING_CODE: u16 = 4067;

#[derive(Default)]
pub(crate) struct PackageCache {
    entries: HashMap<String, CachedPackage>,
}

struct CachedPackage {
    bytes: Option<Vec<u8>>,
    checked_at: Option<u64>,
    compiled: Option<Arc<CompiledPackage>>,
}

#[derive(Debug)]
pub(crate) struct CompiledPackage {
    pub(crate) name: String,
    pub(crate) label: String,
    pub(crate) stored: StoredPackage,
    pub(crate) specification: PlsqlPackage,
    pub(crate) body: Option<PlsqlPackage>,
    pub(crate) source: Arc<str>,
    pub(crate) members: Vec<PackageMember>,
    routines: Vec<PackageRoutines>,
}

#[derive(Debug)]
pub(crate) struct PackageMember {
    pub(crate) name: String,
    pub(crate) public: bool,
    pub(crate) definition: Arc<RoutineDefinition>,
    pub(crate) program: Arc<Program>,
}

#[derive(Debug)]
struct PackageRoutines {
    name: String,
    all: Arc<NamedRoutines>,
    public: Option<Arc<NamedRoutines>>,
}

fn declares_subprogram(declarations: &[Declaration], name: &str) -> bool {
    declarations.iter().any(|declaration| {
        declaration.name == name && matches!(declaration.kind, DeclarationKind::Subprogram(_))
    })
}

fn declared_in_specification(
    specification: &PlsqlPackage,
    name: &str,
    definition: &RoutineDefinition,
) -> bool {
    let identity = definition.identity();
    specification.block.declarations.iter().any(|declaration| {
        declaration.name == name
            && matches!(&declaration.kind, DeclarationKind::Subprogram(declared)
                if declared.definition.kind == definition.kind
                    && declared.definition.identity() == identity)
    })
}

fn body_missing(package: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!(
            "not executed, package body \"{}\" does not exist",
            shown_object(package)
        ),
    )
    .with_oracle_code(BODY_MISSING_CODE)
}

fn members_of(
    internal: &str,
    body: &PlsqlPackage,
    specification: &PlsqlPackage,
) -> SqlResult<Vec<PackageMember>> {
    let tokens = tokenize_in(&body.text, Dialect::Oracle)?;
    let candidates = package_candidates(&tokens);
    let mut members = Vec::new();
    for declaration in &body.block.declarations {
        let DeclarationKind::Subprogram(subprogram) = &declaration.kind else {
            continue;
        };
        let Some(program) = &subprogram.program else {
            continue;
        };
        let mut definition = subprogram.definition.clone();
        definition.name = format!("{internal}.{}", declaration.name);
        if let Some(plsql) = definition.plsql.as_mut() {
            plsql.package = Some(internal.to_owned());
        }
        let public = declared_in_specification(specification, &declaration.name, &definition);
        let mut program = program.clone();
        program.package_candidates.clone_from(&candidates);
        members.push(PackageMember {
            name: declaration.name.clone(),
            public,
            definition: Arc::new(definition),
            program: Arc::new(program),
        });
    }
    Ok(members)
}

fn routines_of(internal: &str, members: &[PackageMember]) -> Vec<PackageRoutines> {
    let mut routines: Vec<PackageRoutines> = Vec::new();
    for member in members {
        if routines.iter().any(|known| known.name == member.name) {
            continue;
        }
        let named = |public_only: bool| NamedRoutines {
            name: format!("{internal}.{}", member.name),
            routines: members
                .iter()
                .filter(|other| other.name == member.name && (other.public || !public_only))
                .map(|other| Arc::clone(&other.definition))
                .collect(),
        };
        let public = named(true);
        routines.push(PackageRoutines {
            name: member.name.clone(),
            all: Arc::new(named(false)),
            public: (!public.routines.is_empty()).then(|| Arc::new(public)),
        });
    }
    routines
}

fn compile_stored(internal: &str, stored: StoredPackage) -> SqlResult<CompiledPackage> {
    let parsed = parse_package(&stored.specification)?;
    let specification = compile_package(&parsed.text, None)?;
    let body = match &stored.body {
        Some(text) => {
            let parsed = parse_package(text)?;
            Some(compile_package(&parsed.text, Some(&specification.block))?)
        }
        None => None,
    };
    let members = match &body {
        Some(body) => members_of(internal, body, &specification)?,
        None => Vec::new(),
    };
    let routines = routines_of(internal, &members);
    let source = Arc::from(
        body.as_ref()
            .map_or(specification.text.as_str(), |body| body.text.as_str()),
    );
    Ok(CompiledPackage {
        name: internal.to_owned(),
        label: base_name(internal).to_owned(),
        stored,
        specification,
        body,
        source,
        members,
        routines,
    })
}

impl CompiledPackage {
    pub(crate) fn declarations(&self, private: bool) -> impl Iterator<Item = &Declaration> {
        let body = self
            .body
            .as_ref()
            .filter(|_| private)
            .map(|body| body.block.declarations.iter())
            .into_iter()
            .flatten();
        self.specification.block.declarations.iter().chain(body)
    }

    pub(crate) fn exception_code(&self, name: &str) -> Option<u16> {
        self.declarations(true).find_map(|declaration| match &declaration.kind {
            DeclarationKind::Exception { code } if declaration.name == name => *code,
            _ => None,
        })
    }

    pub(crate) fn initialization_definition(&self) -> RoutineDefinition {
        let mut definition = anonymous_block_definition(&self.source);
        definition.name = self.name.clone();
        if let Some(plsql) = definition.plsql.as_mut() {
            plsql.package = Some(self.name.clone());
        }
        definition
    }

    fn routines_named(&self, member: &str, private: bool) -> SqlResult<Option<Arc<NamedRoutines>>> {
        let Some(found) = self.routines.iter().find(|known| known.name == member) else {
            if self.body.is_none()
                && declares_subprogram(&self.specification.block.declarations, member)
            {
                return Err(body_missing(&self.name));
            }
            return Ok(None);
        };
        Ok(if private {
            Some(Arc::clone(&found.all))
        } else {
            found.public.clone()
        })
    }
}

pub(crate) fn compiled_package(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    internal: &str,
) -> SqlResult<Option<Arc<CompiledPackage>>> {
    let version = reader.committed_version();
    let cache = &mut tables.routines.packages.entries;
    if let Some(cached) = cache.get(internal)
        && version.is_some()
        && cached.checked_at == version
    {
        return Ok(cached.compiled.clone());
    }
    let bytes = reader.get(&package_key(internal))?;
    if let Some(cached) = cache.get_mut(internal)
        && cached.bytes == bytes
    {
        cached.checked_at = version;
        return Ok(cached.compiled.clone());
    }
    let compiled = match &bytes {
        Some(bytes) => Some(Arc::new(compile_stored(
            internal,
            decode_package(internal, bytes)?,
        )?)),
        None => None,
    };
    if cache.len() >= CACHED_PACKAGES {
        cache.clear();
    }
    cache.insert(
        internal.to_owned(),
        CachedPackage {
            bytes,
            checked_at: version,
            compiled: compiled.clone(),
        },
    );
    Ok(compiled)
}

pub(crate) fn resolve_compiled_package(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    written: &str,
) -> SqlResult<Option<Arc<CompiledPackage>>> {
    let plain = normalized(written);
    if is_qualified(plain) || plain.len() != written.len() {
        return compiled_package(reader, tables, plain);
    }
    for schema in crate::schemas::relation_path(reader)? {
        if let Some(found) = compiled_package(reader, tables, &qualified(&schema, written))? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

pub(crate) fn member_program(
    tables: &TableCache,
    definition: &Arc<RoutineDefinition>,
) -> Option<Arc<Program>> {
    let package = definition.plsql.as_ref()?.package.as_deref()?;
    let compiled = tables.routines.packages.entries.get(package)?.compiled.as_ref()?;
    compiled
        .members
        .iter()
        .find(|member| Arc::ptr_eq(&member.definition, definition))
        .map(|member| Arc::clone(&member.program))
}

pub(crate) fn current_package_routines(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    name: &str,
) -> SqlResult<Option<Arc<NamedRoutines>>> {
    if is_qualified(name) {
        return Ok(None);
    }
    let Some(package) = current_package() else {
        return Ok(None);
    };
    match compiled_package(reader, tables, &package)? {
        Some(compiled) => compiled.routines_named(name, true),
        None => Ok(None),
    }
}

pub(crate) fn package_member_routines(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    name: &str,
) -> SqlResult<Option<Arc<NamedRoutines>>> {
    if !is_qualified(name) {
        return Ok(None);
    }
    let Some(compiled) = resolve_compiled_package(reader, tables, schema_of(name))? else {
        return Ok(None);
    };
    let private = current_package().as_deref() == Some(compiled.name.as_str());
    compiled.routines_named(base_name(name), private)
}

thread_local! {
    static PACKAGE_SCOPES: RefCell<Vec<Option<String>>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn enter_package_scope(package: Option<String>) {
    PACKAGE_SCOPES.with(|scopes| {
        if let Ok(mut scopes) = scopes.try_borrow_mut() {
            scopes.push(package);
        }
    });
}

pub(crate) fn leave_package_scope() {
    PACKAGE_SCOPES.with(|scopes| {
        if let Ok(mut scopes) = scopes.try_borrow_mut() {
            scopes.pop();
        }
    });
}

pub(crate) fn current_package() -> Option<String> {
    PACKAGE_SCOPES.with(|scopes| {
        scopes
            .try_borrow()
            .ok()
            .and_then(|scopes| scopes.last().cloned().flatten())
    })
}
