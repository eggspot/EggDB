use crate::ast::{BinaryOperator, Expr, ExprKind, Name};
use crate::dialect::Dialect;
use crate::encoding::TableDefinition;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope};
use crate::execute::TableCache;
use crate::partitions::{
    self, BoundText, IntervalPartitioning, PartitionKey, PartitionStrategy, checked_range,
    compare_bound_texts,
};
use crate::store::KeyValueWrite;
use crate::types::{Conversion, SqlType, Value, convert};
use std::cmp::Ordering;

const MAX_INTERVAL_STEPS: u64 = 1_048_575;

fn constant(value: Value) -> Expr {
    Expr {
        kind: ExprKind::Constant {
            sql_type: value.sql_type(),
            value,
        },
        position: 0,
    }
}

fn binary(operator: BinaryOperator, left: Expr, right: Expr) -> Expr {
    Expr {
        kind: ExprKind::Binary(operator, Box::new(left), Box::new(right)),
        position: 0,
    }
}

fn not_constant(position: usize) -> SqlError {
    SqlError::new(
        SqlState::INVALID_TABLE_DEFINITION,
        "Interval expression is not a constant of the correct type",
    )
    .with_oracle_code(14752)
    .at(position)
}

fn text_of(value: Value, position: usize) -> SqlResult<BoundText> {
    match convert(value, SqlType::Text, Conversion::Explicit)? {
        Value::Text(text) => Ok(BoundText::Value(text)),
        _ => Err(not_constant(position)),
    }
}

struct Grid {
    interval: Expr,
    transition: Value,
    sql_type: SqlType,
    types: Vec<SqlType>,
}

impl Grid {
    fn new(
        interval: &IntervalPartitioning,
        sql_type: SqlType,
        position: usize,
    ) -> SqlResult<Self> {
        let expr = partitions::parse_key_expression(&interval.text, Dialect::Oracle)
            .map_err(|_| not_constant(position))?;
        let Some(BoundText::Value(transition)) = interval.transition.first() else {
            return Err(crate::encoding::corrupt("interval partitioning"));
        };
        let transition = convert(
            Value::Text(transition.clone()),
            sql_type,
            Conversion::Explicit,
        )?;
        Ok(Self {
            interval: expr,
            transition,
            sql_type,
            types: vec![sql_type],
        })
    }

    fn edge(&self, steps: u64, position: usize) -> SqlResult<BoundText> {
        let count = i64::try_from(steps).map_err(|_| not_constant(position))?;
        let expr = binary(
            BinaryOperator::Add,
            constant(self.transition.clone()),
            binary(
                BinaryOperator::Multiply,
                self.interval.clone(),
                constant(Value::BigInt(count)),
            ),
        );
        let evaluator = Evaluator {
            scope: Scope::empty(),
            row: &[],
            correlation: None,
        };
        let value = evaluator
            .eval(&expr, Some(self.sql_type))
            .and_then(|value| convert(value, self.sql_type, Conversion::Assignment))
            .map_err(|_| not_constant(position))?;
        text_of(value, position)
    }

    fn above(&self, edge: &BoundText, key: &BoundText) -> bool {
        compare_bound_texts(
            std::slice::from_ref(edge),
            std::slice::from_ref(key),
            &self.types,
        ) == Ordering::Greater
    }

    fn transition_text(&self, position: usize) -> SqlResult<BoundText> {
        text_of(self.transition.clone(), position)
    }
}

fn outside_maximum(position: usize) -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        "partitioning key maps to a partition outside maximum permitted number of partitions",
    )
    .with_oracle_code(14300)
    .at(position)
}

pub(crate) fn validated(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    table: &TableDefinition,
    key: &PartitionKey,
    text: &str,
    position: usize,
) -> SqlResult<IntervalPartitioning> {
    if key.strategy != PartitionStrategy::Range {
        return Err(SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            "INTERVAL is only for range partitioning",
        )
        .with_oracle_code(14753)
        .at(position));
    }
    if key.parts.len() != 1 {
        return Err(SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            "Range partitioned table with INTERVAL clause has more than one column",
        )
        .with_oracle_code(14750)
        .at(position));
    }
    let types = partitions::key_types(table, key)?;
    let catalog = partitions::catalog(writer, tables)?;
    let Some(transition) = partitions::highest_range_end(&catalog, &table.name, &types)? else {
        return Err(SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            "an interval partitioned table needs at least one range partition",
        )
        .with_oracle_code(14757)
        .at(position));
    };
    if transition.iter().any(|edge| !matches!(edge, BoundText::Value(_))) {
        return Err(SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            "MAXVALUE partition cannot be specified for Interval partitioned objects",
        )
        .with_oracle_code(14761)
        .at(position));
    }
    let interval = IntervalPartitioning {
        text: text.to_owned(),
        transition,
    };
    let sql_type = types
        .first()
        .copied()
        .ok_or_else(|| crate::encoding::corrupt("partition key"))?;
    let grid = Grid::new(&interval, sql_type, position)?;
    let first = grid.edge(1, position)?;
    if !grid.above(&first, &grid.transition_text(position)?) {
        return Err(not_constant(position));
    }
    Ok(interval)
}

pub(crate) fn create_for(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &TableDefinition,
    interval: &IntervalPartitioning,
    key: &[Value],
    sql_type: SqlType,
    dialect: Dialect,
) -> SqlResult<bool> {
    let position = 0;
    let Some(value) = key.first() else {
        return Ok(false);
    };
    if matches!(value, Value::Null) {
        return Ok(false);
    }
    let grid = Grid::new(interval, sql_type, position)?;
    let wanted = text_of(value.clone(), position)?;
    if !grid.above(&wanted, &grid.transition_text(position)?)
        && compare_bound_texts(
            std::slice::from_ref(&wanted),
            &interval.transition,
            &grid.types,
        ) != Ordering::Equal
    {
        return Ok(false);
    }
    let mut low = 0u64;
    let mut high = 1u64;
    while !grid.above(&grid.edge(high, position)?, &wanted) {
        low = high;
        high = high.saturating_mul(2);
        if high > MAX_INTERVAL_STEPS.saturating_mul(2) {
            return Err(outside_maximum(position));
        }
    }
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        if grid.above(&grid.edge(middle, position)?, &wanted) {
            high = middle;
        } else {
            low = middle;
        }
    }
    if high > MAX_INTERVAL_STEPS {
        return Err(outside_maximum(position));
    }
    let from = if low == 0 {
        grid.transition_text(position)?
    } else {
        grid.edge(low, position)?
    };
    let to = grid.edge(high, position)?;
    let bound = checked_range(vec![from], vec![to], &grid.types, position)?;
    let name = system_name(writer, tables, parent)?;
    crate::partition_alter::create_child(writer, tables, parent, &name, bound, dialect)?;
    Ok(true)
}

fn system_name(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &TableDefinition,
) -> SqlResult<Name> {
    let catalog = partitions::catalog(writer, tables)?;
    let taken: Vec<String> = catalog
        .children_of(&parent.name)
        .into_iter()
        .map(|(name, _)| name.to_ascii_uppercase())
        .collect();
    let mut number = taken.len() + 1;
    loop {
        let name = crate::partition_ddl::named_child(parent, &format!("SYS_P{number}"), 0);
        if !taken.contains(&name.text.to_ascii_uppercase()) {
            return Ok(name);
        }
        number += 1;
    }
}
