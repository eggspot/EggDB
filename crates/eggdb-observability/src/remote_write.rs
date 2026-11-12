use crate::line_protocol::{FieldValue, Point};

const MAX_DECOMPRESSED: usize = 64 << 20;
const MOST_POINTS: usize = 1 << 20;
const MOST_EXPANDED_BYTES: usize = 256 << 20;

pub(crate) struct Reader<'b> {
    pub(crate) bytes: &'b [u8],
    pub(crate) at: usize,
}

impl<'b> Reader<'b> {
    fn varint(&mut self) -> Result<u64, String> {
        eggdb_columnar::read_varint(self.bytes, &mut self.at).map_err(|error| error.0)
    }

    fn take(&mut self, count: usize) -> Result<&'b [u8], String> {
        let end = self
            .at
            .checked_add(count)
            .filter(|end| *end <= self.bytes.len())
            .ok_or("a protobuf field runs past its message")?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    pub(crate) fn next(&mut self) -> Result<Option<(u64, Field<'b>)>, String> {
        if self.at >= self.bytes.len() {
            return Ok(None);
        }
        let key = self.varint()?;
        let field = match key & 7 {
            0 => Field::Varint(self.varint()?),
            1 => Field::Fixed64(u64::from_le_bytes(
                self.take(8)?
                    .try_into()
                    .map_err(|_| "a fixed64 is cut short")?,
            )),
            2 => {
                let length = usize::try_from(self.varint()?)
                    .map_err(|_| "a protobuf length does not fit")?;
                Field::Bytes(self.take(length)?)
            }
            5 => {
                self.take(4)?;
                Field::Other
            }
            other => return Err(format!("protobuf wire type {other} is not supported")),
        };
        Ok(Some((key >> 3, field)))
    }
}

pub(crate) enum Field<'b> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'b [u8]),
    Other,
}

pub(crate) fn text(bytes: &[u8]) -> Result<String, String> {
    String::from_utf8(bytes.to_vec()).map_err(|_| "a label is not UTF-8".to_owned())
}

/// The samples of a Prometheus remote write request (version 1: a Snappy-compressed
/// `WriteRequest`), each as a point of its metric's measurement with the field `value`.
pub(crate) fn decode(compressed: &[u8]) -> Result<Vec<Point>, String> {
    let body = eggdb_columnar::snappy_decompress(compressed, MAX_DECOMPRESSED)
        .map_err(|error| format!("the body is not a Snappy block: {}", error.0))?;
    let mut request = Reader {
        bytes: &body,
        at: 0,
    };
    let mut points = Vec::new();
    let mut expanded_bytes = 0usize;
    while let Some((number, field)) = request.next()? {
        let (1, Field::Bytes(series)) = (number, field) else {
            continue;
        };
        let mut series = Reader {
            bytes: series,
            at: 0,
        };
        let mut name = None;
        let mut tags = Vec::new();
        let mut samples = Vec::new();
        while let Some((number, field)) = series.next()? {
            match (number, field) {
                (1, Field::Bytes(label)) => {
                    let mut label = Reader {
                        bytes: label,
                        at: 0,
                    };
                    let (mut key, mut value) = (String::new(), String::new());
                    while let Some((number, field)) = label.next()? {
                        match (number, field) {
                            (1, Field::Bytes(bytes)) => key = text(bytes)?,
                            (2, Field::Bytes(bytes)) => value = text(bytes)?,
                            _ => {}
                        }
                    }
                    if key == "__name__" {
                        name = Some(value);
                    } else if !value.is_empty() {
                        tags.push((key, value));
                    }
                }
                (2, Field::Bytes(sample)) => {
                    let mut sample = Reader {
                        bytes: sample,
                        at: 0,
                    };
                    let (mut value, mut time_ms) = (0.0, 0i64);
                    while let Some((number, field)) = sample.next()? {
                        match (number, field) {
                            (1, Field::Fixed64(bits)) => value = f64::from_bits(bits),
                            (2, Field::Varint(raw)) => time_ms = raw as i64,
                            _ => {}
                        }
                    }
                    samples.push((value, time_ms));
                }
                _ => {}
            }
        }
        let name = name.ok_or("a time series has no __name__ label")?;
        let per_point = name.len()
            + tags
                .iter()
                .map(|(key, value)| key.len() + value.len())
                .sum::<usize>();
        for (value, time_ms) in samples {
            expanded_bytes = expanded_bytes.saturating_add(per_point.max(1));
            if points.len() >= MOST_POINTS || expanded_bytes > MOST_EXPANDED_BYTES {
                return Err(format!(
                    "a write may hold at most {MOST_POINTS} samples and {MOST_EXPANDED_BYTES} bytes of their labels"
                ));
            }
            if value.is_finite() {
                points.push(Point {
                    measurement: name.clone(),
                    tags: tags.clone(),
                    fields: vec![("value".to_owned(), FieldValue::Float(value))],
                    time_ns: time_ms.saturating_mul(1_000_000),
                });
            }
        }
    }
    Ok(points)
}

/// What a remote write 2.0 request held, for the `X-Prometheus-Remote-Write-*-Written` headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Written {
    pub(crate) samples: usize,
    pub(crate) histograms: usize,
    pub(crate) exemplars: usize,
}

fn symbol<'s>(symbols: &'s [String], reference: u64) -> Result<&'s str, String> {
    usize::try_from(reference)
        .ok()
        .and_then(|at| symbols.get(at))
        .map(String::as_str)
        .ok_or_else(|| format!("a label refers to symbol {reference}, past the symbol table"))
}

fn label_references(field: Field<'_>, into: &mut Vec<u64>) -> Result<(), String> {
    match field {
        Field::Varint(reference) => into.push(reference),
        Field::Bytes(packed) => {
            let mut reader = Reader {
                bytes: packed,
                at: 0,
            };
            while reader.at < packed.len() {
                into.push(reader.varint()?);
            }
        }
        _ => {}
    }
    Ok(())
}

const CUSTOM_BUCKETS_SCHEMA: i64 = -53;
const MOST_BUCKETS: usize = 1 << 16;

fn zigzag(raw: u64) -> i64 {
    ((raw >> 1) as i64) ^ -((raw & 1) as i64)
}

fn varints(field: Field<'_>, into: &mut Vec<u64>) -> Result<(), String> {
    match field {
        Field::Varint(value) => into.push(value),
        Field::Bytes(packed) => {
            let mut reader = Reader { bytes: packed, at: 0 };
            while reader.at < packed.len() {
                into.push(reader.varint()?);
            }
        }
        _ => {}
    }
    Ok(())
}

fn doubles(field: Field<'_>, into: &mut Vec<f64>) -> Result<(), String> {
    match field {
        Field::Fixed64(bits) => into.push(f64::from_bits(bits)),
        Field::Bytes(packed) => {
            if packed.len() % 8 != 0 {
                return Err("packed doubles are cut short".to_owned());
            }
            into.extend(
                packed
                    .chunks_exact(8)
                    .map(|chunk| f64::from_bits(u64::from_le_bytes(chunk.try_into().unwrap_or([0; 8])))),
            );
        }
        _ => {}
    }
    Ok(())
}

fn span(bytes: &[u8]) -> Result<(i64, u64), String> {
    let mut reader = Reader { bytes, at: 0 };
    let (mut offset, mut length) = (0i64, 0u64);
    while let Some((number, field)) = reader.next()? {
        match (number, field) {
            (1, Field::Varint(raw)) => offset = zigzag(raw),
            (2, Field::Varint(raw)) => length = raw,
            _ => {}
        }
    }
    Ok((offset, length))
}

#[derive(Debug, Default)]
struct Buckets {
    spans: Vec<(i64, u64)>,
    deltas: Vec<u64>,
    counts: Vec<f64>,
}

impl Buckets {
    fn indexed(&self) -> Result<Vec<(i64, f64)>, String> {
        let mut out = Vec::new();
        let mut index = 0i64;
        let mut running = 0i64;
        let mut position = 0usize;
        for (span_at, (offset, length)) in self.spans.iter().enumerate() {
            index = if span_at == 0 { *offset } else { index.saturating_add(*offset) };
            for _ in 0..*length {
                if out.len() >= MOST_BUCKETS {
                    return Err(format!("a histogram may have at most {MOST_BUCKETS} buckets"));
                }
                let count = if self.counts.is_empty() {
                    running = running.saturating_add(zigzag(*self.deltas.get(position).ok_or("a histogram has fewer deltas than buckets")?));
                    running as f64
                } else {
                    *self.counts.get(position).ok_or("a histogram has fewer counts than buckets")?
                };
                out.push((index, count));
                index = index.saturating_add(1);
                position += 1;
            }
        }
        Ok(out)
    }
}

/// A native histogram of a remote write 2.0 request: its count, sum and time, and its
/// buckets as classic cumulative `le` bounds, ending with `+Inf`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NativeHistogram {
    pub(crate) count: f64,
    pub(crate) sum: f64,
    pub(crate) time_ms: i64,
    pub(crate) cumulative: Vec<(f64, f64)>,
}

fn upper_bound(schema: i64, index: i64, custom: &[f64]) -> f64 {
    if schema == CUSTOM_BUCKETS_SCHEMA {
        return usize::try_from(index)
            .ok()
            .and_then(|at| custom.get(at).copied())
            .unwrap_or(f64::INFINITY);
    }
    let base = 2f64.powf(2f64.powi(-(schema.clamp(-4, 8) as i32)));
    base.powf(index as f64)
}

fn native_histogram(bytes: &[u8]) -> Result<NativeHistogram, String> {
    let mut reader = Reader { bytes, at: 0 };
    let (mut count, mut sum, mut time_ms, mut schema) = (0.0, 0.0, 0i64, 0i64);
    let (mut zero_threshold, mut zero_count) = (0.0, 0.0);
    let (mut negative, mut positive) = (Buckets::default(), Buckets::default());
    let mut custom = Vec::new();
    while let Some((number, field)) = reader.next()? {
        match (number, field) {
            (1, Field::Varint(whole)) => count = whole as f64,
            (2, Field::Fixed64(bits)) => count = f64::from_bits(bits),
            (3, Field::Fixed64(bits)) => sum = f64::from_bits(bits),
            (4, Field::Varint(raw)) => schema = zigzag(raw),
            (5, Field::Fixed64(bits)) => zero_threshold = f64::from_bits(bits),
            (6, Field::Varint(whole)) => zero_count = whole as f64,
            (7, Field::Fixed64(bits)) => zero_count = f64::from_bits(bits),
            (8, Field::Bytes(bytes)) => negative.spans.push(span(bytes)?),
            (9, field) => varints(field, &mut negative.deltas)?,
            (10, field) => doubles(field, &mut negative.counts)?,
            (11, Field::Bytes(bytes)) => positive.spans.push(span(bytes)?),
            (12, field) => varints(field, &mut positive.deltas)?,
            (13, field) => doubles(field, &mut positive.counts)?,
            (15, Field::Varint(raw)) => time_ms = raw as i64,
            (16, field) => doubles(field, &mut custom)?,
            _ => {}
        }
    }
    let mut cumulative = Vec::new();
    let mut running = 0.0;
    let mut negatives = negative.indexed()?;
    negatives.sort_by(|left, right| right.0.cmp(&left.0));
    for (index, bucket) in negatives {
        running += bucket;
        cumulative.push((-upper_bound(schema, index - 1, &custom), running));
    }
    if schema != CUSTOM_BUCKETS_SCHEMA || zero_count > 0.0 {
        running += zero_count;
        cumulative.push((zero_threshold, running));
    }
    for (index, bucket) in positive.indexed()? {
        running += bucket;
        cumulative.push((upper_bound(schema, index, &custom), running));
    }
    cumulative.retain(|(bound, _)| bound.is_finite());
    cumulative.push((f64::INFINITY, count.max(running)));
    Ok(NativeHistogram {
        count,
        sum,
        time_ms,
        cumulative,
    })
}

/// An exemplar of a remote write 2.0 series: its labels (such as `trace_id`), value and time.
#[derive(Debug, Clone, PartialEq)]
struct Exemplar {
    labels: Vec<(String, String)>,
    value: f64,
    time_ms: i64,
}

fn exemplar(bytes: &[u8], symbols: &[String]) -> Result<Exemplar, String> {
    let mut reader = Reader { bytes, at: 0 };
    let mut references = Vec::new();
    let (mut value, mut time_ms) = (0.0, 0i64);
    while let Some((number, field)) = reader.next()? {
        match (number, field) {
            (1, field) => label_references(field, &mut references)?,
            (2, Field::Fixed64(bits)) => value = f64::from_bits(bits),
            (3, Field::Varint(raw)) => time_ms = raw as i64,
            _ => {}
        }
    }
    if references.len() % 2 != 0 {
        return Err("an exemplar has an odd number of label references".to_owned());
    }
    let mut labels = Vec::new();
    for pair in references.chunks(2) {
        labels.push((symbol(symbols, pair[0])?.to_owned(), symbol(symbols, pair[1])?.to_owned()));
    }
    Ok(Exemplar { labels, value, time_ms })
}

/// The `le` label of a classic bucket bound, as Prometheus writes it.
pub(crate) fn bound_label(bound: f64) -> String {
    if bound == f64::INFINITY {
        "+Inf".to_owned()
    } else if bound == f64::NEG_INFINITY {
        "-Inf".to_owned()
    } else {
        format!("{bound}")
    }
}

/// The suffix of the measurement that keeps a metric's exemplars.
pub(crate) const EXEMPLARS_SUFFIX: &str = "_exemplars";
/// The prefix of the string fields that keep an exemplar's labels.
pub(crate) const EXEMPLAR_LABEL_PREFIX: &str = "exemplar_";

/// The samples of a Prometheus remote write 2.0 request (a Snappy-compressed
/// `io.prometheus.write.v2.Request`): labels resolved through the request's symbol table, float
/// samples as points with the field `value`, each native histogram as `<name>_count`,
/// `<name>_sum` and cumulative `<name>_bucket` points with an `le` tag, as the classic exposition
/// names them, and each exemplar as a `<name>_exemplars` point whose string fields
/// `exemplar_<label>` keep its labels. Metadata is skipped.
pub(crate) fn decode_v2(compressed: &[u8]) -> Result<(Vec<Point>, Written), String> {
    let body = eggdb_columnar::snappy_decompress(compressed, MAX_DECOMPRESSED)
        .map_err(|error| format!("the body is not a Snappy block: {}", error.0))?;
    let mut request = Reader { bytes: &body, at: 0 };
    let mut symbols = Vec::new();
    let mut series_list = Vec::new();
    while let Some((number, field)) = request.next()? {
        match (number, field) {
            (4, Field::Bytes(bytes)) => symbols.push(text(bytes)?),
            (5, Field::Bytes(series)) => series_list.push(series),
            _ => {}
        }
    }
    let mut points = Vec::new();
    let mut written = Written::default();
    let mut expanded_bytes = 0usize;
    for series in series_list {
        let mut reader = Reader { bytes: series, at: 0 };
        let mut references = Vec::new();
        let mut samples = Vec::new();
        let mut histograms = Vec::new();
        let mut exemplars = Vec::new();
        while let Some((number, field)) = reader.next()? {
            match (number, field) {
                (1, field) => label_references(field, &mut references)?,
                (2, Field::Bytes(sample)) => {
                    let mut sample = Reader { bytes: sample, at: 0 };
                    let (mut value, mut time_ms) = (0.0, 0i64);
                    while let Some((number, field)) = sample.next()? {
                        match (number, field) {
                            (1, Field::Fixed64(bits)) => value = f64::from_bits(bits),
                            (2, Field::Varint(raw)) => time_ms = raw as i64,
                            _ => {}
                        }
                    }
                    samples.push((value, time_ms));
                }
                (3, Field::Bytes(histogram)) => histograms.push(native_histogram(histogram)?),
                (4, Field::Bytes(found)) => exemplars.push(exemplar(found, &symbols)?),
                _ => {}
            }
        }
        if references.len() % 2 != 0 {
            return Err("a time series has an odd number of label references".to_owned());
        }
        let mut name = None;
        let mut tags = Vec::new();
        for pair in references.chunks(2) {
            let key = symbol(&symbols, pair[0])?;
            let value = symbol(&symbols, pair[1])?;
            if key == "__name__" {
                name = Some(value.to_owned());
            } else if !value.is_empty() {
                tags.push((key.to_owned(), value.to_owned()));
            }
        }
        let name = name.ok_or("a time series has no __name__ label")?;
        let mut push = |measurement: String,
                        extra_tags: Vec<(String, String)>,
                        fields: Vec<(String, FieldValue)>,
                        time_ms: i64|
         -> Result<(), String> {
            expanded_bytes = expanded_bytes.saturating_add(measurement.len().max(1));
            if points.len() >= MOST_POINTS || expanded_bytes > MOST_EXPANDED_BYTES {
                return Err(format!(
                    "a write may hold at most {MOST_POINTS} samples and {MOST_EXPANDED_BYTES} bytes of their labels"
                ));
            }
            let finite = fields.iter().all(|(_, value)| !matches!(value, FieldValue::Float(number) if !number.is_finite()));
            if finite {
                let mut all_tags = tags.clone();
                all_tags.extend(extra_tags);
                points.push(Point {
                    measurement,
                    tags: all_tags,
                    fields,
                    time_ns: time_ms.saturating_mul(1_000_000),
                });
            }
            Ok(())
        };
        let value = |number: f64| vec![("value".to_owned(), FieldValue::Float(number))];
        for (number, time_ms) in samples {
            push(name.clone(), Vec::new(), value(number), time_ms)?;
            written.samples += 1;
        }
        for histogram in histograms {
            push(format!("{name}_count"), Vec::new(), value(histogram.count), histogram.time_ms)?;
            push(format!("{name}_sum"), Vec::new(), value(histogram.sum), histogram.time_ms)?;
            for (bound, cumulative) in &histogram.cumulative {
                push(
                    format!("{name}_bucket"),
                    vec![("le".to_owned(), bound_label(*bound))],
                    value(*cumulative),
                    histogram.time_ms,
                )?;
            }
            written.histograms += 1;
        }
        for found in exemplars {
            let mut fields = value(found.value);
            fields.extend(
                found
                    .labels
                    .into_iter()
                    .map(|(key, label)| (format!("{EXEMPLAR_LABEL_PREFIX}{key}"), FieldValue::Text(label))),
            );
            push(format!("{name}{EXEMPLARS_SUFFIX}"), Vec::new(), fields, found.time_ms)?;
            written.exemplars += 1;
        }
    }
    Ok((points, written))
}
