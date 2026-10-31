use crate::line_protocol::{FieldValue, Point};

const MAX_DECOMPRESSED: usize = 64 << 20;

struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Reader<'b> {
    fn varint(&mut self) -> Result<u64, String> {
        eggdb_columnar::read_varint(self.bytes, &mut self.at).map_err(|error| error.0)
    }

    fn take(&mut self, count: usize) -> Result<&'b [u8], String> {
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len()).ok_or("a protobuf field runs past its message")?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn next(&mut self) -> Result<Option<(u64, Field<'b>)>, String> {
        if self.at >= self.bytes.len() {
            return Ok(None);
        }
        let key = self.varint()?;
        let field = match key & 7 {
            0 => Field::Varint(self.varint()?),
            1 => Field::Fixed64(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| "a fixed64 is cut short")?)),
            2 => {
                let length = usize::try_from(self.varint()?).map_err(|_| "a protobuf length does not fit")?;
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

enum Field<'b> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'b [u8]),
    Other,
}

fn text(bytes: &[u8]) -> Result<String, String> {
    String::from_utf8(bytes.to_vec()).map_err(|_| "a label is not UTF-8".to_owned())
}

/// The samples of a Prometheus remote write request (version 1: a Snappy-compressed
/// `WriteRequest`), each as a point of its metric's measurement with the field `value`.
pub(crate) fn decode(compressed: &[u8]) -> Result<Vec<Point>, String> {
    let body = eggdb_columnar::snappy_decompress(compressed, MAX_DECOMPRESSED).map_err(|error| format!("the body is not a Snappy block: {}", error.0))?;
    let mut request = Reader { bytes: &body, at: 0 };
    let mut points = Vec::new();
    while let Some((number, field)) = request.next()? {
        let (1, Field::Bytes(series)) = (number, field) else {
            continue;
        };
        let mut series = Reader { bytes: series, at: 0 };
        let mut name = None;
        let mut tags = Vec::new();
        let mut samples = Vec::new();
        while let Some((number, field)) = series.next()? {
            match (number, field) {
                (1, Field::Bytes(label)) => {
                    let mut label = Reader { bytes: label, at: 0 };
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
                _ => {}
            }
        }
        let name = name.ok_or("a time series has no __name__ label")?;
        for (value, time_ms) in samples {
            if value.is_finite() {
                points.push(Point { measurement: name.clone(), tags: tags.clone(), fields: vec![("value".to_owned(), FieldValue::Float(value))], time_ns: time_ms.saturating_mul(1_000_000) });
            }
        }
    }
    Ok(points)
}
