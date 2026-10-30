use crate::ast::{
    AlterDomain, AlterDomainAction, AlterTypeAction, AlterType, CreateDomain, CreateType,
    DomainConstraintSpec, Name, TypeSpec,
};
use crate::encoding::{
    Column, TableDefinition, decode_row, decode_user_type, encode_user_type, prefix_end,
    rows_prefix, user_type_key, user_types_prefix,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope, Source};
use crate::execute::TableCache;
use crate::memory::WorkMemory;
use crate::session::QueryResult;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
use crate::types::{Conversion, DefinedKind, DefinedType, DefinedValue, SqlType, Value};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) const MAX_ENUM_LABELS: usize = 100_000;
pub(crate) const MAX_LABEL_BYTES: usize = 63;
pub(crate) const MAX_ATTRIBUTES: usize = crate::encoding::MAX_COLUMNS;
pub(crate) const MAX_DOMAIN_CHECKS: usize = crate::encoding::MAX_CONSTRAINTS;
const MAX_DOMAIN_DEPTH: usize = 32;
const FIRST_USER_TYPE_OID: u32 = 16_384;
const LAST_USER_TYPE_OID: u32 = i32::MAX as u32;
const SCAN_BATCH: usize = 1024;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EnumLabel {
    pub(crate) sort_order: f64,
    pub(crate) label: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Attribute {
    pub(crate) name: String,
    pub(crate) sql_type: SqlType,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DomainCheck {
    pub(crate) name: String,
    pub(crate) expression: String,
    pub(crate) validated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DomainDefinition {
    pub(crate) base: SqlType,
    pub(crate) not_null: bool,
    pub(crate) default: Option<String>,
    pub(crate) checks: Vec<DomainCheck>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TypeBody {
    Enum(Vec<EnumLabel>),
    Composite(Vec<Attribute>),
    Range { subtype: SqlType },
    Domain(DomainDefinition),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UserType {
    pub(crate) oid: u32,
    pub(crate) name: String,
    pub(crate) body: TypeBody,
}

impl UserType {
    pub(crate) fn kind(&self) -> DefinedKind {
        match self.body {
            TypeBody::Enum(_) => DefinedKind::Enum,
            TypeBody::Composite(_) => DefinedKind::Composite,
            TypeBody::Range { .. } => DefinedKind::Range,
            TypeBody::Domain(_) => DefinedKind::Domain,
        }
    }

    pub(crate) fn defined(&self) -> DefinedType {
        DefinedType {
            kind: self.kind(),
            oid: self.oid,
        }
    }

    pub(crate) fn sql_type(&self) -> SqlType {
        SqlType::Defined(self.defined())
    }

    pub(crate) fn labels(&self) -> &[EnumLabel] {
        match &self.body {
            TypeBody::Enum(labels) => labels,
            _ => &[],
        }
    }

    pub(crate) fn attributes(&self) -> &[Attribute] {
        match &self.body {
            TypeBody::Composite(attributes) => attributes,
            _ => &[],
        }
    }

    pub(crate) fn domain(&self) -> Option<&DomainDefinition> {
        match &self.body {
            TypeBody::Domain(domain) => Some(domain),
            _ => None,
        }
    }

    fn mentions(&self, oid: u32) -> bool {
        let names = |sql_type: SqlType| sql_type.defined().is_some_and(|found| found.oid == oid);
        match &self.body {
            TypeBody::Enum(_) => false,
            TypeBody::Composite(attributes) => {
                attributes.iter().any(|attribute| names(attribute.sql_type))
            }
            TypeBody::Range { subtype } => names(*subtype),
            TypeBody::Domain(domain) => names(domain.base),
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct Registry {
    types: Vec<UserType>,
    by_name: HashMap<String, usize>,
    by_oid: HashMap<u32, usize>,
    path: Vec<String>,
}

impl Registry {
    pub(crate) fn load(reader: &dyn KeyValueRead) -> SqlResult<Self> {
        let prefix = user_types_prefix();
        let mut found: Vec<UserType> = Vec::new();
        let mut memory = WorkMemory::new();
        let mut failure = None;
        reader.scan(
            &prefix,
            &prefix_end(&prefix),
            ScanDirection::Forward,
            &mut |key, value| {
                memory.charge(key.len() + value.len() + 64)?;
                let Some(name) = key
                    .get(prefix.len()..)
                    .and_then(|name| std::str::from_utf8(name).ok())
                else {
                    failure = Some(crate::encoding::corrupt("type definition"));
                    return Ok(false);
                };
                match decode_user_type(name, value) {
                    Ok(found_type) => {
                        found.push(found_type);
                        Ok(true)
                    }
                    Err(error) => {
                        failure = Some(error);
                        Ok(false)
                    }
                }
            },
        )?;
        if let Some(error) = failure {
            return Err(error);
        }
        let path = crate::schemas::effective_path(reader)?;
        let mut registry = Self {
            path,
            ..Self::default()
        };
        for user_type in found {
            if registry.by_oid.contains_key(&user_type.oid) {
                return Err(crate::encoding::corrupt("type definition"));
            }
            registry.add(user_type);
        }
        Ok(registry)
    }

    fn add(&mut self, user_type: UserType) {
        let at = self.types.len();
        self.by_name.insert(user_type.name.clone(), at);
        self.by_oid.insert(user_type.oid, at);
        self.types.push(user_type);
    }

    pub(crate) fn types(&self) -> &[UserType] {
        &self.types
    }

    pub(crate) fn named(&self, internal: &str) -> Option<&UserType> {
        self.by_name
            .get(internal)
            .and_then(|&at| self.types.get(at))
    }

    pub(crate) fn of_oid(&self, oid: u32) -> Option<&UserType> {
        self.by_oid.get(&oid).and_then(|&at| self.types.get(at))
    }

    pub(crate) fn resolve(&self, schema: Option<&str>, name: &str) -> Option<&UserType> {
        match schema {
            Some(schema) => self.named(&crate::schemas::qualified(schema, name)),
            None => self
                .path
                .iter()
                .filter(|schema| {
                    schema.as_str() != crate::schemas::CATALOG_SCHEMA
                        && schema.as_str() != crate::schemas::INFORMATION_SCHEMA
                })
                .find_map(|schema| self.named(&crate::schemas::qualified(schema, name))),
        }
    }

    fn resolve_written(&self, name: &str) -> Option<&UserType> {
        let normalized = crate::schemas::normalized(name);
        match normalized.split_once(crate::schemas::QUALIFIER) {
            Some((schema, base)) => self.resolve(Some(schema), base),
            None if normalized.len() != name.len() => self.named(normalized),
            None => self.resolve(None, name),
        }
    }

    pub(crate) fn shown_name(&self, user_type: &UserType) -> String {
        let schema = crate::schemas::schema_of(&user_type.name);
        let base = crate::schemas::base_name(&user_type.name);
        let visible = self.path.iter().any(|entry| entry == schema)
            && self.resolve(None, base).is_some_and(|found| found.oid == user_type.oid);
        if visible {
            crate::pg_keywords::quote_ident(base)
        } else {
            format!(
                "{}.{}",
                crate::pg_keywords::quote_ident(schema),
                crate::pg_keywords::quote_ident(base)
            )
        }
    }

    fn domain_chain(&self, oid: u32) -> Vec<&UserType> {
        let mut chain = Vec::new();
        let mut next = Some(oid);
        while let Some(oid) = next {
            let Some(found) = self.of_oid(oid) else {
                break;
            };
            let Some(domain) = found.domain() else {
                break;
            };
            if chain.len() >= MAX_DOMAIN_DEPTH {
                break;
            }
            chain.push(found);
            next = domain
                .base
                .defined()
                .filter(|base| base.kind == DefinedKind::Domain)
                .map(|base| base.oid);
        }
        chain
    }

    fn unused_oid(&self, name: &str) -> u32 {
        let mut hash: u32 = 0x811c_9dc5;
        for byte in b"type\xFF".iter().chain(name.as_bytes()) {
            hash ^= u32::from(*byte);
            hash = hash.wrapping_mul(0x0100_0193);
        }
        let span = u64::from(LAST_USER_TYPE_OID - FIRST_USER_TYPE_OID) + 1;
        let mut oid = FIRST_USER_TYPE_OID + (u64::from(hash) % span) as u32;
        while self.by_oid.contains_key(&oid) {
            oid = if oid >= LAST_USER_TYPE_OID {
                FIRST_USER_TYPE_OID
            } else {
                oid + 1
            };
        }
        oid
    }
}

thread_local! {
    static FRAMES: RefCell<Vec<Option<Arc<Registry>>>> = const { RefCell::new(Vec::new()) };
    static WANTED: Cell<bool> = const { Cell::new(false) };
}

pub(crate) struct EnteredFrame {
    depth: usize,
}

impl Drop for EnteredFrame {
    fn drop(&mut self) {
        FRAMES.with(|frames| {
            if let Ok(mut frames) = frames.try_borrow_mut() {
                frames.truncate(self.depth);
            }
        });
    }
}

pub(crate) fn enter(preloaded: Option<Arc<Registry>>) -> EnteredFrame {
    let depth = FRAMES.with(|frames| match frames.try_borrow_mut() {
        Ok(mut frames) => {
            let depth = frames.len();
            frames.push(preloaded);
            depth
        }
        Err(_) => usize::MAX,
    });
    EnteredFrame { depth }
}

pub(crate) fn enter_if_absent() -> Option<EnteredFrame> {
    let present = FRAMES.with(|frames| frames.try_borrow().is_ok_and(|frames| !frames.is_empty()));
    (!present).then(|| enter(None))
}

pub(crate) fn current() -> Option<Arc<Registry>> {
    FRAMES.with(|frames| {
        frames
            .try_borrow()
            .ok()
            .and_then(|frames| frames.last().cloned().flatten())
    })
}

pub(crate) fn install(registry: Arc<Registry>) {
    FRAMES.with(|frames| {
        if let Ok(mut frames) = frames.try_borrow_mut()
            && let Some(top) = frames.last_mut()
        {
            *top = Some(registry);
        }
    });
}

fn forget() {
    FRAMES.with(|frames| {
        if let Ok(mut frames) = frames.try_borrow_mut()
            && let Some(top) = frames.last_mut()
        {
            *top = None;
        }
    });
}

pub(crate) fn ensure_loaded(reader: &dyn KeyValueRead) -> SqlResult<()> {
    let needed = FRAMES.with(|frames| {
        frames
            .try_borrow()
            .ok()
            .is_some_and(|frames| frames.last().is_some_and(Option::is_none))
    });
    if needed {
        install(Arc::new(Registry::load(reader)?));
    }
    Ok(())
}

pub(crate) fn load(reader: &dyn KeyValueRead) -> SqlResult<Arc<Registry>> {
    Registry::load(reader).map(Arc::new)
}

pub(crate) fn take_wanted() -> bool {
    WANTED.with(|wanted| wanted.replace(false))
}

fn registry() -> SqlResult<Arc<Registry>> {
    current().ok_or_else(|| {
        SqlError::new(
            SqlState::INTERNAL_ERROR,
            "the definitions of user-defined types were not loaded for this statement",
        )
    })
}

fn undefined_type(name: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_OBJECT,
        format!("type \"{name}\" does not exist"),
    )
}

pub(crate) fn resolve_type(
    schema: Option<&str>,
    name: &str,
    position: usize,
) -> SqlResult<SqlType> {
    if schema.is_none_or(|schema| schema == crate::schemas::CATALOG_SCHEMA) {
        if let Some(range) = crate::ranges::builtin_named(name) {
            return Ok(DefinedType::range(range.oid));
        }
        if name == "record" {
            return Ok(SqlType::Defined(DefinedType::RECORD));
        }
    }
    let shown = match schema {
        Some(schema) => format!("{schema}.{name}"),
        None => name.to_owned(),
    };
    let Some(registry) = current() else {
        WANTED.with(|wanted| wanted.set(true));
        return Err(undefined_type(&shown).at(position));
    };
    match registry.resolve(schema, name) {
        Some(found) => Ok(found.sql_type()),
        None => Err(undefined_type(&shown).at(position)),
    }
}

pub(crate) fn type_name(defined: DefinedType) -> String {
    if defined.oid == crate::records::RECORD_OID {
        return "record".to_owned();
    }
    if let Some(range) = crate::ranges::builtin(defined.oid) {
        return range.name.to_owned();
    }
    match current() {
        Some(registry) => match registry.of_oid(defined.oid) {
            Some(found) => registry.shown_name(found),
            None => defined.oid.to_string(),
        },
        None => defined.oid.to_string(),
    }
}

pub(crate) fn formatted_type(oid: u32) -> Option<String> {
    if oid == crate::records::RECORD_OID {
        return Some("record".to_owned());
    }
    if let Some(range) = crate::ranges::builtin(oid) {
        return Some(range.name.to_owned());
    }
    let registry = current()?;
    registry.of_oid(oid).map(|found| registry.shown_name(found))
}

pub(crate) fn base_type_name(oid: u32) -> Option<String> {
    if oid == crate::records::RECORD_OID {
        return Some("record".to_owned());
    }
    if let Some(range) = crate::ranges::builtin(oid) {
        return Some(range.name.to_owned());
    }
    current()?
        .of_oid(oid)
        .map(|found| crate::schemas::base_name(&found.name).to_owned())
}

pub(crate) fn is_visible(oid: u32) -> Option<bool> {
    if crate::ranges::builtin(oid).is_some() || oid == crate::records::RECORD_OID {
        return Some(true);
    }
    let registry = current()?;
    let found = registry.of_oid(oid)?;
    let base = crate::schemas::base_name(&found.name);
    Some(
        registry
            .resolve(None, base)
            .is_some_and(|visible| visible.oid == oid),
    )
}

pub(crate) fn with_type<T>(oid: u32, read: impl FnOnce(&UserType) -> T) -> SqlResult<T> {
    let registry = registry()?;
    registry
        .of_oid(oid)
        .map(read)
        .ok_or_else(|| crate::encoding::corrupt("type definition"))
}

pub(crate) fn enum_label(type_oid: u32, sort_order: f64) -> SqlResult<String> {
    with_type(type_oid, |found| {
        found
            .labels()
            .iter()
            .find(|label| label.sort_order.to_bits() == sort_order.to_bits())
            .map(|label| label.label.clone())
    })?
    .ok_or_else(|| crate::encoding::corrupt("enum value"))
}

pub(crate) fn composite_attributes(oid: u32) -> Option<Vec<Attribute>> {
    current()?
        .of_oid(oid)
        .map(|found| found.attributes().to_vec())
}

pub(crate) fn range_subtype(oid: u32) -> Option<SqlType> {
    if let Some(range) = crate::ranges::builtin(oid) {
        return Some(range.subtype);
    }
    match &current()?.of_oid(oid)?.body {
        TypeBody::Range { subtype } => Some(*subtype),
        _ => None,
    }
}

pub(crate) fn domain_base(oid: u32) -> Option<SqlType> {
    let registry = current()?;
    let chain = registry.domain_chain(oid);
    chain.last().and_then(|last| last.domain()).map(|domain| domain.base)
}

pub(crate) fn domain_default(oid: u32) -> Option<String> {
    let registry = current()?;
    registry
        .domain_chain(oid)
        .into_iter()
        .find_map(|found| found.domain().and_then(|domain| domain.default.clone()))
}

pub(crate) fn storable_base(sql_type: SqlType) -> SqlType {
    match sql_type.defined() {
        Some(defined) if defined.kind == DefinedKind::Domain => {
            domain_base(defined.oid).unwrap_or(sql_type)
        }
        _ => sql_type,
    }
}

pub(crate) fn parse_input(text: &str, defined: DefinedType) -> SqlResult<Value> {
    match defined.kind {
        DefinedKind::Enum => {
            let registry = registry()?;
            let found = registry
                .of_oid(defined.oid)
                .ok_or_else(|| crate::encoding::corrupt("type definition"))?;
            match found.labels().iter().find(|label| label.label == text) {
                Some(label) => Ok(crate::enums::value(defined.oid, label)),
                None => Err(SqlError::new(
                    SqlState::INVALID_TEXT_REPRESENTATION,
                    format!(
                        "invalid input value for enum {}: \"{text}\"",
                        registry.shown_name(found)
                    ),
                )),
            }
        }
        DefinedKind::Composite if defined.oid == crate::records::RECORD_OID => {
            Err(SqlError::not_supported(
                "input of anonymous composite types is not implemented",
            ))
        }
        DefinedKind::Composite => {
            let attributes = composite_attributes(defined.oid)
                .ok_or_else(|| crate::encoding::corrupt("type definition"))?;
            crate::records::parse_text(text, defined.oid, &attributes)
        }
        DefinedKind::Range => {
            let subtype =
                range_subtype(defined.oid).ok_or_else(|| crate::encoding::corrupt("type definition"))?;
            crate::ranges::parse_text(text, defined.oid, subtype)
        }
        DefinedKind::Domain => {
            let base =
                domain_base(defined.oid).ok_or_else(|| crate::encoding::corrupt("type definition"))?;
            let value = crate::types::parse_input(text, base)?;
            check_domain_value(defined.oid, &value)?;
            Ok(value)
        }
    }
}

fn cannot_cast(value: &Value, target: DefinedType) -> SqlError {
    SqlError::new(
        SqlState::CANNOT_COERCE,
        format!(
            "cannot cast type {} to {}",
            value.type_name(),
            type_name(target)
        ),
    )
}

pub(crate) fn convert(value: Value, target: DefinedType, conversion: Conversion) -> SqlResult<Value> {
    if value.is_null() {
        if target.kind == DefinedKind::Domain {
            check_domain_value(target.oid, &value)?;
        }
        return Ok(Value::Null);
    }
    if value.sql_type() == Some(SqlType::Defined(target)) {
        return Ok(value);
    }
    if target.kind == DefinedKind::Domain {
        let base =
            domain_base(target.oid).ok_or_else(|| crate::encoding::corrupt("type definition"))?;
        let converted = crate::types::convert(value, base, conversion)?;
        check_domain_value(target.oid, &converted)?;
        return Ok(converted);
    }
    match (&value, conversion) {
        (Value::Text(text), Conversion::Explicit) => return parse_input(text, target),
        (Value::Bpchar(text), Conversion::Explicit) => {
            return parse_input(crate::types::without_trailing_spaces(text), target);
        }
        _ => {}
    }
    if target.kind == DefinedKind::Composite
        && let Value::Defined(defined) = &value
        && let DefinedValue::Record(record) = defined.as_ref()
    {
        return crate::records::converted(record, target.oid, conversion)
            .ok_or_else(|| cannot_cast(&value, target))?;
    }
    Err(cannot_cast(&value, target))
}

pub(crate) fn comparable(defined: DefinedType, other: SqlType) -> bool {
    let Some(other) = other.defined() else {
        return false;
    };
    defined.oid == other.oid
        || (defined.kind == DefinedKind::Composite && other.kind == DefinedKind::Composite)
}

fn domain_scope_table(name: &str, base: SqlType) -> TableDefinition {
    TableDefinition {
        name: name.to_owned(),
        columns: vec![Column {
            name: "value".to_owned(),
            sql_type: base,
            not_null: false,
            default: None,
            auto_increment: false,
        }],
        ..TableDefinition::default()
    }
}

fn parse_domain_check(check: &DomainCheck) -> SqlResult<crate::ast::Expr> {
    let damaged = || crate::encoding::corrupt("domain constraint");
    let tokens = crate::lexer::tokenize_in(&check.expression, crate::Dialect::PostgreSql)
        .map_err(|_| damaged())?;
    let mut parser = crate::parser::Parser::new(&check.expression, &tokens);
    let expr = parser.expression().map_err(|_| damaged())?;
    if !parser.at_end() {
        return Err(damaged());
    }
    Ok(expr)
}

struct DomainStep {
    shown: String,
    not_null: bool,
    scope: TableDefinition,
    checks: Vec<(String, crate::ast::Expr)>,
}

impl DomainStep {
    fn of(registry: &Registry, found: &UserType) -> SqlResult<Option<Self>> {
        let Some(domain) = found.domain() else {
            return Ok(None);
        };
        let mut checks = Vec::with_capacity(domain.checks.len());
        for check in &domain.checks {
            checks.push((check.name.clone(), parse_domain_check(check)?));
        }
        let base = storable_base(domain.base);
        Ok(Some(Self {
            shown: registry.shown_name(found),
            not_null: domain.not_null,
            scope: domain_scope_table(&found.name, base),
            checks,
        }))
    }

    fn check(&self, value: &Value) -> SqlResult<()> {
        if value.is_null() && self.not_null {
            return Err(SqlError::new(
                SqlState::NOT_NULL_VIOLATION,
                format!("domain {} does not allow null values", self.shown),
            ));
        }
        let row = std::slice::from_ref(value);
        for (name, expr) in &self.checks {
            let evaluator = Evaluator {
                scope: Scope::One(Source {
                    table: &self.scope,
                    label: &self.scope.name,
                    offset: 0,
                }),
                row,
                correlation: None,
            };
            if evaluator.boolean(expr, "CHECK")? == Some(false) {
                return Err(SqlError::new(
                    SqlState::CHECK_VIOLATION,
                    format!(
                        "value for domain {} violates check constraint \"{name}\"",
                        self.shown
                    ),
                ));
            }
        }
        Ok(())
    }
}

fn domain_steps(registry: &Registry, oid: u32) -> SqlResult<Vec<DomainStep>> {
    let mut steps = Vec::new();
    for found in registry.domain_chain(oid) {
        if let Some(step) = DomainStep::of(registry, found)? {
            steps.push(step);
        }
    }
    Ok(steps)
}

pub(crate) fn check_domain_value(oid: u32, value: &Value) -> SqlResult<()> {
    let registry = registry()?;
    for step in domain_steps(&registry, oid)?.iter().rev() {
        step.check(value)?;
    }
    Ok(())
}

pub(crate) struct DomainChecks {
    columns: Vec<(usize, Vec<DomainStep>)>,
}

impl DomainChecks {
    pub(crate) fn of(table: &TableDefinition) -> SqlResult<Option<Self>> {
        if table.domains.is_empty() {
            return Ok(None);
        }
        let registry = registry()?;
        let mut columns = Vec::with_capacity(table.domains.len());
        for entry in &table.domains {
            let mut steps = domain_steps(&registry, entry.oid)?;
            steps.reverse();
            columns.push((entry.column, steps));
        }
        Ok(Some(Self { columns }))
    }

    pub(crate) fn check(&self, row: &[Value]) -> SqlResult<()> {
        for (column, steps) in &self.columns {
            let value = row.get(*column).unwrap_or(&Value::Null);
            for step in steps {
                step.check(value)?;
            }
        }
        Ok(())
    }
}

pub(crate) fn table_needs_registry(table: &TableDefinition) -> bool {
    let stored_text = |text: &str| {
        let lower = text.to_ascii_lowercase();
        lower.contains("::") || lower.contains("cast")
    };
    !table.domains.is_empty()
        || table
            .columns
            .iter()
            .any(|column| column.sql_type.defined().is_some())
        || table
            .columns
            .iter()
            .filter_map(|column| column.default.as_deref())
            .any(stored_text)
        || table.checks.iter().any(|check| stored_text(&check.expression))
}

fn duplicate_type(name: &str) -> SqlError {
    SqlError::new(
        SqlState::DUPLICATE_OBJECT,
        format!(
            "type \"{}\" already exists",
            crate::schemas::base_name(name)
        ),
    )
}

fn is_builtin_type_name(name: &str) -> bool {
    crate::pg_catalog::builtin_type_named(name).is_some()
        || crate::ranges::builtin_named(name).is_some()
        || name == "record"
}

fn check_new_name(writer: &dyn KeyValueRead, registry: &Registry, internal: &str) -> SqlResult<()> {
    if registry.named(internal).is_some()
        || crate::sequences::relation_exists(writer, internal)?
        || (crate::schemas::schema_of(internal) == crate::schemas::DEFAULT_SCHEMA
            && is_builtin_type_name(crate::schemas::base_name(internal)))
    {
        return Err(duplicate_type(internal));
    }
    Ok(())
}

fn check_label(label: &str) -> SqlResult<()> {
    if label.is_empty() || label.len() > MAX_LABEL_BYTES {
        return Err(SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("invalid enum label \"{label}\""),
        )
        .with_detail("Labels must be 63 bytes or less."));
    }
    Ok(())
}

fn check_member_type(sql_type: SqlType, position: usize, what: &str) -> SqlResult<()> {
    match sql_type {
        SqlType::Catalog(_) => Err(SqlError::not_supported(format!(
            "a system catalog type as {what} is not supported yet"
        ))
        .at(position)),
        SqlType::Defined(defined) if defined.kind == DefinedKind::Domain => {
            Err(SqlError::not_supported(format!("a domain as {what} is not supported yet")).at(position))
        }
        SqlType::Defined(defined) if defined.oid == crate::records::RECORD_OID => {
            Err(SqlError::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!("{what} cannot be of type record"),
            )
            .at(position))
        }
        SqlType::Array(element) if !element.is_storable() => Err(SqlError::not_supported(
            format!("{} as {what} is not supported yet", element.type_name()),
        )
        .at(position)),
        _ => Ok(()),
    }
}

fn save(writer: &mut dyn KeyValueWrite, user_type: &UserType) -> SqlResult<()> {
    writer.put(&user_type_key(&user_type.name), &encode_user_type(user_type))
}

pub(crate) fn create_type(
    writer: &mut dyn KeyValueWrite,
    create: &CreateType,
) -> SqlResult<QueryResult> {
    crate::constraints::check_identifier(&create.name)?;
    let internal = crate::schemas::creation_name(writer, &create.name)?;
    let registry = Registry::load(writer)?;
    check_new_name(writer, &registry, &internal).map_err(|error| error.at(create.name.position))?;
    let body = match &create.body {
        TypeSpec::Enum(labels) => {
            if labels.len() > MAX_ENUM_LABELS {
                return Err(SqlError::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    format!("an enum may have at most {MAX_ENUM_LABELS} labels"),
                ));
            }
            let mut listed: Vec<EnumLabel> = Vec::with_capacity(labels.len());
            for (at, label) in labels.iter().enumerate() {
                check_label(label)?;
                if listed.iter().any(|known| known.label == *label) {
                    return Err(SqlError::new(
                        SqlState::DUPLICATE_OBJECT,
                        format!("enum label \"{label}\" used more than once"),
                    ));
                }
                listed.push(EnumLabel {
                    sort_order: (at + 1) as f64,
                    label: label.clone(),
                });
            }
            TypeBody::Enum(listed)
        }
        TypeSpec::Composite(attributes) => {
            if attributes.len() > MAX_ATTRIBUTES {
                return Err(SqlError::new(
                    SqlState::TOO_MANY_COLUMNS,
                    format!("composite types can have at most {MAX_ATTRIBUTES} attributes"),
                ));
            }
            let mut listed: Vec<Attribute> = Vec::with_capacity(attributes.len());
            for (name, sql_type) in attributes {
                crate::constraints::check_identifier(name)?;
                if listed.iter().any(|known| known.name == name.text) {
                    return Err(SqlError::new(
                        SqlState::DUPLICATE_COLUMN,
                        format!("column \"{}\" specified more than once", name.text),
                    )
                    .at(name.position));
                }
                check_member_type(*sql_type, name.position, "the type of an attribute")?;
                listed.push(Attribute {
                    name: name.text.clone(),
                    sql_type: *sql_type,
                });
            }
            TypeBody::Composite(listed)
        }
        TypeSpec::Range { subtype, position } => {
            if !crate::ranges::is_subtype(*subtype) {
                return Err(SqlError::not_supported(format!(
                    "a range of type {} is not supported yet",
                    subtype.name()
                ))
                .at(*position));
            }
            TypeBody::Range { subtype: *subtype }
        }
    };
    let user_type = UserType {
        oid: registry.unused_oid(&internal),
        name: internal,
        body,
    };
    save(writer, &user_type)?;
    forget();
    Ok(QueryResult::command("CREATE TYPE"))
}

fn constraint_name_taken(domain: &DomainDefinition, name: &str) -> bool {
    domain.checks.iter().any(|check| check.name == name)
}

fn generated_check_name(domain: &DomainDefinition, base: &str) -> String {
    let first = format!("{base}_check");
    if !constraint_name_taken(domain, &first) {
        return first;
    }
    let mut number = 1u32;
    loop {
        let candidate = format!("{first}{number}");
        if !constraint_name_taken(domain, &candidate) {
            return candidate;
        }
        number = number.saturating_add(1);
    }
}

fn add_domain_check(
    domain: &mut DomainDefinition,
    internal: &str,
    spec: &DomainConstraintSpec,
    validated: bool,
) -> SqlResult<String> {
    if domain.checks.len() >= MAX_DOMAIN_CHECKS {
        return Err(SqlError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("a domain can have at most {MAX_DOMAIN_CHECKS} constraints"),
        ));
    }
    crate::eval::reject_aggregates(&spec.expr, "check constraints")?;
    if crate::subquery::mentions_subquery(&spec.expr) {
        return Err(SqlError::not_supported("cannot use subquery in check constraint").at(spec.position));
    }
    let scope_table = domain_scope_table(internal, storable_base(domain.base));
    crate::eval::check_columns(
        &spec.expr,
        Scope::One(Source {
            table: &scope_table,
            label: &scope_table.name,
            offset: 0,
        }),
    )?;
    let name = match &spec.name {
        Some(name) => {
            crate::constraints::check_identifier(name)?;
            if constraint_name_taken(domain, &name.text) {
                return Err(SqlError::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!(
                        "constraint \"{}\" for domain \"{}\" already exists",
                        name.text,
                        crate::schemas::base_name(internal)
                    ),
                )
                .at(name.position));
            }
            name.text.clone()
        }
        None => generated_check_name(domain, crate::schemas::base_name(internal)),
    };
    domain.checks.push(DomainCheck {
        name: name.clone(),
        expression: spec.text.trim().to_owned(),
        validated,
    });
    Ok(name)
}

pub(crate) fn create_domain(
    writer: &mut dyn KeyValueWrite,
    create: &CreateDomain,
) -> SqlResult<QueryResult> {
    crate::constraints::check_identifier(&create.name)?;
    let internal = crate::schemas::creation_name(writer, &create.name)?;
    let registry = Registry::load(writer)?;
    check_new_name(writer, &registry, &internal).map_err(|error| error.at(create.name.position))?;
    match create.base {
        SqlType::Defined(defined) if defined.oid == crate::records::RECORD_OID => {
            return Err(SqlError::new(
                SqlState::DATATYPE_MISMATCH,
                "\"record\" is not a valid base type for a domain",
            ));
        }
        SqlType::Catalog(_) => {
            return Err(SqlError::not_supported(
                "a domain over a system catalog type is not supported yet",
            ));
        }
        SqlType::Array(element) if !element.is_storable() => {
            return Err(SqlError::not_supported(format!(
                "a domain over {} is not supported yet",
                element.type_name()
            )));
        }
        _ => {}
    }
    if let Some((expr, _)) = &create.default {
        crate::execute::check_default_expr(expr)?;
    }
    let mut domain = DomainDefinition {
        base: create.base,
        not_null: create.not_null,
        default: create.default.as_ref().map(|(_, text)| text.trim().to_owned()),
        checks: Vec::new(),
    };
    let frame_registry = Arc::new(registry);
    let previous = current();
    install(Arc::clone(&frame_registry));
    let added = create
        .checks
        .iter()
        .try_for_each(|spec| add_domain_check(&mut domain, &internal, spec, true).map(|_| ()));
    match previous {
        Some(previous) => install(previous),
        None => forget(),
    }
    added?;
    let user_type = UserType {
        oid: frame_registry.unused_oid(&internal),
        name: internal,
        body: TypeBody::Domain(domain),
    };
    save(writer, &user_type)?;
    forget();
    Ok(QueryResult::command("CREATE DOMAIN"))
}

pub(crate) struct ColumnUse {
    pub(crate) table: Arc<TableDefinition>,
    pub(crate) column: usize,
}

fn columns_using(
    writer: &dyn KeyValueRead,
    registry: &Registry,
    oid: u32,
) -> SqlResult<Vec<ColumnUse>> {
    let mut uses = Vec::new();
    for table in crate::indexes::every_table(writer)? {
        let table = Arc::new(table);
        for (column, found) in table.columns.iter().enumerate() {
            let typed = found.sql_type.defined().is_some_and(|defined| defined.oid == oid);
            let through_domain = table.domain_of(column).is_some_and(|domain| {
                registry
                    .domain_chain(domain)
                    .iter()
                    .any(|step| step.oid == oid)
            });
            if typed || through_domain {
                uses.push(ColumnUse {
                    table: Arc::clone(&table),
                    column,
                });
            }
        }
    }
    Ok(uses)
}

fn scan_rows(
    reader: &dyn KeyValueRead,
    table: &TableDefinition,
    visit: &mut dyn FnMut(&[Value]) -> SqlResult<()>,
) -> SqlResult<()> {
    let prefix = rows_prefix(table.id);
    let end = prefix_end(&prefix);
    let mut start = prefix;
    loop {
        let mut batch: Vec<Vec<Value>> = Vec::new();
        let mut last = None;
        reader.scan(&start, &end, ScanDirection::Forward, &mut |key, bytes| {
            batch.push(decode_row(bytes, &table.columns)?);
            last = Some(key.to_vec());
            Ok(batch.len() < SCAN_BATCH)
        })?;
        for row in &batch {
            visit(row)?;
        }
        let Some(mut next) = last else {
            return Ok(());
        };
        if batch.len() < SCAN_BATCH {
            return Ok(());
        }
        next.push(0);
        start = next;
    }
}

fn resolve_existing<'r>(registry: &'r Registry, name: &Name) -> SqlResult<&'r UserType> {
    registry
        .resolve_written(&name.text)
        .ok_or_else(|| undefined_type(&crate::schemas::display(&name.text)).at(name.position))
}

pub(crate) fn drop_types(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    names: &[Name],
    if_exists: bool,
    cascade: bool,
    domain_statement: bool,
) -> SqlResult<QueryResult> {
    let tag = if domain_statement {
        "DROP DOMAIN"
    } else {
        "DROP TYPE"
    };
    let mut result = QueryResult::command(tag);
    let registry = Registry::load(writer)?;
    let mut dropping: Vec<UserType> = Vec::with_capacity(names.len());
    for name in names {
        let Some(found) = registry.resolve_written(&name.text) else {
            if if_exists {
                result = result.with_notice(format!(
                    "NOTICE:  type \"{}\" does not exist, skipping",
                    crate::schemas::display(&name.text)
                ));
                continue;
            }
            return Err(undefined_type(&crate::schemas::display(&name.text)).at(name.position));
        };
        let is_domain = found.kind() == DefinedKind::Domain;
        if domain_statement && !is_domain {
            return Err(SqlError::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{}\" is not a domain", registry.shown_name(found)),
            )
            .at(name.position));
        }
        if !domain_statement && is_domain {
            return Err(SqlError::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{}\" is a domain", registry.shown_name(found)),
            )
            .with_hint("Use DROP DOMAIN to remove a domain.")
            .at(name.position));
        }
        if !dropping.iter().any(|known| known.oid == found.oid) {
            dropping.push(found.clone());
        }
    }
    for found in &dropping {
        if let Some(dependent) = registry
            .types()
            .iter()
            .find(|other| other.mentions(found.oid) && !dropping.iter().any(|known| known.oid == other.oid))
        {
            return Err(SqlError::new(
                SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                format!(
                    "cannot drop type {} because other objects depend on it",
                    registry.shown_name(found)
                ),
            )
            .with_detail(format!(
                "type {} depends on type {}",
                registry.shown_name(dependent),
                registry.shown_name(found)
            ))
            .with_hint("Drop the dependent type first."));
        }
        let uses = columns_using(writer, &registry, found.oid)?;
        if let Some(first) = uses.first() {
            if !cascade {
                return Err(SqlError::new(
                    SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop type {} because other objects depend on it",
                        registry.shown_name(found)
                    ),
                )
                .with_detail(format!(
                    "column {} of table {} depends on type {}",
                    first.table.columns[first.column].name,
                    crate::schemas::display(&first.table.name),
                    registry.shown_name(found)
                ))
                .with_hint("Use DROP ... CASCADE to drop the dependent objects too."));
            }
            for found_use in &uses {
                let column = &found_use.table.columns[found_use.column];
                result = result.with_notice(format!(
                    "NOTICE:  drop cascades to column {} of table {}",
                    column.name,
                    crate::schemas::display(&found_use.table.name)
                ));
                let alter = crate::ast::AlterTable {
                    name: Name {
                        text: found_use.table.name.clone(),
                        position: 0,
                    },
                    if_exists: true,
                    actions: vec![crate::ast::AlterAction::DropColumn {
                        name: Name {
                            text: column.name.clone(),
                            position: 0,
                        },
                        if_exists: true,
                        cascade: true,
                    }],
                };
                crate::alter::alter_table(writer, tables, &alter, crate::Dialect::PostgreSql)?;
            }
        }
        writer.delete(&user_type_key(&found.name))?;
    }
    forget();
    Ok(result)
}

fn not_an_enum(registry: &Registry, found: &UserType) -> SqlError {
    SqlError::new(
        SqlState::WRONG_OBJECT_TYPE,
        format!("\"{}\" is not an enum", registry.shown_name(found)),
    )
}

fn not_a_label(label: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("\"{label}\" is not an existing enum label"),
    )
}

fn label_exists(label: &str) -> SqlError {
    SqlError::new(
        SqlState::DUPLICATE_OBJECT,
        format!("enum label \"{label}\" already exists"),
    )
}

fn added_sort_order(labels: &[EnumLabel], neighbor: Option<(&str, bool)>) -> SqlResult<f64> {
    let Some((neighbor, before)) = neighbor else {
        return Ok(labels.last().map_or(1.0, |last| last.sort_order + 1.0));
    };
    let at = labels
        .iter()
        .position(|label| label.label == neighbor)
        .ok_or_else(|| not_a_label(neighbor))?;
    let (low, high) = if before {
        (
            at.checked_sub(1).and_then(|previous| labels.get(previous)),
            labels.get(at),
        )
    } else {
        (labels.get(at), labels.get(at + 1))
    };
    let order = match (low, high) {
        (None, Some(high)) => high.sort_order - 1.0,
        (Some(low), None) => low.sort_order + 1.0,
        (Some(low), Some(high)) => low.sort_order + (high.sort_order - low.sort_order) / 2.0,
        (None, None) => 1.0,
    };
    let collides = |label: Option<&EnumLabel>| label.is_some_and(|label| label.sort_order == order);
    if !order.is_finite() || collides(low) || collides(high) {
        return Err(SqlError::not_supported(
            "no room is left between these enum labels for another one",
        )
        .with_hint("Add the label at another place, or create the type again with every label in order."));
    }
    Ok(order)
}

fn moved_name(
    writer: &dyn KeyValueRead,
    registry: &Registry,
    found: &UserType,
    internal: String,
) -> SqlResult<UserType> {
    check_new_name(writer, registry, &internal)?;
    Ok(UserType {
        oid: found.oid,
        name: internal,
        body: found.body.clone(),
    })
}

fn renamed(
    writer: &mut dyn KeyValueWrite,
    registry: &Registry,
    found: &UserType,
    to: &Name,
    schema: Option<&Name>,
) -> SqlResult<()> {
    let internal = match schema {
        Some(schema) => {
            if !crate::schemas::schema_exists(writer, &schema.text)? {
                return Err(crate::schemas::missing_schema(&schema.text, crate::Dialect::PostgreSql)
                    .at(schema.position));
            }
            crate::schemas::qualified(&schema.text, crate::schemas::base_name(&found.name))
        }
        None => {
            crate::constraints::check_identifier(to)?;
            crate::schemas::qualified(crate::schemas::schema_of(&found.name), &to.text)
        }
    };
    if internal == found.name {
        return Ok(());
    }
    let moved = moved_name(writer, registry, found, internal).map_err(|error| error.at(to.position))?;
    writer.delete(&user_type_key(&found.name))?;
    save(writer, &moved)
}

pub(crate) fn alter_type(
    writer: &mut dyn KeyValueWrite,
    alter: &AlterType,
) -> SqlResult<QueryResult> {
    let registry = Registry::load(writer)?;
    let found = resolve_existing(&registry, &alter.name)?.clone();
    let mut result = QueryResult::command("ALTER TYPE");
    match &alter.action {
        AlterTypeAction::Accepted => {}
        AlterTypeAction::AddValue {
            label,
            if_not_exists,
            neighbor,
        } => {
            let TypeBody::Enum(labels) = &found.body else {
                return Err(not_an_enum(&registry, &found));
            };
            check_label(label)?;
            if labels.iter().any(|known| known.label == *label) {
                if *if_not_exists {
                    result = result.with_notice(format!(
                        "NOTICE:  enum label \"{label}\" already exists, skipping"
                    ));
                    return Ok(result);
                }
                return Err(label_exists(label));
            }
            if labels.len() >= MAX_ENUM_LABELS {
                return Err(SqlError::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    format!("an enum may have at most {MAX_ENUM_LABELS} labels"),
                ));
            }
            let order = added_sort_order(
                labels,
                neighbor
                    .as_ref()
                    .map(|(neighbor, before)| (neighbor.as_str(), *before)),
            )?;
            let mut labels = labels.clone();
            labels.push(EnumLabel {
                sort_order: order,
                label: label.clone(),
            });
            labels.sort_by(|left, right| crate::types::compare_floats(left.sort_order, right.sort_order));
            save(
                writer,
                &UserType {
                    body: TypeBody::Enum(labels),
                    ..found
                },
            )?;
        }
        AlterTypeAction::RenameValue { from, to } => {
            let TypeBody::Enum(labels) = &found.body else {
                return Err(not_an_enum(&registry, &found));
            };
            check_label(to)?;
            if labels.iter().any(|known| known.label == *to) {
                return Err(label_exists(to));
            }
            let mut labels = labels.clone();
            let Some(renamed_label) = labels.iter_mut().find(|known| known.label == *from) else {
                return Err(not_a_label(from));
            };
            renamed_label.label = to.clone();
            save(
                writer,
                &UserType {
                    body: TypeBody::Enum(labels),
                    ..found
                },
            )?;
        }
        AlterTypeAction::RenameTo(to) => renamed(writer, &registry, &found, to, None)?,
        AlterTypeAction::SetSchema(schema) => {
            renamed(writer, &registry, &found, schema, Some(schema))?;
        }
    }
    forget();
    Ok(result)
}

fn require_domain<'r>(registry: &'r Registry, name: &Name) -> SqlResult<&'r UserType> {
    let found = resolve_existing(registry, name)?;
    if found.domain().is_none() {
        return Err(SqlError::new(
            SqlState::WRONG_OBJECT_TYPE,
            format!("\"{}\" is not a domain", registry.shown_name(found)),
        )
        .at(name.position));
    }
    Ok(found)
}

fn validate_domain_users(
    writer: &dyn KeyValueRead,
    registry: &Registry,
    found: &UserType,
    step: &DomainStep,
    check_only: Option<&str>,
) -> SqlResult<()> {
    for found_use in columns_using(writer, registry, found.oid)? {
        let column = found_use.column;
        let table = &found_use.table;
        scan_rows(writer, table, &mut |row| {
            let value = row.get(column).unwrap_or(&Value::Null);
            let outcome = match check_only {
                Some(name) => {
                    let single = DomainStep {
                        shown: step.shown.clone(),
                        not_null: false,
                        scope: step.scope.clone(),
                        checks: step
                            .checks
                            .iter()
                            .filter(|(check, _)| check == name)
                            .cloned()
                            .collect(),
                    };
                    single.check(value)
                }
                None => step.check(value),
            };
            outcome.map_err(|error| {
                let what = if error.state() == SqlState::NOT_NULL_VIOLATION {
                    "contains null values"
                } else {
                    "contains values that violate the new constraint"
                };
                SqlError::new(
                    error.state(),
                    format!(
                        "column \"{}\" of table \"{}\" {what}",
                        table.columns[column].name,
                        crate::schemas::display(&table.name)
                    ),
                )
            })
        })?;
    }
    Ok(())
}

fn missing_domain_constraint(name: &Name, found: &UserType) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_OBJECT,
        format!(
            "constraint \"{}\" of domain \"{}\" does not exist",
            name.text,
            crate::schemas::base_name(&found.name)
        ),
    )
    .at(name.position)
}

pub(crate) fn alter_domain(
    writer: &mut dyn KeyValueWrite,
    alter: &AlterDomain,
) -> SqlResult<QueryResult> {
    let registry = Arc::new(Registry::load(writer)?);
    let found = require_domain(&registry, &alter.name)?.clone();
    let Some(domain) = found.domain().cloned() else {
        return Err(crate::encoding::corrupt("type definition"));
    };
    let mut domain = domain;
    let mut result = QueryResult::command("ALTER DOMAIN");
    let previous = current();
    install(Arc::clone(&registry));
    let outcome = (|| -> SqlResult<()> {
        match &alter.action {
            AlterDomainAction::Accepted => {}
            AlterDomainAction::SetDefault(default) => {
                if let Some((expr, _)) = default {
                    crate::execute::check_default_expr(expr)?;
                }
                domain.default = default.as_ref().map(|(_, text)| text.trim().to_owned());
            }
            AlterDomainAction::SetNotNull(not_null) => {
                if *not_null && !domain.not_null {
                    let step = DomainStep {
                        shown: registry.shown_name(&found),
                        not_null: true,
                        scope: domain_scope_table(&found.name, storable_base(domain.base)),
                        checks: Vec::new(),
                    };
                    validate_domain_users(writer, &registry, &found, &step, None)?;
                }
                domain.not_null = *not_null;
            }
            AlterDomainAction::AddCheck { spec, not_valid } => {
                let name = add_domain_check(&mut domain, &found.name, spec, !*not_valid)?;
                if !*not_valid {
                    let checked = UserType {
                        body: TypeBody::Domain(domain.clone()),
                        ..found.clone()
                    };
                    if let Some(step) = DomainStep::of(&registry, &checked)? {
                        validate_domain_users(writer, &registry, &found, &step, Some(&name))?;
                    }
                }
            }
            AlterDomainAction::DropConstraint { name, if_exists } => {
                let before = domain.checks.len();
                domain.checks.retain(|check| check.name != name.text);
                if domain.checks.len() == before {
                    if !*if_exists {
                        return Err(missing_domain_constraint(name, &found));
                    }
                    result = std::mem::replace(&mut result, QueryResult::command("ALTER DOMAIN"))
                        .with_notice(format!(
                            "NOTICE:  constraint \"{}\" of domain \"{}\" does not exist, skipping",
                            name.text,
                            crate::schemas::base_name(&found.name)
                        ));
                }
            }
            AlterDomainAction::ValidateConstraint(name) => {
                let Some(check) = domain.checks.iter_mut().find(|check| check.name == name.text) else {
                    return Err(missing_domain_constraint(name, &found));
                };
                if !check.validated {
                    check.validated = true;
                    let checked = UserType {
                        body: TypeBody::Domain(domain.clone()),
                        ..found.clone()
                    };
                    if let Some(step) = DomainStep::of(&registry, &checked)? {
                        validate_domain_users(writer, &registry, &found, &step, Some(&name.text))?;
                    }
                }
            }
            AlterDomainAction::RenameConstraint { from, to } => {
                crate::constraints::check_identifier(to)?;
                if constraint_name_taken(&domain, &to.text) {
                    return Err(SqlError::new(
                        SqlState::DUPLICATE_OBJECT,
                        format!(
                            "constraint \"{}\" for domain \"{}\" already exists",
                            to.text,
                            crate::schemas::base_name(&found.name)
                        ),
                    )
                    .at(to.position));
                }
                let Some(check) = domain.checks.iter_mut().find(|check| check.name == from.text) else {
                    return Err(missing_domain_constraint(from, &found));
                };
                check.name = to.text.clone();
            }
            AlterDomainAction::RenameTo(to) => {
                return renamed(writer, &registry, &found, to, None);
            }
            AlterDomainAction::SetSchema(schema) => {
                return renamed(writer, &registry, &found, schema, Some(schema));
            }
        }
        save(
            writer,
            &UserType {
                body: TypeBody::Domain(domain.clone()),
                ..found.clone()
            },
        )
    })();
    match previous {
        Some(previous) => install(previous),
        None => forget(),
    }
    outcome?;
    forget();
    Ok(result)
}

pub(crate) fn every_type(reader: &dyn KeyValueRead) -> SqlResult<Arc<Registry>> {
    match current() {
        Some(registry) => Ok(registry),
        None => load(reader),
    }
}

#[cfg(test)]
mod tests {
    use super::{EnumLabel, added_sort_order};

    fn labels(orders: &[f64]) -> Vec<EnumLabel> {
        orders
            .iter()
            .enumerate()
            .map(|(at, order)| EnumLabel {
                sort_order: *order,
                label: format!("l{at}"),
            })
            .collect()
    }

    #[test]
    fn added_labels_go_between_their_neighbours_without_moving_them() {
        let listed = labels(&[1.0, 2.0, 3.0]);
        assert_eq!(added_sort_order(&listed, None).unwrap(), 4.0);
        assert_eq!(added_sort_order(&listed, Some(("l0", true))).unwrap(), 0.0);
        assert_eq!(added_sort_order(&listed, Some(("l0", false))).unwrap(), 1.5);
        assert_eq!(added_sort_order(&listed, Some(("l2", false))).unwrap(), 4.0);
        assert_eq!(added_sort_order(&listed, Some(("l1", true))).unwrap(), 1.5);
        assert!(added_sort_order(&listed, Some(("missing", true))).is_err());
    }

    #[test]
    fn labels_added_at_one_place_run_out_of_room_with_an_error() {
        let mut listed = labels(&[1.0, 2.0]);
        let mut failed = false;
        for round in 0..200 {
            match added_sort_order(&listed, Some(("l0", false))) {
                Ok(order) => {
                    listed.insert(
                        1,
                        EnumLabel {
                            sort_order: order,
                            label: format!("n{round}"),
                        },
                    );
                }
                Err(error) => {
                    assert_eq!(error.state(), crate::error::SqlState::FEATURE_NOT_SUPPORTED);
                    failed = true;
                    break;
                }
            }
        }
        assert!(failed);
    }
}
