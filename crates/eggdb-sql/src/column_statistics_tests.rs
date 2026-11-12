use super::*;

fn integer_column(name: &str) -> Column {
    Column {
        name: name.to_owned(),
        sql_type: SqlType::Integer,
        not_null: false,
        default: None,
        auto_increment: false,
    }
}

fn sampled(values: impl IntoIterator<Item = Value>) -> RowSample {
    let mut sample = RowSample::new(7);
    for value in values {
        sample.offer(vec![value]);
    }
    sample
}

#[test]
fn a_unique_ascending_column_is_distinct_everywhere_and_perfectly_correlated() {
    let sample = sampled((0..1000).map(Value::Integer));
    let statistics = sample.statistics(&[integer_column("id")]);
    let id = &statistics[0];
    assert_eq!(id.null_fraction, 0.0);
    assert_eq!(id.average_width, 4);
    assert_eq!(id.distinct, -1.0);
    assert!(id.most_common_values.is_empty());
    assert_eq!(id.histogram_bounds.first().map(String::as_str), Some("0"));
    assert_eq!(id.histogram_bounds.last().map(String::as_str), Some("999"));
    assert_eq!(id.histogram_bounds.len(), 101);
    assert_eq!(id.correlation, Some(1.0));
}

#[test]
fn repeated_values_become_most_common_values_with_frequencies() {
    let values = (0..900)
        .map(|at| Value::Integer(at % 3))
        .chain((0..100).map(|_| Value::Null));
    let statistics = sampled(values).statistics(&[integer_column("kind")]);
    let kind = &statistics[0];
    assert!((kind.null_fraction - 0.1).abs() < 1e-6);
    assert_eq!(kind.distinct, 3.0);
    assert_eq!(kind.most_common_values.len(), 3);
    assert!(kind
        .most_common_frequencies
        .iter()
        .all(|frequency| (frequency - 0.3).abs() < 1e-6));
    assert!(kind.histogram_bounds.is_empty());
}

#[test]
fn a_descending_column_correlates_negatively() {
    let statistics = sampled((0..500).rev().map(Value::Integer)).statistics(&[integer_column("n")]);
    assert_eq!(statistics[0].correlation, Some(-1.0));
}

#[test]
fn the_sample_stops_growing_at_its_limit_but_counts_every_row() {
    let sample = sampled((0..(SAMPLE_ROWS as i32 + 500)).map(Value::Integer));
    assert_eq!(sample.total_rows(), SAMPLE_ROWS as u64 + 500);
    assert_eq!(sample.rows.len(), SAMPLE_ROWS);
}

#[test]
fn statistics_round_trip_and_damaged_bytes_are_refused() {
    let statistics = sampled((0..50).map(|at| Value::Integer(at % 5)))
        .statistics(&[integer_column("a"), integer_column("b")]);
    let mut bytes = Vec::new();
    encode_into(&mut bytes, &statistics);
    assert_eq!(decode(&bytes).ok(), Some(statistics));
    assert!(decode(&bytes[..bytes.len() - 1]).is_err());
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(decode(&longer).is_err());
    assert!(decode(&[0xff, 0xff, 0xff, 0xff]).is_err());
}
