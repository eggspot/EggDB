use crate::query::Query;
use eggdb_sql::json::{Json, Node};
use std::collections::HashMap;
use std::net::IpAddr;

const MAX_DEPTH: usize = 32;
const KEYWORD_SUBFIELD: &str = ".keyword";
const MILLIS_PER_SECOND: i64 = 1000;
const MILLIS_PER_MINUTE: i64 = 60 * MILLIS_PER_SECOND;
const MILLIS_PER_HOUR: i64 = 60 * MILLIS_PER_MINUTE;
const MILLIS_PER_DAY: i64 = 24 * MILLIS_PER_HOUR;
const IPV4_MAPPED_PREFIX: u128 = 0xffff << 32;
const IPV4_PREFIX_OFFSET: u32 = 96;

/// The kind of value a mapped field holds, as far as queries treat them differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldType {
    Text,
    Keyword,
    Date,
    Ip,
    Other,
}

impl FieldType {
    fn named(name: &str) -> Self {
        match name {
            "text" | "match_only_text" => Self::Text,
            "keyword" | "constant_keyword" | "wildcard" => Self::Keyword,
            "date" | "date_nanos" => Self::Date,
            "ip" => Self::Ip,
            _ => Self::Other,
        }
    }
}

/// A field a query names: its type, and the path of the `_source` value it reads, which for a
/// multi-field such as `title.keyword` is its parent's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MappedField {
    pub(crate) kind: FieldType,
    pub(crate) source: String,
}

/// The field types of the indices a search reads, from their stored mappings.
#[derive(Debug, Default)]
pub(crate) struct FieldTypes {
    fields: HashMap<String, MappedField>,
    vector_similarities: HashMap<String, String>,
}

fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Node> {
    match &node.value {
        Json::Object(members) => members
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value),
        _ => None,
    }
}

fn text(node: &Node) -> Option<&str> {
    match &node.value {
        Json::String(text) => Some(text),
        _ => None,
    }
}

impl FieldTypes {
    /// Adds an index's `mappings` object; a path already typed by an earlier index keeps its
    /// type.
    pub(crate) fn add_mappings(&mut self, mappings: &Node) {
        if let Some(properties) = member(mappings, "properties") {
            self.add_properties(properties, "", 0);
        }
    }

    fn add_properties(&mut self, properties: &Node, prefix: &str, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        let Json::Object(members) = &properties.value else {
            return;
        };
        for (name, spec) in members {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            if let Some(kind) = member(spec, "type").and_then(text) {
                self.fields.entry(path.clone()).or_insert(MappedField {
                    kind: FieldType::named(kind),
                    source: path.clone(),
                });
                if kind == "dense_vector" {
                    let similarity = member(spec, "similarity").and_then(text).unwrap_or("cosine");
                    self.vector_similarities
                        .entry(path.clone())
                        .or_insert_with(|| similarity.to_owned());
                }
            }
            if let Some(Json::Object(subfields)) = member(spec, "fields").map(|node| &node.value) {
                for (subfield, sub_spec) in subfields {
                    if let Some(kind) = member(sub_spec, "type").and_then(text) {
                        self.fields
                            .entry(format!("{path}.{subfield}"))
                            .or_insert(MappedField {
                                kind: FieldType::named(kind),
                                source: path.clone(),
                            });
                    }
                }
            }
            if let Some(nested) = member(spec, "properties") {
                self.add_properties(nested, &path, depth + 1);
            }
        }
    }

    /// The similarity a `dense_vector` field's mapping names, `cosine` when it names none or
    /// the field is not mapped.
    pub(crate) fn vector_similarity(&self, name: &str) -> &str {
        self.vector_similarities.get(name).map_or("cosine", String::as_str)
    }

    /// The field `name` resolves to: a mapped field or multi-field, else the `keyword`
    /// sub-field Elasticsearch's dynamic mapping gives every string (`name.keyword` of an
    /// unmapped or `text` field), else `None`.
    pub(crate) fn resolve(&self, name: &str) -> Option<MappedField> {
        if let Some(found) = self.fields.get(name) {
            return Some(found.clone());
        }
        let parent = name.strip_suffix(KEYWORD_SUBFIELD)?;
        match self.fields.get(parent) {
            Some(mapped) if mapped.kind != FieldType::Text => None,
            _ => Some(MappedField {
                kind: FieldType::Keyword,
                source: parent.to_owned(),
            }),
        }
    }
}

/// `query` with the indices' field types applied: multi-fields read their parent's value,
/// `term` on a `text` field matches one of its analyzed tokens, and `range` on a `date` or `ip`
/// field compares dates and addresses instead of text. `now_millis` anchors date math.
pub(crate) fn apply(query: Query, types: &FieldTypes, now_millis: i64) -> Query {
    let source_of = |field: String| match types.resolve(&field) {
        Some(mapped) => mapped.source,
        None => field,
    };
    match query {
        Query::Term { field, value } => {
            let Some(mapped) = types.resolve(&field) else {
                return Query::Term { field, value };
            };
            match (mapped.kind, value) {
                (FieldType::Text, Json::String(wanted)) => Query::Match {
                    fields: vec![mapped.source],
                    terms: vec![wanted],
                    all: false,
                },
                (FieldType::Ip, Json::String(wanted)) => match cidr(&wanted) {
                    Some((low, high)) => Query::IpRange {
                        field: mapped.source,
                        low,
                        high,
                    },
                    None => Query::MatchNone,
                },
                (_, value) => Query::Term {
                    field: mapped.source,
                    value,
                },
            }
        }
        Query::Terms { field, values } => match types.resolve(&field) {
            Some(mapped) if matches!(mapped.kind, FieldType::Text | FieldType::Ip) => {
                any_of(
                    values
                        .into_iter()
                        .map(|value| {
                            apply(
                                Query::Term {
                                    field: field.clone(),
                                    value,
                                },
                                types,
                                now_millis,
                            )
                        })
                        .collect(),
                )
            }
            Some(mapped) => Query::Terms {
                field: mapped.source,
                values,
            },
            None => Query::Terms { field, values },
        },
        Query::Range { field, bounds } => match types.resolve(&field) {
            Some(mapped) if mapped.kind == FieldType::Date => {
                date_range(&mapped.source, &bounds, now_millis).unwrap_or(Query::Range {
                    field: mapped.source,
                    bounds,
                })
            }
            Some(mapped) if mapped.kind == FieldType::Ip => {
                ip_range(&mapped.source, &bounds).unwrap_or(Query::MatchNone)
            }
            Some(mapped) => Query::Range {
                field: mapped.source,
                bounds,
            },
            None => Query::Range { field, bounds },
        },
        Query::Exists { field } => Query::Exists {
            field: source_of(field),
        },
        Query::Prefix { field, prefix } => Query::Prefix {
            field: source_of(field),
            prefix,
        },
        Query::MatchPhrase { field, terms } => Query::MatchPhrase {
            field: source_of(field),
            terms,
        },
        Query::Match { fields, terms, all } => Query::Match {
            fields: fields.into_iter().map(source_of).collect(),
            terms,
            all,
        },
        Query::Bool {
            must,
            filter,
            should,
            must_not,
            minimum_should_match,
        } => {
            let each = |queries: Vec<Query>| {
                queries
                    .into_iter()
                    .map(|query| apply(query, types, now_millis))
                    .collect()
            };
            Query::Bool {
                must: each(must),
                filter: each(filter),
                should: each(should),
                must_not: each(must_not),
                minimum_should_match,
            }
        }
        Query::Nested { path, query, mode } => Query::Nested {
            path,
            query: Box::new(apply(*query, types, now_millis)),
            mode,
        },
        other => other,
    }
}

fn any_of(should: Vec<Query>) -> Query {
    Query::Bool {
        must: Vec::new(),
        filter: Vec::new(),
        should,
        must_not: Vec::new(),
        minimum_should_match: 1,
    }
}

fn bound_text(value: &Json) -> Option<String> {
    match value {
        Json::String(text) => Some(text.clone()),
        Json::Number(text) => Some(text.clone()),
        _ => None,
    }
}

fn date_range(field: &str, bounds: &[(String, Json)], now_millis: i64) -> Option<Query> {
    let mut low = None;
    let mut high = None;
    for (operator, value) in bounds {
        let text = bound_text(value)?;
        let rounds_up = matches!(operator.as_str(), "gt" | "lte");
        let millis = date_math(&text, now_millis, rounds_up)?;
        match operator.as_str() {
            "gt" => low = Some((millis, false)),
            "gte" => low = Some((millis, true)),
            "lt" => high = Some((millis, false)),
            _ => high = Some((millis, true)),
        }
    }
    Some(Query::DateRange {
        field: field.to_owned(),
        low,
        high,
    })
}

fn ip_range(field: &str, bounds: &[(String, Json)]) -> Option<Query> {
    let mut low = 0u128;
    let mut high = u128::MAX;
    for (operator, value) in bounds {
        let address = ip_number(&bound_text(value)?)?;
        match operator.as_str() {
            "gt" => low = address.checked_add(1)?,
            "gte" => low = address,
            "lt" => high = address.checked_sub(1)?,
            _ => high = address,
        }
    }
    Some(Query::IpRange {
        field: field.to_owned(),
        low,
        high,
    })
}

/// An address as a number in IPv6's space, IPv4 mapped into `::ffff:0:0/96`.
pub(crate) fn ip_number(text: &str) -> Option<u128> {
    match text.trim().parse::<IpAddr>().ok()? {
        IpAddr::V4(address) => Some(IPV4_MAPPED_PREFIX | u128::from(u32::from(address))),
        IpAddr::V6(address) => Some(u128::from(address)),
    }
}

fn cidr(text: &str) -> Option<(u128, u128)> {
    let Some((address, length)) = text.split_once('/') else {
        let address = ip_number(text)?;
        return Some((address, address));
    };
    let is_v4 = address.contains('.');
    let length: u32 = length.trim().parse().ok()?;
    let length = if is_v4 {
        (length <= 32).then_some(length + IPV4_PREFIX_OFFSET)?
    } else {
        (length <= 128).then_some(length)?
    };
    let address = ip_number(address)?;
    let host_bits = 128 - length;
    let mask = if host_bits == 128 {
        u128::MAX
    } else {
        (1u128 << host_bits) - 1
    };
    Some((address & !mask, address | mask))
}

/// A document's date value in milliseconds since 1970: a number of milliseconds, or a string
/// in `strict_date_optional_time` (with or without a time and an offset, UTC when it has none)
/// or of digits only.
pub(crate) fn date_millis(value: &Json) -> Option<i64> {
    match value {
        Json::Number(text) => text.parse::<f64>().ok().map(|millis| millis as i64),
        Json::String(text) if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) => {
            text.parse().ok()
        }
        Json::String(text) => parse_date_time(text),
        _ => None,
    }
}

fn digits(text: &str, at: &mut usize, count: usize) -> Option<i64> {
    let slice = text.get(*at..*at + count)?;
    if !slice.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    *at += count;
    slice.parse().ok()
}

fn parse_date_time(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let mut at = 0;
    let year = digits(text, &mut at, 4)?;
    let (mut month, mut day) = (1, 1);
    if bytes.get(at) == Some(&b'-') {
        at += 1;
        month = digits(text, &mut at, 2)?;
        if bytes.get(at) == Some(&b'-') {
            at += 1;
            day = digits(text, &mut at, 2)?;
        }
    }
    if !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
        return None;
    }
    let mut millis = days_from_civil(year, month, day) * MILLIS_PER_DAY;
    if matches!(bytes.get(at), Some(b'T' | b't' | b' ')) {
        at += 1;
        let hour = digits(text, &mut at, 2)?;
        let mut minute = 0;
        let mut second = 0;
        let mut fraction = 0;
        if bytes.get(at) == Some(&b':') {
            at += 1;
            minute = digits(text, &mut at, 2)?;
            if bytes.get(at) == Some(&b':') {
                at += 1;
                second = digits(text, &mut at, 2)?;
                if matches!(bytes.get(at), Some(b'.' | b',')) {
                    at += 1;
                    let start = at;
                    while bytes.get(at).is_some_and(u8::is_ascii_digit) {
                        at += 1;
                    }
                    let shown = text.get(start..at)?;
                    let padded = format!("{:0<3}", shown.get(..shown.len().min(3))?);
                    fraction = padded.parse().ok()?;
                }
            }
        }
        if hour > 23 || minute > 59 || second > 60 {
            return None;
        }
        millis += hour * MILLIS_PER_HOUR + minute * MILLIS_PER_MINUTE + second * MILLIS_PER_SECOND
            + fraction;
    }
    match bytes.get(at) {
        None => Some(millis),
        Some(b'Z' | b'z') if at + 1 == bytes.len() => Some(millis),
        Some(sign @ (b'+' | b'-')) => {
            at += 1;
            let hours = digits(text, &mut at, 2)?;
            if bytes.get(at) == Some(&b':') {
                at += 1;
            }
            let minutes = if at < bytes.len() {
                digits(text, &mut at, 2)?
            } else {
                0
            };
            if at != bytes.len() {
                return None;
            }
            let offset = hours * MILLIS_PER_HOUR + minutes * MILLIS_PER_MINUTE;
            Some(if *sign == b'+' { millis - offset } else { millis + offset })
        }
        _ => None,
    }
}

/// Elasticsearch's date math: `now` or `<date>||`, then `+1d`, `-2h`, `/d` steps (`y`, `M`,
/// `w`, `d`, `h`/`H`, `m`, `s`); a rounding goes to the unit's start, or to its last
/// millisecond when `rounds_up` (`gt` and `lte` bounds).
pub(crate) fn date_math(text: &str, now_millis: i64, rounds_up: bool) -> Option<i64> {
    let text = text.trim();
    let (mut millis, mut steps) = if let Some(rest) = text.strip_prefix("now") {
        (now_millis, rest)
    } else if let Some((anchor, rest)) = text.split_once("||") {
        (date_millis(&Json::String(anchor.to_owned()))?, rest)
    } else {
        return date_millis(&Json::String(text.to_owned()));
    };
    while !steps.is_empty() {
        let (operator, rest) = steps.split_at(1);
        let count_length = rest.bytes().take_while(u8::is_ascii_digit).count();
        let (count, rest) = rest.split_at(count_length);
        let mut units = rest.chars();
        let unit = units.next()?;
        steps = units.as_str();
        match operator {
            "/" if count.is_empty() => {
                let start = round_down(millis, unit)?;
                millis = if rounds_up {
                    add(start, unit, 1)? - 1
                } else {
                    start
                };
            }
            "+" | "-" => {
                let count: i64 = if count.is_empty() { 1 } else { count.parse().ok()? };
                millis = add(millis, unit, if operator == "-" { -count } else { count })?;
            }
            _ => return None,
        }
    }
    Some(millis)
}

fn add(millis: i64, unit: char, count: i64) -> Option<i64> {
    let fixed = |size: i64| millis.checked_add(size.checked_mul(count)?);
    match unit {
        'y' => add_months(millis, count.checked_mul(12)?),
        'M' => add_months(millis, count),
        'w' => fixed(7 * MILLIS_PER_DAY),
        'd' => fixed(MILLIS_PER_DAY),
        'h' | 'H' => fixed(MILLIS_PER_HOUR),
        'm' => fixed(MILLIS_PER_MINUTE),
        's' => fixed(MILLIS_PER_SECOND),
        _ => None,
    }
}

fn add_months(millis: i64, months: i64) -> Option<i64> {
    let days = millis.div_euclid(MILLIS_PER_DAY);
    let of_day = millis.rem_euclid(MILLIS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    let total = year.checked_mul(12)?.checked_add(month - 1)?.checked_add(months)?;
    let (year, month) = (total.div_euclid(12), total.rem_euclid(12) + 1);
    let day = day.min(days_in_month(year, month));
    days_from_civil(year, month, day)
        .checked_mul(MILLIS_PER_DAY)?
        .checked_add(of_day)
}

fn round_down(millis: i64, unit: char) -> Option<i64> {
    let floor = |size: i64| Some(millis.div_euclid(size) * size);
    let days = millis.div_euclid(MILLIS_PER_DAY);
    let (year, month, _) = civil_from_days(days);
    match unit {
        'y' => Some(days_from_civil(year, 1, 1) * MILLIS_PER_DAY),
        'M' => Some(days_from_civil(year, month, 1) * MILLIS_PER_DAY),
        'w' => {
            let monday = days - (days + 3).rem_euclid(7);
            Some(monday * MILLIS_PER_DAY)
        }
        'd' => floor(MILLIS_PER_DAY),
        'h' | 'H' => floor(MILLIS_PER_HOUR),
        'm' => floor(MILLIS_PER_MINUTE),
        's' => floor(MILLIS_PER_SECOND),
        _ => None,
    }
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_sql::json::parse_document;

    fn types(mappings: &str) -> FieldTypes {
        let mut types = FieldTypes::default();
        types.add_mappings(&parse_document(mappings).expect("mappings parse"));
        types
    }

    #[test]
    fn multi_fields_and_dynamic_keyword_subfields_read_their_parent() {
        let types = types(
            r#"{"properties": {"title": {"type": "text", "fields": {"raw": {"type": "keyword"}}},
                "tags": {"type": "keyword"}, "seen": {"type": "date"},
                "user": {"properties": {"ip": {"type": "ip"}}}}}"#,
        );
        let field = |kind, source: &str| {
            Some(MappedField {
                kind,
                source: source.to_owned(),
            })
        };
        assert_eq!(types.resolve("title"), field(FieldType::Text, "title"));
        assert_eq!(types.resolve("title.raw"), field(FieldType::Keyword, "title"));
        assert_eq!(types.resolve("title.keyword"), field(FieldType::Keyword, "title"));
        assert_eq!(types.resolve("city.keyword"), field(FieldType::Keyword, "city"));
        assert_eq!(types.resolve("tags.keyword"), None);
        assert_eq!(types.resolve("user.ip"), field(FieldType::Ip, "user.ip"));
        assert_eq!(types.resolve("unknown"), None);
    }

    #[test]
    fn term_on_text_matches_a_token_and_ranges_read_dates_and_addresses() {
        let types = types(
            r#"{"properties": {"body": {"type": "text"}, "at": {"type": "date"},
                "client": {"type": "ip"}}}"#,
        );
        let term = Query::Term {
            field: "body".to_owned(),
            value: Json::String("quick".to_owned()),
        };
        assert_eq!(
            apply(term, &types, 0),
            Query::Match {
                fields: vec!["body".to_owned()],
                terms: vec!["quick".to_owned()],
                all: false
            }
        );
        let day = 19_737 * MILLIS_PER_DAY;
        let range = Query::Range {
            field: "at".to_owned(),
            bounds: vec![
                ("gte".to_owned(), Json::String("now-1d/d".to_owned())),
                ("lte".to_owned(), Json::String("now/d".to_owned())),
            ],
        };
        assert_eq!(
            apply(range, &types, day + 5 * MILLIS_PER_HOUR),
            Query::DateRange {
                field: "at".to_owned(),
                low: Some((day - MILLIS_PER_DAY, true)),
                high: Some((day + MILLIS_PER_DAY - 1, true)),
            }
        );
        let subnet = Query::Term {
            field: "client".to_owned(),
            value: Json::String("10.1.0.0/16".to_owned()),
        };
        let Query::IpRange { low, high, .. } = apply(subnet, &types, 0) else {
            panic!("a CIDR term is a range of addresses");
        };
        assert_eq!(ip_number("10.1.0.0"), Some(low));
        assert_eq!(ip_number("10.1.255.255"), Some(high));
    }

    #[test]
    fn dates_parse_with_and_without_times_and_offsets() {
        let at = |text: &str| date_millis(&Json::String(text.to_owned()));
        assert_eq!(at("1970-01-02"), Some(MILLIS_PER_DAY));
        assert_eq!(at("1970-01-01T01:00:00Z"), Some(MILLIS_PER_HOUR));
        assert_eq!(at("1970-01-01T02:00:00+01:00"), Some(MILLIS_PER_HOUR));
        assert_eq!(at("1970-01-01T00:00:00.5"), Some(500));
        assert_eq!(at("86400000"), Some(MILLIS_PER_DAY));
        assert_eq!(at("1970-02-30"), None);
        assert_eq!(at("yesterday"), None);
        assert_eq!(date_math("2024-01-31||+1M", 0, false), at("2024-02-29"));
        assert_eq!(date_math("2024-03-15||/M", 0, false), at("2024-03-01"));
        assert_eq!(date_math("2024-03-15||/y", 0, true), at("2024-12-31T23:59:59.999"));
        assert_eq!(date_math("now+", 0, false), None);
    }
}
