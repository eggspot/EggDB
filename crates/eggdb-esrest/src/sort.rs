use crate::json_out::Out;
use crate::query::values_at;
use eggdb_sql::json::{Json, Node};
use std::cmp::Ordering;

const KEYWORD_SUBFIELD: &str = ".keyword";
const EARTH_MEAN_RADIUS_METERS: f64 = 6_371_008.771_4;
const GEO_SORT_OPTIONS: [&str; 7] = [
    "order",
    "unit",
    "mode",
    "distance_type",
    "ignore_unmapped",
    "validation_method",
    "nested",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Ascending,
    Descending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Min,
    Max,
    Sum,
    Avg,
    Median,
}

#[derive(Debug, Clone, PartialEq)]
enum Missing {
    Last,
    First,
    Value(Json),
}

#[derive(Debug, Clone, PartialEq)]
enum Target {
    Score,
    Doc,
    Field {
        name: String,
        mode: Option<Mode>,
        missing: Missing,
    },
    GeoDistance {
        field: String,
        origins: Vec<(f64, f64)>,
        meters_per_unit: f64,
        mode: Option<Mode>,
        plane: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
struct SortField {
    target: Target,
    direction: Direction,
}

/// A search's `sort`: the fields, `_score` and `_doc` its hits are ordered by, each ascending
/// or descending, with where documents missing the field go.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Sort {
    fields: Vec<SortField>,
}

/// One hit's value for one sort field: a number (with the text the hit's `sort` shows), a
/// string, or nothing (`null`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SortValue {
    Number { value: f64, shown: String },
    Text(String),
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Numeric { integral: bool },
    Keyword,
}

/// What a sort compares: each field's kind as the searched documents hold it, so the hits'
/// values and the `search_after` values are read the same way.
#[derive(Debug)]
pub(crate) struct Ordered<'s> {
    sort: &'s Sort,
    kinds: Vec<Kind>,
}

fn direction_of(text: &str) -> Result<Direction, String> {
    match text.to_ascii_lowercase().as_str() {
        "asc" => Ok(Direction::Ascending),
        "desc" => Ok(Direction::Descending),
        _ => Err(format!("Unknown SortOrder [{text}]")),
    }
}

fn mode_of(text: &str) -> Result<Mode, String> {
    match text.to_ascii_lowercase().as_str() {
        "min" => Ok(Mode::Min),
        "max" => Ok(Mode::Max),
        "sum" => Ok(Mode::Sum),
        "avg" => Ok(Mode::Avg),
        "median" => Ok(Mode::Median),
        _ => Err(format!("Unknown SortMode [{text}]")),
    }
}

fn target_of(name: &str) -> Result<Target, String> {
    match name {
        "_score" => Ok(Target::Score),
        "_doc" | "_shard_doc" => Ok(Target::Doc),
        "_script" => Err(format!("sorting by [{name}] is not supported")),
        "_geo_distance" => Err("[_geo_distance] needs a field and an origin".to_owned()),
        "" => Err("a sort field has an empty name".to_owned()),
        field => Ok(Target::Field {
            name: field.to_owned(),
            mode: None,
            missing: Missing::Last,
        }),
    }
}

fn default_direction(target: &Target) -> Direction {
    match target {
        Target::Score => Direction::Descending,
        _ => Direction::Ascending,
    }
}

fn named(name: &str) -> Result<SortField, String> {
    let target = target_of(name)?;
    Ok(SortField {
        direction: default_direction(&target),
        target,
    })
}

fn number_of(node: &Node) -> Option<f64> {
    match &node.value {
        Json::Number(text) | Json::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn point_of(node: &Node) -> Option<(f64, f64)> {
    match &node.value {
        Json::Object(members) => {
            let find = |name: &str| members.iter().find(|(key, _)| key == name).and_then(|(_, value)| number_of(value));
            Some((find("lat")?, find("lon")?))
        }
        Json::Array(items) if items.len() >= 2 && items.iter().all(|item| matches!(item.value, Json::Number(_))) => {
            Some((number_of(&items[1])?, number_of(&items[0])?))
        }
        Json::String(text) => {
            let (lat, lon) = text.split_once(',')?;
            Some((lat.trim().parse().ok()?, lon.trim().parse().ok()?))
        }
        _ => None,
    }
    .filter(|(lat, lon)| (-90.0..=90.0).contains(lat) && (-180.0..=180.0).contains(lon))
}

fn points_of(node: &Node) -> Vec<(f64, f64)> {
    if let Some(point) = point_of(node) {
        return vec![point];
    }
    match &node.value {
        Json::Array(items) => items.iter().filter_map(point_of).collect(),
        _ => Vec::new(),
    }
}

fn meters_per(unit: &str) -> Result<f64, String> {
    Ok(match unit {
        "m" | "meters" => 1.0,
        "km" | "kilometers" => 1000.0,
        "cm" | "centimeters" => 0.01,
        "mm" | "millimeters" => 0.001,
        "mi" | "miles" => 1609.344,
        "yd" | "yards" => 0.9144,
        "ft" | "feet" => 0.3048,
        "in" | "inch" => 0.0254,
        "nmi" | "NM" | "nauticalmiles" => 1852.0,
        other => return Err(format!("unknown distance unit [{other}]")),
    })
}

fn distance_meters(from: (f64, f64), to: (f64, f64), plane: bool) -> f64 {
    let (lat1, lon1) = (from.0.to_radians(), from.1.to_radians());
    let (lat2, lon2) = (to.0.to_radians(), to.1.to_radians());
    if plane {
        let x = (lon2 - lon1) * ((lat1 + lat2) / 2.0).cos();
        let y = lat2 - lat1;
        return (x * x + y * y).sqrt() * EARTH_MEAN_RADIUS_METERS;
    }
    let half = ((lat2 - lat1) / 2.0).sin().powi(2)
        + lat1.cos() * lat2.cos() * ((lon2 - lon1) / 2.0).sin().powi(2);
    2.0 * EARTH_MEAN_RADIUS_METERS * half.sqrt().min(1.0).asin()
}

fn geo_distance(spec: &Node) -> Result<SortField, String> {
    let Json::Object(options) = &spec.value else {
        return Err("[_geo_distance] takes an object".to_owned());
    };
    let mut field = None;
    let mut direction = Direction::Ascending;
    let mut meters_per_unit = 1.0;
    let mut mode = None;
    let mut plane = false;
    for (key, value) in options {
        match (key.as_str(), &value.value) {
            ("order", Json::String(order)) => direction = direction_of(order)?,
            ("unit", Json::String(unit)) => meters_per_unit = meters_per(unit)?,
            ("mode", Json::String(text)) => mode = Some(mode_of(text)?),
            ("distance_type", Json::String(kind)) => plane = kind.eq_ignore_ascii_case("plane"),
            (option, _) if GEO_SORT_OPTIONS.contains(&option) => {}
            (name, _) => {
                let origins = points_of(value);
                if origins.is_empty() {
                    return Err(format!("[_geo_distance] cannot read the origin of [{name}]"));
                }
                field = Some((name.to_owned(), origins));
            }
        }
    }
    let (field, origins) = field.ok_or("[_geo_distance] needs a field and an origin")?;
    Ok(SortField {
        target: Target::GeoDistance {
            field,
            origins,
            meters_per_unit,
            mode,
            plane,
        },
        direction,
    })
}

fn raw_at<'n>(source: &'n Node, path: &str) -> Option<&'n Node> {
    let mut current = source;
    for part in path.split('.') {
        let Json::Object(members) = &current.value else {
            return None;
        };
        current = members.iter().find(|(key, _)| key == part).map(|(_, value)| value)?;
    }
    Some(current)
}

fn configured(name: &str, spec: &Node) -> Result<SortField, String> {
    if name == "_geo_distance" {
        return geo_distance(spec);
    }
    let mut field = named(name)?;
    match &spec.value {
        Json::String(order) => field.direction = direction_of(order)?,
        Json::Object(options) => {
            for (key, value) in options {
                match (key.as_str(), &value.value) {
                    ("order", Json::String(order)) => field.direction = direction_of(order)?,
                    ("mode", Json::String(mode)) => {
                        if let Target::Field { mode: held, .. } = &mut field.target {
                            *held = Some(mode_of(mode)?);
                        }
                    }
                    ("missing", missing) => {
                        if let Target::Field { missing: held, .. } = &mut field.target {
                            *held = match missing {
                                Json::String(text) if text == "_last" => Missing::Last,
                                Json::String(text) if text == "_first" => Missing::First,
                                Json::Null => Missing::Last,
                                other => Missing::Value(other.clone()),
                            };
                        }
                    }
                    ("unmapped_type" | "numeric_type" | "format", _) => {}
                    ("order" | "mode", _) => {
                        return Err(format!("[{key}] of sort field [{name}] must be a string"));
                    }
                    (other, _) => return Err(format!("[field_sort] unknown field [{other}]")),
                }
            }
        }
        _ => {
            return Err(format!(
                "sort field [{name}] takes an order string or an object of options"
            ));
        }
    }
    Ok(field)
}

fn entries(node: &Node, fields: &mut Vec<SortField>) -> Result<(), String> {
    match &node.value {
        Json::String(name) => fields.push(named(name)?),
        Json::Object(members) => {
            for (name, spec) in members {
                fields.push(configured(name, spec)?);
            }
        }
        _ => return Err("[sort] entries are field names or objects".to_owned()),
    }
    Ok(())
}

fn number_text(value: f64) -> String {
    let mut out = Out::default();
    out.number(value);
    out.finish()
}

fn integral_text(text: &str) -> bool {
    !text.contains(['.', 'e', 'E'])
}

fn field_values<'n>(source: &'n Node, field: &str) -> Vec<&'n Node> {
    let values = values_at(source, field);
    if values.is_empty()
        && let Some(base) = field.strip_suffix(KEYWORD_SUBFIELD)
    {
        return values_at(source, base);
    }
    values
}

fn numeric(node: &Node) -> Option<(f64, String)> {
    match &node.value {
        Json::Number(text) => text.parse().ok().map(|value| (value, text.clone())),
        Json::Boolean(value) => Some((f64::from(u8::from(*value)), u8::from(*value).to_string())),
        Json::String(text) => text
            .trim()
            .parse()
            .ok()
            .map(|value: f64| (value, number_text(value))),
        _ => None,
    }
}

fn textual(node: &Node) -> Option<String> {
    match &node.value {
        Json::String(text) | Json::Number(text) => Some(text.clone()),
        Json::Boolean(value) => Some(value.to_string()),
        _ => None,
    }
}

fn kind_of(field: &str, documents: &[&Node]) -> Kind {
    let mut numbers = false;
    let mut integral = true;
    for source in documents {
        for value in field_values(source, field) {
            match &value.value {
                Json::Number(text) => {
                    numbers = true;
                    integral &= integral_text(text);
                }
                Json::Boolean(_) => numbers = true,
                _ => {}
            }
        }
    }
    if numbers {
        Kind::Numeric { integral }
    } else {
        Kind::Keyword
    }
}

fn computed_number(value: f64, integral: bool) -> (f64, String) {
    let value = if integral { value.round() } else { value };
    (value, number_text(value))
}

fn chosen_number(mut values: Vec<(f64, String)>, mode: Mode, integral: bool) -> SortValue {
    values.sort_by(|left, right| left.0.total_cmp(&right.0));
    let picked = match mode {
        Mode::Min => values.first().cloned(),
        Mode::Max => values.last().cloned(),
        Mode::Sum => {
            let sum: f64 = values.iter().map(|(value, _)| value).sum();
            Some((sum, number_text(sum)))
        }
        Mode::Avg => {
            let average =
                values.iter().map(|(value, _)| value).sum::<f64>() / values.len().max(1) as f64;
            Some(computed_number(average, integral))
        }
        Mode::Median => {
            let middle = values.len() / 2;
            let median = if values.len().is_multiple_of(2) {
                match (values.get(middle.wrapping_sub(1)), values.get(middle)) {
                    (Some(low), Some(high)) => Some((low.0 + high.0) / 2.0),
                    _ => None,
                }
            } else {
                values.get(middle).map(|(value, _)| *value)
            };
            median.map(|value| computed_number(value, integral))
        }
    };
    match picked {
        Some((value, shown)) => SortValue::Number { value, shown },
        None => SortValue::Missing,
    }
}

fn chosen_text(mut values: Vec<String>, direction: Direction) -> SortValue {
    values.sort();
    let picked = match direction {
        Direction::Ascending => values.into_iter().next(),
        Direction::Descending => values.pop(),
    };
    picked.map_or(SortValue::Missing, SortValue::Text)
}

fn extreme(kind: Kind, low: bool) -> SortValue {
    match (kind, low) {
        (Kind::Numeric { integral: true }, true) => SortValue::Number {
            value: i64::MIN as f64,
            shown: i64::MIN.to_string(),
        },
        (Kind::Numeric { integral: true }, false) => SortValue::Number {
            value: i64::MAX as f64,
            shown: i64::MAX.to_string(),
        },
        (_, true) => SortValue::Number {
            value: f64::NEG_INFINITY,
            shown: "\"-Infinity\"".to_owned(),
        },
        (_, false) => SortValue::Number {
            value: f64::INFINITY,
            shown: "\"Infinity\"".to_owned(),
        },
    }
}

fn missing_value(kind: Kind, missing: &Missing, direction: Direction) -> SortValue {
    let first = match missing {
        Missing::First => true,
        Missing::Last => false,
        Missing::Value(value) => {
            let node = Node {
                value: value.clone(),
                start: 0,
                end: 0,
            };
            return match kind {
                Kind::Numeric { .. } => match numeric(&node) {
                    Some((value, shown)) => SortValue::Number { value, shown },
                    None => SortValue::Missing,
                },
                Kind::Keyword => textual(&node).map_or(SortValue::Missing, SortValue::Text),
            };
        }
    };
    match kind {
        Kind::Numeric { .. } => extreme(kind, first == (direction == Direction::Ascending)),
        Kind::Keyword => SortValue::Missing,
    }
}

fn after_number(node: &Node) -> Option<f64> {
    match &node.value {
        Json::Number(text) => text.parse().ok(),
        Json::String(text) => text.trim().parse().ok(),
        Json::Boolean(value) => Some(f64::from(u8::from(*value))),
        _ => None,
    }
}

impl Sort {
    /// The sort a search body's `sort` member asks for: a field name, an object of fields with
    /// their order or options (`order`, `mode`, `missing`), or an array of either.
    pub(crate) fn from_node(node: &Node) -> Result<Self, String> {
        let mut fields = Vec::new();
        match &node.value {
            Json::Array(items) => {
                for item in items {
                    entries(item, &mut fields)?;
                }
            }
            _ => entries(node, &mut fields)?,
        }
        Ok(Self { fields })
    }

    /// The sort a URL's `sort` parameter asks for: comma-separated `field` or `field:order`.
    pub(crate) fn from_flag(text: &str) -> Result<Self, String> {
        let mut fields = Vec::new();
        for part in text
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
        {
            let field = match part.rsplit_once(':') {
                Some((name, order)) => {
                    let mut field = named(name)?;
                    field.direction = direction_of(order)?;
                    field
                }
                None => named(part)?,
            };
            fields.push(field);
        }
        Ok(Self { fields })
    }

    /// Whether the sort names no field, so hits keep the order of their scores.
    pub(crate) fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// Whether `_score` is one of the sort fields, so the hits' scores are worth reporting.
    pub(crate) fn uses_score(&self) -> bool {
        self.fields
            .iter()
            .any(|field| field.target == Target::Score)
    }

    /// The sort ready to compare the given matched documents' values.
    pub(crate) fn over(&self, matched: &[&Node]) -> Ordered<'_> {
        let kinds = self
            .fields
            .iter()
            .map(|field| match &field.target {
                Target::Score | Target::Doc | Target::GeoDistance { .. } => {
                    Kind::Numeric { integral: false }
                }
                Target::Field { name, .. } => kind_of(name, matched),
            })
            .collect();
        Ordered { sort: self, kinds }
    }
}

impl Ordered<'_> {
    fn columns(&self) -> impl Iterator<Item = (&SortField, Kind)> {
        self.sort.fields.iter().zip(self.kinds.iter().copied())
    }

    /// One hit's sort values: its `score`, its position `at` among the searched documents
    /// (what `_doc` sorts by) and its `source`.
    pub(crate) fn values_of(&self, score: f64, at: usize, source: &Node) -> Vec<SortValue> {
        self.columns()
            .map(|(field, kind)| match &field.target {
                Target::Score => SortValue::Number {
                    value: score,
                    shown: number_text(score),
                },
                Target::Doc => SortValue::Number {
                    value: at as f64,
                    shown: at.to_string(),
                },
                Target::GeoDistance {
                    field: name,
                    origins,
                    meters_per_unit,
                    mode,
                    plane,
                } => {
                    let distances: Vec<(f64, String)> = raw_at(source, name)
                        .map(points_of)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|point| {
                            let nearest = origins
                                .iter()
                                .map(|origin| distance_meters(*origin, point, *plane))
                                .fold(f64::INFINITY, f64::min);
                            let value = nearest / meters_per_unit;
                            (value, number_text(value))
                        })
                        .collect();
                    if distances.is_empty() {
                        SortValue::Number {
                            value: f64::INFINITY,
                            shown: "\"Infinity\"".to_owned(),
                        }
                    } else {
                        let mode = mode.unwrap_or(match field.direction {
                            Direction::Ascending => Mode::Min,
                            Direction::Descending => Mode::Max,
                        });
                        chosen_number(distances, mode, false)
                    }
                }
                Target::Field {
                    name,
                    mode,
                    missing,
                } => {
                    let values = field_values(source, name);
                    let picked = match kind {
                        Kind::Numeric { integral } => {
                            let numbers: Vec<(f64, String)> =
                                values.iter().copied().filter_map(numeric).collect();
                            let mode = mode.unwrap_or(match field.direction {
                                Direction::Ascending => Mode::Min,
                                Direction::Descending => Mode::Max,
                            });
                            if numbers.is_empty() {
                                SortValue::Missing
                            } else {
                                chosen_number(numbers, mode, integral)
                            }
                        }
                        Kind::Keyword => {
                            let texts: Vec<String> =
                                values.iter().copied().filter_map(textual).collect();
                            let direction = match mode {
                                Some(Mode::Min) => Direction::Ascending,
                                Some(Mode::Max) => Direction::Descending,
                                _ => field.direction,
                            };
                            chosen_text(texts, direction)
                        }
                    };
                    match picked {
                        SortValue::Missing => missing_value(kind, missing, field.direction),
                        found => found,
                    }
                }
            })
            .collect()
    }

    /// Compares two hits' sort values field by field: each field in its direction, documents
    /// missing a text field last (or first with `"missing": "_first"`) in either direction.
    pub(crate) fn compare(&self, left: &[SortValue], right: &[SortValue]) -> Ordering {
        for ((field, _), (left, right)) in self.columns().zip(left.iter().zip(right)) {
            let first = matches!(
                field.target,
                Target::Field {
                    missing: Missing::First,
                    ..
                }
            );
            let ordering = match (left, right) {
                (SortValue::Missing, SortValue::Missing) => Ordering::Equal,
                (SortValue::Missing, _) if first => Ordering::Less,
                (SortValue::Missing, _) => Ordering::Greater,
                (_, SortValue::Missing) if first => Ordering::Greater,
                (_, SortValue::Missing) => Ordering::Less,
                (left, right) => {
                    let ordering = match (left, right) {
                        (
                            SortValue::Number { value: left, .. },
                            SortValue::Number { value: right, .. },
                        ) => left.total_cmp(right),
                        (SortValue::Text(left), SortValue::Text(right)) => left.cmp(right),
                        (SortValue::Number { .. }, _) => Ordering::Less,
                        _ => Ordering::Greater,
                    };
                    match field.direction {
                        Direction::Ascending => ordering,
                        Direction::Descending => ordering.reverse(),
                    }
                }
            };
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        Ordering::Equal
    }

    /// The sort values a request's `search_after` holds, read as this sort's fields are; an
    /// error names the field whose value cannot be read, as Elasticsearch does.
    pub(crate) fn after(&self, values: &[Node]) -> Result<Vec<SortValue>, String> {
        if self.sort.fields.len() != values.len() {
            return Err(format!(
                "search_after has {} value(s) but sort has {}.",
                values.len(),
                self.sort.fields.len()
            ));
        }
        self.columns()
            .zip(values)
            .map(|((field, kind), value)| {
                let name = match &field.target {
                    Target::Score => "_score",
                    Target::Doc => "_doc",
                    Target::Field { name, .. } | Target::GeoDistance { field: name, .. } => {
                        name.as_str()
                    }
                };
                let unreadable =
                    || format!("Failed to parse search_after value for field [{name}].");
                match (kind, &value.value) {
                    (Kind::Keyword, Json::Null) => Ok(SortValue::Missing),
                    (Kind::Keyword, _) => {
                        textual(value).map(SortValue::Text).ok_or_else(unreadable)
                    }
                    (Kind::Numeric { .. }, _) => after_number(value)
                        .map(|number| SortValue::Number {
                            value: number,
                            shown: number_text(number),
                        })
                        .ok_or_else(unreadable),
                }
            })
            .collect()
    }
}

/// Writes one hit's sort values as its `sort` array.
pub(crate) fn write_values(out: &mut Out, values: &[SortValue]) {
    out.begin_array();
    for value in values {
        match value {
            SortValue::Number { shown, .. } => out.raw(shown),
            SortValue::Text(text) => out.string(text),
            SortValue::Missing => out.null(),
        };
    }
    out.end_array();
}

#[cfg(test)]
mod tests {
    use super::{Sort, SortValue};
    use eggdb_sql::json::{Node, parse_document};

    fn sort(spec: &str) -> Result<Sort, String> {
        Sort::from_node(&parse_document(spec).expect("JSON"))
    }

    fn document(text: &str) -> Node {
        parse_document(text).expect("JSON")
    }

    #[test]
    fn the_sort_forms_of_the_reference_parse() {
        let parsed = sort(
            "[{\"post_date\":{\"order\":\"asc\",\"format\":\"strict_date_optional_time_nanos\"}},\"user\",{\"name\":\"desc\"},{\"age\":\"desc\"},\"_score\"]",
        )
        .expect("the reference example parses");
        assert_eq!(parsed.fields.len(), 5);
        assert!(parsed.uses_score());
        assert_eq!(sort("\"_doc\"").expect("a name").fields.len(), 1);
        assert_eq!(
            sort("[{\"a\":\"sideways\"}]"),
            Err("Unknown SortOrder [sideways]".to_owned())
        );
        assert_eq!(
            sort("[{\"a\":{\"mode\":\"most\"}}]"),
            Err("Unknown SortMode [most]".to_owned())
        );
        assert_eq!(
            sort("[{\"a\":{\"bogus\":1}}]"),
            Err("[field_sort] unknown field [bogus]".to_owned())
        );
        assert!(sort("[1]").is_err());
        assert!(sort("[{\"_script\":{}}]").is_err());
        let flag = Sort::from_flag("pages:desc,_doc").expect("a URL sort");
        assert_eq!(flag.fields.len(), 2);
        assert!(Sort::from_flag("pages:up").is_err());
    }

    #[test]
    fn missing_numbers_take_the_extreme_long_and_text_stays_null() {
        let sort = sort("[{\"n\":\"asc\"},{\"s\":{\"order\":\"desc\",\"missing\":\"_first\"}}]")
            .expect("a sort");
        let with = document("{\"n\":5,\"s\":\"b\"}");
        let without = document("{}");
        let ordered = sort.over(&[&with, &without]);
        let present = ordered.values_of(1.0, 0, &with);
        let absent = ordered.values_of(1.0, 1, &without);
        assert_eq!(
            absent,
            vec![
                SortValue::Number {
                    value: i64::MAX as f64,
                    shown: "9223372036854775807".to_owned()
                },
                SortValue::Missing
            ]
        );
        assert_eq!(ordered.compare(&present, &absent), std::cmp::Ordering::Less);
        let first = sort_first();
        let ordered = first.over(&[&with, &without]);
        assert_eq!(
            ordered.compare(
                &ordered.values_of(1.0, 0, &with),
                &ordered.values_of(1.0, 1, &without)
            ),
            std::cmp::Ordering::Greater
        );
    }

    fn sort_first() -> Sort {
        sort("[{\"s\":{\"order\":\"desc\",\"missing\":\"_first\"}}]").expect("a sort")
    }

    #[test]
    fn arrays_sort_by_their_minimum_ascending_and_maximum_descending() {
        let source = document("{\"n\":[3,9,1]}");
        for (spec, shown) in [
            ("\"n\"", "1"),
            ("{\"n\":\"desc\"}", "9"),
            ("{\"n\":{\"mode\":\"sum\"}}", "13"),
            ("{\"n\":{\"mode\":\"median\"}}", "3"),
        ] {
            let sort = sort(spec).expect("a sort");
            let ordered = sort.over(&[&source]);
            assert_eq!(
                ordered.values_of(0.0, 0, &source),
                vec![SortValue::Number {
                    value: shown.parse().expect("a number"),
                    shown: shown.to_owned()
                }],
                "{spec}"
            );
        }
    }

    #[test]
    fn the_median_of_an_even_count_of_whole_numbers_rounds_like_the_average() {
        for (values, shown) in [("[2,3]", "3"), ("[3,9,1,4]", "4"), ("[2.0,3.5]", "2.75")] {
            let source = document(&format!("{{\"n\":{values}}}"));
            let sort = sort("{\"n\":{\"mode\":\"median\"}}").expect("a sort");
            let ordered = sort.over(&[&source]);
            assert_eq!(
                ordered.values_of(0.0, 0, &source),
                vec![SortValue::Number {
                    value: shown.parse().expect("a number"),
                    shown: shown.to_owned()
                }],
                "{values}"
            );
        }
    }

    #[test]
    fn search_after_values_are_counted_and_read_per_field() {
        let sort = sort("[{\"n\":\"asc\"},\"_doc\"]").expect("a sort");
        let source = document("{\"n\":1}");
        let ordered = sort.over(&[&source]);
        let one = document("[1]");
        let eggdb_sql::json::Json::Array(one) = one.value else {
            panic!("an array")
        };
        assert_eq!(
            ordered.after(&one),
            Err("search_after has 1 value(s) but sort has 2.".to_owned())
        );
        let bad = document("[\"x\",0]");
        let eggdb_sql::json::Json::Array(bad) = bad.value else {
            panic!("an array")
        };
        assert_eq!(
            ordered.after(&bad),
            Err("Failed to parse search_after value for field [n].".to_owned())
        );
        let good = document("[\"9223372036854775807\",4]");
        let eggdb_sql::json::Json::Array(good) = good.value else {
            panic!("an array")
        };
        assert!(ordered.after(&good).is_ok());
    }

    #[test]
    fn geo_distance_sorts_by_the_nearest_point() {
        let geo = sort("[{\"_geo_distance\":{\"pin\":[0,0],\"unit\":\"km\",\"order\":\"asc\"}}]").unwrap();
        let near = document("{\"pin\":{\"lat\":0,\"lon\":1}}");
        let far = document("{\"pin\":\"0,2\"}");
        let none = document("{}");
        let ordered = geo.over(&[&near, &far, &none]);
        let distances: Vec<f64> = [&near, &far, &none]
            .iter()
            .map(|source| match &ordered.values_of(0.0, 0, source)[0] {
                SortValue::Number { value, .. } => *value,
                other => panic!("{other:?}"),
            })
            .collect();
        assert!((distances[0] - 111.195).abs() < 0.01, "{distances:?}");
        assert!((distances[1] - 222.39).abs() < 0.02, "{distances:?}");
        assert!(distances[2].is_infinite());
        assert!(sort("{\"_geo_distance\":{\"unit\":\"km\"}}").is_err());
    }
}
