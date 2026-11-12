use crate::error::{SqlError, SqlResult, SqlState};
use crate::plpgsql::Returned;
use crate::routines::{ReturnType, RoutineDefinition, RoutineKind, RoutineReturns};
use crate::types::{SqlType, Value};
use eggdb_wasm_vm::{Instance, Limits, Module, ValueType};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Separates a WebAssembly module's bytes from the export a routine calls in its stored body:
/// `AS 'module', 'export'`.
pub(crate) const SYMBOL_SEPARATOR: char = '\0';
const MOST_CACHED_MODULES: usize = 32;
const MOST_MODULE_BYTES: usize = 64 << 20;

thread_local! {
    static MODULES: RefCell<HashMap<(usize, u64), (String, Rc<Module>)>> =
        RefCell::new(HashMap::new());
}

fn invalid(message: impl Into<String>) -> SqlError {
    SqlError::new(SqlState::INVALID_FUNCTION_DEFINITION, message)
}

fn parts(definition: &RoutineDefinition) -> (&str, &str) {
    match definition.body.split_once(SYMBOL_SEPARATOR) {
        Some((module, export)) => (module, export),
        None => (
            definition.body.as_str(),
            crate::schemas::base_name(&definition.name),
        ),
    }
}

fn module_bytes(written: &str) -> SqlResult<Vec<u8>> {
    let trimmed = written.trim();
    let hex = trimmed
        .strip_prefix("\\x")
        .or_else(|| trimmed.strip_prefix("0x"))
        .ok_or_else(|| {
            invalid("a LANGUAGE wasm body is the module's bytes in hex, written '\\x0061736d...'")
        })?;
    if hex.len() % 2 != 0 || hex.len() / 2 > MOST_MODULE_BYTES {
        return Err(invalid(
            "a LANGUAGE wasm module's hex has an odd number of digits or is larger than 64 MiB",
        ));
    }
    hex.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|digits| u8::from_str_radix(digits, 16).ok())
                .ok_or_else(|| invalid("a LANGUAGE wasm module's hex has a digit that is not hex"))
        })
        .collect()
}

fn cache_key(written: &str) -> (usize, u64) {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    written.hash(&mut hasher);
    (written.len(), hasher.finish())
}

fn parsed(written: &str) -> SqlResult<Rc<Module>> {
    let key = cache_key(written);
    let cached = MODULES.with(|modules| {
        modules
            .borrow()
            .get(&key)
            .filter(|(text, _)| text == written)
            .map(|(_, module)| Rc::clone(module))
    });
    if let Some(found) = cached {
        return Ok(found);
    }
    let bytes = module_bytes(written)?;
    let module = Rc::new(Module::parse(&bytes).map_err(|error| invalid(error.to_string()))?);
    MODULES.with(|modules| {
        let mut modules = modules.borrow_mut();
        if modules.len() >= MOST_CACHED_MODULES {
            modules.clear();
        }
        modules.insert(key, (written.to_owned(), Rc::clone(&module)));
    });
    Ok(module)
}

fn wasm_type_of(sql_type: SqlType) -> Option<ValueType> {
    match sql_type {
        SqlType::Boolean | SqlType::SmallInt | SqlType::Integer => Some(ValueType::I32),
        SqlType::BigInt => Some(ValueType::I64),
        SqlType::Real => Some(ValueType::F32),
        SqlType::Double => Some(ValueType::F64),
        _ => None,
    }
}

fn shown(value_type: Option<ValueType>) -> &'static str {
    match value_type {
        Some(ValueType::I32) => "i32",
        Some(ValueType::I64) => "i64",
        Some(ValueType::F32) => "f32",
        Some(ValueType::F64) => "f64",
        None => "nothing",
    }
}

fn result_type(definition: &RoutineDefinition) -> SqlResult<Option<SqlType>> {
    match &definition.returns {
        RoutineReturns::One(ReturnType::Type(sql_type)) => Ok(Some(*sql_type)),
        RoutineReturns::One(ReturnType::Void) | RoutineReturns::Nothing => Ok(None),
        _ => Err(SqlError::not_supported(
            "a LANGUAGE wasm function returning a set, a row or a trigger is not supported yet",
        )),
    }
}

/// Checks a `LANGUAGE wasm` routine when it is created: its module parses, exports the named
/// function, and that function's WebAssembly types match the routine's SQL types (`boolean`,
/// `smallint` and `integer` as `i32`, `bigint` as `i64`, `real` as `f32`, `double precision` as
/// `f64`).
pub(crate) fn check(definition: &RoutineDefinition) -> SqlResult<()> {
    if definition.kind != RoutineKind::Function {
        return Err(SqlError::not_supported(
            "LANGUAGE wasm procedures are not supported yet; create a function",
        ));
    }
    let (written, export) = parts(definition);
    let module = parsed(written)?;
    let Some((params, result)) = module.export_signature(export) else {
        return Err(invalid(format!(
            "the WebAssembly module exports no function named \"{export}\""
        )));
    };
    if module.export_result_count(export).unwrap_or(0) > 1 {
        return Err(SqlError::not_supported(format!(
            "the WebAssembly function \"{export}\" returns several values; a SQL function returns one"
        )));
    }
    if definition
        .parameters
        .iter()
        .any(|parameter| parameter.mode.is_output() || parameter.default.is_some())
    {
        return Err(SqlError::not_supported(
            "OUT parameters and defaults in LANGUAGE wasm functions are not supported yet",
        ));
    }
    let wanted: Vec<Option<ValueType>> = definition
        .parameters
        .iter()
        .map(|parameter| wasm_type_of(parameter.sql_type))
        .collect();
    if wanted.iter().any(Option::is_none) {
        return Err(SqlError::not_supported(
            "LANGUAGE wasm functions take only boolean, smallint, integer, bigint, real and double precision arguments",
        ));
    }
    let returned = match result_type(definition)? {
        Some(sql_type) => Some(wasm_type_of(sql_type).ok_or_else(|| {
            SqlError::not_supported(
                "LANGUAGE wasm functions return only boolean, smallint, integer, bigint, real or double precision",
            )
        })?),
        None => None,
    };
    let given: Vec<Option<ValueType>> = params.iter().copied().map(Some).collect();
    if given != wanted || result != returned {
        return Err(invalid(format!(
            "the WebAssembly function \"{export}\" takes ({}) and returns {}, which does not match the SQL declaration",
            params
                .iter()
                .map(|value_type| shown(Some(*value_type)))
                .collect::<Vec<_>>()
                .join(", "),
            shown(result),
        )));
    }
    Ok(())
}

fn argument(value: &Value, sql_type: SqlType) -> SqlResult<eggdb_wasm_vm::Value> {
    let converted = crate::types::convert(value.clone(), sql_type, crate::types::Conversion::Assignment)?;
    Ok(match converted {
        Value::Boolean(flag) => eggdb_wasm_vm::Value::I32(i32::from(flag)),
        Value::SmallInt(number) => eggdb_wasm_vm::Value::I32(i32::from(number)),
        Value::Integer(number) => eggdb_wasm_vm::Value::I32(number),
        Value::BigInt(number) => eggdb_wasm_vm::Value::I64(number),
        Value::Real(number) => eggdb_wasm_vm::Value::F32(number),
        Value::Double(number) => eggdb_wasm_vm::Value::F64(number),
        _ => {
            return Err(SqlError::new(
                SqlState::DATATYPE_MISMATCH,
                "a LANGUAGE wasm argument has a type WebAssembly cannot take",
            ));
        }
    })
}

fn result_value(value: Option<eggdb_wasm_vm::Value>, sql_type: Option<SqlType>) -> SqlResult<Value> {
    let Some(sql_type) = sql_type else {
        return Ok(Value::Null);
    };
    let value = match (value, sql_type) {
        (Some(eggdb_wasm_vm::Value::I32(number)), SqlType::Boolean) => Value::Boolean(number != 0),
        (Some(eggdb_wasm_vm::Value::I32(number)), SqlType::SmallInt) => {
            Value::SmallInt(i16::try_from(number).map_err(|_| {
                SqlError::new(
                    SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
                    "smallint out of range",
                )
            })?)
        }
        (Some(eggdb_wasm_vm::Value::I32(number)), _) => Value::Integer(number),
        (Some(eggdb_wasm_vm::Value::I64(number)), _) => Value::BigInt(number),
        (Some(eggdb_wasm_vm::Value::F32(number)), _) => Value::Real(number),
        (Some(eggdb_wasm_vm::Value::F64(number)), _) => Value::Double(number),
        (None, _) => Value::Null,
    };
    Ok(value)
}

/// Runs a `LANGUAGE wasm` function on a fresh instance of its module, within the default
/// fuel, memory and call-depth limits; a trap fails the statement with 38000.
pub(crate) fn invoke(definition: &RoutineDefinition, arguments: Vec<Value>) -> SqlResult<Returned> {
    if definition.strict && arguments.iter().any(Value::is_null) {
        return Ok(Returned::Value(Value::Null));
    }
    if arguments.len() != definition.parameters.len() {
        return Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!(
                "function {} takes {} arguments",
                definition.signature(),
                definition.parameters.len()
            ),
        ));
    }
    let (written, export) = parts(definition);
    let module = parsed(written)?;
    let limits = Limits::default();
    let mut instance = Instance::new((*module).clone(), &limits).map_err(trapped)?;
    let mut values = Vec::with_capacity(arguments.len());
    for (value, parameter) in arguments.iter().zip(&definition.parameters) {
        if value.is_null() {
            return Ok(Returned::Value(Value::Null));
        }
        values.push(argument(value, parameter.sql_type)?);
    }
    let answer = instance.call(export, &values, &limits).map_err(trapped)?;
    Ok(Returned::Value(result_value(answer, result_type(definition)?)?))
}

fn trapped(trap: eggdb_wasm_vm::Trap) -> SqlError {
    SqlError::new(SqlState::EXTERNAL_ROUTINE_EXCEPTION, trap.to_string())
}

#[cfg(test)]
#[path = "wasm_routines_tests.rs"]
mod tests;
