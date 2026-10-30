use super::oids;
use super::snapshot::{CatalogSnapshot, RoutineEntry};
use crate::pg_keywords::quote_ident;
use crate::records::RECORD_OID;
use crate::routines::{
    ParameterMode, ReturnType, RoutineDefinition, RoutineKind, RoutineLanguage, RoutineParameter,
    RoutineReturns, Volatility,
};
use crate::types::SqlType;

pub(crate) const INTERNAL_LANGUAGE: u32 = 12;
const C_LANGUAGE: u32 = 13;
const SQL_LANGUAGE: u32 = 14;
const PLPGSQL_LANGUAGE: u32 = 13576;
const TSQL_LANGUAGE: u32 = 13577;
const PLSQL_LANGUAGE: u32 = 13578;
const VOID_TYPE: u32 = 2278;
const TRIGGER_TYPE: u32 = 2279;
pub(crate) const ROUTINE_COST: f32 = 100.0;
const SET_RETURNING_ROWS: f32 = 1000.0;

pub(crate) struct Language {
    pub(crate) oid: u32,
    pub(crate) name: &'static str,
    pub(crate) procedural: bool,
    pub(crate) trusted: bool,
}

pub(crate) const LANGUAGES: [Language; 6] = [
    Language {
        oid: INTERNAL_LANGUAGE,
        name: "internal",
        procedural: false,
        trusted: false,
    },
    Language {
        oid: C_LANGUAGE,
        name: "c",
        procedural: false,
        trusted: false,
    },
    Language {
        oid: SQL_LANGUAGE,
        name: "sql",
        procedural: false,
        trusted: true,
    },
    Language {
        oid: PLPGSQL_LANGUAGE,
        name: "plpgsql",
        procedural: true,
        trusted: true,
    },
    Language {
        oid: TSQL_LANGUAGE,
        name: "tsql",
        procedural: true,
        trusted: true,
    },
    Language {
        oid: PLSQL_LANGUAGE,
        name: "plsql",
        procedural: true,
        trusted: true,
    },
];

pub(crate) fn language_oid(language: RoutineLanguage) -> u32 {
    match language {
        RoutineLanguage::Sql => SQL_LANGUAGE,
        RoutineLanguage::PlPgSql => PLPGSQL_LANGUAGE,
        RoutineLanguage::Tsql => TSQL_LANGUAGE,
        RoutineLanguage::PlSql => PLSQL_LANGUAGE,
    }
}

pub(crate) fn estimated_rows(definition: &RoutineDefinition) -> f32 {
    if definition.returns.is_set() {
        SET_RETURNING_ROWS
    } else {
        0.0
    }
}

pub(crate) fn kind_code(kind: RoutineKind) -> &'static str {
    match kind {
        RoutineKind::Function => "f",
        RoutineKind::Procedure => "p",
    }
}

pub(crate) fn mode_code(mode: ParameterMode) -> &'static str {
    match mode {
        ParameterMode::In => "i",
        ParameterMode::Out => "o",
        ParameterMode::InOut => "b",
        ParameterMode::Variadic => "v",
    }
}

pub(crate) fn type_shown(sql_type: SqlType) -> String {
    oids::format_type(oids::type_oid(sql_type), None)
}

fn return_type_oid(snapshot: &CatalogSnapshot, returned: &ReturnType) -> u32 {
    match returned {
        ReturnType::Void => VOID_TYPE,
        ReturnType::Trigger => TRIGGER_TYPE,
        ReturnType::Record => RECORD_OID,
        ReturnType::Type(sql_type) => oids::type_oid(*sql_type),
        ReturnType::Relation(name) => snapshot
            .user_types
            .named(name)
            .map_or(RECORD_OID, |found| found.oid),
    }
}

pub(crate) fn result_type_oid(snapshot: &CatalogSnapshot, definition: &RoutineDefinition) -> u32 {
    match &definition.returns {
        RoutineReturns::Nothing if has_outputs(definition) => RECORD_OID,
        RoutineReturns::Nothing => VOID_TYPE,
        RoutineReturns::One(returned) | RoutineReturns::Set(returned) => {
            return_type_oid(snapshot, returned)
        }
        RoutineReturns::Table(columns) => match columns.as_slice() {
            [(_, only)] => oids::type_oid(*only),
            _ => RECORD_OID,
        },
    }
}

fn has_outputs(definition: &RoutineDefinition) -> bool {
    definition
        .parameters
        .iter()
        .any(|parameter| parameter.mode.is_output())
}

fn return_type_name(returned: &ReturnType) -> String {
    match returned {
        ReturnType::Void => "void".to_owned(),
        ReturnType::Trigger => "trigger".to_owned(),
        ReturnType::Record => "record".to_owned(),
        ReturnType::Type(sql_type) => type_shown(*sql_type),
        ReturnType::Relation(name) => crate::schemas::display(name),
    }
}

pub(crate) fn function_result(definition: &RoutineDefinition) -> Option<String> {
    match &definition.returns {
        RoutineReturns::Nothing => None,
        RoutineReturns::One(returned) => Some(return_type_name(returned)),
        RoutineReturns::Set(returned) => Some(format!("SETOF {}", return_type_name(returned))),
        RoutineReturns::Table(columns) => Some(format!(
            "TABLE({})",
            columns
                .iter()
                .map(|(name, sql_type)| format!("{} {}", quote_ident(name), type_shown(*sql_type)))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArgumentList {
    WithDefaults,
    Identity,
}

fn shown_parameter(parameter: &RoutineParameter, list: ArgumentList) -> String {
    let mut shown = String::new();
    if let Some(keyword) = parameter.mode.keyword() {
        shown.push_str(keyword);
        shown.push(' ');
    }
    if let Some(name) = &parameter.name {
        shown.push_str(&quote_ident(name));
        shown.push(' ');
    }
    shown.push_str(&type_shown(parameter.sql_type));
    if list == ArgumentList::WithDefaults
        && let Some(default) = &parameter.default
    {
        shown.push_str(" DEFAULT ");
        shown.push_str(default.trim());
    }
    shown
}

pub(crate) fn function_arguments(definition: &RoutineDefinition, list: ArgumentList) -> String {
    definition
        .parameters
        .iter()
        .filter(|parameter| {
            list == ArgumentList::WithDefaults
                || definition.kind == RoutineKind::Procedure
                || parameter.mode.is_input()
        })
        .map(|parameter| shown_parameter(parameter, list))
        .collect::<Vec<_>>()
        .join(", ")
}

fn dollar_quote_tag(body: &str, base: &str) -> String {
    let mut tag = base.to_owned();
    let mut suffix = 0usize;
    while body.contains(&format!("${tag}$")) {
        suffix += 1;
        tag = format!("{base}{suffix}");
    }
    tag
}

fn setting_values(value: &str) -> String {
    value
        .split(',')
        .map(|item| format!("'{}'", item.trim().replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn function_definition(routine: &RoutineEntry) -> String {
    let definition = &routine.definition;
    let mut text = format!(
        "CREATE OR REPLACE {} {}.{}({})\n",
        definition.kind.keyword(),
        quote_ident(&routine.schema),
        quote_ident(&routine.name),
        function_arguments(definition, ArgumentList::WithDefaults)
    );
    if let Some(result) = function_result(definition) {
        text.push_str(&format!(" RETURNS {result}\n"));
    }
    text.push_str(&format!(" LANGUAGE {}\n", definition.language.name()));
    if definition.volatility != Volatility::Volatile {
        text.push_str(&format!(" {}\n", definition.volatility.keyword()));
    }
    if definition.strict {
        text.push_str(" STRICT\n");
    }
    if definition.security_definer {
        text.push_str(" SECURITY DEFINER\n");
    }
    for (name, value) in &definition.settings {
        text.push_str(&format!(" SET {name} TO {}\n", setting_values(value)));
    }
    let tag = dollar_quote_tag(&definition.body, definition.kind.noun());
    text.push_str(&format!("AS ${tag}${}${tag}$\n", definition.body));
    text
}

pub(crate) fn variadic_element_oid(definition: &RoutineDefinition) -> u32 {
    definition
        .parameters
        .iter()
        .find(|parameter| parameter.mode == ParameterMode::Variadic)
        .map_or(0, |parameter| match parameter.sql_type {
            SqlType::Array(element) => oids::type_oid(element.sql_type()),
            other => oids::type_oid(other),
        })
}

pub(crate) fn argument_count(definition: &RoutineDefinition) -> usize {
    definition
        .parameters
        .iter()
        .filter(|parameter| parameter.mode.is_input())
        .count()
}

pub(crate) fn default_count(definition: &RoutineDefinition) -> usize {
    definition
        .parameters
        .iter()
        .filter(|parameter| parameter.mode.is_input() && parameter.default.is_some())
        .count()
}

pub(crate) fn defaults_text(definition: &RoutineDefinition) -> Option<String> {
    let defaults: Vec<&str> = definition
        .parameters
        .iter()
        .filter_map(|parameter| parameter.default.as_deref().map(str::trim))
        .collect();
    (!defaults.is_empty()).then(|| defaults.join(", "))
}

pub(crate) fn lists_every_parameter(definition: &RoutineDefinition) -> bool {
    matches!(definition.returns, RoutineReturns::Table(_))
        || definition
            .parameters
            .iter()
            .any(|parameter| parameter.mode != ParameterMode::In)
}

pub(crate) struct ListedParameter {
    pub(crate) name: String,
    pub(crate) mode: &'static str,
    pub(crate) sql_type: SqlType,
}

pub(crate) fn every_parameter(definition: &RoutineDefinition) -> Vec<ListedParameter> {
    let mut listed: Vec<ListedParameter> = definition
        .parameters
        .iter()
        .map(|parameter| ListedParameter {
            name: parameter.name.clone().unwrap_or_default(),
            mode: mode_code(parameter.mode),
            sql_type: parameter.sql_type,
        })
        .collect();
    if let RoutineReturns::Table(columns) = &definition.returns {
        listed.extend(columns.iter().map(|(name, sql_type)| ListedParameter {
            name: name.clone(),
            mode: "t",
            sql_type: *sql_type,
        }));
    }
    listed
}
