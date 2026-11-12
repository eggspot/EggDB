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
const MICROS_FROM_1970_TO_2000: i64 = 946_684_800_000_000;
const NOW_IMPORT: (&str, &str) = ("eggdb", "now_micros");
const ABORT_IMPORT: (&str, &str) = ("env", "abort");

fn provided(module: &str, name: &str) -> Option<(Vec<ValueType>, Vec<ValueType>)> {
    match (module, name) {
        NOW_IMPORT => Some((Vec::new(), vec![ValueType::I64])),
        ABORT_IMPORT => Some((vec![ValueType::I32; 4], Vec::new())),
        _ => None,
    }
}

fn check_imports(module: &Module) -> SqlResult<()> {
    for (from, name) in module.imports() {
        let fits = provided(from, name).is_some_and(|wanted| module.import_types(from, name) == Some(wanted));
        if !fits {
            return Err(invalid(format!(
                "the WebAssembly module imports {from}.{name}, which EggDB does not provide: a module may import eggdb.now_micros() -> i64 (the statement's time in microseconds since 1970) and env.abort(i32, i32, i32, i32), which traps"
            )));
        }
    }
    Ok(())
}

fn host_for(now_micros: i64) -> Box<eggdb_wasm_vm::HostCall> {
    Box::new(move |from: &str, name: &str, _: &[eggdb_wasm_vm::Value]| match (from, name) {
        NOW_IMPORT => Ok(vec![eggdb_wasm_vm::Value::I64(now_micros)]),
        _ => Err(eggdb_wasm_vm::Trap::Unreachable),
    })
}

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

const ALLOCATOR_EXPORT: &str = "eggdb_alloc";

fn wasm_type_of(sql_type: SqlType) -> Option<ValueType> {
    match sql_type {
        SqlType::Boolean | SqlType::SmallInt | SqlType::Integer => Some(ValueType::I32),
        SqlType::BigInt => Some(ValueType::I64),
        SqlType::Real => Some(ValueType::F32),
        SqlType::Double => Some(ValueType::F64),
        _ => None,
    }
}

fn through_memory(sql_type: SqlType) -> bool {
    matches!(sql_type, SqlType::Text | SqlType::Varchar(_) | SqlType::Bytea)
}

fn wasm_types_of(sql_type: SqlType) -> Option<Vec<ValueType>> {
    if through_memory(sql_type) {
        return Some(vec![ValueType::I32, ValueType::I32]);
    }
    wasm_type_of(sql_type).map(|value_type| vec![value_type])
}

fn shown(value_types: &[ValueType]) -> String {
    if value_types.is_empty() {
        return "nothing".to_owned();
    }
    value_types
        .iter()
        .map(|value_type| match value_type {
            ValueType::I32 => "i32",
            ValueType::I64 => "i64",
            ValueType::F32 => "f32",
            ValueType::F64 => "f64",
        })
        .collect::<Vec<_>>()
        .join(", ")
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
/// `f64`; `text`, `varchar` and `bytea` as a pointer and a length, both `i32`, in the module's
/// memory, which also needs an `eggdb_alloc(i32) -> i32` export when an argument is passed
/// that way).
pub(crate) fn check(definition: &RoutineDefinition) -> SqlResult<()> {
    if definition.kind != RoutineKind::Function {
        return Err(SqlError::not_supported(
            "LANGUAGE wasm procedures are not supported yet; create a function",
        ));
    }
    let (written, export) = parts(definition);
    let module = parsed(written)?;
    check_imports(&module)?;
    let Some((params, results)) = module.export_types(export) else {
        return Err(invalid(format!(
            "the WebAssembly module exports no function named \"{export}\" over number types"
        )));
    };
    if definition
        .parameters
        .iter()
        .any(|parameter| parameter.mode.is_output() || parameter.default.is_some())
    {
        return Err(SqlError::not_supported(
            "OUT parameters and defaults in LANGUAGE wasm functions are not supported yet",
        ));
    }
    let mut wanted = Vec::new();
    for parameter in &definition.parameters {
        wanted.extend(wasm_types_of(parameter.sql_type).ok_or_else(|| {
            SqlError::not_supported(
                "LANGUAGE wasm functions take only boolean, smallint, integer, bigint, real, double precision, text, varchar and bytea arguments",
            )
        })?);
    }
    let returned = match result_type(definition)? {
        Some(sql_type) => wasm_types_of(sql_type).ok_or_else(|| {
            SqlError::not_supported(
                "LANGUAGE wasm functions return only boolean, smallint, integer, bigint, real, double precision, text, varchar or bytea",
            )
        })?,
        None => Vec::new(),
    };
    if params != wanted || results != returned {
        return Err(invalid(format!(
            "the WebAssembly function \"{export}\" takes ({}) and returns {}, which does not match the SQL declaration",
            shown(&params),
            shown(&results),
        )));
    }
    let passes_memory = definition
        .parameters
        .iter()
        .any(|parameter| through_memory(parameter.sql_type));
    if passes_memory
        && module.export_types(ALLOCATOR_EXPORT) != Some((vec![ValueType::I32], vec![ValueType::I32]))
    {
        return Err(invalid(format!(
            "a LANGUAGE wasm function that takes text or bytea needs the module to export {ALLOCATOR_EXPORT}(i32) -> i32, which returns where to write that many bytes"
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

fn memory_bytes(value: &Value, sql_type: SqlType) -> SqlResult<Vec<u8>> {
    let converted = crate::types::convert(value.clone(), sql_type, crate::types::Conversion::Assignment)?;
    Ok(match converted {
        Value::Bytea(bytes) => bytes,
        other => other.to_text().unwrap_or_default().into_bytes(),
    })
}

fn passed_in_memory(
    instance: &mut Instance,
    bytes: &[u8],
    limits: &Limits,
) -> SqlResult<[eggdb_wasm_vm::Value; 2]> {
    let length = i32::try_from(bytes.len()).map_err(|_| {
        SqlError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            "a LANGUAGE wasm argument is larger than 2 GiB",
        )
    })?;
    let at = match instance
        .call(ALLOCATOR_EXPORT, &[eggdb_wasm_vm::Value::I32(length)], limits)
        .map_err(trapped)?
    {
        Some(eggdb_wasm_vm::Value::I32(at)) => at,
        _ => {
            return Err(SqlError::new(
                SqlState::EXTERNAL_ROUTINE_EXCEPTION,
                "eggdb_alloc returned no address",
            ));
        }
    };
    instance.write_memory(at as u32, bytes).map_err(trapped)?;
    Ok([eggdb_wasm_vm::Value::I32(at), eggdb_wasm_vm::Value::I32(length)])
}

fn result_value(results: &[eggdb_wasm_vm::Value], sql_type: Option<SqlType>, instance: &Instance) -> SqlResult<Value> {
    let Some(sql_type) = sql_type else {
        return Ok(Value::Null);
    };
    if through_memory(sql_type) {
        let [eggdb_wasm_vm::Value::I32(at), eggdb_wasm_vm::Value::I32(length)] = results else {
            return Err(SqlError::new(
                SqlState::EXTERNAL_ROUTINE_EXCEPTION,
                "a LANGUAGE wasm function returning text or bytea must return an address and a length",
            ));
        };
        crate::memory::check_value_bytes(*length as u32 as usize)?;
        let bytes = instance
            .read_memory(*at as u32, *length as u32)
            .map_err(trapped)?
            .to_vec();
        return Ok(if sql_type == SqlType::Bytea {
            Value::Bytea(bytes)
        } else {
            Value::Text(String::from_utf8(bytes).map_err(|_| {
                SqlError::new(
                    SqlState::CHARACTER_NOT_IN_REPERTOIRE,
                    "a LANGUAGE wasm function returned text that is not UTF-8",
                )
            })?)
        });
    }
    let value = match (results.first().copied(), sql_type) {
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
    check_imports(&module)?;
    let now_micros = crate::datetime::statement_now().saturating_add(MICROS_FROM_1970_TO_2000);
    let mut instance = Instance::with_host((*module).clone(), &limits, host_for(now_micros)).map_err(trapped)?;
    let mut values = Vec::with_capacity(arguments.len());
    for (value, parameter) in arguments.iter().zip(&definition.parameters) {
        if value.is_null() {
            return Ok(Returned::Value(Value::Null));
        }
        if through_memory(parameter.sql_type) {
            let bytes = memory_bytes(value, parameter.sql_type)?;
            values.extend(passed_in_memory(&mut instance, &bytes, &limits)?);
        } else {
            values.push(argument(value, parameter.sql_type)?);
        }
    }
    let results = instance.call_values(export, &values, &limits).map_err(trapped)?;
    Ok(Returned::Value(result_value(&results, result_type(definition)?, &instance)?))
}

fn trapped(trap: eggdb_wasm_vm::Trap) -> SqlError {
    SqlError::new(SqlState::EXTERNAL_ROUTINE_EXCEPTION, trap.to_string())
}

#[cfg(test)]
#[path = "wasm_routines_tests.rs"]
mod tests;
