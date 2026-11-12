use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

/// A scalar function the embedding program supplies: it gets the call's argument values and
/// answers a value, or a message that fails the statement.
pub type NativeFunction = Arc<dyn Fn(&[Value]) -> Result<Value, String> + Send + Sync>;

/// The functions a program defined for its connection, by lower-case name and argument count
/// (`None` for any count). One defined for an exact count wins over one for any count, and
/// both win over EggDB's built-in function of the same name, as `sqlite3_create_function`'s do.
#[derive(Clone, Default)]
pub struct NativeFunctions {
    functions: HashMap<String, Vec<(Option<usize>, NativeFunction)>>,
}

impl std::fmt::Debug for NativeFunctions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeFunctions")
            .field("names", &self.functions.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl NativeFunctions {
    /// Defines `name` for `arguments` arguments (`None` for any number), replacing an earlier
    /// definition for the same name and count.
    pub fn define(&mut self, name: &str, arguments: Option<usize>, function: NativeFunction) {
        let overloads = self.functions.entry(name.to_lowercase()).or_default();
        overloads.retain(|(count, _)| *count != arguments);
        overloads.push((arguments, function));
    }

    /// Removes the definition of `name` for `arguments`; whether there was one.
    pub fn remove(&mut self, name: &str, arguments: Option<usize>) -> bool {
        let lowered = name.to_lowercase();
        let Some(overloads) = self.functions.get_mut(&lowered) else {
            return false;
        };
        let before = overloads.len();
        overloads.retain(|(count, _)| *count != arguments);
        let removed = overloads.len() != before;
        if overloads.is_empty() {
            self.functions.remove(&lowered);
        }
        removed
    }

    fn find(&self, name: &str, arguments: usize) -> Option<&NativeFunction> {
        let overloads = self.functions.get(name)?;
        overloads
            .iter()
            .find(|(count, _)| *count == Some(arguments))
            .or_else(|| overloads.iter().find(|(count, _)| count.is_none()))
            .map(|(_, function)| function)
    }

    /// Whether no function is defined.
    pub fn is_empty(&self) -> bool {
        self.functions.is_empty()
    }
}

thread_local! {
    static ACTIVE: RefCell<Vec<Arc<NativeFunctions>>> = const { RefCell::new(Vec::new()) };
}

struct Active;

impl Drop for Active {
    fn drop(&mut self) {
        ACTIVE.with(|active| {
            active.borrow_mut().pop();
        });
    }
}

/// Runs `body` with `functions` callable from the SQL it runs on this thread.
pub fn with_native_functions<T>(functions: &Arc<NativeFunctions>, body: impl FnOnce() -> T) -> T {
    if functions.is_empty() {
        return body();
    }
    ACTIVE.with(|active| active.borrow_mut().push(Arc::clone(functions)));
    let _active = Active;
    body()
}

pub(crate) fn call(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    let function = ACTIVE.with(|active| {
        active
            .borrow()
            .last()
            .and_then(|functions| functions.find(name, values.len()).cloned())
    })?;
    Some(function(values).map_err(|message| {
        SqlError::new(SqlState::EXTERNAL_ROUTINE_EXCEPTION, message).with_sqlite_code(1)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn constant(answer: i32) -> NativeFunction {
        Arc::new(move |_| Ok(Value::Integer(answer)))
    }

    #[test]
    fn an_exact_count_wins_over_any_count_and_names_ignore_case() {
        let mut functions = NativeFunctions::default();
        functions.define("Twice", None, constant(1));
        functions.define("twice", Some(2), constant(2));
        let functions = Arc::new(functions);
        with_native_functions(&functions, || {
            let two = [Value::Null, Value::Null];
            assert_eq!(call("twice", &two).map(Result::ok), Some(Some(Value::Integer(2))));
            assert_eq!(call("twice", &[]).map(Result::ok), Some(Some(Value::Integer(1))));
            assert!(call("other", &[]).is_none());
        });
        assert!(call("twice", &[]).is_none());
    }

    #[test]
    fn a_failing_function_fails_the_statement_and_removal_forgets_it() {
        let mut functions = NativeFunctions::default();
        functions.define("boom", Some(0), Arc::new(|_| Err("no".to_owned())));
        let shared = Arc::new(functions.clone());
        with_native_functions(&shared, || {
            assert!(matches!(call("boom", &[]), Some(Err(_))));
        });
        assert!(functions.remove("BOOM", Some(0)));
        assert!(functions.is_empty());
        assert!(!functions.remove("boom", Some(0)));
    }
}
