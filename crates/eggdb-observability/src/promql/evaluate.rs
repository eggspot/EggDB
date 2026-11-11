use super::functions::{
    aggregate_values, ensure_unique_label_sets, histogram_quantile, label_join, label_replace,
    range_function, scalar_function, set_label,
};
use super::{
    Aggregation, Binary, Cardinality, Expr, Failure, Fetched, Grouping, LOOKBACK_MS, Labels,
    Selector, Subquery, VectorMatching, is_comparison, is_set_operator,
};
use crate::store::Series;
use eggdb_platform::connection_limits::{MemoryBudget, Reservation};
use std::collections::{BTreeMap, BTreeSet};

const SAMPLE_BYTES: usize = std::mem::size_of::<(i64, f64)>();
const DEFAULT_SUBQUERY_STEP_MS: i64 = 60_000;
const MAX_SUBQUERY_STEPS: usize = 11_000_000;
const TOO_MANY_SAMPLES: &str =
    "query processing would load too many samples into memory in query execution";

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Data {
    Scalar(f64),
    Vector(Vec<(Labels, f64)>),
    Matrix(Vec<(Labels, Vec<(i64, f64)>)>),
}

/// What one query holds: the samples it keeps for its whole run (what it read, the points of
/// a range query's answer), the samples one evaluation step builds, and the subquery steps it
/// has run, each bounded, with the bytes held against the server's memory budget (D307).
#[derive(Debug)]
pub(crate) struct Work<'b> {
    reservation: Reservation<'b>,
    kept_samples: usize,
    step_samples: usize,
    step_label_bytes: usize,
    max_samples: usize,
    subquery_steps: usize,
}

impl<'b> Work<'b> {
    pub(crate) fn new(budget: &'b MemoryBudget, max_samples: usize) -> Self {
        Self {
            reservation: budget.empty_reservation(),
            kept_samples: 0,
            step_samples: 0,
            step_label_bytes: 0,
            max_samples,
            subquery_steps: 0,
        }
    }

    fn hold(&mut self) -> Result<(), Failure> {
        let held = self.kept_samples.saturating_add(self.step_samples);
        if held > self.max_samples {
            return Err(Failure::Execution(TOO_MANY_SAMPLES.to_owned()));
        }
        if !self.reservation.hold_if_large(
            held.saturating_mul(SAMPLE_BYTES)
                .saturating_add(self.step_label_bytes),
        ) {
            return Err(Failure::Unavailable(
                "the query needs more memory than the server's memory budget has left".to_owned(),
            ));
        }
        Ok(())
    }

    /// Holds `samples` more until the query ends.
    pub(crate) fn keep(&mut self, samples: usize) -> Result<(), Failure> {
        self.kept_samples = self.kept_samples.saturating_add(samples);
        self.hold()
    }

    fn take(&mut self, samples: usize) -> Result<(), Failure> {
        self.step_samples = self.step_samples.saturating_add(samples);
        self.hold()
    }

    fn take_label_bytes(&mut self, bytes: usize) -> Result<(), Failure> {
        self.step_label_bytes = self.step_label_bytes.saturating_add(bytes);
        self.hold()
    }

    fn give_back_to(&mut self, step_samples: usize, step_label_bytes: usize) {
        self.step_samples = self.step_samples.min(step_samples);
        self.step_label_bytes = self.step_label_bytes.min(step_label_bytes);
        let _ = self.reservation.hold_if_large(
            self.kept_samples
                .saturating_add(self.step_samples)
                .saturating_mul(SAMPLE_BYTES)
                .saturating_add(self.step_label_bytes),
        );
    }

    /// Gives back what the last evaluation step held.
    pub(crate) fn finish_step(&mut self) {
        self.step_samples = 0;
        self.step_label_bytes = 0;
        let _ = self
            .reservation
            .hold_if_large(self.kept_samples.saturating_mul(SAMPLE_BYTES));
    }

    /// How many more samples the query may hold.
    pub(crate) fn remaining_samples(&self) -> usize {
        self.max_samples
            .saturating_sub(self.kept_samples.saturating_add(self.step_samples))
    }

    fn run_subquery_steps(&mut self, steps: usize) -> Result<(), Failure> {
        self.subquery_steps = self.subquery_steps.saturating_add(steps);
        if self.subquery_steps > MAX_SUBQUERY_STEPS {
            return Err(Failure::Execution(format!(
                "the query's subqueries would evaluate more than {MAX_SUBQUERY_STEPS} steps: use a coarser subquery resolution"
            )));
        }
        Ok(())
    }
}

/// Every series of `data` that `selector` selects.
pub(crate) fn chosen<'d>(selector: &Selector, data: &'d Fetched) -> Vec<&'d Series> {
    let candidates: Vec<&Series> = match &selector.name {
        Some(name) => data
            .get(name)
            .map(|series| series.iter().collect())
            .unwrap_or_default(),
        None => data.values().flatten().collect(),
    };
    candidates
        .into_iter()
        .filter(|series| {
            selector
                .matchers
                .iter()
                .all(|matcher| matcher.matches(&series.labels))
        })
        .collect()
}

fn absent_labels(argument: &Expr) -> Labels {
    let selector = match argument {
        Expr::Selector(selector) => Some(selector),
        Expr::Subquery(_) | Expr::Number(_) | Expr::Text(_) | Expr::Call(..) => None,
        Expr::Aggregate(_) | Expr::Binary(_) => None,
    };
    let mut labels: Labels = Vec::new();
    if let Some(selector) = selector {
        for matcher in &selector.matchers {
            if matcher.op == super::MatchOp::Equal && matcher.name != "__name__" {
                set_label(&mut labels, &matcher.name, matcher.value.clone());
            }
        }
    }
    labels
}

pub(crate) fn without_name(labels: &Labels) -> Labels {
    labels
        .iter()
        .filter(|(name, _)| name != "__name__")
        .cloned()
        .collect()
}

fn labels_hash(labels: &[(String, String)]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for (name, value) in labels {
        for byte in name.bytes().chain([0xff]).chain(value.bytes()).chain([0xfe]) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash
}

pub(crate) fn window(samples: &[(i64, f64)], start: i64, end: i64) -> &[(i64, f64)] {
    let from = samples.partition_point(|(time, _)| *time <= start);
    let to = samples.partition_point(|(time, _)| *time <= end);
    &samples[from..to.max(from)]
}

fn range_and_offset(expr: &Expr) -> Option<(i64, i64)> {
    match expr {
        Expr::Selector(selector) => selector.range_ms.map(|range| (range, selector.offset_ms)),
        Expr::Subquery(subquery) => Some((subquery.range_ms, subquery.offset_ms)),
        _ => None,
    }
}

fn text_argument<'e>(function: &str, expr: &'e Expr) -> Result<&'e str, Failure> {
    match expr {
        Expr::Text(text) => Ok(text),
        _ => Err(Failure::BadData(format!(
            "expected type string in call to function {function}"
        ))),
    }
}

fn labels_text(labels: &Labels) -> String {
    let pairs: Vec<String> = labels
        .iter()
        .map(|(name, value)| format!("{name}={value:?}"))
        .collect();
    format!("{{{}}}", pairs.join(", "))
}

fn operate(op: &str, left: f64, right: f64) -> (f64, bool) {
    match op {
        "+" => (left + right, true),
        "-" => (left - right, true),
        "*" => (left * right, true),
        "/" => (left / right, true),
        "%" => (left % right, true),
        "^" => (left.powf(right), true),
        "==" => (left, left == right),
        "!=" => (left, left != right),
        ">" => (left, left > right),
        "<" => (left, left < right),
        ">=" => (left, left >= right),
        "<=" => (left, left <= right),
        _ => (f64::NAN, false),
    }
}

fn drops_name(binary: &Binary) -> bool {
    !is_comparison(binary.op) || binary.returns_bool
}

fn signature(matching: &VectorMatching, labels: &Labels) -> Labels {
    labels
        .iter()
        .filter(|(name, _)| {
            if matching.on {
                matching.labels.contains(name)
            } else {
                name != "__name__" && !matching.labels.contains(name)
            }
        })
        .cloned()
        .collect()
}

fn result_metric(
    binary: &Binary,
    matching: &VectorMatching,
    many: &Labels,
    one: &Labels,
) -> Labels {
    let mut labels = many.clone();
    if drops_name(binary) {
        labels.retain(|(name, _)| name != "__name__");
    }
    match &matching.cardinality {
        Cardinality::OneToOne => {
            labels.retain(|(name, _)| matching.labels.contains(name) == matching.on)
        }
        Cardinality::ManyToOne(included) | Cardinality::OneToMany(included) => {
            for name in included {
                let value = super::label_value(one, name).to_owned();
                set_label(&mut labels, name, value);
            }
        }
    }
    labels
}

fn set_operation(
    op: &str,
    matching: &VectorMatching,
    left: Vec<(Labels, f64)>,
    right: Vec<(Labels, f64)>,
) -> Vec<(Labels, f64)> {
    let right_signatures: BTreeSet<Labels> = right
        .iter()
        .map(|(labels, _)| signature(matching, labels))
        .collect();
    match op {
        "and" => left
            .into_iter()
            .filter(|(labels, _)| right_signatures.contains(&signature(matching, labels)))
            .collect(),
        "unless" => left
            .into_iter()
            .filter(|(labels, _)| !right_signatures.contains(&signature(matching, labels)))
            .collect(),
        _ => {
            let left_signatures: BTreeSet<Labels> = left
                .iter()
                .map(|(labels, _)| signature(matching, labels))
                .collect();
            let mut out = left;
            out.extend(
                right
                    .into_iter()
                    .filter(|(labels, _)| !left_signatures.contains(&signature(matching, labels))),
            );
            out
        }
    }
}

fn vector_scalar(
    binary: &Binary,
    vector: Vec<(Labels, f64)>,
    scalar: f64,
    scalar_on_left: bool,
) -> Vec<(Labels, f64)> {
    let mut out = Vec::with_capacity(vector.len());
    for (labels, value) in vector {
        let (left, right) = if scalar_on_left {
            (scalar, value)
        } else {
            (value, scalar)
        };
        let (mut result, keep) = operate(binary.op, left, right);
        if is_comparison(binary.op) {
            if binary.returns_bool {
                result = if keep { 1.0 } else { 0.0 };
            } else if keep {
                result = value;
            } else {
                continue;
            }
        }
        let labels = if drops_name(binary) {
            without_name(&labels)
        } else {
            labels
        };
        out.push((labels, result));
    }
    out
}

fn vector_vector(
    binary: &Binary,
    left: Vec<(Labels, f64)>,
    right: Vec<(Labels, f64)>,
) -> Result<Vec<(Labels, f64)>, Failure> {
    let matching = binary.matching.clone().unwrap_or_default();
    if is_set_operator(binary.op) {
        return Ok(set_operation(binary.op, &matching, left, right));
    }
    let swapped = matches!(matching.cardinality, Cardinality::OneToMany(_));
    let (many, one, one_side) = if swapped {
        (right, left, "left")
    } else {
        (left, right, "right")
    };
    let mut ones: BTreeMap<Labels, (Labels, f64)> = BTreeMap::new();
    for (labels, value) in one {
        let key = signature(&matching, &labels);
        if let Some((existing, _)) = ones.get(&key) {
            return Err(Failure::Execution(format!(
                "found duplicate series for the match group {} on the {one_side} hand-side of the operation: [{}, {}];many-to-many matching not allowed: matching labels must be unique on one side",
                labels_text(&key),
                labels_text(existing),
                labels_text(&labels)
            )));
        }
        ones.insert(key, (labels, value));
    }
    let mut matched: BTreeSet<Labels> = BTreeSet::new();
    let mut produced: BTreeSet<Labels> = BTreeSet::new();
    let mut out = Vec::new();
    for (labels, value) in many {
        let key = signature(&matching, &labels);
        let Some((one_labels, one_value)) = ones.get(&key) else {
            continue;
        };
        let (left_value, right_value) = if swapped {
            (*one_value, value)
        } else {
            (value, *one_value)
        };
        let (mut result, keep) = operate(binary.op, left_value, right_value);
        if is_comparison(binary.op) {
            if binary.returns_bool {
                result = if keep { 1.0 } else { 0.0 };
            } else if !keep {
                continue;
            }
        }
        let metric = result_metric(binary, &matching, &labels, one_labels);
        if matching.cardinality == Cardinality::OneToOne {
            if !matched.insert(key) {
                return Err(Failure::Execution(
                    "multiple matches for labels: many-to-one matching must be explicit (group_left/group_right)".to_owned(),
                ));
            }
        } else if !produced.insert(metric.clone()) {
            return Err(Failure::Execution(
                "multiple matches for labels: grouping labels must ensure unique matches"
                    .to_owned(),
            ));
        }
        out.push((metric, result));
    }
    Ok(out)
}

fn group_key(grouping: Option<&Grouping>, labels: &Labels) -> Labels {
    match grouping {
        Some(Grouping::By(names)) => labels
            .iter()
            .filter(|(name, _)| names.contains(name))
            .cloned()
            .collect(),
        Some(Grouping::Without(names)) => labels
            .iter()
            .filter(|(name, _)| name != "__name__" && !names.contains(name))
            .cloned()
            .collect(),
        None => Vec::new(),
    }
}

fn whole_number(value: f64) -> Result<i64, Failure> {
    if value.is_nan() || value >= i64::MAX as f64 || value < i64::MIN as f64 {
        return Err(Failure::BadData(format!(
            "Scalar value {value} overflows int64"
        )));
    }
    Ok(value as i64)
}

/// Evaluates expressions over the series a query read, within the query's [`Work`] limits.
#[derive(Debug)]
pub(crate) struct Evaluator<'d, 'b> {
    data: &'d Fetched,
    pub(crate) work: Work<'b>,
    range: Option<(i64, i64)>,
}

impl<'d, 'b> Evaluator<'d, 'b> {
    pub(crate) fn new(data: &'d Fetched, work: Work<'b>) -> Self {
        Self { data, work, range: None }
    }

    /// The query's start and end, which `@ start()` and `@ end()` read.
    pub(crate) fn over(mut self, start_ms: i64, end_ms: i64) -> Self {
        self.range = Some((start_ms, end_ms));
        self
    }

    fn base_time(&self, at: Option<super::At>, time_ms: i64) -> i64 {
        match at {
            None => time_ms,
            Some(super::At::Fixed(fixed)) => fixed,
            Some(super::At::Start) => self.range.map_or(time_ms, |(start, _)| start),
            Some(super::At::End) => self.range.map_or(time_ms, |(_, end)| end),
        }
    }

    /// Evaluates `expr` at `time_ms`.
    pub(crate) fn evaluate(&mut self, expr: &Expr, time_ms: i64) -> Result<Data, Failure> {
        let data = self.evaluate_unchecked(expr, time_ms)?;
        if let Data::Vector(vector) = &data {
            ensure_unique_label_sets(vector)?;
        }
        Ok(data)
    }

    fn evaluate_unchecked(&mut self, expr: &Expr, time_ms: i64) -> Result<Data, Failure> {
        match expr {
            Expr::Number(number) => Ok(Data::Scalar(*number)),
            Expr::Text(_) => Err(Failure::BadData(
                "a string is only an argument of a function or an aggregation".to_owned(),
            )),
            Expr::Selector(selector) => self.select(selector, time_ms),
            Expr::Call(name, arguments) => self.call(name, arguments, time_ms),
            Expr::Aggregate(aggregation) => self.aggregate(aggregation, time_ms),
            Expr::Binary(binary) => self.binary(binary, time_ms),
            Expr::Subquery(subquery) => self.subquery(subquery, time_ms),
        }
    }

    fn select(&mut self, selector: &Selector, time_ms: i64) -> Result<Data, Failure> {
        let at = self
            .base_time(selector.at, time_ms)
            .saturating_sub(selector.offset_ms);
        let chosen = chosen(selector, self.data);
        match selector.range_ms {
            Some(range) => {
                let mut matrix = Vec::new();
                for series in chosen {
                    let samples = window(&series.samples, at.saturating_sub(range), at);
                    if samples.is_empty() {
                        continue;
                    }
                    self.work.take(samples.len())?;
                    matrix.push((series.labels.clone(), samples.to_vec()));
                }
                Ok(Data::Matrix(matrix))
            }
            None => {
                let vector: Vec<(Labels, f64)> = chosen
                    .into_iter()
                    .filter_map(|series| {
                        window(&series.samples, at.saturating_sub(LOOKBACK_MS), at)
                            .last()
                            .map(|(_, value)| (series.labels.clone(), *value))
                    })
                    .collect();
                self.work.take(vector.len())?;
                Ok(Data::Vector(vector))
            }
        }
    }

    fn scalar_argument(
        &mut self,
        function: &str,
        expr: &Expr,
        time_ms: i64,
    ) -> Result<f64, Failure> {
        match self.evaluate(expr, time_ms)? {
            Data::Scalar(value) => Ok(value),
            _ => Err(Failure::BadData(format!(
                "expected type scalar in call to function {function}"
            ))),
        }
    }

    fn vector_argument(
        &mut self,
        function: &str,
        expr: &Expr,
        time_ms: i64,
    ) -> Result<Vec<(Labels, f64)>, Failure> {
        match self.evaluate(expr, time_ms)? {
            Data::Vector(vector) => Ok(vector),
            _ => Err(Failure::BadData(format!(
                "expected type instant vector in call to function {function}"
            ))),
        }
    }

    fn range_argument(
        &mut self,
        name: &str,
        argument: &Expr,
        time_ms: i64,
    ) -> Result<(Vec<(Labels, Vec<(i64, f64)>)>, i64, i64), Failure> {
        match (self.evaluate(argument, time_ms)?, range_and_offset(argument)) {
            (Data::Matrix(matrix), Some((range, offset))) => {
                let end = time_ms.saturating_sub(offset);
                Ok((matrix, end.saturating_sub(range), end))
            }
            _ => Err(Failure::BadData(format!("{name} needs a range selector"))),
        }
    }

    fn mapped(
        &mut self,
        name: &str,
        argument: &Expr,
        time_ms: i64,
        change: impl Fn(f64) -> f64,
    ) -> Result<Data, Failure> {
        let vector = self.vector_argument(name, argument, time_ms)?;
        Ok(Data::Vector(
            vector
                .into_iter()
                .map(|(labels, value)| (without_name(&labels), change(value)))
                .collect(),
        ))
    }

    fn call(&mut self, name: &str, arguments: &[Expr], time_ms: i64) -> Result<Data, Failure> {
        match (name, arguments) {
            ("time", []) => Ok(Data::Scalar(time_ms as f64 / 1000.0)),
            (date, []) if super::functions::DATE_FUNCTIONS.contains(&date) => {
                let value = super::functions::date_function(date, time_ms as f64 / 1000.0)
                    .unwrap_or(f64::NAN);
                Ok(Data::Vector(vec![(Vec::new(), value)]))
            }
            ("clamp", [vector, low, high]) => {
                let low = self.scalar_argument(name, low, time_ms)?;
                let high = self.scalar_argument(name, high, time_ms)?;
                if low > high {
                    return Ok(Data::Vector(Vec::new()));
                }
                self.mapped(name, vector, time_ms, |value| value.max(low).min(high))
            }
            ("clamp_min", [vector, low]) => {
                let low = self.scalar_argument(name, low, time_ms)?;
                self.mapped(name, vector, time_ms, |value| value.max(low))
            }
            ("clamp_max", [vector, high]) => {
                let high = self.scalar_argument(name, high, time_ms)?;
                self.mapped(name, vector, time_ms, |value| value.min(high))
            }
            ("round", [vector, nearest]) => {
                let nearest = self.scalar_argument(name, nearest, time_ms)?;
                let inverse = 1.0 / nearest;
                self.mapped(name, vector, time_ms, |value| (value * inverse + 0.5).floor() / inverse)
            }
            ("sort" | "sort_desc", [vector]) => {
                let mut vector = self.vector_argument(name, vector, time_ms)?;
                let descending = name == "sort_desc";
                vector.sort_by(|(_, a), (_, b)| {
                    let order = match (a.is_nan(), b.is_nan()) {
                        (true, true) => std::cmp::Ordering::Equal,
                        (true, false) => std::cmp::Ordering::Greater,
                        (false, true) => std::cmp::Ordering::Less,
                        (false, false) if descending => b.total_cmp(a),
                        (false, false) => a.total_cmp(b),
                    };
                    order
                });
                Ok(Data::Vector(vector))
            }
            ("absent", [argument]) => {
                let vector = self.vector_argument(name, argument, time_ms)?;
                Ok(Data::Vector(if vector.is_empty() {
                    vec![(absent_labels(argument), 1.0)]
                } else {
                    Vec::new()
                }))
            }
            ("absent_over_time", [argument]) => {
                let (matrix, _, _) = self.range_argument(name, argument, time_ms)?;
                Ok(Data::Vector(if matrix.iter().all(|(_, samples)| samples.is_empty()) {
                    vec![(absent_labels(argument), 1.0)]
                } else {
                    Vec::new()
                }))
            }
            ("predict_linear", [range, seconds]) => {
                let seconds = self.scalar_argument(name, seconds, time_ms)?;
                let (matrix, _, end) = self.range_argument(name, range, time_ms)?;
                let mut out = Vec::new();
                for (labels, samples) in matrix {
                    if let Some((slope, intercept)) = super::functions::regression(&samples, end) {
                        out.push((without_name(&labels), intercept + slope * seconds));
                    }
                }
                Ok(Data::Vector(out))
            }
            ("quantile_over_time", [phi, range]) => {
                let phi = self.scalar_argument(name, phi, time_ms)?;
                let (matrix, _, _) = self.range_argument(name, range, time_ms)?;
                Ok(Data::Vector(
                    matrix
                        .into_iter()
                        .filter(|(_, samples)| !samples.is_empty())
                        .map(|(labels, samples)| {
                            let values = samples.iter().map(|(_, value)| *value).collect();
                            (without_name(&labels), super::functions::quantile(phi, values))
                        })
                        .collect(),
                ))
            }
            ("vector", [argument]) => {
                let value = self.scalar_argument(name, argument, time_ms)?;
                Ok(Data::Vector(vec![(Vec::new(), value)]))
            }
            ("scalar", [argument]) => {
                let vector = self.vector_argument(name, argument, time_ms)?;
                Ok(Data::Scalar(match vector.as_slice() {
                    [(_, value)] => *value,
                    _ => f64::NAN,
                }))
            }
            ("histogram_quantile", [quantile, buckets]) => {
                let quantile = self.scalar_argument(name, quantile, time_ms)?;
                let buckets = self.vector_argument(name, buckets, time_ms)?;
                Ok(Data::Vector(histogram_quantile(quantile, buckets)))
            }
            ("label_replace", [vector, destination, replacement, source, pattern]) => {
                let destination = text_argument(name, destination)?;
                let replacement = text_argument(name, replacement)?;
                let source = text_argument(name, source)?;
                let pattern = text_argument(name, pattern)?;
                let vector = self.vector_argument(name, vector, time_ms)?;
                let work = &mut self.work;
                label_replace(
                    vector,
                    destination,
                    replacement,
                    source,
                    pattern,
                    &mut |bytes| work.take_label_bytes(bytes),
                )
                .map(Data::Vector)
            }
            ("label_join", [vector, destination, separator, sources @ ..]) => {
                let destination = text_argument(name, destination)?;
                let separator = text_argument(name, separator)?;
                let sources = sources
                    .iter()
                    .map(|source| text_argument(name, source))
                    .collect::<Result<Vec<&str>, Failure>>()?;
                let vector = self.vector_argument(name, vector, time_ms)?;
                let work = &mut self.work;
                label_join(vector, destination, separator, &sources, &mut |bytes| {
                    work.take_label_bytes(bytes)
                })
                .map(Data::Vector)
            }
            (
                "time" | "vector" | "scalar" | "histogram_quantile" | "label_replace"
                | "label_join",
                _,
            ) => Err(Failure::BadData(format!(
                "wrong number of arguments for function {name}"
            ))),
            (_, [argument]) => self.single_argument(name, argument, time_ms),
            _ => Err(Failure::BadData(format!("{name} takes one argument"))),
        }
    }

    fn single_argument(
        &mut self,
        name: &str,
        argument: &Expr,
        time_ms: i64,
    ) -> Result<Data, Failure> {
        match (
            self.evaluate(argument, time_ms)?,
            range_and_offset(argument),
        ) {
            (Data::Matrix(matrix), Some((range, offset))) => {
                let end = time_ms.saturating_sub(offset);
                let mut out = Vec::new();
                for (labels, samples) in matrix {
                    if let Some(value) =
                        range_function(name, &samples, end.saturating_sub(range), end)?
                    {
                        out.push((without_name(&labels), value));
                    }
                }
                Ok(Data::Vector(out))
            }
            (Data::Vector(vector), _) => {
                let mut out = Vec::with_capacity(vector.len());
                for (labels, value) in vector {
                    out.push((
                        without_name(&labels),
                        scalar_function(name, value)
                            .ok_or_else(|| Failure::BadData(format!("unknown function {name}")))?,
                    ));
                }
                Ok(Data::Vector(out))
            }
            (Data::Scalar(value), _) => scalar_function(name, value)
                .map(Data::Scalar)
                .ok_or_else(|| Failure::BadData(format!("{name} does not take a scalar"))),
            (Data::Matrix(_), None) => {
                Err(Failure::BadData(format!("{name} needs a range selector")))
            }
        }
    }

    fn aggregate(&mut self, aggregation: &Aggregation, time_ms: i64) -> Result<Data, Failure> {
        let op = aggregation.op.as_str();
        let parameter = match &aggregation.parameter {
            Some(Expr::Text(_)) | None => f64::NAN,
            Some(expr) => match self.evaluate(expr, time_ms)? {
                Data::Scalar(value) => value,
                _ => {
                    return Err(Failure::BadData(format!(
                        "expected type scalar in aggregation parameter of {op}"
                    )));
                }
            },
        };
        let Data::Vector(vector) = self.evaluate(&aggregation.inner, time_ms)? else {
            return Err(Failure::BadData(format!("{op} needs an instant vector")));
        };
        let mut groups: BTreeMap<Labels, Vec<(Labels, f64)>> = BTreeMap::new();
        for (labels, value) in vector {
            groups
                .entry(group_key(aggregation.grouping.as_ref(), &labels))
                .or_default()
                .push((labels, value));
        }
        let mut out = Vec::new();
        match op {
            "topk" | "bottomk" => {
                let k = whole_number(parameter)?;
                let taken = usize::try_from(k).unwrap_or(0);
                for (_, mut members) in groups {
                    members.sort_by(|(_, a), (_, b)| ranked(*a, *b, op == "topk"));
                    members.truncate(taken);
                    out.extend(members);
                }
            }
            "limitk" => {
                let k = whole_number(parameter)?;
                let taken = usize::try_from(k).unwrap_or(0);
                for (_, mut members) in groups {
                    members.sort_by_key(|(labels, _)| labels_hash(labels));
                    members.truncate(taken);
                    out.extend(members);
                }
            }
            "limit_ratio" => {
                if !(-1.0..=1.0).contains(&parameter) {
                    return Err(Failure::BadData(format!(
                        "invalid ratio {parameter} for limit_ratio, must be between -1 and 1"
                    )));
                }
                for (_, members) in groups {
                    for (labels, value) in members {
                        let share = labels_hash(&labels) as f64 / u64::MAX as f64;
                        let kept = if parameter >= 0.0 {
                            share < parameter
                        } else {
                            share >= 1.0 + parameter
                        };
                        if kept {
                            out.push((labels, value));
                        }
                    }
                }
            }
            "count_values" => {
                let label = match &aggregation.parameter {
                    Some(Expr::Text(label)) => label.as_str(),
                    _ => "",
                };
                if !super::is_label_name(label) {
                    return Err(Failure::BadData(format!("invalid label name {label:?}")));
                }
                for (key, members) in groups {
                    let mut counts: BTreeMap<String, f64> = BTreeMap::new();
                    for (_, value) in members {
                        *counts.entry(value_text(value)).or_default() += 1.0;
                    }
                    for (text, count) in counts {
                        let mut labels = key.clone();
                        set_label(&mut labels, label, text);
                        out.push((labels, count));
                    }
                }
            }
            _ => {
                for (key, members) in groups {
                    let values: Vec<f64> = members.into_iter().map(|(_, value)| value).collect();
                    out.push((key, aggregate_values(op, parameter, values)));
                }
            }
        }
        Ok(Data::Vector(out))
    }

    fn binary(&mut self, binary: &Binary, time_ms: i64) -> Result<Data, Failure> {
        let left = self.evaluate(&binary.left, time_ms)?;
        let right = self.evaluate(&binary.right, time_ms)?;
        let vectors_only = || {
            Failure::BadData(format!(
                "set operator or vector matching on \"{}\" is only allowed between instant vectors",
                binary.op
            ))
        };
        let explicit = binary.matching.is_some() || is_set_operator(binary.op);
        match (left, right) {
            (Data::Scalar(left), Data::Scalar(right)) => {
                if explicit {
                    return Err(vectors_only());
                }
                if is_comparison(binary.op) && !binary.returns_bool {
                    return Err(Failure::BadData(
                        "comparisons between scalars must use BOOL modifier".to_owned(),
                    ));
                }
                let (result, keep) = operate(binary.op, left, right);
                Ok(Data::Scalar(if is_comparison(binary.op) {
                    if keep { 1.0 } else { 0.0 }
                } else {
                    result
                }))
            }
            (Data::Vector(vector), Data::Scalar(scalar)) if !explicit => {
                Ok(Data::Vector(vector_scalar(binary, vector, scalar, false)))
            }
            (Data::Scalar(scalar), Data::Vector(vector)) if !explicit => {
                Ok(Data::Vector(vector_scalar(binary, vector, scalar, true)))
            }
            (Data::Vector(left), Data::Vector(right)) => {
                vector_vector(binary, left, right).map(Data::Vector)
            }
            (Data::Vector(_), Data::Scalar(_)) | (Data::Scalar(_), Data::Vector(_)) => {
                Err(vectors_only())
            }
            _ => Err(Failure::BadData(
                "binary operations need scalars or instant vectors".to_owned(),
            )),
        }
    }

    fn subquery(&mut self, subquery: &Subquery, time_ms: i64) -> Result<Data, Failure> {
        let step = subquery.step_ms.unwrap_or(DEFAULT_SUBQUERY_STEP_MS);
        let end = self
            .base_time(subquery.at, time_ms)
            .saturating_sub(subquery.offset_ms);
        let start = end.saturating_sub(subquery.range_ms);
        let Some(mut at) = start.div_euclid(step).checked_mul(step) else {
            return Ok(Data::Matrix(Vec::new()));
        };
        if at <= start {
            let Some(next) = at.checked_add(step) else {
                return Ok(Data::Matrix(Vec::new()));
            };
            at = next;
        }
        let steps = if at <= end {
            usize::try_from((end - at) / step).map_or(usize::MAX, |steps| steps.saturating_add(1))
        } else {
            0
        };
        self.work.run_subquery_steps(steps)?;
        let mut series: BTreeMap<Labels, Vec<(i64, f64)>> = BTreeMap::new();
        let held_before = self.work.step_samples;
        let label_bytes_before = self.work.step_label_bytes;
        let mut collected = 0usize;
        while at <= end {
            let inner = self.evaluate(&subquery.inner, at)?;
            self.work
                .give_back_to(held_before.saturating_add(collected), label_bytes_before);
            match inner {
                Data::Vector(vector) => {
                    collected = collected.saturating_add(vector.len());
                    self.work.take(vector.len())?;
                    for (labels, value) in vector {
                        series.entry(labels).or_default().push((at, value));
                    }
                }
                Data::Scalar(_) => {
                    return Err(Failure::BadData(
                        "subquery is only allowed on instant vector, got scalar".to_owned(),
                    ));
                }
                Data::Matrix(_) => {
                    return Err(Failure::BadData(
                        "subquery is only allowed on instant vector, got range vector".to_owned(),
                    ));
                }
            }
            let Some(next) = at.checked_add(step) else {
                break;
            };
            at = next;
        }
        Ok(Data::Matrix(series.into_iter().collect()))
    }
}

fn ranked(a: f64, b: f64, largest_first: bool) -> std::cmp::Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        (false, false) if largest_first => b.total_cmp(&a),
        (false, false) => a.total_cmp(&b),
    }
}

fn value_text(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else if value == f64::INFINITY {
        "+Inf".to_owned()
    } else if value == f64::NEG_INFINITY {
        "-Inf".to_owned()
    } else {
        format!("{value}")
    }
}
