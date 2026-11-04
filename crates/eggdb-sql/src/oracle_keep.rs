use crate::aggregate::WindowAggregate;
use crate::ast::AggregateFunction;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{Value, compare_values};
use std::cmp::Ordering;
use std::ops::Range;

pub(crate) const KEPT_AGGREGATES: [AggregateFunction; 11] = [
    AggregateFunction::Min,
    AggregateFunction::Max,
    AggregateFunction::Sum,
    AggregateFunction::Avg,
    AggregateFunction::Count,
    AggregateFunction::OracleVariance,
    AggregateFunction::OracleStddev,
    AggregateFunction::VarSamp,
    AggregateFunction::VarPop,
    AggregateFunction::StddevSamp,
    AggregateFunction::StddevPop,
];

fn items(value: &Value) -> &[Value] {
    match value {
        Value::Array(array) => &array.items,
        _ => &[],
    }
}

fn same_key(keys: &[&[Value]], left: usize, right: usize) -> bool {
    keys.iter()
        .all(|column| match (column.get(left), column.get(right)) {
            (Some(left), Some(right)) if left.is_null() || right.is_null() => {
                left.is_null() && right.is_null()
            }
            (Some(left), Some(right)) => compare_values(left, right) == Ordering::Equal,
            _ => false,
        })
}

fn kept_rows(keys: &[&[Value]], count: usize, last: bool) -> Range<usize> {
    if count == 0 {
        return 0..0;
    }
    if last {
        let mut start = count - 1;
        while start > 0 && same_key(keys, start - 1, count - 1) {
            start -= 1;
        }
        return start..count;
    }
    let mut end = 1;
    while end < count && same_key(keys, 0, end) {
        end += 1;
    }
    0..end
}

pub(crate) fn kept(
    function: &Value,
    last: &Value,
    values: &Value,
    keys: &[Value],
) -> SqlResult<Value> {
    let Some(function) = function.character_text().and_then(|name| {
        KEPT_AGGREGATES
            .into_iter()
            .find(|function| function.name() == name)
    }) else {
        return Err(SqlError::new(
            SqlState::INTERNAL_ERROR,
            "a KEEP aggregate lost its function",
        ));
    };
    let values = items(values);
    let keys: Vec<&[Value]> = keys.iter().map(items).collect();
    let rows = kept_rows(&keys, values.len(), matches!(last, Value::Boolean(true)));
    let argument_type = values.iter().find_map(Value::sql_type);
    let mut aggregate = WindowAggregate::new(function, argument_type, 0);
    for value in values.get(rows).unwrap_or(&[]) {
        aggregate.add(value.clone(), None)?;
    }
    aggregate.current()
}

#[cfg(test)]
mod tests {
    use super::kept;
    use crate::arrays::{ArrayElement, ArrayValue};
    use crate::types::Value;

    fn array(items: &[Option<i32>]) -> Value {
        Value::Array(Box::new(ArrayValue::new(
            ArrayElement::Integer,
            items
                .iter()
                .map(|item| item.map_or(Value::Null, Value::Integer))
                .collect(),
        )))
    }

    #[test]
    fn the_first_and_last_ranked_rows_are_aggregated_with_their_ties() {
        let salaries = array(&[Some(2500), Some(2600), Some(3000), Some(1000), Some(1100)]);
        let commissions = [array(&[Some(1), Some(1), Some(2), Some(5), None])];
        let name = |function: &str| Value::Text(function.to_owned());
        assert_eq!(
            kept(
                &name("min"),
                &Value::Boolean(false),
                &salaries,
                &commissions
            ),
            Ok(Value::Integer(2500))
        );
        assert_eq!(
            kept(
                &name("max"),
                &Value::Boolean(false),
                &salaries,
                &commissions
            ),
            Ok(Value::Integer(2600))
        );
        assert_eq!(
            kept(&name("max"), &Value::Boolean(true), &salaries, &commissions),
            Ok(Value::Integer(1100))
        );
        assert_eq!(
            kept(
                &name("count"),
                &Value::Boolean(false),
                &Value::Null,
                &[Value::Null]
            ),
            Ok(Value::BigInt(0))
        );
    }
}
