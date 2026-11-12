use crate::encoding::Column;
use crate::error::SqlResult;
use crate::types::{SqlType, Value, compare_values};
use std::cmp::Ordering;
use std::collections::HashMap;

pub(crate) const SAMPLE_ROWS: usize = 30_000;
const STATISTICS_TARGET: usize = 100;
const DISTINCT_AS_FRACTION_ABOVE: f64 = 0.1;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ColumnStatistics {
    pub(crate) column: String,
    pub(crate) null_fraction: f32,
    pub(crate) average_width: i32,
    pub(crate) distinct: f32,
    pub(crate) most_common_values: Vec<String>,
    pub(crate) most_common_frequencies: Vec<f32>,
    pub(crate) histogram_bounds: Vec<String>,
    pub(crate) correlation: Option<f32>,
}

pub(crate) struct RowSample {
    seen: u64,
    state: u64,
    rows: Vec<(u64, Vec<Value>)>,
}

impl RowSample {
    pub(crate) fn new(seed: u64) -> Self {
        Self {
            seen: 0,
            state: seed | 1,
            rows: Vec::new(),
        }
    }

    fn next_random(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    pub(crate) fn offer(&mut self, row: Vec<Value>) {
        let position = self.seen;
        self.seen += 1;
        if self.rows.len() < SAMPLE_ROWS {
            self.rows.push((position, row));
            return;
        }
        let pick = self.next_random() % self.seen;
        if let Ok(slot) = usize::try_from(pick)
            && slot < SAMPLE_ROWS
        {
            self.rows[slot] = (position, row);
        }
    }

    pub(crate) fn total_rows(&self) -> u64 {
        self.seen
    }

    pub(crate) fn statistics(&self, columns: &[Column]) -> Vec<ColumnStatistics> {
        columns
            .iter()
            .enumerate()
            .map(|(at, column)| self.column_statistics(at, column))
            .collect()
    }

    fn column_statistics(&self, at: usize, column: &Column) -> ColumnStatistics {
        let sampled = self.rows.len();
        let mut present: Vec<(u64, &Value)> = Vec::with_capacity(sampled);
        let mut width_total = 0u64;
        for (position, row) in &self.rows {
            match row.get(at) {
                None | Some(Value::Null) => {}
                Some(value) => {
                    width_total += value_width(&column.sql_type, value);
                    present.push((*position, value));
                }
            }
        }
        let nulls = sampled - present.len();
        let null_fraction = if sampled == 0 {
            0.0
        } else {
            nulls as f64 / sampled as f64
        };
        let average_width = if present.is_empty() {
            0
        } else {
            i32::try_from(width_total / present.len() as u64).unwrap_or(i32::MAX)
        };
        let mut sorted: Vec<&Value> = present.iter().map(|(_, value)| *value).collect();
        sorted.sort_by(|left, right| compare_values(left, right));
        let groups = equal_runs(&sorted);
        let distinct = estimate_distinct(
            present.len(),
            sampled,
            self.seen,
            &groups,
            null_fraction,
        );
        let (most_common, rest) = most_common_values(&groups, sampled, present.len());
        let histogram_bounds = histogram(&rest);
        let correlation = correlation(&present);
        ColumnStatistics {
            column: column.name.clone(),
            null_fraction: null_fraction as f32,
            average_width,
            distinct,
            most_common_values: most_common
                .iter()
                .map(|(value, _)| text_of(value))
                .collect(),
            most_common_frequencies: most_common
                .iter()
                .map(|(_, frequency)| *frequency as f32)
                .collect(),
            histogram_bounds,
            correlation,
        }
    }
}

fn value_width(sql_type: &SqlType, value: &Value) -> u64 {
    match sql_type {
        SqlType::Boolean => 1,
        SqlType::SmallInt => 2,
        SqlType::Integer | SqlType::Real | SqlType::Date => 4,
        SqlType::BigInt | SqlType::Double | SqlType::Timestamp(_) | SqlType::TimestampTz(_) => 8,
        SqlType::Uuid => 16,
        _ => value.to_text().map_or(0, |text| text.len() as u64),
    }
}

fn text_of(value: &Value) -> String {
    value.to_text().unwrap_or_default()
}

fn equal_runs<'a>(sorted: &[&'a Value]) -> Vec<(&'a Value, usize)> {
    let mut runs: Vec<(&'a Value, usize)> = Vec::new();
    for value in sorted {
        match runs.last_mut() {
            Some((last, count)) if compare_values(*last, *value) == Ordering::Equal => *count += 1,
            _ => runs.push((value, 1)),
        }
    }
    runs
}

fn estimate_distinct(
    present: usize,
    sampled: usize,
    total: u64,
    groups: &[(&Value, usize)],
    null_fraction: f64,
) -> f32 {
    if present == 0 {
        return 0.0;
    }
    let seen_distinct = groups.len() as f64;
    let seen_once = groups.iter().filter(|(_, count)| *count == 1).count() as f64;
    let total_present = (total as f64 * (1.0 - null_fraction)).max(present as f64);
    let estimate = if sampled as u64 >= total || seen_once == 0.0 {
        seen_distinct
    } else if seen_once as usize == present {
        -1.0 * (1.0 - null_fraction)
    } else {
        let n = present as f64;
        let denominator = n - seen_once + seen_once * n / total_present;
        if denominator <= 0.0 {
            seen_distinct
        } else {
            (n * seen_distinct / denominator)
                .max(seen_distinct)
                .min(total_present)
        }
    };
    if estimate < 0.0 {
        return estimate as f32;
    }
    if total > 0 && estimate > DISTINCT_AS_FRACTION_ABOVE * total as f64 {
        return -((estimate / total as f64).min(1.0)) as f32;
    }
    estimate.round() as f32
}

type CommonValues<'a> = Vec<(&'a Value, f64)>;

fn most_common_values<'a>(
    groups: &[(&'a Value, usize)],
    sampled: usize,
    present: usize,
) -> (CommonValues<'a>, Vec<&'a Value>) {
    if groups.is_empty() || sampled == 0 {
        return (Vec::new(), Vec::new());
    }
    let average = present as f64 / groups.len() as f64;
    let keep_all = groups.len() <= STATISTICS_TARGET && groups.iter().all(|(_, count)| *count > 1);
    let mut candidates: Vec<(usize, &(&'a Value, usize))> = groups
        .iter()
        .enumerate()
        .filter(|(_, (_, count))| keep_all || (*count > 1 && *count as f64 > average * 1.25))
        .collect();
    candidates.sort_by(|left, right| right.1.1.cmp(&left.1.1).then(left.0.cmp(&right.0)));
    candidates.truncate(STATISTICS_TARGET);
    let chosen: std::collections::HashSet<usize> = candidates.iter().map(|(at, _)| *at).collect();
    let common = candidates
        .iter()
        .map(|(_, (value, count))| (*value, *count as f64 / sampled as f64))
        .collect();
    let rest = groups
        .iter()
        .enumerate()
        .filter(|(at, _)| !chosen.contains(at))
        .flat_map(|(_, (value, count))| std::iter::repeat_n(*value, *count))
        .collect();
    (common, rest)
}

fn histogram(rest: &[&Value]) -> Vec<String> {
    let mut distinct = 0usize;
    for (at, value) in rest.iter().enumerate() {
        if at == 0 || compare_values(rest[at - 1], value) != Ordering::Equal {
            distinct += 1;
        }
    }
    if distinct < 2 {
        return Vec::new();
    }
    let buckets = STATISTICS_TARGET.min(distinct - 1);
    let last = rest.len() - 1;
    let mut bounds: Vec<String> = Vec::with_capacity(buckets + 1);
    for step in 0..=buckets {
        let at = step * last / buckets;
        let text = text_of(rest[at]);
        if bounds.last() != Some(&text) {
            bounds.push(text);
        }
    }
    bounds
}

fn correlation(present: &[(u64, &Value)]) -> Option<f32> {
    if present.len() < 2 {
        return None;
    }
    let mut by_value: Vec<usize> = (0..present.len()).collect();
    by_value.sort_by(|left, right| {
        compare_values(present[*left].1, present[*right].1)
            .then(present[*left].0.cmp(&present[*right].0))
    });
    let mut value_rank = vec![0f64; present.len()];
    for (rank, at) in by_value.iter().enumerate() {
        value_rank[*at] = rank as f64;
    }
    let mut by_position: Vec<usize> = (0..present.len()).collect();
    by_position.sort_by_key(|at| present[*at].0);
    let mut position_rank = vec![0f64; present.len()];
    for (rank, at) in by_position.iter().enumerate() {
        position_rank[*at] = rank as f64;
    }
    let n = present.len() as f64;
    let mean = (n - 1.0) / 2.0;
    let mut covariance = 0.0;
    let mut spread = 0.0;
    for at in 0..present.len() {
        let x = position_rank[at] - mean;
        let y = value_rank[at] - mean;
        covariance += x * y;
        spread += x * x;
    }
    if spread == 0.0 {
        return None;
    }
    Some((covariance / spread).clamp(-1.0, 1.0) as f32)
}

pub(crate) fn encode_into(out: &mut Vec<u8>, statistics: &[ColumnStatistics]) {
    push_u32(out, statistics.len());
    for column in statistics {
        push_text(out, &column.column);
        out.extend_from_slice(&column.null_fraction.to_le_bytes());
        out.extend_from_slice(&column.average_width.to_le_bytes());
        out.extend_from_slice(&column.distinct.to_le_bytes());
        match column.correlation {
            Some(value) => {
                out.push(1);
                out.extend_from_slice(&value.to_le_bytes());
            }
            None => out.push(0),
        }
        push_u32(out, column.most_common_values.len());
        for (value, frequency) in column
            .most_common_values
            .iter()
            .zip(&column.most_common_frequencies)
        {
            push_text(out, value);
            out.extend_from_slice(&frequency.to_le_bytes());
        }
        push_u32(out, column.histogram_bounds.len());
        for bound in &column.histogram_bounds {
            push_text(out, bound);
        }
    }
}

fn push_u32(out: &mut Vec<u8>, value: usize) {
    out.extend_from_slice(&u32::try_from(value).unwrap_or(u32::MAX).to_le_bytes());
}

fn push_text(out: &mut Vec<u8>, text: &str) {
    push_u32(out, text.len());
    out.extend_from_slice(text.as_bytes());
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> SqlResult<&'a [u8]> {
        if self.bytes.len() < count {
            return Err(crate::encoding::corrupt("column statistics"));
        }
        let (taken, rest) = self.bytes.split_at(count);
        self.bytes = rest;
        Ok(taken)
    }

    fn word(&mut self) -> SqlResult<[u8; 4]> {
        let mut word = [0u8; 4];
        word.copy_from_slice(self.take(4)?);
        Ok(word)
    }

    fn count(&mut self) -> SqlResult<usize> {
        let count = u32::from_le_bytes(self.word()?) as usize;
        if count > self.bytes.len() {
            return Err(crate::encoding::corrupt("column statistics"));
        }
        Ok(count)
    }

    fn text(&mut self) -> SqlResult<String> {
        let length = self.count()?;
        let bytes = self.take(length)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| crate::encoding::corrupt("column statistics"))
    }

    fn real(&mut self) -> SqlResult<f32> {
        Ok(f32::from_le_bytes(self.word()?))
    }
}

pub(crate) fn decode(bytes: &[u8]) -> SqlResult<Vec<ColumnStatistics>> {
    let mut reader = Reader { bytes };
    let columns = reader.count()?;
    let mut decoded = Vec::with_capacity(columns);
    for _ in 0..columns {
        let column = reader.text()?;
        let null_fraction = reader.real()?;
        let average_width = i32::from_le_bytes(reader.word()?);
        let distinct = reader.real()?;
        let correlation = match reader.take(1)? {
            [0] => None,
            [1] => Some(reader.real()?),
            _ => return Err(crate::encoding::corrupt("column statistics")),
        };
        let common = reader.count()?;
        let mut most_common_values = Vec::with_capacity(common);
        let mut most_common_frequencies = Vec::with_capacity(common);
        for _ in 0..common {
            most_common_values.push(reader.text()?);
            most_common_frequencies.push(reader.real()?);
        }
        let bounds = reader.count()?;
        let mut histogram_bounds = Vec::with_capacity(bounds);
        for _ in 0..bounds {
            histogram_bounds.push(reader.text()?);
        }
        decoded.push(ColumnStatistics {
            column,
            null_fraction,
            average_width,
            distinct,
            most_common_values,
            most_common_frequencies,
            histogram_bounds,
            correlation,
        });
    }
    if !reader.bytes.is_empty() {
        return Err(crate::encoding::corrupt("column statistics"));
    }
    Ok(decoded)
}

pub(crate) type EveryColumnStatistics = HashMap<String, Vec<ColumnStatistics>>;

#[cfg(test)]
#[path = "column_statistics_tests.rs"]
mod tests;
