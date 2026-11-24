use crate::dialect::Dialect;
use crate::memory_store::MemoryStore;
use crate::native_functions::{NativeAggregate, NativeAggregateState, NativeFunctions, with_native_functions};
use crate::tests::render;
use crate::types::Value;
use crate::{Session, SqlError};
use std::sync::Arc;

fn run(session: &mut Session<'_>, sql: &str) -> Result<String, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs.pop().unwrap_or_default())
}

struct Product(Option<i64>);

impl NativeAggregateState for Product {
    fn step(&mut self, arguments: &[Value]) -> Result<(), String> {
        let factor = match arguments {
            [Value::Null] => return Ok(()),
            [value] => value.as_integer().ok_or("product_of takes integers")?,
            _ => return Err("product_of takes one argument".to_owned()),
        };
        self.0 = Some(self.0.unwrap_or(1).checked_mul(factor).ok_or("product_of overflowed")?);
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<Value, String> {
        Ok(self.0.map_or(Value::Null, Value::BigInt))
    }
}

struct Weighted {
    total: f64,
    weights: f64,
}

impl NativeAggregateState for Weighted {
    fn step(&mut self, arguments: &[Value]) -> Result<(), String> {
        if let [value, weight] = arguments
            && let (Some(value), Some(weight)) = (value.as_float(), weight.as_float())
        {
            self.total += value * weight;
            self.weights += weight;
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<Value, String> {
        Ok(if self.weights == 0.0 {
            Value::Null
        } else {
            Value::Double(self.total / self.weights)
        })
    }
}

fn defined() -> Arc<NativeFunctions> {
    let mut functions = NativeFunctions::default();
    let product: NativeAggregate = Arc::new(|| Box::new(Product(None)) as Box<dyn NativeAggregateState>);
    let weighted: NativeAggregate = Arc::new(|| {
        Box::new(Weighted {
            total: 0.0,
            weights: 0.0,
        }) as Box<dyn NativeAggregateState>
    });
    functions.define_aggregate("product_of", Some(1), product);
    functions.define_aggregate("weighted_mean", Some(2), weighted);
    Arc::new(functions)
}

#[test]
fn an_aggregate_the_program_defines_runs_per_group_with_distinct_and_filter() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Sqlite);
    let functions = defined();
    with_native_functions(&functions, || {
        run(
            &mut session,
            "CREATE TABLE readings (team TEXT, value INTEGER, weight INTEGER); \
             INSERT INTO readings VALUES ('a', 2, 1), ('a', 3, 3), ('a', 3, 0), ('b', 5, 1), ('b', NULL, 2), ('b', 4, 1);",
        )
        .unwrap();
        assert_eq!(
            run(
                &mut session,
                "SELECT team, product_of(value) AS p, weighted_mean(value, weight) AS w \
                 FROM readings GROUP BY team ORDER BY team",
            )
            .unwrap(),
            "team|p|w\na|18|2.75\nb|20|4.5\nSELECT 2"
        );
        assert_eq!(
            run(&mut session, "SELECT product_of(DISTINCT value) AS p FROM readings").unwrap(),
            "p\n120\nSELECT 1"
        );
        assert_eq!(
            run(
                &mut session,
                "SELECT product_of(value) FILTER (WHERE team = 'b') AS p FROM readings",
            )
            .unwrap(),
            "p\n20\nSELECT 1"
        );
        assert_eq!(
            run(&mut session, "SELECT product_of(value) AS p FROM readings WHERE team = 'none'").unwrap(),
            "p\nNULL\nSELECT 1"
        );
        let failed = run(&mut session, "SELECT product_of('x') FROM readings").unwrap_err();
        assert!(failed.to_string().contains("product_of takes integers"), "{failed}");
        assert!(run(&mut session, "SELECT product_of(value) OVER () FROM readings").is_err());
    });
    assert!(run(&mut session, "SELECT product_of(value) FROM readings").is_err());
}

#[test]
fn any_value_picks_a_value_of_each_group() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::PostgreSql);
    assert_eq!(
        run(
            &mut session,
            "SELECT g, any_value(v) AS v FROM (VALUES (1, NULL), (1, 'a'), (2, 'b')) AS t(g, v) \
             GROUP BY g ORDER BY g",
        )
        .unwrap(),
        "g|v\n1|a\n2|b\nSELECT 2"
    );
    assert_eq!(
        run(&mut session, "SELECT any_value(v) AS v FROM (VALUES (1)) AS t(v) WHERE v > 5").unwrap(),
        "v\nNULL\nSELECT 1"
    );
}

#[test]
fn mysql_any_value_lets_a_grouped_query_name_an_ungrouped_column() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    run(
        &mut session,
        "CREATE TABLE staff (team VARCHAR(10), name VARCHAR(10)); \
         INSERT INTO staff VALUES ('a', 'ada'), ('b', 'bob'), ('b', 'bob');",
    )
    .unwrap();
    assert_eq!(
        run(
            &mut session,
            "SELECT team, ANY_VALUE(name) AS name, COUNT(*) AS n FROM staff GROUP BY team ORDER BY team",
        )
        .unwrap(),
        "team|name|n\na|ada|1\nb|bob|2\nSELECT 2"
    );
    assert_eq!(
        run(&mut session, "SELECT ANY_VALUE(name) AS name FROM staff ORDER BY name").unwrap(),
        "name\nada\nbob\nbob\nSELECT 3"
    );
    assert!(run(&mut session, "SELECT mysql_any_value(name) FROM staff").is_err());
}
