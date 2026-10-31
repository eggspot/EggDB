#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FieldValue {
    Float(f64),
    Integer(i64),
    Unsigned(u64),
    Text(String),
    Boolean(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Point {
    pub(crate) measurement: String,
    pub(crate) tags: Vec<(String, String)>,
    pub(crate) fields: Vec<(String, FieldValue)>,
    pub(crate) time_ns: i64,
}

/// How many nanoseconds one unit of a written timestamp is.
pub(crate) fn precision_ns(precision: &str) -> Option<i64> {
    match precision {
        "" | "ns" | "n" => Some(1),
        "us" | "u" => Some(1_000),
        "ms" => Some(1_000_000),
        "s" => Some(1_000_000_000),
        _ => None,
    }
}

struct Line<'l> {
    bytes: &'l [u8],
    at: usize,
}

impl Line<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn until(&mut self, stops: &[u8]) -> Result<String, String> {
        let mut out = Vec::new();
        while let Some(byte) = self.peek() {
            if stops.contains(&byte) {
                break;
            }
            if byte == b'\\' && self.bytes.get(self.at + 1).is_some_and(|next| stops.contains(next) || *next == b'\\' || *next == b'"') {
                out.push(self.bytes[self.at + 1]);
                self.at += 2;
                continue;
            }
            out.push(byte);
            self.at += 1;
        }
        String::from_utf8(out).map_err(|_| "a name is not UTF-8".to_owned())
    }

    fn quoted(&mut self) -> Result<String, String> {
        self.at += 1;
        let mut out = Vec::new();
        loop {
            match self.peek() {
                None => return Err("a string field value is not closed".to_owned()),
                Some(b'"') => {
                    self.at += 1;
                    return String::from_utf8(out).map_err(|_| "a string field value is not UTF-8".to_owned());
                }
                Some(b'\\') if matches!(self.bytes.get(self.at + 1), Some(b'"' | b'\\')) => {
                    out.push(self.bytes[self.at + 1]);
                    self.at += 2;
                }
                Some(byte) => {
                    out.push(byte);
                    self.at += 1;
                }
            }
        }
    }
}

fn field_value(text: &str) -> Result<FieldValue, String> {
    let invalid = || format!("invalid field value {text}");
    if let Some(number) = text.strip_suffix('i') {
        return number.parse().map(FieldValue::Integer).map_err(|_| invalid());
    }
    if let Some(number) = text.strip_suffix('u') {
        return number.parse().map(FieldValue::Unsigned).map_err(|_| invalid());
    }
    match text {
        "t" | "T" | "true" | "True" | "TRUE" => return Ok(FieldValue::Boolean(true)),
        "f" | "F" | "false" | "False" | "FALSE" => return Ok(FieldValue::Boolean(false)),
        _ => {}
    }
    let value: f64 = text.parse().map_err(|_| invalid())?;
    if !value.is_finite() || text.bytes().any(|byte| byte.is_ascii_alphabetic() && !matches!(byte, b'e' | b'E')) {
        return Err(invalid());
    }
    Ok(FieldValue::Float(value))
}

fn parse_line(text: &str, unit_ns: i64, default_ns: i64) -> Result<Point, String> {
    let mut line = Line { bytes: text.as_bytes(), at: 0 };
    let measurement = line.until(b", ")?;
    if measurement.is_empty() {
        return Err("missing measurement".to_owned());
    }
    let mut tags = Vec::new();
    while line.peek() == Some(b',') {
        line.at += 1;
        let key = line.until(b"=, ")?;
        if line.peek() != Some(b'=') || key.is_empty() {
            return Err("missing tag value".to_owned());
        }
        line.at += 1;
        let value = line.until(b", ")?;
        if value.is_empty() {
            return Err(format!("missing tag value for {key}"));
        }
        tags.push((key, value));
    }
    while line.peek() == Some(b' ') {
        line.at += 1;
    }
    let mut fields = Vec::new();
    loop {
        let key = line.until(b"=, ")?;
        if line.peek() != Some(b'=') || key.is_empty() {
            return Err("missing fields".to_owned());
        }
        line.at += 1;
        let value = if line.peek() == Some(b'"') { FieldValue::Text(line.quoted()?) } else { field_value(&line.until(b", ")?)? };
        fields.push((key, value));
        if line.peek() == Some(b',') {
            line.at += 1;
        } else {
            break;
        }
    }
    while line.peek() == Some(b' ') {
        line.at += 1;
    }
    let rest = text.get(line.at..).unwrap_or_default().trim();
    let time_ns = if rest.is_empty() {
        default_ns
    } else {
        rest.parse::<i64>().ok().and_then(|stamp| stamp.checked_mul(unit_ns)).ok_or_else(|| format!("invalid timestamp {rest}"))?
    };
    if tags.iter().map(|(key, _)| key).chain(fields.iter().map(|(key, _)| key)).any(|key| key == "time") {
        return Err("the key time is reserved".to_owned());
    }
    Ok(Point { measurement, tags, fields, time_ns })
}

/// The points of a line-protocol body, each timestamp in units of `unit_ns`, `now_ns` for a line
/// without one.
pub(crate) fn parse(body: &str, unit_ns: i64, now_ns: i64) -> Result<Vec<Point>, String> {
    let mut points = Vec::new();
    for (number, text) in body.lines().enumerate() {
        let text = text.trim_start();
        if text.is_empty() || text.starts_with('#') {
            continue;
        }
        points.push(parse_line(text.trim_end_matches('\r'), unit_ns, now_ns).map_err(|message| format!("unable to parse line {}: {message}", number + 1))?);
    }
    Ok(points)
}
