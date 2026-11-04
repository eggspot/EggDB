use super::{Failure, Labels, anchored, is_label_name, label_value};
use std::collections::{BTreeMap, BTreeSet};

const SMALL_DELTA_TOLERANCE: f64 = 1e-12;

/// Sets label `name` to `value` (removing it when `value` is empty), keeping `__name__` first
/// and the other labels sorted.
pub(crate) fn set_label(labels: &mut Labels, name: &str, value: String) {
    labels.retain(|(existing, _)| existing != name);
    if value.is_empty() {
        return;
    }
    let at = if name == "__name__" {
        0
    } else {
        labels
            .iter()
            .position(|(existing, _)| existing != "__name__" && existing.as_str() > name)
            .unwrap_or(labels.len())
    };
    labels.insert(at, (name.to_owned(), value));
}

fn extrapolated(
    samples: &[(i64, f64)],
    start: i64,
    end: i64,
    counter: bool,
    per_second: bool,
) -> Option<f64> {
    let (&first, &last) = (samples.first()?, samples.last()?);
    if samples.len() < 2 {
        return None;
    }
    let mut result = last.1 - first.1;
    if counter {
        for pair in samples.windows(2) {
            if pair[1].1 < pair[0].1 {
                result += pair[0].1;
            }
        }
    }
    let sampled = (last.0 - first.0) as f64 / 1000.0;
    let average = sampled / (samples.len() - 1) as f64;
    let mut to_start = (first.0 - start) as f64 / 1000.0;
    let to_end = (end - last.0) as f64 / 1000.0;
    if counter && result > 0.0 && first.1 >= 0.0 {
        let to_zero = sampled * (first.1 / result);
        if to_zero < to_start {
            to_start = to_zero;
        }
    }
    let threshold = average * 1.1;
    let mut interval = sampled;
    interval += if to_start < threshold {
        to_start
    } else {
        average / 2.0
    };
    interval += if to_end < threshold {
        to_end
    } else {
        average / 2.0
    };
    let mut factor = interval / sampled;
    if per_second {
        factor /= (end - start) as f64 / 1000.0;
    }
    Some(result * factor)
}

pub(crate) fn range_function(
    name: &str,
    samples: &[(i64, f64)],
    start: i64,
    end: i64,
) -> Result<Option<f64>, Failure> {
    let values = || samples.iter().map(|(_, value)| *value);
    Ok(match name {
        "rate" => extrapolated(samples, start, end, true, true),
        "increase" => extrapolated(samples, start, end, true, false),
        "delta" => extrapolated(samples, start, end, false, false),
        "irate" => match samples {
            [.., before, last] if last.0 > before.0 => {
                let change = if last.1 < before.1 {
                    last.1
                } else {
                    last.1 - before.1
                };
                Some(change / ((last.0 - before.0) as f64 / 1000.0))
            }
            _ => None,
        },
        "avg_over_time" => {
            (!samples.is_empty()).then(|| values().sum::<f64>() / samples.len() as f64)
        }
        "sum_over_time" => (!samples.is_empty()).then(|| values().sum()),
        "min_over_time" => values().reduce(f64::min),
        "max_over_time" => values().reduce(f64::max),
        "count_over_time" => (!samples.is_empty()).then_some(samples.len() as f64),
        "last_over_time" => samples.last().map(|(_, value)| *value),
        other => return Err(Failure::BadData(format!("unknown function {other}"))),
    })
}

pub(crate) fn scalar_function(name: &str, value: f64) -> Option<f64> {
    Some(match name {
        "abs" => value.abs(),
        "ceil" => value.ceil(),
        "floor" => value.floor(),
        "round" => value.round(),
        "sqrt" => value.sqrt(),
        "exp" => value.exp(),
        "ln" => value.ln(),
        "log2" => value.log2(),
        "log10" => value.log10(),
        _ => return None,
    })
}

fn variance(values: &[f64]) -> f64 {
    let count = values.len() as f64;
    let mean = values.iter().sum::<f64>() / count;
    values
        .iter()
        .map(|value| (value - mean) * (value - mean))
        .sum::<f64>()
        / count
}

/// Prometheus's `quantile`: the `quantile`-quantile of `values`, interpolated between the two
/// nearest ranks; `-Inf` below 0, `+Inf` above 1 and `NaN` for no values or a `NaN` quantile.
pub(crate) fn quantile(quantile: f64, mut values: Vec<f64>) -> f64 {
    if values.is_empty() || quantile.is_nan() {
        return f64::NAN;
    }
    if quantile < 0.0 {
        return f64::NEG_INFINITY;
    }
    if quantile > 1.0 {
        return f64::INFINITY;
    }
    values.sort_by(|a, b| match (a.is_nan(), b.is_nan()) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        (false, false) => a.total_cmp(b),
    });
    let last = values.len() - 1;
    let rank = quantile * last as f64;
    let lower = (rank.floor() as usize).min(last);
    let upper = (lower + 1).min(last);
    let weight = rank - rank.floor();
    let at = |index: usize| values.get(index).copied().unwrap_or(f64::NAN);
    at(lower) * (1.0 - weight) + at(upper) * weight
}

/// The value of aggregation `op` over one group's `values`; `parameter` is `quantile`'s φ.
pub(crate) fn aggregate_values(op: &str, parameter: f64, values: Vec<f64>) -> f64 {
    let count = values.len() as f64;
    match op {
        "sum" => values.iter().sum(),
        "avg" => values.iter().sum::<f64>() / count,
        "min" => values
            .iter()
            .copied()
            .reduce(|lowest, value| {
                if value < lowest || lowest.is_nan() {
                    value
                } else {
                    lowest
                }
            })
            .unwrap_or(f64::NAN),
        "max" => values
            .iter()
            .copied()
            .reduce(|highest, value| {
                if value > highest || highest.is_nan() {
                    value
                } else {
                    highest
                }
            })
            .unwrap_or(f64::NAN),
        "group" => 1.0,
        "stdvar" => variance(&values),
        "stddev" => variance(&values).sqrt(),
        "quantile" => quantile(parameter, values),
        _ => count,
    }
}

fn bucket_bound(text: &str) -> Option<f64> {
    match text.trim() {
        "+Inf" | "Inf" | "+inf" | "inf" => Some(f64::INFINITY),
        other => other.parse::<f64>().ok().filter(|bound| !bound.is_nan()),
    }
}

fn almost_equal(a: f64, b: f64, epsilon: f64) -> bool {
    if a == b {
        return true;
    }
    let absolute_sum = a.abs() + b.abs();
    let difference = (a - b).abs();
    if a == 0.0 || b == 0.0 || absolute_sum < f64::MIN_POSITIVE {
        return difference < epsilon * f64::MIN_POSITIVE;
    }
    difference / absolute_sum.min(f64::MAX) < epsilon
}

fn ensure_monotonic(buckets: &mut [(f64, f64)]) {
    let Some(mut previous) = buckets.first().map(|(_, count)| *count) else {
        return;
    };
    for bucket in buckets.iter_mut().skip(1) {
        let current = bucket.1;
        if current == previous {
            continue;
        }
        if almost_equal(previous, current, SMALL_DELTA_TOLERANCE) || current < previous {
            bucket.1 = previous;
            continue;
        }
        previous = current;
    }
}

/// Prometheus's quantile over one classic histogram's cumulative `(upper bound, count)`
/// buckets: linear interpolation inside the bucket the rank falls in, the second-highest bound
/// when it falls in the `+Inf` bucket, and `NaN` without a `+Inf` bucket, with fewer than two
/// buckets or with no observations.
pub(crate) fn bucket_quantile(quantile: f64, mut buckets: Vec<(f64, f64)>) -> f64 {
    if quantile.is_nan() {
        return f64::NAN;
    }
    if quantile < 0.0 {
        return f64::NEG_INFINITY;
    }
    if quantile > 1.0 {
        return f64::INFINITY;
    }
    buckets.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut merged: Vec<(f64, f64)> = Vec::with_capacity(buckets.len());
    for (bound, count) in buckets {
        match merged.last_mut() {
            Some(last) if last.0 == bound => last.1 += count,
            _ => merged.push((bound, count)),
        }
    }
    if merged
        .last()
        .is_none_or(|(bound, _)| *bound != f64::INFINITY)
    {
        return f64::NAN;
    }
    ensure_monotonic(&mut merged);
    if merged.len() < 2 {
        return f64::NAN;
    }
    let last = merged.len() - 1;
    let bound = |index: usize| merged.get(index).map_or(f64::NAN, |(bound, _)| *bound);
    let count = |index: usize| merged.get(index).map_or(f64::NAN, |(_, count)| *count);
    let observations = count(last);
    if observations == 0.0 {
        return f64::NAN;
    }
    let mut rank = quantile * observations;
    let found = merged
        .get(..last)
        .unwrap_or_default()
        .partition_point(|(_, count)| *count < rank);
    if found == last {
        return bound(last - 1);
    }
    if found == 0 && bound(0) <= 0.0 {
        return bound(0);
    }
    let bucket_end = bound(found);
    let mut in_bucket = count(found);
    let mut bucket_start = 0.0;
    if found > 0 {
        bucket_start = bound(found - 1);
        in_bucket -= count(found - 1);
        rank -= count(found - 1);
    }
    bucket_start + (bucket_end - bucket_start) * (rank / in_bucket)
}

/// `histogram_quantile`: the series of `buckets` grouped by their labels other than `le` (and
/// the metric name), each group's quantile computed from its buckets; series without a
/// readable `le` are left out.
pub(crate) fn histogram_quantile(quantile: f64, buckets: Vec<(Labels, f64)>) -> Vec<(Labels, f64)> {
    let mut groups: BTreeMap<Labels, Vec<(f64, f64)>> = BTreeMap::new();
    for (labels, count) in buckets {
        let Some(bound) = bucket_bound(label_value(&labels, "le")) else {
            continue;
        };
        let key: Labels = labels
            .into_iter()
            .filter(|(name, _)| name != "le" && name != "__name__")
            .collect();
        groups.entry(key).or_default().push((bound, count));
    }
    groups
        .into_iter()
        .map(|(labels, buckets)| (labels, bucket_quantile(quantile, buckets)))
        .collect()
}

/// Refuses a vector in which two elements have the same labels, as Prometheus refuses the
/// result of every function, aggregation and operator.
pub(crate) fn ensure_unique_label_sets(vector: &[(Labels, f64)]) -> Result<(), Failure> {
    let mut seen: BTreeSet<&Labels> = BTreeSet::new();
    if vector.iter().any(|(labels, _)| !seen.insert(labels)) {
        return Err(Failure::Execution(
            "vector cannot contain metrics with the same labelset".to_owned(),
        ));
    }
    Ok(())
}

fn unique_label_sets(vector: Vec<(Labels, f64)>) -> Result<Vec<(Labels, f64)>, Failure> {
    ensure_unique_label_sets(&vector)?;
    Ok(vector)
}

fn is_group_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Go's `regexp.Expand`, which Prometheus's `label_replace` uses: `$1` and `${1}` are capture
/// groups, `$$` is a dollar sign, a name that is no group's (`$1x` included) is empty, and a `$`
/// with no name after it stays as it is; `None` when the expansion would be longer than `limit`
/// bytes.
pub(crate) fn expand(template: &str, groups: &[Option<String>], limit: usize) -> Option<String> {
    let mut out = String::with_capacity(template.len().min(limit));
    let mut rest = template;
    let fits = |out: &String, more: usize| out.len().saturating_add(more) <= limit;
    while let Some(at) = rest.find('$') {
        if !fits(&out, at + 1) {
            return None;
        }
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        if let Some(stripped) = after.strip_prefix('$') {
            out.push('$');
            rest = stripped;
            continue;
        }
        let named = match after.strip_prefix('{') {
            Some(braced) => braced
                .find('}')
                .map(|end| (&braced[..end], &braced[end + 1..])),
            None => {
                let length = after
                    .bytes()
                    .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                    .count();
                Some((&after[..length], &after[length..]))
            }
        };
        match named {
            Some((name, remaining)) if is_group_name(name) => {
                if let Some(Some(text)) = name
                    .parse::<usize>()
                    .ok()
                    .and_then(|index| groups.get(index))
                {
                    if !fits(&out, text.len()) {
                        return None;
                    }
                    out.push_str(text);
                }
                rest = remaining;
            }
            _ => {
                out.push('$');
                rest = after;
            }
        }
    }
    if !fits(&out, rest.len()) {
        return None;
    }
    out.push_str(rest);
    Some(out)
}

/// The longest label value `label_replace` and `label_join` build, so that nesting them cannot
/// grow a value without bound.
pub(crate) const MAX_LABEL_VALUE_BYTES: usize = 1 << 20;

fn label_too_long(function: &str) -> Failure {
    Failure::Execution(format!(
        "{function}() would build a label value longer than {MAX_LABEL_VALUE_BYTES} bytes"
    ))
}

/// `label_replace`: for each series whose `source` label fully matches `pattern`, sets
/// `destination` to `replacement` with the capture groups expanded (removing it when that is
/// empty); other series pass unchanged. `hold` is given the bytes of each value it builds, which
/// are at most [`MAX_LABEL_VALUE_BYTES`].
pub(crate) fn label_replace(
    vector: Vec<(Labels, f64)>,
    destination: &str,
    replacement: &str,
    source: &str,
    pattern: &str,
    hold: &mut dyn FnMut(usize) -> Result<(), Failure>,
) -> Result<Vec<(Labels, f64)>, Failure> {
    if !is_label_name(destination) {
        return Err(Failure::BadData(format!(
            "invalid destination label name in label_replace(): {destination}"
        )));
    }
    let regex = anchored(pattern);
    let invalid = |error: String| {
        Failure::BadData(format!(
            "invalid regular expression in label_replace(): {pattern}: {error}"
        ))
    };
    eggdb_sql::regex_captures("", &regex).map_err(invalid)?;
    let mut out = Vec::with_capacity(vector.len());
    for (mut labels, value) in vector {
        let captured =
            eggdb_sql::regex_captures(label_value(&labels, source), &regex).map_err(invalid)?;
        if let Some(groups) = captured {
            let expanded = expand(replacement, &groups, MAX_LABEL_VALUE_BYTES)
                .ok_or_else(|| label_too_long("label_replace"))?;
            hold(expanded.len())?;
            set_label(&mut labels, destination, expanded);
        }
        out.push((labels, value));
    }
    unique_label_sets(out)
}

/// `label_join`: sets `destination` to the values of the `sources` labels joined by
/// `separator` (removing it when that is empty). `hold` is given the bytes of each value it
/// builds, which are at most [`MAX_LABEL_VALUE_BYTES`].
pub(crate) fn label_join(
    vector: Vec<(Labels, f64)>,
    destination: &str,
    separator: &str,
    sources: &[&str],
    hold: &mut dyn FnMut(usize) -> Result<(), Failure>,
) -> Result<Vec<(Labels, f64)>, Failure> {
    if !is_label_name(destination) {
        return Err(Failure::BadData(format!(
            "invalid destination label name in label_join(): {destination}"
        )));
    }
    if let Some(source) = sources.iter().find(|source| !is_label_name(source)) {
        return Err(Failure::BadData(format!(
            "invalid source label name in label_join(): {source}"
        )));
    }
    let mut out = Vec::with_capacity(vector.len());
    let separators = separator
        .len()
        .saturating_mul(sources.len().saturating_sub(1));
    for (mut labels, value) in vector {
        let joined_bytes = sources.iter().fold(separators, |bytes, source| {
            bytes.saturating_add(label_value(&labels, source).len())
        });
        if joined_bytes > MAX_LABEL_VALUE_BYTES {
            return Err(label_too_long("label_join"));
        }
        hold(joined_bytes)?;
        let joined = sources
            .iter()
            .map(|source| label_value(&labels, source))
            .collect::<Vec<&str>>()
            .join(separator);
        set_label(&mut labels, destination, joined);
        out.push((labels, value));
    }
    unique_label_sets(out)
}
