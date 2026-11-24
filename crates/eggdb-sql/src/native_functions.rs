use crate::ast::{Expr, ExprKind, Literal};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

/// A scalar function the embedding program supplies: it gets the call's argument values and
/// answers a value, or a message that fails the statement.
pub type NativeFunction = Arc<dyn Fn(&[Value]) -> Result<Value, String> + Send + Sync>;

/// One group's run of an aggregate the program supplies: `step` sees each input row's argument
/// values in turn, then `finish` answers the group's value, or a message that fails the
/// statement.
pub trait NativeAggregateState {
    /// Takes one input row's argument values.
    ///
    /// # Errors
    /// A message that fails the statement.
    fn step(&mut self, arguments: &[Value]) -> Result<(), String>;

    /// Answers the group's value once every row has been stepped.
    ///
    /// # Errors
    /// A message that fails the statement.
    fn finish(self: Box<Self>) -> Result<Value, String>;
}

/// An aggregate the embedding program supplies: each call starts a fresh state for one group.
pub type NativeAggregate = Arc<dyn Fn() -> Box<dyn NativeAggregateState> + Send + Sync>;

type Overloads<T> = HashMap<String, Vec<(Option<usize>, T)>>;

/// The functions and aggregates a program defined for its connection, by lower-case name and
/// argument count (`None` for any count). One defined for an exact count wins over one for any
/// count, and both win over EggDB's built-in function of the same name, as
/// `sqlite3_create_function`'s do. A scalar function and an aggregate never share a name and
/// count: defining one replaces the other.
#[derive(Clone, Default)]
pub struct NativeFunctions {
    functions: Overloads<NativeFunction>,
    aggregates: Overloads<NativeAggregate>,
}

pub(crate) const AGGREGATE_ARGUMENTS: &str = "native_aggregate_arguments";

fn put<T>(overloads: &mut Overloads<T>, name: String, arguments: Option<usize>, item: T) {
    let entries = overloads.entry(name).or_default();
    entries.retain(|(count, _)| *count != arguments);
    entries.push((arguments, item));
}

fn take<T>(overloads: &mut Overloads<T>, name: &str, arguments: Option<usize>) -> bool {
    let Some(entries) = overloads.get_mut(name) else {
        return false;
    };
    let before = entries.len();
    entries.retain(|(count, _)| *count != arguments);
    let removed = entries.len() != before;
    if entries.is_empty() {
        overloads.remove(name);
    }
    removed
}

fn best<'a, T>(overloads: &'a Overloads<T>, name: &str, arguments: usize) -> Option<&'a T> {
    let entries = overloads.get(name)?;
    entries
        .iter()
        .find(|(count, _)| *count == Some(arguments))
        .or_else(|| entries.iter().find(|(count, _)| count.is_none()))
        .map(|(_, item)| item)
}

impl std::fmt::Debug for NativeFunctions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeFunctions")
            .field("names", &self.functions.keys().collect::<Vec<_>>())
            .field("aggregates", &self.aggregates.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl NativeFunctions {
    /// Defines `name` for `arguments` arguments (`None` for any number), replacing an earlier
    /// definition for the same name and count.
    pub fn define(&mut self, name: &str, arguments: Option<usize>, function: NativeFunction) {
        let lowered = name.to_lowercase();
        take(&mut self.aggregates, &lowered, arguments);
        put(&mut self.functions, lowered, arguments, function);
    }

    /// Defines the aggregate `name` for `arguments` arguments (`None` for any number),
    /// replacing an earlier function or aggregate for the same name and count.
    pub fn define_aggregate(&mut self, name: &str, arguments: Option<usize>, aggregate: NativeAggregate) {
        let lowered = name.to_lowercase();
        take(&mut self.functions, &lowered, arguments);
        put(&mut self.aggregates, lowered, arguments, aggregate);
    }

    /// Removes the function or aggregate `name` defined for `arguments`; whether there was one.
    pub fn remove(&mut self, name: &str, arguments: Option<usize>) -> bool {
        let lowered = name.to_lowercase();
        let function = take(&mut self.functions, &lowered, arguments);
        let aggregate = take(&mut self.aggregates, &lowered, arguments);
        function || aggregate
    }

    /// Whether no function or aggregate is defined.
    pub fn is_empty(&self) -> bool {
        self.functions.is_empty() && self.aggregates.is_empty()
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

fn failed(message: String) -> SqlError {
    SqlError::new(SqlState::EXTERNAL_ROUTINE_EXCEPTION, message).with_sqlite_code(1)
}

pub(crate) fn call(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    let function = ACTIVE.with(|active| {
        active
            .borrow()
            .last()
            .and_then(|functions| best(&functions.functions, name, values.len()).cloned())
    })?;
    Some(function(values).map_err(failed))
}

fn active_aggregate(name: &str, arguments: usize) -> Option<NativeAggregate> {
    let lowered = name.to_lowercase();
    ACTIVE.with(|active| {
        active
            .borrow()
            .last()
            .and_then(|functions| best(&functions.aggregates, &lowered, arguments).cloned())
    })
}

pub(crate) fn aggregate_defined(name: &str) -> bool {
    let lowered = name.to_lowercase();
    ACTIVE.with(|active| {
        active
            .borrow()
            .last()
            .is_some_and(|functions| functions.aggregates.contains_key(&lowered))
    })
}

pub(crate) fn aggregate_call(name: String, inputs: Vec<Expr>, position: usize) -> Expr {
    let mut arguments = Vec::with_capacity(inputs.len() + 1);
    arguments.push(Expr {
        kind: ExprKind::Literal(Literal::String(name)),
        position,
    });
    arguments.extend(inputs);
    Expr {
        kind: ExprKind::Function {
            name: AGGREGATE_ARGUMENTS.to_owned(),
            arguments,
        },
        position,
    }
}

pub(crate) fn aggregate_parts(argument: &Expr) -> Option<(&str, &[Expr])> {
    let ExprKind::Function { name, arguments } = &argument.kind else {
        return None;
    };
    if name != AGGREGATE_ARGUMENTS {
        return None;
    }
    let [called, inputs @ ..] = arguments.as_slice() else {
        return None;
    };
    let ExprKind::Literal(Literal::String(called)) = &called.kind else {
        return None;
    };
    Some((called.as_str(), inputs))
}

pub(crate) fn finish_aggregate(name: &str, width: usize, rows: &[Vec<Value>]) -> SqlResult<Value> {
    let Some(aggregate) = active_aggregate(name, width) else {
        return Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("aggregate {name} with {width} arguments is no longer defined"),
        )
        .with_sqlite_code(1));
    };
    let mut state = aggregate();
    for row in rows {
        state.step(row).map_err(failed)?;
    }
    state.finish().map_err(failed)
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

    struct Total(i64);

    impl NativeAggregateState for Total {
        fn step(&mut self, arguments: &[Value]) -> Result<(), String> {
            match arguments {
                [Value::Integer(value)] => {
                    self.0 += i64::from(*value);
                    Ok(())
                }
                _ => Err("total takes integers".to_owned()),
            }
        }

        fn finish(self: Box<Self>) -> Result<Value, String> {
            Ok(Value::BigInt(self.0))
        }
    }

    fn total() -> NativeAggregate {
        Arc::new(|| Box::new(Total(0)) as Box<dyn NativeAggregateState>)
    }

    #[test]
    fn an_aggregate_steps_every_row_and_replaces_a_function_of_its_name() {
        let mut functions = NativeFunctions::default();
        functions.define("total", Some(1), constant(7));
        functions.define_aggregate("Total", Some(1), total());
        let shared = Arc::new(functions.clone());
        with_native_functions(&shared, || {
            assert!(aggregate_defined("TOTAL"));
            assert!(call("total", &[Value::Null]).is_none());
            let rows = vec![vec![Value::Integer(2)], vec![Value::Integer(5)]];
            assert_eq!(finish_aggregate("total", 1, &rows).ok(), Some(Value::BigInt(7)));
            assert_eq!(finish_aggregate("total", 1, &[]).ok(), Some(Value::BigInt(0)));
            assert!(finish_aggregate("total", 1, &[vec![Value::Null]]).is_err());
            assert!(finish_aggregate("total", 2, &rows).is_err());
        });
        assert!(!aggregate_defined("total"));
        assert!(functions.remove("total", Some(1)));
        assert!(functions.is_empty());
    }

    #[test]
    fn aggregate_parts_find_the_name_and_inputs_again() {
        let input = Expr {
            kind: ExprKind::Literal(Literal::Null),
            position: 3,
        };
        let call = aggregate_call("total".to_owned(), vec![input.clone()], 3);
        let (name, inputs) = aggregate_parts(&call).expect("parts");
        assert_eq!(name, "total");
        assert_eq!(inputs.len(), 1);
        assert!(aggregate_parts(&input).is_none());
    }
}
