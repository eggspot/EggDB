use super::{ReturnType, RoutineDefinition, RoutineReturns};
use crate::arrays::{ArrayElement, ArrayValue};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::TableCache;
use crate::pg_catalog::{CatalogType, PseudoType};
use crate::types::{Conversion, DefinedKind, SqlType, Value, convert};
use std::sync::Arc;

const CACHED_SPECIALIZATIONS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Element,
    Array,
    NonArray,
    Enum,
    Compatible,
    CompatibleArray,
    CompatibleNonArray,
}

impl Role {
    fn of(sql_type: SqlType) -> Option<Self> {
        match sql_type {
            SqlType::Catalog(CatalogType::AnyArray) => Some(Self::Array),
            SqlType::Catalog(CatalogType::Pseudo(pseudo)) => Some(match pseudo {
                PseudoType::AnyElement => Self::Element,
                PseudoType::AnyNonArray => Self::NonArray,
                PseudoType::AnyEnum => Self::Enum,
                PseudoType::AnyCompatible => Self::Compatible,
                PseudoType::AnyCompatibleArray => Self::CompatibleArray,
                PseudoType::AnyCompatibleNonArray => Self::CompatibleNonArray,
            }),
            _ => None,
        }
    }

    fn is_compatible(self) -> bool {
        matches!(
            self,
            Self::Compatible | Self::CompatibleArray | Self::CompatibleNonArray
        )
    }

    fn is_array(self) -> bool {
        matches!(self, Self::Array | Self::CompatibleArray)
    }

    fn element_of(self, actual: SqlType) -> Option<SqlType> {
        match self {
            Self::Element | Self::Compatible => Some(actual),
            Self::NonArray | Self::CompatibleNonArray => {
                (!matches!(actual, SqlType::Array(_))).then_some(actual)
            }
            Self::Enum => is_enum(actual).then_some(actual),
            Self::Array | Self::CompatibleArray => match actual {
                SqlType::Array(element) => Some(element.sql_type()),
                _ => None,
            },
        }
    }

    fn argument_type(self, family: SqlType) -> Option<SqlType> {
        if self.is_array() {
            ArrayElement::of(family).map(SqlType::Array)
        } else {
            Some(family)
        }
    }

    fn accepts(self, family: SqlType) -> bool {
        match self {
            Self::NonArray | Self::CompatibleNonArray => !matches!(family, SqlType::Array(_)),
            Self::Enum => is_enum(family),
            _ => true,
        }
    }
}

fn is_enum(sql_type: SqlType) -> bool {
    matches!(sql_type, SqlType::Defined(defined) if defined.kind == DefinedKind::Enum)
}

/// The types a call's arguments give a polymorphic routine's two families of pseudo-types.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Resolution {
    element: Option<SqlType>,
    compatible: Option<SqlType>,
}

impl Resolution {
    fn family(&self, role: Role) -> Option<SqlType> {
        if role.is_compatible() {
            self.compatible
        } else {
            self.element
        }
    }

    fn settle(&mut self, role: Role, family: SqlType) {
        if role.is_compatible() {
            self.compatible = Some(family);
        } else {
            self.element = Some(family);
        }
    }
}

pub(crate) struct Specialization {
    generic: Arc<RoutineDefinition>,
    resolution: Resolution,
    specialized: Arc<RoutineDefinition>,
}

pub(crate) fn is_polymorphic(sql_type: SqlType) -> bool {
    Role::of(sql_type).is_some()
}

pub(crate) fn is_polymorphic_array(sql_type: SqlType) -> bool {
    Role::of(sql_type).is_some_and(Role::is_array)
}

fn returned_types(returns: &RoutineReturns) -> Vec<SqlType> {
    match returns {
        RoutineReturns::One(ReturnType::Type(sql_type))
        | RoutineReturns::Set(ReturnType::Type(sql_type)) => vec![*sql_type],
        RoutineReturns::Table(columns) => columns.iter().map(|(_, sql_type)| *sql_type).collect(),
        _ => Vec::new(),
    }
}

pub(crate) fn declares_polymorphism(definition: &RoutineDefinition) -> bool {
    definition
        .parameters
        .iter()
        .any(|parameter| is_polymorphic(parameter.sql_type))
        || returned_types(&definition.returns)
            .into_iter()
            .any(is_polymorphic)
}

/// Settles the types the polymorphic parameters stand for from the arguments given to them:
/// each input is a parameter's declared type, its argument (if given) and whether that argument
/// is an untyped literal, which takes its type from the others instead of setting it.
pub(crate) fn resolve<'v>(
    inputs: impl IntoIterator<Item = (SqlType, Option<&'v Value>, bool)>,
) -> Option<Resolution> {
    let mut resolution = Resolution::default();
    let mut roles = Vec::new();
    let mut untyped = Vec::new();
    for (declared, value, is_untyped) in inputs {
        let Some(role) = Role::of(declared) else {
            continue;
        };
        roles.push(role);
        let Some(value) = value else {
            continue;
        };
        let Some(actual) = value.sql_type() else {
            continue;
        };
        if is_untyped && matches!(value, Value::Text(_)) {
            untyped.push(role);
            continue;
        }
        let found = role.element_of(actual)?.without_modifier();
        let merged = match resolution.family(role) {
            None => found,
            Some(known) if known == found => known,
            Some(known) if role.is_compatible() => {
                crate::set_operations::common_type(known, found)?
            }
            Some(_) => return None,
        };
        resolution.settle(role, merged);
    }
    for role in untyped {
        let family = match resolution.family(role) {
            Some(family) => family,
            None if role == Role::Enum => return None,
            None => SqlType::Text,
        };
        role.argument_type(family)?;
        resolution.settle(role, family);
    }
    for role in roles {
        if let Some(family) = resolution.family(role)
            && !role.accepts(family)
        {
            return None;
        }
    }
    Some(resolution)
}

fn coerced(value: &Value, target: SqlType, untyped: bool) -> Option<Value> {
    if value.is_null() {
        return Some(Value::Null);
    }
    if value
        .sql_type()
        .is_some_and(|actual| actual.without_modifier() == target.without_modifier())
    {
        return Some(value.clone());
    }
    convert(value.clone(), target, Conversion::Assignment)
        .ok()
        .or_else(|| {
            (untyped && matches!(value, Value::Text(_)))
                .then(|| convert(value.clone(), target, Conversion::Explicit).ok())
                .flatten()
        })
}

/// The array a polymorphic `VARIADIC` parameter receives from the spread arguments `items`,
/// each paired with whether it is an untyped literal.
pub(crate) fn variadic_array(declared: SqlType, items: Vec<(Value, bool)>) -> Option<Value> {
    let role = Role::of(declared)?;
    let mut family: Option<SqlType> = None;
    for (item, untyped) in &items {
        let Some(actual) = item.sql_type() else {
            continue;
        };
        if *untyped && matches!(item, Value::Text(_)) {
            continue;
        }
        let actual = actual.without_modifier();
        family = Some(match family {
            None => actual,
            Some(known) if known == actual => known,
            Some(known) if role.is_compatible() => {
                crate::set_operations::common_type(known, actual)?
            }
            Some(_) => return None,
        });
    }
    let family = family.unwrap_or(SqlType::Text);
    if matches!(family, SqlType::Array(_)) {
        return None;
    }
    let element = ArrayElement::of(family)?;
    let mut converted = Vec::with_capacity(items.len());
    for (item, untyped) in &items {
        converted.push(coerced(item, family, *untyped)?);
    }
    Some(Value::Array(Box::new(ArrayValue::new(element, converted))))
}

fn specialized_type(declared: SqlType, resolution: Resolution) -> SqlResult<SqlType> {
    let Some(role) = Role::of(declared) else {
        return Ok(declared);
    };
    let family = resolution.family(role).unwrap_or(SqlType::Text);
    role.argument_type(family).ok_or_else(|| {
        SqlError::new(
            SqlState::UNDEFINED_OBJECT,
            format!("could not find array type for data type {}", family.name()),
        )
    })
}

fn not_alike(definition: &RoutineDefinition) -> SqlError {
    let declared = definition
        .input_types()
        .into_iter()
        .find(|sql_type| is_polymorphic(*sql_type))
        .map_or_else(|| "anyelement".to_owned(), SqlType::name);
    SqlError::new(
        SqlState::DATATYPE_MISMATCH,
        format!("arguments declared \"{declared}\" are not all alike"),
    )
}

/// What a call says about each of its input arguments, by position.
pub(crate) struct ArgumentKinds<'a> {
    /// Whether the argument is an untyped literal (one past the end counts as one).
    pub(crate) untyped: &'a [bool],
    /// Whether the call gave the argument, rather than its parameter's default.
    pub(crate) given: &'a [bool],
}

/// The definition specialized to the types its polymorphic parameters take in this call, and
/// the arguments converted to them. A given untyped literal that does not convert to its
/// resolved type fails with the conversion's own error, as PostgreSQL's input functions do.
pub(crate) fn specialize(
    tables: &mut TableCache,
    definition: Arc<RoutineDefinition>,
    arguments: Vec<Value>,
    kinds: ArgumentKinds<'_>,
) -> SqlResult<(Arc<RoutineDefinition>, Vec<Value>)> {
    if !declares_polymorphism(&definition) {
        return Ok((definition, arguments));
    }
    let inputs = definition.input_types();
    let is_untyped = |at: usize| kinds.untyped.get(at).copied().unwrap_or(true);
    let is_given = |at: usize| kinds.given.get(at).copied().unwrap_or(false);
    let resolution = resolve(
        inputs
            .iter()
            .copied()
            .zip(arguments.iter().map(Some))
            .enumerate()
            .map(|(at, (declared, value))| (declared, value, is_untyped(at))),
    )
    .ok_or_else(|| not_alike(&definition))?;
    let mut converted = Vec::with_capacity(arguments.len());
    for (at, value) in arguments.into_iter().enumerate() {
        match inputs.get(at) {
            Some(declared) if is_polymorphic(*declared) => {
                let target = specialized_type(*declared, resolution)?;
                let untyped_literal = is_untyped(at) && matches!(value, Value::Text(_));
                match coerced(&value, target, untyped_literal) {
                    Some(value) => converted.push(value),
                    None if untyped_literal && is_given(at) => {
                        return Err(convert(value, target, Conversion::Explicit)
                            .err()
                            .unwrap_or_else(|| not_alike(&definition)));
                    }
                    None => return Err(not_alike(&definition)),
                }
            }
            _ => converted.push(value),
        }
    }
    let specialized = specialized_definition(tables, definition, resolution)?;
    Ok((specialized, converted))
}

fn specialized_definition(
    tables: &mut TableCache,
    generic: Arc<RoutineDefinition>,
    resolution: Resolution,
) -> SqlResult<Arc<RoutineDefinition>> {
    let known = &mut tables.routines.specializations;
    if let Some(found) = known.iter().find(|specialization| {
        specialization.resolution == resolution && Arc::ptr_eq(&specialization.generic, &generic)
    }) {
        return Ok(Arc::clone(&found.specialized));
    }
    let mut definition = (*generic).clone();
    for parameter in &mut definition.parameters {
        parameter.sql_type = specialized_type(parameter.sql_type, resolution)?;
    }
    definition.returns = match definition.returns {
        RoutineReturns::One(ReturnType::Type(sql_type)) => {
            RoutineReturns::One(ReturnType::Type(specialized_type(sql_type, resolution)?))
        }
        RoutineReturns::Set(ReturnType::Type(sql_type)) => {
            RoutineReturns::Set(ReturnType::Type(specialized_type(sql_type, resolution)?))
        }
        RoutineReturns::Table(columns) => {
            let mut typed = Vec::with_capacity(columns.len());
            for (name, sql_type) in columns {
                typed.push((name, specialized_type(sql_type, resolution)?));
            }
            RoutineReturns::Table(typed)
        }
        other => other,
    };
    let specialized = Arc::new(definition);
    let known = &mut tables.routines.specializations;
    if known.len() >= CACHED_SPECIALIZATIONS {
        known.clear();
    }
    known.push(Specialization {
        generic,
        resolution,
        specialized: Arc::clone(&specialized),
    });
    Ok(specialized)
}

pub(crate) fn check_result(definition: &RoutineDefinition) -> SqlResult<()> {
    let inputs: Vec<Role> = definition
        .parameters
        .iter()
        .filter(|parameter| parameter.mode.is_input())
        .filter_map(|parameter| Role::of(parameter.sql_type))
        .collect();
    let outputs = definition
        .parameters
        .iter()
        .filter(|parameter| !parameter.mode.is_input())
        .map(|parameter| parameter.sql_type)
        .chain(returned_types(&definition.returns));
    for output in outputs {
        let Some(role) = Role::of(output) else {
            continue;
        };
        if inputs
            .iter()
            .any(|input| input.is_compatible() == role.is_compatible())
        {
            continue;
        }
        let family = if role.is_compatible() {
            "anycompatible, anycompatiblearray or anycompatiblenonarray"
        } else {
            "anyelement, anyarray, anynonarray or anyenum"
        };
        return Err(SqlError::new(
            SqlState::INVALID_FUNCTION_DEFINITION,
            "cannot determine result data type",
        )
        .with_detail(format!(
            "A result of type {} requires at least one input of type {family}.",
            output.name()
        )));
    }
    Ok(())
}
