#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Spec {
    pub(crate) left: bool,
    pub(crate) plus: bool,
    pub(crate) space: bool,
    pub(crate) alternate: bool,
    pub(crate) zero: bool,
    pub(crate) width: usize,
    pub(crate) precision: Option<usize>,
    pub(crate) conversion: u8,
}

/// Reads a C format directive after its `%`: flags, width, precision and the conversion byte.
/// Returns the spec and how many bytes it took.
pub(crate) fn parse_spec(text: &[u8]) -> Result<(Spec, usize), String> {
    let mut spec = Spec::default();
    let mut at = 0;
    while let Some(&flag) = text.get(at) {
        match flag {
            b'-' => spec.left = true,
            b'+' => spec.plus = true,
            b' ' => spec.space = true,
            b'#' => spec.alternate = true,
            b'0' => spec.zero = true,
            _ => break,
        }
        at += 1;
        if at > 5 {
            return Err("invalid format (repeated flags)".to_owned());
        }
    }
    let digits = |at: &mut usize| {
        let start = *at;
        while text.get(*at).is_some_and(u8::is_ascii_digit) {
            *at += 1;
        }
        if *at - start > 2 {
            return Err("invalid format (width or precision too long)".to_owned());
        }
        Ok(std::str::from_utf8(&text[start..*at]).ok().and_then(|digits| digits.parse().ok()))
    };
    spec.width = digits(&mut at)?.unwrap_or(0);
    if text.get(at) == Some(&b'.') {
        at += 1;
        spec.precision = Some(digits(&mut at)?.unwrap_or(0));
    }
    let Some(&conversion) = text.get(at) else {
        return Err("invalid option '%' to 'format'".to_owned());
    };
    spec.conversion = conversion;
    Ok((spec, at + 1))
}

/// Pads `body` (with its `sign` kept in front) to the spec's width.
pub(crate) fn pad(spec: &Spec, sign: &str, body: &str, zero_allowed: bool) -> String {
    let length = sign.len() + body.len();
    if length >= spec.width {
        return format!("{sign}{body}");
    }
    let fill = spec.width - length;
    if spec.left {
        format!("{sign}{body}{}", " ".repeat(fill))
    } else if spec.zero && zero_allowed {
        format!("{sign}{}{body}", "0".repeat(fill))
    } else {
        format!("{}{sign}{body}", " ".repeat(fill))
    }
}

fn sign_of(spec: &Spec, negative: bool) -> &'static str {
    if negative {
        "-"
    } else if spec.plus {
        "+"
    } else if spec.space {
        " "
    } else {
        ""
    }
}

/// Formats `number` as an integer conversion (`d`, `i`, `u`, `x`, `X`, `o`, `c` excluded).
pub(crate) fn integer(spec: &Spec, number: f64) -> String {
    let value = if number.is_finite() { number as i64 } else { 0 };
    let (negative, digits) = match spec.conversion {
        b'x' => (false, format!("{:x}", value as u64)),
        b'X' => (false, format!("{:X}", value as u64)),
        b'o' => (false, format!("{:o}", value as u64)),
        b'u' => (false, (value as u64).to_string()),
        _ => (value < 0, value.unsigned_abs().to_string()),
    };
    let mut digits = match spec.precision {
        Some(0) if value == 0 => String::new(),
        Some(precision) if digits.len() < precision => format!("{}{digits}", "0".repeat(precision - digits.len())),
        _ => digits,
    };
    if spec.alternate && value != 0 {
        digits = match spec.conversion {
            b'x' => format!("0x{digits}"),
            b'X' => format!("0X{digits}"),
            b'o' if !digits.starts_with('0') => format!("0{digits}"),
            _ => digits,
        };
    }
    let signed = matches!(spec.conversion, b'd' | b'i');
    let sign = if signed { sign_of(spec, negative) } else { "" };
    pad(spec, sign, &digits, spec.precision.is_none())
}

fn exponent_form(number: f64, precision: usize, upper: bool) -> String {
    let text = format!("{number:.precision$e}");
    let (mantissa, power) = text.split_once('e').unwrap_or((&text, "0"));
    let power: i32 = power.parse().unwrap_or(0);
    let sign = if power < 0 { '-' } else { '+' };
    let marker = if upper { 'E' } else { 'e' };
    format!("{mantissa}{marker}{sign}{:02}", power.abs())
}

fn trimmed(text: String) -> String {
    let (body, exponent) = match text.find(['e', 'E']) {
        Some(at) => (text[..at].to_owned(), text[at..].to_owned()),
        None => (text, String::new()),
    };
    let body = if body.contains('.') {
        body.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        body
    };
    format!("{body}{exponent}")
}

/// Formats `number` as a floating conversion (`f`, `F`, `e`, `E`, `g`, `G`) the way C's
/// `printf` does.
pub(crate) fn float(spec: &Spec, number: f64) -> String {
    let negative = number.is_sign_negative();
    let magnitude = number.abs();
    let upper = spec.conversion.is_ascii_uppercase();
    let sign = sign_of(spec, negative);
    if !magnitude.is_finite() {
        let text = if magnitude.is_nan() { "nan" } else { "inf" };
        let text = if upper { text.to_uppercase() } else { text.to_owned() };
        return pad(spec, sign, &text, false);
    }
    let precision = spec.precision.unwrap_or(6);
    let body = match spec.conversion.to_ascii_lowercase() {
        b'f' => format!("{magnitude:.precision$}"),
        b'e' => exponent_form(magnitude, precision, upper),
        _ => {
            let significant = precision.max(1);
            let probe = exponent_form(magnitude, significant - 1, false);
            let exponent: i32 = probe
                .split_once('e')
                .and_then(|(_, power)| power.parse().ok())
                .unwrap_or(0);
            let text = if exponent < -4 || exponent >= significant as i32 {
                exponent_form(magnitude, significant - 1, upper)
            } else {
                let decimals = usize::try_from(significant as i32 - 1 - exponent).unwrap_or(0);
                format!("{magnitude:.decimals$}")
            };
            if spec.alternate { text } else { trimmed(text) }
        }
    };
    let body = if spec.alternate && !body.contains('.') && spec.conversion.to_ascii_lowercase() != b'g' {
        match body.find(['e', 'E']) {
            Some(at) => format!("{}.{}", &body[..at], &body[at..]),
            None => format!("{body}."),
        }
    } else {
        body
    };
    pad(spec, sign, &body, true)
}
