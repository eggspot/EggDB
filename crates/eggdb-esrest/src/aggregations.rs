use crate::json_out::Out;
use crate::query::{Stats, parse, score, values_at};
use eggdb_sql::json::{Json, Node};
use std::collections::{BTreeMap, BTreeSet, HashSet};

const MAX_DEPTH: usize = 16;
const MAX_BUCKETS: usize = 65_536;
const DEFAULT_PERCENTS: [f64; 7] = [1.0, 5.0, 25.0, 50.0, 75.0, 95.0, 99.0];
const MILLIS_PER_DAY: i64 = 86_400_000;

fn setting<'n>(settings: &'n Node, name: &str) -> Option<&'n Node> {
    match &settings.value {
        Json::Object(members) => members
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, node)| node),
        _ => None,
    }
}

fn text_setting(settings: &Node, name: &str) -> Option<String> {
    match &setting(settings, name)?.value {
        Json::String(text) => Some(text.clone()),
        Json::Number(text) => Some(text.clone()),
        _ => None,
    }
}

fn number_setting(settings: &Node, name: &str) -> Option<f64> {
    match &setting(settings, name)?.value {
        Json::Number(text) | Json::String(text) => text.parse().ok(),
        _ => None,
    }
}

fn number_of(node: &Node) -> Option<f64> {
    match &node.value {
        Json::Number(text) => text.parse().ok(),
        _ => None,
    }
}

fn numbers(documents: &[&Node], field: &str) -> Vec<f64> {
    documents
        .iter()
        .flat_map(|source| values_at(source, field))
        .filter_map(number_of)
        .collect()
}

fn present(node: &Node) -> bool {
    !matches!(node.value, Json::Null)
}

/// Writes the `aggregations` object of a search over `documents`.
pub(crate) fn write(out: &mut Out, aggregations: &Node, documents: &[&Node]) {
    named(out, aggregations, documents, 0);
}

fn named(out: &mut Out, aggregations: &Node, documents: &[&Node], depth: usize) {
    out.begin_object();
    if let Json::Object(members) = &aggregations.value {
        for (name, spec) in members {
            out.key(name);
            one(out, spec, documents, depth);
        }
    }
    out.end_object();
}

fn bucket_body(out: &mut Out, documents: &[&Node], subs: Option<&Node>, depth: usize) {
    out.key("doc_count").integer(documents.len() as u64);
    if let (Some(subs), true) = (subs, depth < MAX_DEPTH)
        && let Json::Object(members) = &subs.value
    {
        for (name, spec) in members {
            out.key(name);
            one(out, spec, documents, depth + 1);
        }
    }
}

fn one(out: &mut Out, spec: &Node, documents: &[&Node], depth: usize) {
    let Json::Object(members) = &spec.value else {
        out.begin_object().end_object();
        return;
    };
    let subs = members
        .iter()
        .find(|(key, _)| key == "aggs" || key == "aggregations")
        .map(|(_, node)| node);
    let Some((kind, settings)) = members
        .iter()
        .find(|(key, _)| !matches!(key.as_str(), "aggs" | "aggregations" | "meta"))
    else {
        out.begin_object().end_object();
        return;
    };
    let field = text_setting(settings, "field").unwrap_or_default();
    out.begin_object();
    match kind.as_str() {
        "terms" => terms(out, settings, &field, documents, subs, depth),
        "histogram" => histogram(out, settings, &field, documents, subs, depth),
        "date_histogram" => date_histogram(out, settings, &field, documents, subs, depth),
        "range" => range(out, settings, &field, documents, subs, depth),
        "filter" => {
            let kept = filtered(settings, documents);
            bucket_body(out, &kept, subs, depth);
        }
        "filters" => filters(out, settings, documents, subs, depth),
        "missing" => {
            let kept: Vec<&Node> = documents
                .iter()
                .copied()
                .filter(|source| !values_at(source, &field).into_iter().any(present))
                .collect();
            bucket_body(out, &kept, subs, depth);
        }
        "avg" | "sum" | "min" | "max" => {
            let values = numbers(documents, &field);
            let value = match kind.as_str() {
                "sum" => Some(values.iter().sum()),
                "avg" => {
                    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
                }
                "min" => values.iter().copied().reduce(f64::min),
                _ => values.iter().copied().reduce(f64::max),
            };
            out.key("value");
            match value {
                Some(value) => out.number(value),
                None => out.null(),
            };
        }
        "value_count" => {
            let count = documents
                .iter()
                .flat_map(|source| values_at(source, &field))
                .filter(|node| present(node))
                .count();
            out.key("value").integer(count as u64);
        }
        "cardinality" => {
            let distinct: HashSet<String> = documents
                .iter()
                .flat_map(|source| values_at(source, &field))
                .filter(|node| present(node))
                .map(|node| match &node.value {
                    Json::String(text) => format!("s{text}"),
                    Json::Number(text) => format!("n{}", text.parse::<f64>().unwrap_or(0.0)),
                    other => format!("o{other:?}"),
                })
                .collect();
            out.key("value").integer(distinct.len() as u64);
        }
        "stats" | "extended_stats" => stats(out, kind == "extended_stats", &numbers(documents, &field)),
        "percentiles" => percentiles(out, settings, &numbers(documents, &field)),
        _ => {}
    }
    out.end_object();
}

fn filtered<'d>(query: &Node, documents: &[&'d Node]) -> Vec<&'d Node> {
    let Ok(query) = parse(query, 0) else {
        return Vec::new();
    };
    let stats = Stats::default();
    documents
        .iter()
        .copied()
        .filter(|source| score(&query, "", source, &stats).is_some())
        .collect()
}

fn filters(out: &mut Out, settings: &Node, documents: &[&Node], subs: Option<&Node>, depth: usize) {
    let Some(listed) = setting(settings, "filters") else {
        return;
    };
    match &listed.value {
        Json::Object(members) => {
            out.key("buckets").begin_object();
            for (name, query) in members {
                out.key(name).begin_object();
                bucket_body(out, &filtered(query, documents), subs, depth);
                out.end_object();
            }
            out.end_object();
        }
        Json::Array(items) => {
            out.key("buckets").begin_array();
            for query in items {
                out.begin_object();
                bucket_body(out, &filtered(query, documents), subs, depth);
                out.end_object();
            }
            out.end_array();
        }
        _ => {}
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum TermKey {
    Number(String),
    Text(String),
}

fn terms(
    out: &mut Out,
    settings: &Node,
    field: &str,
    documents: &[&Node],
    subs: Option<&Node>,
    depth: usize,
) {
    let size = number_setting(settings, "size").map_or(10, |size| size.max(0.0) as usize);
    let min_doc_count = number_setting(settings, "min_doc_count").map_or(1, |count| count.max(0.0) as usize);
    let mut groups: BTreeMap<TermKey, Vec<&Node>> = BTreeMap::new();
    for source in documents {
        let mut seen = HashSet::new();
        for node in values_at(source, field) {
            let key = match &node.value {
                Json::String(text) => TermKey::Text(text.clone()),
                Json::Number(text) => TermKey::Number(text.clone()),
                Json::Boolean(flag) => TermKey::Text(flag.to_string()),
                _ => continue,
            };
            if seen.insert(key.clone()) {
                groups.entry(key).or_default().push(source);
            }
        }
    }
    let mut buckets: Vec<(TermKey, Vec<&Node>)> = groups
        .into_iter()
        .filter(|(_, members)| members.len() >= min_doc_count)
        .collect();
    let order = setting(settings, "order").and_then(|order| match &order.value {
        Json::Object(members) => members.first().map(|(key, direction)| {
            (
                key.clone(),
                matches!(&direction.value, Json::String(text) if text == "asc"),
            )
        }),
        _ => None,
    });
    let numeric = |key: &TermKey| match key {
        TermKey::Number(text) => text.parse::<f64>().ok(),
        TermKey::Text(_) => None,
    };
    match order {
        Some((key, ascending)) if key == "_key" || key == "_term" => buckets.sort_by(|left, right| {
            let ordering = match (numeric(&left.0), numeric(&right.0)) {
                (Some(a), Some(b)) => a.total_cmp(&b),
                _ => left.0.cmp(&right.0),
            };
            if ascending { ordering } else { ordering.reverse() }
        }),
        Some((_, true)) => buckets.sort_by(|left, right| {
            left.1.len().cmp(&right.1.len()).then_with(|| left.0.cmp(&right.0))
        }),
        _ => buckets.sort_by(|left, right| {
            right.1.len().cmp(&left.1.len()).then_with(|| left.0.cmp(&right.0))
        }),
    }
    let other: usize = buckets.iter().skip(size).map(|(_, members)| members.len()).sum();
    buckets.truncate(size);
    out.key("doc_count_error_upper_bound")
        .integer(0)
        .key("sum_other_doc_count")
        .integer(other as u64)
        .key("buckets")
        .begin_array();
    for (key, members) in &buckets {
        out.begin_object().key("key");
        match key {
            TermKey::Number(text) => out.raw(text),
            TermKey::Text(text) => out.string(text),
        };
        bucket_body(out, members, subs, depth);
        out.end_object();
    }
    out.end_array();
}

fn histogram(
    out: &mut Out,
    settings: &Node,
    field: &str,
    documents: &[&Node],
    subs: Option<&Node>,
    depth: usize,
) {
    let interval = number_setting(settings, "interval").filter(|interval| *interval > 0.0);
    let Some(interval) = interval else {
        out.key("buckets").begin_array().end_array();
        return;
    };
    let offset = number_setting(settings, "offset").unwrap_or(0.0);
    let min_doc_count = number_setting(settings, "min_doc_count").map_or(0, |count| count.max(0.0) as usize);
    let mut groups: BTreeMap<i64, Vec<&Node>> = BTreeMap::new();
    for source in documents {
        let mut seen = BTreeSet::new();
        for value in values_at(source, field).into_iter().filter_map(number_of) {
            let slot = ((value - offset) / interval).floor() as i64;
            if seen.insert(slot) {
                groups.entry(slot).or_default().push(source);
            }
        }
    }
    filled_buckets(out, groups, min_doc_count, subs, depth, |slot| {
        let key = slot as f64 * interval + offset;
        (key, None)
    });
}

fn filled_buckets(
    out: &mut Out,
    mut groups: BTreeMap<i64, Vec<&Node>>,
    min_doc_count: usize,
    subs: Option<&Node>,
    depth: usize,
    key_of: impl Fn(i64) -> (f64, Option<String>),
) {
    if min_doc_count == 0
        && let (Some(&first), Some(&last)) = (groups.keys().next(), groups.keys().next_back())
        && usize::try_from(last.saturating_sub(first)).is_ok_and(|span| span < MAX_BUCKETS)
    {
        for slot in first..=last {
            groups.entry(slot).or_default();
        }
    }
    out.key("buckets").begin_array();
    for (slot, members) in groups.iter().take(MAX_BUCKETS) {
        if members.len() < min_doc_count {
            continue;
        }
        let (key, key_as_string) = key_of(*slot);
        out.begin_object();
        if let Some(text) = &key_as_string {
            out.key("key_as_string").string(text);
        }
        out.key("key").number(key);
        bucket_body(out, members, subs, depth);
        out.end_object();
    }
    out.end_array();
}

enum DateStep {
    Fixed(i64),
    Months(i64),
}

fn fixed_millis(text: &str) -> Option<i64> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    let amount: i64 = text.get(..digits)?.parse().ok()?;
    let unit = match text.get(digits..)? {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => MILLIS_PER_DAY,
        _ => return None,
    };
    amount.checked_mul(unit).filter(|millis| *millis > 0)
}

fn calendar_step(text: &str) -> Option<DateStep> {
    Some(match text {
        "minute" | "1m" => DateStep::Fixed(60_000),
        "hour" | "1h" => DateStep::Fixed(3_600_000),
        "day" | "1d" => DateStep::Fixed(MILLIS_PER_DAY),
        "week" | "1w" => DateStep::Fixed(7 * MILLIS_PER_DAY),
        "month" | "1M" => DateStep::Months(1),
        "quarter" | "1q" => DateStep::Months(3),
        "year" | "1y" => DateStep::Months(12),
        _ => return None,
    })
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn two_digits(text: &str, at: usize) -> Option<i64> {
    let part = text.get(at..at + 2)?;
    if !part.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    part.parse().ok()
}

fn date_millis(node: &Node) -> Option<i64> {
    match &node.value {
        Json::Number(text) => text.parse::<f64>().ok().map(|millis| millis as i64),
        Json::String(text) => parse_date(text.trim()),
        _ => None,
    }
}

fn parse_date(text: &str) -> Option<i64> {
    let year: i64 = text.get(..4)?.parse().ok()?;
    if text.get(4..5)? != "-" || text.get(7..8)? != "-" {
        return None;
    }
    let month = two_digits(text, 5)?;
    let day = two_digits(text, 8)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut millis = days_from_civil(year, month, day) * MILLIS_PER_DAY;
    let rest = &text[10..];
    if rest.is_empty() {
        return Some(millis);
    }
    let time = rest.strip_prefix('T').or_else(|| rest.strip_prefix(' '))?;
    let hours = two_digits(time, 0)?;
    let minutes = two_digits(time, 3)?;
    let mut at = 5;
    let mut seconds = 0;
    if time.get(5..6) == Some(":") {
        seconds = two_digits(time, 6)?;
        at = 8;
    }
    millis += ((hours * 60 + minutes) * 60 + seconds) * 1000;
    let mut tail = &time[at..];
    if let Some(fraction) = tail.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        let padded = format!("{:0<3}", &fraction[..digits.min(3)]);
        millis += padded.parse::<i64>().ok()?;
        tail = &fraction[digits..];
    }
    match tail {
        "" | "Z" => Some(millis),
        zone => {
            let sign = match zone.get(..1)? {
                "+" => 1,
                "-" => -1,
                _ => return None,
            };
            let zone = zone[1..].replace(':', "");
            let hours = two_digits(&zone, 0)?;
            let minutes = two_digits(&zone, 2).unwrap_or(0);
            Some(millis - sign * (hours * 60 + minutes) * 60_000)
        }
    }
}

fn iso_millis(millis: i64) -> String {
    let days = millis.div_euclid(MILLIS_PER_DAY);
    let within = millis.rem_euclid(MILLIS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        within / 3_600_000,
        within / 60_000 % 60,
        within / 1000 % 60,
        within % 1000
    )
}

fn date_histogram(
    out: &mut Out,
    settings: &Node,
    field: &str,
    documents: &[&Node],
    subs: Option<&Node>,
    depth: usize,
) {
    let step = text_setting(settings, "fixed_interval")
        .and_then(|text| fixed_millis(&text).map(DateStep::Fixed))
        .or_else(|| text_setting(settings, "calendar_interval").and_then(|text| calendar_step(&text)))
        .or_else(|| {
            text_setting(settings, "interval").and_then(|text| {
                calendar_step(&text).or_else(|| fixed_millis(&text).map(DateStep::Fixed))
            })
        });
    let Some(step) = step else {
        out.key("buckets").begin_array().end_array();
        return;
    };
    let min_doc_count = number_setting(settings, "min_doc_count").map_or(0, |count| count.max(0.0) as usize);
    let slot_of = |millis: i64| match step {
        DateStep::Fixed(width) => millis.div_euclid(width),
        DateStep::Months(months) => {
            let (year, month, _) = civil_from_days(millis.div_euclid(MILLIS_PER_DAY));
            (year * 12 + month - 1).div_euclid(months)
        }
    };
    let start_of = |slot: i64| match step {
        DateStep::Fixed(width) => slot.saturating_mul(width),
        DateStep::Months(months) => {
            let month_number = slot * months;
            days_from_civil(month_number.div_euclid(12), month_number.rem_euclid(12) + 1, 1)
                * MILLIS_PER_DAY
        }
    };
    let mut groups: BTreeMap<i64, Vec<&Node>> = BTreeMap::new();
    for source in documents {
        let mut seen = BTreeSet::new();
        for millis in values_at(source, field).into_iter().filter_map(date_millis) {
            let slot = slot_of(millis);
            if seen.insert(slot) {
                groups.entry(slot).or_default().push(source);
            }
        }
    }
    filled_buckets(out, groups, min_doc_count, subs, depth, |slot| {
        let start = start_of(slot);
        (start as f64, Some(iso_millis(start)))
    });
}

fn bound_text(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{value:.1}")
    } else {
        format!("{value}")
    }
}

fn range(
    out: &mut Out,
    settings: &Node,
    field: &str,
    documents: &[&Node],
    subs: Option<&Node>,
    depth: usize,
) {
    let keyed = matches!(setting(settings, "keyed").map(|node| &node.value), Some(Json::Boolean(true)));
    let ranges: Vec<&Node> = match setting(settings, "ranges").map(|node| &node.value) {
        Some(Json::Array(items)) => items.iter().collect(),
        _ => Vec::new(),
    };
    if keyed {
        out.key("buckets").begin_object();
    } else {
        out.key("buckets").begin_array();
    }
    for spec in ranges.into_iter().take(MAX_BUCKETS) {
        let from = number_setting(spec, "from");
        let to = number_setting(spec, "to");
        let key = text_setting(spec, "key").unwrap_or_else(|| {
            format!(
                "{}-{}",
                from.map_or_else(|| "*".to_owned(), bound_text),
                to.map_or_else(|| "*".to_owned(), bound_text)
            )
        });
        let members: Vec<&Node> = documents
            .iter()
            .copied()
            .filter(|source| {
                values_at(source, field).into_iter().filter_map(number_of).any(|value| {
                    from.is_none_or(|from| value >= from) && to.is_none_or(|to| value < to)
                })
            })
            .collect();
        if keyed {
            out.key(&key).begin_object();
        } else {
            out.begin_object().key("key").string(&key);
        }
        if let Some(from) = from {
            out.key("from").number(from);
        }
        if let Some(to) = to {
            out.key("to").number(to);
        }
        bucket_body(out, &members, subs, depth);
        out.end_object();
    }
    if keyed {
        out.end_object();
    } else {
        out.end_array();
    }
}

fn stats(out: &mut Out, extended: bool, values: &[f64]) {
    let count = values.len();
    out.key("count").integer(count as u64);
    let sum: f64 = values.iter().sum();
    let optional = |out: &mut Out, name: &str, value: Option<f64>| {
        out.key(name);
        match value {
            Some(value) => out.number(value),
            None => out.null(),
        };
    };
    optional(out, "min", values.iter().copied().reduce(f64::min));
    optional(out, "max", values.iter().copied().reduce(f64::max));
    let average = (count > 0).then(|| sum / count as f64);
    optional(out, "avg", average);
    out.key("sum").number(sum);
    if !extended {
        return;
    }
    let squares: f64 = values.iter().map(|value| value * value).sum();
    out.key("sum_of_squares").number(squares);
    let population = average.map(|average| squares / count as f64 - average * average);
    let sampling = (count > 1)
        .then(|| average.map(|average| (squares - count as f64 * average * average) / (count as f64 - 1.0)))
        .flatten();
    optional(out, "variance", population);
    optional(out, "variance_population", population);
    optional(out, "variance_sampling", sampling);
    let deviation = population.map(|variance| variance.max(0.0).sqrt());
    optional(out, "std_deviation", deviation);
    optional(out, "std_deviation_population", deviation);
    optional(out, "std_deviation_sampling", sampling.map(|variance| variance.max(0.0).sqrt()));
    out.key("std_deviation_bounds").begin_object();
    let bound = |sign: f64| average.zip(deviation).map(|(average, deviation)| average + sign * 2.0 * deviation);
    optional(out, "upper", bound(1.0));
    optional(out, "lower", bound(-1.0));
    out.end_object();
}

fn percentiles(out: &mut Out, settings: &Node, values: &[f64]) {
    let percents: Vec<f64> = match setting(settings, "percents").map(|node| &node.value) {
        Some(Json::Array(items)) => items.iter().filter_map(number_of).collect(),
        _ => DEFAULT_PERCENTS.to_vec(),
    };
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    out.key("values").begin_object();
    for percent in percents {
        out.key(&bound_text(percent));
        if sorted.is_empty() {
            out.null();
            continue;
        }
        let rank = (percent.clamp(0.0, 100.0) / 100.0) * (sorted.len() - 1) as f64;
        let low = rank.floor() as usize;
        let high = rank.ceil() as usize;
        let value = sorted[low] + (sorted[high] - sorted[low]) * (rank - low as f64);
        out.number(value);
    }
    out.end_object();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_read_and_write_in_utc_millis() {
        let millis = parse_date("2024-02-29T12:30:15.250Z").unwrap();
        assert_eq!(iso_millis(millis), "2024-02-29T12:30:15.250Z");
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(civil_from_days(days_from_civil(1999, 12, 31)), (1999, 12, 31));
        assert_eq!(bound_text(100.0), "100.0");
        assert_eq!(bound_text(2.5), "2.5");
    }
}
