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

fn histogram_count_and_sum(bytes: &[u8]) -> Result<(f64, f64, i64), String> {
    let mut reader = Reader { bytes, at: 0 };
    let (mut count, mut sum, mut time_ms) = (0.0, 0.0, 0i64);
    while let Some((number, field)) = reader.next()? {
        match (number, field) {
            (1, Field::Varint(whole)) => count = whole as f64,
            (2, Field::Fixed64(bits)) => count = f64::from_bits(bits),
            (3, Field::Fixed64(bits)) => sum = f64::from_bits(bits),
            (15, Field::Varint(raw)) => time_ms = raw as i64,
            _ => {}
        }
    }
    Ok((count, sum, time_ms))
}

/// The samples of a Prometheus remote write 2.0 request (a Snappy-compressed
/// `io.prometheus.write.v2.Request`): labels resolved through the request's symbol table, float
/// samples as points with the field `value`, and each native histogram as two points,
/// `<name>_count` and `<name>_sum`, as the classic exposition names them. Exemplars and metadata
/// are skipped.
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
                (3, Field::Bytes(histogram)) => histograms.push(histogram_count_and_sum(histogram)?),
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
        let mut push = |measurement: String, value: f64, time_ms: i64| -> Result<(), String> {
            expanded_bytes = expanded_bytes.saturating_add(measurement.len().max(1));
            if points.len() >= MOST_POINTS || expanded_bytes > MOST_EXPANDED_BYTES {
                return Err(format!(
                    "a write may hold at most {MOST_POINTS} samples and {MOST_EXPANDED_BYTES} bytes of their labels"
                ));
            }
            if value.is_finite() {
                points.push(Point {
                    measurement,
                    tags: tags.clone(),
                    fields: vec![("value".to_owned(), FieldValue::Float(value))],
                    time_ns: time_ms.saturating_mul(1_000_000),
                });
            }
            Ok(())
        };
        for (value, time_ms) in samples {
            push(name.clone(), value, time_ms)?;
            written.samples += 1;
        }
        for (count, sum, time_ms) in histograms {
            push(format!("{name}_count"), count, time_ms)?;
            push(format!("{name}_sum"), sum, time_ms)?;
            written.histograms += 1;
        }
    }
    Ok((points, written))
}
