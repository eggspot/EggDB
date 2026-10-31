use super::{
    Dictionary, HIDDEN_SCHEMAS, data_type, flag, number, object_row, optional_number,
    optional_text, precision_and_scale, relation_columns, shown, text,
};
use crate::dialect::Dialect;
use crate::lexer::{Token, TokenKind, swap_uniform_case, tokenize_in};
use crate::pg_catalog::{PackageEntry, RelationEntry, RelationSource, RoutineEntry, TriggerEntry};
use crate::plpgsql::{Declaration, DeclarationKind, TypeDeclaration, VariableType};
use crate::plsql::{DeclaredType, PlsqlPackage, parse_package, unit_source};
use crate::routines::{
    ParameterMode, ReturnType, RoutineDefinition, RoutineKind, RoutineLanguage, RoutineReturns,
};
use crate::triggers::{TriggerAction, TriggerDefinition, TriggerLevel, TriggerTiming};
use crate::types::{SqlType, Value};

pub(super) const PACKAGE: &str = "PACKAGE";
pub(super) const PACKAGE_BODY: &str = "PACKAGE BODY";
pub(super) const TRIGGER: &str = "TRIGGER";
const UNDEFINED_TYPE: &str = "UNDEFINED";
const RECORD_TYPE: &str = "PL/SQL RECORD";
const CONTAINER_ID: i64 = 0;
const PASSED_THROUGH_TYPES: [&str; 15] = [
    "VARCHAR2",
    "NVARCHAR2",
    "CHAR",
    "NCHAR",
    "NUMBER",
    "FLOAT",
    "DATE",
    "CLOB",
    "NCLOB",
    "BLOB",
    "RAW",
    "ROWID",
    "BINARY_FLOAT",
    "BINARY_DOUBLE",
    "BINARY_INTEGER",
];

impl<'a> Dictionary<'a> {
    fn lists(&self, schema: &str) -> bool {
        !HIDDEN_SCHEMAS.contains(&schema) && self.includes(schema)
    }

    fn routines(&self) -> Vec<&'a RoutineEntry> {
        self.snapshot
            .routines
            .iter()
            .filter(|routine| self.lists(&routine.schema))
            .collect()
    }

    fn packages(&self) -> Vec<&'a PackageEntry> {
        self.snapshot
            .packages
            .iter()
            .filter(|package| self.lists(&package.schema))
            .collect()
    }

    fn triggers(&self) -> Vec<(&'a TriggerEntry, &'a RelationEntry)> {
        self.snapshot
            .triggers
            .iter()
            .filter_map(|trigger| {
                self.snapshot
                    .relations
                    .get(trigger.relation)
                    .map(|relation| (trigger, relation))
            })
            .filter(|(_, relation)| self.lists(&relation.schema))
            .collect()
    }
}

pub(super) fn code_object_rows(dictionary: &Dictionary<'_>, rows: &mut Vec<Vec<Value>>) {
    let mut previous: Option<(&str, &str, RoutineKind)> = None;
    for routine in dictionary.routines() {
        let key = (
            routine.schema.as_str(),
            routine.name.as_str(),
            routine.definition.kind,
        );
        if previous == Some(key) {
            continue;
        }
        previous = Some(key);
        rows.push(object_row(
            dictionary,
            &routine.schema,
            &routine.name,
            routine.oid,
            routine.definition.kind.keyword(),
            false,
        ));
    }
    for package in dictionary.packages() {
        rows.push(object_row(
            dictionary,
            &package.schema,
            &package.name,
            package.oid,
            PACKAGE,
            false,
        ));
        if package.stored.body.is_some() {
            rows.push(object_row(
                dictionary,
                &package.schema,
                &package.name,
                package.body_oid,
                PACKAGE_BODY,
                false,
            ));
        }
    }
    for (trigger, relation) in dictionary.triggers() {
        rows.push(object_row(
            dictionary,
            &relation.schema,
            &trigger.definition.name,
            trigger.oid,
            TRIGGER,
            false,
        ));
    }
}

pub(super) fn source_rows(dictionary: &Dictionary<'_>) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for routine in dictionary.routines() {
        if routine.definition.language == RoutineLanguage::PlSql {
            push_source_lines(
                dictionary,
                &mut rows,
                SourceUnit {
                    schema: &routine.schema,
                    name: &routine.name,
                    kind: routine.definition.kind.keyword(),
                    source: &routine.definition.body,
                },
            );
        }
    }
    for package in dictionary.packages() {
        push_source_lines(
            dictionary,
            &mut rows,
            SourceUnit {
                schema: &package.schema,
                name: &package.name,
                kind: PACKAGE,
                source: unit_source(&package.stored.specification),
            },
        );
        if let Some(body) = &package.stored.body {
            push_source_lines(
                dictionary,
                &mut rows,
                SourceUnit {
                    schema: &package.schema,
                    name: &package.name,
                    kind: PACKAGE_BODY,
                    source: unit_source(body),
                },
            );
        }
    }
    for (trigger, relation) in dictionary.triggers() {
        if trigger.definition.dialect == Dialect::Oracle {
            push_source_lines(
                dictionary,
                &mut rows,
                SourceUnit {
                    schema: &relation.schema,
                    name: &trigger.definition.name,
                    kind: TRIGGER,
                    source: unit_source(&trigger.definition.text),
                },
            );
        }
    }
    rows
}

struct SourceUnit<'s> {
    schema: &'s str,
    name: &'s str,
    kind: &'static str,
    source: &'s str,
}

fn push_source_lines(dictionary: &Dictionary<'_>, rows: &mut Vec<Vec<Value>>, unit: SourceUnit<'_>) {
    for (at, line) in unit.source.split_inclusive('\n').enumerate() {
        rows.push(
            dictionary
                .row()
                .set("owner", dictionary.owner(unit.schema))
                .set("name", shown(unit.name))
                .set("type", text(unit.kind))
                .set("line", number(at as i64 + 1))
                .set("text", text(line))
                .set("origin_con_id", number(CONTAINER_ID))
                .done(),
        );
    }
}

struct ParsedPackage<'a> {
    entry: &'a PackageEntry,
    specification: Option<PlsqlPackage>,
}

impl ParsedPackage<'_> {
    fn declarations(&self) -> &[Declaration] {
        match &self.specification {
            Some(specification) => &specification.block.declarations,
            None => &[],
        }
    }

    fn subprograms(&self) -> Vec<(&str, &RoutineDefinition)> {
        self.declarations()
            .iter()
            .filter_map(|declaration| match &declaration.kind {
                DeclarationKind::Subprogram(subprogram) => {
                    Some((declaration.name.as_str(), &subprogram.definition))
                }
                _ => None,
            })
            .collect()
    }

    fn type_named(&self, name: &str) -> Option<&TypeDeclaration> {
        self.declarations()
            .iter()
            .find_map(|declaration| match &declaration.kind {
                DeclarationKind::Type(declared) if declaration.name == name => Some(declared),
                _ => None,
            })
    }
}

fn parsed_packages<'a>(dictionary: &Dictionary<'a>) -> Vec<ParsedPackage<'a>> {
    dictionary
        .packages()
        .into_iter()
        .map(|entry| ParsedPackage {
            entry,
            specification: parse_package(&entry.stored.specification).ok(),
        })
        .collect()
}

fn find_package<'p>(
    packages: &'p [ParsedPackage<'p>],
    schema: Option<&str>,
    name: &str,
) -> Option<&'p ParsedPackage<'p>> {
    packages.iter().find(|package| {
        package.entry.name == name && schema.is_none_or(|schema| package.entry.schema == schema)
    })
}

struct Signature<'p> {
    schema: &'p str,
    object_name: &'p str,
    member: Option<&'p str>,
    object_id: u32,
    subprogram_id: i64,
    overload: Option<usize>,
    object_type: &'static str,
    definition: Option<&'p RoutineDefinition>,
    package: Option<&'p ParsedPackage<'p>>,
}

fn signatures<'p>(
    dictionary: &Dictionary<'p>,
    packages: &'p [ParsedPackage<'p>],
) -> Vec<Signature<'p>> {
    let mut signatures = Vec::new();
    for routine in dictionary.routines() {
        signatures.push(Signature {
            schema: &routine.schema,
            object_name: &routine.name,
            member: None,
            object_id: routine.oid,
            subprogram_id: 1,
            overload: None,
            object_type: routine.definition.kind.keyword(),
            definition: Some(routine.definition.as_ref()),
            package: None,
        });
    }
    for package in packages {
        let entry = package.entry;
        signatures.push(Signature {
            schema: &entry.schema,
            object_name: &entry.name,
            member: None,
            object_id: entry.oid,
            subprogram_id: 0,
            overload: None,
            object_type: PACKAGE,
            definition: None,
            package: Some(package),
        });
        let members = package.subprograms();
        for (at, &(name, definition)) in members.iter().enumerate() {
            let overloaded = members.iter().filter(|(other, _)| *other == name).count() > 1;
            let ordinal = members[..at]
                .iter()
                .filter(|(other, _)| *other == name)
                .count()
                + 1;
            signatures.push(Signature {
                schema: &entry.schema,
                object_name: &entry.name,
                member: Some(name),
                object_id: entry.oid,
                subprogram_id: at as i64 + 1,
                overload: overloaded.then_some(ordinal),
                object_type: PACKAGE,
                definition: Some(definition),
                package: Some(package),
            });
        }
    }
    for (trigger, relation) in dictionary.triggers() {
        signatures.push(Signature {
            schema: &relation.schema,
            object_name: &trigger.definition.name,
            member: None,
            object_id: trigger.oid,
            subprogram_id: 1,
            overload: None,
            object_type: TRIGGER,
            definition: None,
            package: None,
        });
    }
    signatures
}

fn overload_text(overload: Option<usize>) -> Value {
    overload.map_or(Value::Null, |ordinal| Value::Text(ordinal.to_string()))
}

pub(super) fn procedure_rows(dictionary: &Dictionary<'_>) -> Vec<Vec<Value>> {
    let packages = parsed_packages(dictionary);
    signatures(dictionary, &packages)
        .into_iter()
        .map(|signature| {
            let plsql = signature
                .definition
                .and_then(|definition| definition.plsql.as_deref());
            let row = dictionary
                .row()
                .set("owner", dictionary.owner(signature.schema))
                .set("object_name", shown(signature.object_name))
                .set("procedure_name", signature.member.map_or(Value::Null, shown))
                .set("object_id", number(i64::from(signature.object_id)))
                .set("subprogram_id", number(signature.subprogram_id))
                .set("overload", overload_text(signature.overload))
                .set("object_type", text(signature.object_type))
                .set("origin_con_id", number(CONTAINER_ID));
            match signature.definition {
                Some(definition) => row
                    .set("aggregate", text("NO"))
                    .set(
                        "pipelined",
                        flag(plsql.is_some_and(|plsql| plsql.pipelined), "YES", "NO"),
                    )
                    .set("parallel", text("NO"))
                    .set("interface", text("NO"))
                    .set(
                        "deterministic",
                        flag(plsql.is_some_and(|plsql| plsql.deterministic), "YES", "NO"),
                    )
                    .set(
                        "authid",
                        flag(definition.security_definer, "DEFINER", "CURRENT_USER"),
                    )
                    .set("result_cache", text("NO"))
                    .set("polymorphic", text("NO"))
                    .done(),
                None => row
                    .set("aggregate", text("NO"))
                    .set("pipelined", text("NO"))
                    .set("parallel", text("NO"))
                    .set("interface", text("NO"))
                    .set("deterministic", text("NO"))
                    .set("authid", text("DEFINER"))
                    .set("result_cache", text("NO"))
                    .done(),
            }
        })
        .collect()
}

#[derive(Default)]
struct ArgumentType {
    data_type: String,
    pls_type: Option<String>,
    precision: Option<i64>,
    scale: Option<i64>,
    type_owner: Option<Value>,
    type_name: Option<String>,
    type_subname: Option<String>,
}

impl ArgumentType {
    fn named(data_type: &str) -> Self {
        Self {
            data_type: data_type.to_owned(),
            ..Self::default()
        }
    }

    fn of_sql(sql_type: SqlType) -> Self {
        let name = data_type(sql_type);
        let (precision, scale) = precision_and_scale(sql_type);
        Self {
            pls_type: Some(name.clone()),
            data_type: name,
            precision,
            scale,
            ..Self::default()
        }
    }

    fn written(written: &str, sql_type: SqlType) -> Self {
        let base = written.split('(').next().unwrap_or(written).trim();
        let shown_type = match base {
            "VARCHAR" | "STRING" => "VARCHAR2".to_owned(),
            "CHARACTER" => "CHAR".to_owned(),
            "INTEGER" | "INT" | "SMALLINT" | "DECIMAL" | "DEC" | "NUMERIC" | "REAL"
            | "DOUBLE PRECISION" => "NUMBER".to_owned(),
            "PLS_INTEGER" | "SIMPLE_INTEGER" | "NATURAL" | "NATURALN" | "POSITIVE"
            | "POSITIVEN" | "SIGNTYPE" => "PL/SQL PLS INTEGER".to_owned(),
            known if PASSED_THROUGH_TYPES.contains(&known) => known.to_owned(),
            _ => data_type(sql_type),
        };
        Self {
            data_type: shown_type,
            pls_type: Some(base.to_owned()),
            ..Self::default()
        }
    }
}

fn package_type_name(declared: &TypeDeclaration) -> String {
    match declared {
        TypeDeclaration::Record(_) => RECORD_TYPE.to_owned(),
        TypeDeclaration::Table { index: Some(_), .. } => "PL/SQL TABLE".to_owned(),
        TypeDeclaration::Table { index: None, .. } => "TABLE".to_owned(),
        TypeDeclaration::Varray { .. } => "VARRAY".to_owned(),
        TypeDeclaration::RefCursor => "REF CURSOR".to_owned(),
        TypeDeclaration::Subtype {
            base: VariableType::Sql(sql_type),
            ..
        } => data_type(*sql_type),
        TypeDeclaration::Subtype { .. } => UNDEFINED_TYPE.to_owned(),
    }
}

fn column_type(dictionary: &Dictionary<'_>, unit_schema: &str, parts: &[String]) -> Option<SqlType> {
    let snapshot = dictionary.snapshot;
    let (at, column) = match parts {
        [table, column] => (
            snapshot
                .find_relation(Some(unit_schema), table)
                .or_else(|| snapshot.find_relation(None, table))?,
            column,
        ),
        [schema, table, column] => (
            snapshot.find_relation(Some(schema.as_str()), table)?,
            column,
        ),
        _ => return None,
    };
    let entry = snapshot.relations.get(at)?;
    relation_columns(entry)
        .into_iter()
        .find(|facts| facts.name == column.as_str())
        .map(|facts| facts.sql_type)
}

fn named_type<'p>(
    dictionary: &Dictionary<'_>,
    signature: &Signature<'p>,
    packages: &'p [ParsedPackage<'p>],
    parts: &[String],
) -> ArgumentType {
    let (name, package) = match parts {
        [name] => (name, signature.package),
        [package, name] => (name, find_package(packages, None, package)),
        [schema, package, name] => (
            name,
            find_package(packages, Some(schema.as_str()), package),
        ),
        _ => return ArgumentType::named(UNDEFINED_TYPE),
    };
    match package.and_then(|package| package.type_named(name).map(|found| (package, found))) {
        Some((package, declared)) => ArgumentType {
            data_type: package_type_name(declared),
            type_owner: Some(dictionary.owner(&package.entry.schema)),
            type_name: Some(package.entry.name.clone()),
            type_subname: Some(name.clone()),
            ..ArgumentType::default()
        },
        None => ArgumentType {
            type_name: Some(name.clone()),
            ..ArgumentType::named(UNDEFINED_TYPE)
        },
    }
}

fn argument_type<'p>(
    dictionary: &Dictionary<'_>,
    signature: &Signature<'p>,
    packages: &'p [ParsedPackage<'p>],
    declared: Option<&DeclaredType>,
    sql_type: SqlType,
) -> ArgumentType {
    match declared {
        None => ArgumentType::of_sql(sql_type),
        Some(DeclaredType::Sql { sql_type, written }) => ArgumentType::written(written, *sql_type),
        Some(DeclaredType::ColumnType(parts)) => column_type(dictionary, signature.schema, parts)
            .map_or_else(|| ArgumentType::named(UNDEFINED_TYPE), ArgumentType::of_sql),
        Some(DeclaredType::RowType(_)) => ArgumentType::named(RECORD_TYPE),
        Some(DeclaredType::Named(parts)) => named_type(dictionary, signature, packages, parts),
    }
}

fn returned_type(returns: &RoutineReturns) -> SqlType {
    match returns {
        RoutineReturns::One(ReturnType::Type(sql_type))
        | RoutineReturns::Set(ReturnType::Type(sql_type)) => *sql_type,
        _ => SqlType::Dynamic,
    }
}

fn mode_text(mode: ParameterMode) -> &'static str {
    match mode {
        ParameterMode::In | ParameterMode::Variadic => "IN",
        ParameterMode::Out => "OUT",
        ParameterMode::InOut => "IN/OUT",
    }
}

struct Argument {
    name: Option<String>,
    position: i64,
    mode: &'static str,
    defaulted: bool,
    shape: ArgumentType,
}

fn arguments_of<'p>(
    dictionary: &Dictionary<'_>,
    signature: &Signature<'p>,
    packages: &'p [ParsedPackage<'p>],
    definition: &RoutineDefinition,
) -> Vec<Argument> {
    let plsql = definition.plsql.as_deref();
    let mut arguments = Vec::new();
    if definition.kind == RoutineKind::Function {
        arguments.push(Argument {
            name: None,
            position: 0,
            mode: "OUT",
            defaulted: false,
            shape: argument_type(
                dictionary,
                signature,
                packages,
                plsql.and_then(|plsql| plsql.returns.as_ref()),
                returned_type(&definition.returns),
            ),
        });
    }
    for (at, parameter) in definition.parameters.iter().enumerate() {
        arguments.push(Argument {
            name: parameter.name.clone(),
            position: at as i64 + 1,
            mode: mode_text(parameter.mode),
            defaulted: parameter.default.is_some(),
            shape: argument_type(
                dictionary,
                signature,
                packages,
                plsql.and_then(|plsql| plsql.declared.get(at)),
                parameter.sql_type,
            ),
        });
    }
    arguments
}

pub(super) fn argument_rows(dictionary: &Dictionary<'_>) -> Vec<Vec<Value>> {
    let packages = parsed_packages(dictionary);
    let mut rows = Vec::new();
    for signature in signatures(dictionary, &packages) {
        let Some(definition) = signature.definition else {
            continue;
        };
        let (object_name, package_name) = match signature.member {
            Some(member) => (member, Some(signature.object_name)),
            None => (signature.object_name, None),
        };
        for (sequence, argument) in arguments_of(dictionary, &signature, &packages, definition)
            .into_iter()
            .enumerate()
        {
            let shape = argument.shape;
            rows.push(
                dictionary
                    .row()
                    .set("owner", dictionary.owner(signature.schema))
                    .set("object_name", shown(object_name))
                    .set("package_name", package_name.map_or(Value::Null, shown))
                    .set("object_id", number(i64::from(signature.object_id)))
                    .set("overload", overload_text(signature.overload))
                    .set("subprogram_id", number(signature.subprogram_id))
                    .set(
                        "argument_name",
                        argument.name.as_deref().map_or(Value::Null, shown),
                    )
                    .set("position", number(argument.position))
                    .set("sequence", number(sequence as i64 + 1))
                    .set("data_level", number(0))
                    .set("data_type", Value::Text(shape.data_type))
                    .set("defaulted", flag(argument.defaulted, "Y", "N"))
                    .set("in_out", text(argument.mode))
                    .set("data_precision", optional_number(shape.precision))
                    .set("data_scale", optional_number(shape.scale))
                    .set("type_owner", shape.type_owner.unwrap_or(Value::Null))
                    .set(
                        "type_name",
                        shape.type_name.as_deref().map_or(Value::Null, shown),
                    )
                    .set(
                        "type_subname",
                        shape.type_subname.as_deref().map_or(Value::Null, shown),
                    )
                    .set("pls_type", optional_text(shape.pls_type.as_deref()))
                    .set("origin_con_id", number(CONTAINER_ID))
                    .done(),
            );
        }
    }
    rows
}

fn trigger_type(timing: TriggerTiming, level: TriggerLevel) -> &'static str {
    match (timing, level) {
        (TriggerTiming::Before, TriggerLevel::Statement) => "BEFORE STATEMENT",
        (TriggerTiming::Before, TriggerLevel::Row) => "BEFORE EACH ROW",
        (TriggerTiming::After, TriggerLevel::Statement) => "AFTER STATEMENT",
        (TriggerTiming::After, TriggerLevel::Row) => "AFTER EACH ROW",
        (TriggerTiming::InsteadOf, _) => "INSTEAD OF",
    }
}

fn referencing_names(action: &TriggerAction) -> String {
    let (new, old) = match action {
        TriggerAction::PlSql { new, old, .. } => (swap_uniform_case(new), swap_uniform_case(old)),
        _ => ("NEW".to_owned(), "OLD".to_owned()),
    };
    format!("REFERENCING NEW AS {new} OLD AS {old}")
}

struct TriggerSource {
    description: String,
    action_type: &'static str,
    body: String,
}

fn is_operator(token: Option<&Token<'_>>, symbol: &str) -> bool {
    matches!(token.map(|token| &token.kind), Some(TokenKind::Operator(found)) if *found == symbol)
}

fn oracle_trigger_source(source: &str) -> Option<TriggerSource> {
    let tokens = tokenize_in(source, Dialect::Oracle).ok()?;
    let name_start = tokens.get(1)?.start;
    let mut depth = 0usize;
    let mut when = None;
    for (at, token) in tokens.iter().enumerate().skip(1) {
        let starts_label = is_operator(Some(token), "<") && is_operator(tokens.get(at + 1), "<");
        let call = match &token.kind {
            TokenKind::LeftParen => {
                depth += 1;
                continue;
            }
            TokenKind::RightParen => {
                depth = depth.saturating_sub(1);
                continue;
            }
            _ if depth > 0 => continue,
            TokenKind::Word(word) if word == "when" => {
                if when.is_none() {
                    when = Some(token.start);
                }
                continue;
            }
            TokenKind::Word(word) if word == "declare" || word == "begin" => false,
            TokenKind::Word(word) if word == "call" => true,
            _ if starts_label => false,
            _ => continue,
        };
        let heading = source.get(name_start..when.unwrap_or(token.start))?;
        return Some(TriggerSource {
            description: format!("{}\n", heading.trim_end()),
            action_type: if call { "CALL" } else { "PL/SQL" },
            body: source.get(token.start..)?.to_owned(),
        });
    }
    None
}

fn trigger_source(definition: &TriggerDefinition) -> TriggerSource {
    let whole = |body: String| TriggerSource {
        description: format!("{}\n", swap_uniform_case(&definition.name)),
        action_type: "PL/SQL",
        body,
    };
    match &definition.action {
        TriggerAction::PlSql { body, .. } => {
            oracle_trigger_source(unit_source(&definition.text)).unwrap_or_else(|| whole(body.clone()))
        }
        TriggerAction::Function { name, arguments } => TriggerSource {
            action_type: "CALL",
            ..whole(format!("EXECUTE FUNCTION {name}({})", arguments.join(", ")))
        },
        TriggerAction::Statements(body) | TriggerAction::Batch(body) => whole(body.clone()),
    }
}

fn timing_flag(definition: &TriggerDefinition, timing: TriggerTiming, level: TriggerLevel) -> Value {
    flag(
        definition.timing == timing && definition.level == level,
        "YES",
        "NO",
    )
}

pub(super) fn trigger_rows(dictionary: &Dictionary<'_>) -> Vec<Vec<Value>> {
    dictionary
        .triggers()
        .into_iter()
        .map(|(trigger, relation)| {
            let definition = &trigger.definition;
            let source = trigger_source(definition);
            let events = definition
                .events
                .listed()
                .into_iter()
                .map(|event| event.keyword())
                .collect::<Vec<_>>()
                .join(" OR ");
            let base_object_type = match relation.source {
                RelationSource::View(_) => "VIEW",
                _ => "TABLE",
            };
            dictionary
                .row()
                .set("owner", dictionary.owner(&relation.schema))
                .set("trigger_name", shown(&definition.name))
                .set(
                    "trigger_type",
                    text(trigger_type(definition.timing, definition.level)),
                )
                .set("triggering_event", Value::Text(events))
                .set("table_owner", dictionary.owner(&relation.schema))
                .set("base_object_type", text(base_object_type))
                .set("table_name", shown(&relation.name))
                .set(
                    "referencing_names",
                    Value::Text(referencing_names(&definition.action)),
                )
                .set("when_clause", optional_text(definition.condition.as_deref()))
                .set("status", flag(definition.enabled, "ENABLED", "DISABLED"))
                .set("description", Value::Text(source.description))
                .set("action_type", text(source.action_type))
                .set("trigger_body", Value::Text(source.body))
                .set("crossedition", text("NO"))
                .set(
                    "before_statement",
                    timing_flag(definition, TriggerTiming::Before, TriggerLevel::Statement),
                )
                .set(
                    "before_row",
                    timing_flag(definition, TriggerTiming::Before, TriggerLevel::Row),
                )
                .set(
                    "after_row",
                    timing_flag(definition, TriggerTiming::After, TriggerLevel::Row),
                )
                .set(
                    "after_statement",
                    timing_flag(definition, TriggerTiming::After, TriggerLevel::Statement),
                )
                .set(
                    "instead_of_row",
                    flag(definition.timing == TriggerTiming::InsteadOf, "YES", "NO"),
                )
                .set("fire_once", text("YES"))
                .set("apply_server_only", text("NO"))
                .done()
        })
        .collect()
}
