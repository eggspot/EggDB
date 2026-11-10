use crate::error::{DocumentError, DocumentResult};
use crate::filter::{regex_matches, type_code};
use eggdb_bson::{Bson, Document, compare};
use std::cmp::Ordering;

const MAX_DEPTH: usize = 100;

const UNSUPPORTED: [&str; 6] = ["$ref", "$schema", "default", "definitions", "format", "id"]; 7] = [
    "$ref",
    "$schema",
    "default",
    "definitions",
    "format",
    "id",
    "integer",
];

fn failed(message: impl Into<String>) -> DocumentError {
    DocumentError::failed_to_parse(message)
}

fn schemas_in<'s>(value: &'s Bson, keyword: &str) -> DocumentResult<Vec<&'s Document>> {
    let Bson::Array(items) = value else {
        return Err(DocumentError::type_mismatch(format!(
            "${keyword} must be an array"
        )));
    };
    if items.is_empty() {
        return Err(failed(format!("${keyword} must be a non-empty array")));
    }
    items
        .iter()
        .map(|item| {
            item.as_document().ok_or_else(|| {
                DocumentError::type_mismatch(format!("${keyword} must contain only objects"))
            })
        })
        .collect()
}

fn schema_of<'s>(value: &'s Bson, keyword: &str) -> DocumentResult<&'s Document> {
    value
        .as_document()
        .ok_or_else(|| DocumentError::type_mismatch(format!("${keyword} must be an object")))
}

fn whole_count(value: &Bson, keyword: &str) -> DocumentResult<i64> {
    match value.as_f64() {
        Some(number) if number >= 0.0 && number.fract() == 0.0 => Ok(number as i64),
        _ => Err(failed(format!(
            "${keyword} must be a representable non-negative integer"
        ))),
    }
}

fn strings_in(value: &Bson, keyword: &str) -> DocumentResult<Vec<String>> {
    let names = match value {
        Bson::String(name) => vec![name.clone()],
        Bson::Array(items) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_owned).ok_or_else(|| {
                    DocumentError::type_mismatch(format!("${keyword} must contain only strings"))
                })
            })
            .collect::<DocumentResult<_>>()?,
        _ => {
            return Err(DocumentError::type_mismatch(format!(
                "${keyword} must be either a string or an array of strings"
            )));
        }
    };
    Ok(names)
}

const JSON_TYPES: [&str; 6] = ["object", "array", "number", "boolean", "string", "null"];

/// Checks a `$jsonSchema` the way MongoDB parses it, before any document is matched.
pub(crate) fn check_schema(schema: &Document, depth: usize) -> DocumentResult<()> {
    if depth > MAX_DEPTH {
        return Err(DocumentError::bad_value(
            "the $jsonSchema is nested too deeply",
        ));
    }
    if schema.contains_key("type") && schema.contains_key("bsonType") {
        return Err(failed(
            "Cannot specify both $jsonSchema keywords 'type' and 'bsonType'",
        ));
    }
    for (keyword, value) in schema.iter() {
        match keyword.as_str() {
            "title" | "description" => {
                if !matches!(value, Bson::String(_)) {
                    return Err(DocumentError::type_mismatch(format!(
                        "${keyword} must be a string"
                    )));
                }
            }
            "bsonType" => {
                for name in strings_in(value, keyword)? {
                    type_code(&Bson::String(name.clone())).map_err(|_| {
                        DocumentError::bad_value(format!("Unknown type name alias: {name}"))
                    })?;
                }
            }
            "type" => {
                for name in strings_in(value, keyword)? {
                    if name == "integer" {
                        return Err(failed(
                            "$jsonSchema type 'integer' is not currently supported.",
                        ));
                    }
                    if !JSON_TYPES.contains(&name.as_str()) {
                        return Err(DocumentError::bad_value(format!(
                            "Unknown type name alias: {name}"
                        )));
                    }
                }
            }
            "enum" => match value {
                Bson::Array(items) if !items.is_empty() => {}
                _ => {
                    return Err(failed(
                        "$jsonSchema keyword 'enum' must be a non-empty array",
                    ));
                }
            },
            "properties" | "patternProperties" => {
                for (name, inner) in schema_of(value, keyword)?.iter() {
                    let Bson::Document(inner) = inner else {
                        return Err(DocumentError::type_mismatch(format!(
                            "Nested schema for ${keyword} property '{name}' must be an object"
                        )));
                    };
                    check_schema(inner, depth + 1)?;
                }
            }
            "required" => {
                let Bson::Array(names) = value else {
                    return Err(DocumentError::type_mismatch("$required must be an array"));
                };
                if names.is_empty() || names.iter().any(|name| !matches!(name, Bson::String(_))) {
                    return Err(failed("$required must be a non-empty array of strings"));
                }
            }
            "additionalProperties" | "additionalItems" => match value {
                Bson::Boolean(_) => {}
                Bson::Document(inner) => check_schema(inner, depth + 1)?,
                _ => {
                    return Err(DocumentError::type_mismatch(format!(
                        "${keyword} must be either an object or a boolean"
                    )));
                }
            },
            "items" => match value {
                Bson::Document(inner) => check_schema(inner, depth + 1)?,
                Bson::Array(_) => {
                    for inner in schemas_in(value, keyword)? {
                        check_schema(inner, depth + 1)?;
                    }
                }
                _ => {
                    return Err(DocumentError::type_mismatch(
                        "$items must be either an object or an array",
                    ));
                }
            },
            "allOf" | "anyOf" | "oneOf" => {
                for inner in schemas_in(value, keyword)? {
                    check_schema(inner, depth + 1)?;
                }
            }
            "not" => check_schema(schema_of(value, keyword)?, depth + 1)?,
            "dependencies" => {
                for (_, dependency) in schema_of(value, keyword)?.iter() {
                    match dependency {
                        Bson::Document(inner) => check_schema(inner, depth + 1)?,
                        Bson::Array(_) => {
                            strings_in(dependency, keyword)?;
                        }
                        _ => {
                            return Err(DocumentError::type_mismatch(
                                "$dependencies values must be either an object or an array of strings",
                            ));
                        }
                    }
                }
            }
            "minimum" | "maximum" | "multipleOf" => {
                let Some(number) = value.as_f64() else {
                    return Err(DocumentError::type_mismatch(format!(
                        "${keyword} must be a number"
                    )));
                };
                if keyword == "multipleOf" && number <= 0.0 {
                    return Err(failed("$multipleOf must have a positive value"));
                }
            }
            "exclusiveMinimum" | "exclusiveMaximum" | "uniqueItems" => {
                if !matches!(value, Bson::Boolean(_)) {
                    return Err(DocumentError::type_mismatch(format!(
                        "${keyword} must be a boolean"
                    )));
                }
                let bound = if keyword == "exclusiveMinimum" {
                    "minimum"
                } else {
                    "maximum"
                };
                if keyword != "uniqueItems" && !schema.contains_key(bound) {
                    return Err(failed(format!(
                        "Cannot specify ${keyword} without ${bound}"
                    )));
                }
            }
            "minLength" | "maxLength" | "minItems" | "maxItems" | "minProperties"
            | "maxProperties" => {
                whole_count(value, keyword)?;
            }
            "pattern" => {
                let Bson::String(pattern) = value else {
                    return Err(DocumentError::type_mismatch("$pattern must be a string"));
                };
                if eggdb_sql::regex_matches_with_flags("", pattern, "").is_err() {
                    return Err(DocumentError::bad_value(format!(
                        "Invalid regular expression: {pattern}"
                    )));
                }
            }
            other if UNSUPPORTED.contains(&other) => {
                return Err(failed(format!(
                    "$jsonSchema keyword '{other}' is not currently supported"
                )));
            }
            other => return Err(failed(format!("Unknown $jsonSchema keyword: {other}"))),
        }
    }
    Ok(())
}

fn json_type_holds(name: &str, value: &Bson) -> bool {
    match name {
        "object" => matches!(value, Bson::Document(_)),
        "array" => matches!(value, Bson::Array(_)),
        "number" => value.is_number(),
        "boolean" => matches!(value, Bson::Boolean(_)),
        "string" => matches!(value, Bson::String(_)),
        "null" => matches!(value, Bson::Null),
        _ => false,
    }
}

fn names_of(value: &Bson) -> Vec<String> {
    match value {
        Bson::String(name) => vec![name.clone()],
        Bson::Array(items) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

fn counted(value: &Bson) -> usize {
    value.as_f64().map_or(0, |number| number.max(0.0) as usize)
}

/// Whether `value` satisfies the checked `schema`.
pub(crate) fn schema_holds(schema: &Document, value: &Bson, depth: usize) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    schema
        .iter()
        .all(|(keyword, argument)| keyword_holds(schema, keyword, argument, value, depth))
}

fn nested_holds(argument: &Bson, value: &Bson, depth: usize) -> bool {
    match argument {
        Bson::Document(inner) => schema_holds(inner, value, depth + 1),
        Bson::Boolean(allowed) => *allowed,
        _ => true,
    }
}

fn keyword_holds(
    schema: &Document,
    keyword: &str,
    argument: &Bson,
    value: &Bson,
    depth: usize,
) -> bool {
    match keyword {
        "bsonType" => names_of(argument).iter().any(|name| {
            type_code(&Bson::String(name.clone()))
                .is_ok_and(|code| code == value.type_number() || (code == 0 && value.is_number()))
        }),
        "type" => names_of(argument)
            .iter()
            .any(|name| json_type_holds(name, value)),
        "enum" => match argument {
            Bson::Array(items) => items
                .iter()
                .any(|item| compare(item, value) == Ordering::Equal),
            _ => true,
        },
        "properties" => match (argument, value) {
            (Bson::Document(properties), Bson::Document(object)) => {
                properties.iter().all(|(name, inner)| {
                    object
                        .get(name)
                        .is_none_or(|field| nested_holds(inner, field, depth))
                })
            }
            _ => true,
        },
        "patternProperties" => match (argument, value) {
            (Bson::Document(patterns), Bson::Document(object)) => {
                object.iter().all(|(name, field)| {
                    patterns.iter().all(|(pattern, inner)| {
                        !regex_matches(&Bson::String(name.clone()), pattern, "")
                            || nested_holds(inner, field, depth)
                    })
                })
            }
            _ => true,
        },
        "additionalProperties" => match value {
            Bson::Document(object) => {
                let declared = schema.get_document("properties");
                let patterns = schema.get_document("patternProperties");
                object.iter().all(|(name, field)| {
                    let named = declared.is_some_and(|declared| declared.contains_key(name));
                    let patterned = patterns.is_some_and(|patterns| {
                        patterns.iter().any(|(pattern, _)| {
                            regex_matches(&Bson::String(name.clone()), pattern, "")
                        })
                    });
                    named || patterned || nested_holds(argument, field, depth)
                })
            }
            _ => true,
        },
        "required" => match value {
            Bson::Document(object) => names_of(argument)
                .iter()
                .all(|name| object.contains_key(name)),
            _ => true,
        },
        "minProperties" => value
            .as_document()
            .is_none_or(|object| object.len() >= counted(argument)),
        "maxProperties" => value
            .as_document()
            .is_none_or(|object| object.len() <= counted(argument)),
        "dependencies" => match (argument, value) {
            (Bson::Document(dependencies), Bson::Document(object)) => {
                dependencies.iter().all(|(name, dependency)| {
                    !object.contains_key(name)
                        || match dependency {
                            Bson::Document(inner) => schema_holds(inner, value, depth + 1),
                            other => names_of(other)
                                .iter()
                                .all(|needed| object.contains_key(needed)),
                        }
                })
            }
            _ => true,
        },
        "items" => match (argument, value) {
            (Bson::Document(inner), Bson::Array(items)) => items
                .iter()
                .all(|item| schema_holds(inner, item, depth + 1)),
            (Bson::Array(schemas), Bson::Array(items)) => items
                .iter()
                .zip(schemas.iter())
                .all(|(item, inner)| nested_holds(inner, item, depth)),
            _ => true,
        },
        "additionalItems" => match (schema.get("items"), value) {
            (Some(Bson::Array(schemas)), Bson::Array(items)) => items
                .iter()
                .skip(schemas.len())
                .all(|item| nested_holds(argument, item, depth)),
            _ => true,
        },
        "minItems" => match value {
            Bson::Array(items) => items.len() >= counted(argument),
            _ => true,
        },
        "maxItems" => match value {
            Bson::Array(items) => items.len() <= counted(argument),
            _ => true,
        },
        "uniqueItems" => match (argument, value) {
            (Bson::Boolean(true), Bson::Array(items)) => {
                items.iter().enumerate().all(|(at, item)| {
                    items[..at]
                        .iter()
                        .all(|earlier| compare(earlier, item) != Ordering::Equal)
                })
            }
            _ => true,
        },
        "minimum" | "maximum" => {
            let (Some(number), Some(bound)) = (
                value.is_number().then(|| value.as_f64()).flatten(),
                argument.as_f64(),
            ) else {
                return true;
            };
            let exclusive_keyword = if keyword == "minimum" {
                "exclusiveMinimum"
            } else {
                "exclusiveMaximum"
            };
            let exclusive = schema.get_bool(exclusive_keyword) == Some(true);
            match (keyword, exclusive) {
                ("minimum", false) => number >= bound,
                ("minimum", true) => number > bound,
                (_, false) => number <= bound,
                (_, true) => number < bound,
            }
        }
        "multipleOf" => {
            let (Some(number), Some(divisor)) = (
                value.is_number().then(|| value.as_f64()).flatten(),
                argument.as_f64(),
            ) else {
                return true;
            };
            (number / divisor).fract() == 0.0
        }
        "minLength" | "maxLength" => match value {
            Bson::String(text) => {
                let length = text.chars().count();
                if keyword == "minLength" {
                    length >= counted(argument)
                } else {
                    length <= counted(argument)
                }
            }
            _ => true,
        },
        "pattern" => match (argument, value) {
            (Bson::String(pattern), Bson::String(_)) => regex_matches(value, pattern, ""),
            _ => true,
        },
        "allOf" => match argument {
            Bson::Array(schemas) => schemas
                .iter()
                .all(|inner| nested_holds(inner, value, depth)),
            _ => true,
        },
        "anyOf" => match argument {
            Bson::Array(schemas) => schemas
                .iter()
                .any(|inner| nested_holds(inner, value, depth)),
            _ => true,
        },
        "oneOf" => match argument {
            Bson::Array(schemas) => {
                schemas
                    .iter()
                    .filter(|inner| nested_holds(inner, value, depth))
                    .count()
                    == 1
            }
            _ => true,
        },
        "not" => !nested_holds(argument, value, depth),
        _ => true,
    }
}
