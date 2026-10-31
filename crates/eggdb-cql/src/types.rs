use eggdb_sql::{SqlType, Value};
use std::net::IpAddr;

const UNIX_EPOCH_FROM_2000_DAYS: i64 = 10_957;
const UNIX_EPOCH_FROM_2000_MS: i64 = 946_684_800_000;
const DATE_CENTER: i64 = 1 << 31;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CqlType {
    Ascii,
    BigInt,
    Blob,
    Boolean,
    Counter,
    Decimal,
    Double,
    Float,
    Int,
    Timestamp,
    Uuid,
    Varchar,
    Varint,
    TimeUuid,
    Inet,
    Date,
    Time,
    SmallInt,
    TinyInt,
}

const NAMES: [(&str, CqlType); 20] = [
    ("ascii", CqlType::Ascii),
    ("bigint", CqlType::BigInt),
    ("blob", CqlType::Blob),
    ("boolean", CqlType::Boolean),
    ("counter", CqlType::Counter),
    ("decimal", CqlType::Decimal),
    ("double", CqlType::Double),
    ("float", CqlType::Float),
    ("int", CqlType::Int),
    ("timestamp", CqlType::Timestamp),
    ("uuid", CqlType::Uuid),
    ("varchar", CqlType::Varchar),
    ("text", CqlType::Varchar),
    ("varint", CqlType::Varint),
    ("timeuuid", CqlType::TimeUuid),
    ("inet", CqlType::Inet),
    ("date", CqlType::Date),
    ("time", CqlType::Time),
    ("smallint", CqlType::SmallInt),
    ("tinyint", CqlType::TinyInt),
];

impl CqlType {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        NAMES.iter().find(|(known, _)| known.eq_ignore_ascii_case(name)).map(|(_, cql)| *cql)
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            CqlType::Varchar => "text",
            other => NAMES.iter().find(|(_, cql)| *cql == other).map_or("text", |(name, _)| name),
        }
    }

    pub(crate) fn id(self) -> u16 {
        match self {
            CqlType::Ascii => 0x0001,
            CqlType::BigInt => 0x0002,
            CqlType::Blob => 0x0003,
            CqlType::Boolean => 0x0004,
            CqlType::Counter => 0x0005,
            CqlType::Decimal => 0x0006,
            CqlType::Double => 0x0007,
            CqlType::Float => 0x0008,
            CqlType::Int => 0x0009,
            CqlType::Timestamp => 0x000b,
            CqlType::Uuid => 0x000c,
            CqlType::Varchar => 0x000d,
            CqlType::Varint => 0x000e,
            CqlType::TimeUuid => 0x000f,
            CqlType::Inet => 0x0010,
            CqlType::Date => 0x0011,
            CqlType::Time => 0x0012,
            CqlType::SmallInt => 0x0013,
            CqlType::TinyInt => 0x0014,
        }
    }

    /// The SQL type a column of this type is stored as.
    pub(crate) fn sql(self) -> &'static str {
        match self {
            CqlType::Ascii | CqlType::Varchar | CqlType::Inet => "text",
            CqlType::BigInt | CqlType::Counter => "bigint",
            CqlType::Blob => "bytea",
            CqlType::Boolean => "boolean",
            CqlType::Decimal | CqlType::Varint => "numeric",
            CqlType::Double => "double precision",
            CqlType::Float => "real",
            CqlType::Int => "integer",
            CqlType::Timestamp => "timestamp with time zone",
            CqlType::Uuid | CqlType::TimeUuid => "uuid",
            CqlType::Date => "date",
            CqlType::Time => "time",
            CqlType::SmallInt | CqlType::TinyInt => "smallint",
        }
    }

    pub(crate) fn from_sql_type(sql_type: Option<&SqlType>) -> Self {
        match sql_type {
            Some(SqlType::SmallInt) => CqlType::SmallInt,
            Some(SqlType::Integer) => CqlType::Int,
            Some(SqlType::BigInt) => CqlType::BigInt,
            Some(SqlType::Real) => CqlType::Float,
            Some(SqlType::Double) => CqlType::Double,
            Some(SqlType::Boolean) => CqlType::Boolean,
            Some(SqlType::Numeric(_)) => CqlType::Decimal,
            Some(SqlType::Date) => CqlType::Date,
            Some(SqlType::Timestamp(_) | SqlType::TimestampTz(_)) => CqlType::Timestamp,
            Some(SqlType::Time(_)) => CqlType::Time,
            Some(SqlType::Bytea) => CqlType::Blob,
            Some(SqlType::Uuid) => CqlType::Uuid,
            _ => CqlType::Varchar,
        }
    }

    /// The type of a column `information_schema.columns` describes with `data_type`.
    pub(crate) fn from_data_type(data_type: &str) -> Self {
        match data_type {
            "smallint" => CqlType::SmallInt,
            "integer" => CqlType::Int,
            "bigint" => CqlType::BigInt,
            "real" => CqlType::Float,
            "double precision" => CqlType::Double,
            "boolean" => CqlType::Boolean,
            "numeric" => CqlType::Decimal,
            "date" => CqlType::Date,
            "timestamp with time zone" | "timestamp without time zone" => CqlType::Timestamp,
            "time without time zone" => CqlType::Time,
            "bytea" => CqlType::Blob,
            "uuid" => CqlType::Uuid,
            _ => CqlType::Varchar,
        }
    }
}

fn negate(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        *byte = !*byte;
    }
    for byte in bytes.iter_mut().rev() {
        let (sum, carry) = byte.overflowing_add(1);
        *byte = sum;
        if !carry {
            break;
        }
    }
}

fn magnitude_from_digits(digits: &str) -> Vec<u8> {
    let mut magnitude = vec![0u8];
    for digit in digits.bytes() {
        let mut carry = u32::from(digit - b'0');
        for byte in magnitude.iter_mut().rev() {
            let value = u32::from(*byte) * 10 + carry;
            *byte = (value & 0xff) as u8;
            carry = value >> 8;
        }
        while carry > 0 {
            magnitude.insert(0, (carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    magnitude
}

fn digits_from_magnitude(mut magnitude: Vec<u8>) -> String {
    let mut digits = Vec::new();
    while magnitude.iter().any(|&byte| byte != 0) {
        let mut remainder = 0u32;
        for byte in &mut magnitude {
            let value = (remainder << 8) | u32::from(*byte);
            *byte = (value / 10) as u8;
            remainder = value % 10;
        }
        digits.push(b'0' + remainder as u8);
    }
    if digits.is_empty() {
        digits.push(b'0');
    }
    digits.reverse();
    String::from_utf8(digits).unwrap_or_default()
}

fn varint_bytes(negative: bool, magnitude: Vec<u8>) -> Vec<u8> {
    let mut bytes = vec![0u8];
    bytes.extend(magnitude);
    if negative {
        negate(&mut bytes);
        while bytes.len() > 1 && bytes[0] == 0xff && bytes[1] & 0x80 != 0 {
            bytes.remove(0);
        }
    } else {
        while bytes.len() > 1 && bytes[0] == 0 && bytes[1] & 0x80 == 0 {
            bytes.remove(0);
        }
    }
    bytes
}

fn varint_text(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "0".to_owned();
    }
    let negative = bytes[0] & 0x80 != 0;
    let mut magnitude = bytes.to_vec();
    if negative {
        negate(&mut magnitude);
    }
    let digits = digits_from_magnitude(magnitude);
    if negative { format!("-{digits}") } else { digits }
}

fn decimal_parts(text: &str) -> Result<(bool, String, i32), String> {
    let (negative, unsigned) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    if whole.is_empty() && fraction.is_empty() || !whole.bytes().chain(fraction.bytes()).all(|byte| byte.is_ascii_digit()) {
        return Err(format!("{text} is not a decimal number CQL can carry"));
    }
    let scale = i32::try_from(fraction.len()).map_err(|_| "the decimal has too many digits".to_owned())?;
    Ok((negative, format!("{whole}{fraction}"), scale))
}

fn civil(days_since_1970: i64) -> (i64, i64, i64) {
    let z = days_since_1970 + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

fn fixed<const N: usize>(bytes: &[u8], cql: CqlType) -> Result<[u8; N], String> {
    bytes.try_into().map_err(|_| format!("a {} value must be {N} bytes, not {}", cql.name(), bytes.len()))
}

fn float_text(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else if value.is_infinite() {
        if value > 0.0 { "Infinity".to_owned() } else { "-Infinity".to_owned() }
    } else {
        format!("{value:?}")
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn uuid_text(bytes: &[u8; 16]) -> String {
    let text = hex(bytes);
    format!("{}-{}-{}-{}-{}", &text[..8], &text[8..12], &text[12..16], &text[16..20], &text[20..])
}

/// A bound value in the text form a SQL parameter takes, or `None` for NULL.
pub(crate) fn parameter_text(bytes: Option<&[u8]>, cql: CqlType) -> Result<Option<String>, String> {
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    let text = match cql {
        CqlType::Ascii | CqlType::Varchar => String::from_utf8(bytes.to_vec()).map_err(|_| "a text value is not UTF-8".to_owned())?,
        CqlType::Int => i32::from_be_bytes(fixed(bytes, cql)?).to_string(),
        CqlType::BigInt | CqlType::Counter => i64::from_be_bytes(fixed(bytes, cql)?).to_string(),
        CqlType::SmallInt => i16::from_be_bytes(fixed(bytes, cql)?).to_string(),
        CqlType::TinyInt => i8::from_be_bytes(fixed(bytes, cql)?).to_string(),
        CqlType::Boolean => (fixed::<1>(bytes, cql)?[0] != 0).to_string(),
        CqlType::Double => float_text(f64::from_be_bytes(fixed(bytes, cql)?)),
        CqlType::Float => float_text(f64::from(f32::from_be_bytes(fixed(bytes, cql)?))),
        CqlType::Varint => varint_text(bytes),
        CqlType::Decimal => {
            let scale = i32::from_be_bytes(fixed(bytes.get(..4).ok_or("a decimal value is cut short")?, cql)?);
            let digits = varint_text(&bytes[4..]);
            let (negative, digits) = match digits.strip_prefix('-') {
                Some(rest) => (true, rest.to_owned()),
                None => (false, digits),
            };
            let scale = usize::try_from(scale).map_err(|_| "a decimal with a negative scale is not supported".to_owned())?;
            let padded = format!("{digits:0>width$}", width = scale + 1);
            let (whole, fraction) = padded.split_at(padded.len() - scale);
            let sign = if negative { "-" } else { "" };
            if fraction.is_empty() { format!("{sign}{whole}") } else { format!("{sign}{whole}.{fraction}") }
        }
        CqlType::Uuid | CqlType::TimeUuid => uuid_text(&fixed(bytes, cql)?),
        CqlType::Timestamp => {
            let ms = i64::from_be_bytes(fixed(bytes, cql)?);
            let (year, month, day) = civil(ms.div_euclid(86_400_000));
            let in_day = ms.rem_euclid(86_400_000);
            format!(
                "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}.{:03}+00",
                in_day / 3_600_000,
                in_day / 60_000 % 60,
                in_day / 1000 % 60,
                in_day % 1000
            )
        }
        CqlType::Date => {
            let (year, month, day) = civil(i64::from(u32::from_be_bytes(fixed(bytes, cql)?)) - DATE_CENTER);
            format!("{year:04}-{month:02}-{day:02}")
        }
        CqlType::Time => {
            let micros = i64::from_be_bytes(fixed(bytes, cql)?) / 1000;
            format!("{:02}:{:02}:{:02}.{:06}", micros / 3_600_000_000, micros / 60_000_000 % 60, micros / 1_000_000 % 60, micros % 1_000_000)
        }
        CqlType::Blob => format!("\\x{}", hex(bytes)),
        CqlType::Inet => match bytes.len() {
            4 => IpAddr::from(fixed::<4>(bytes, cql)?).to_string(),
            16 => IpAddr::from(fixed::<16>(bytes, cql)?).to_string(),
            other => return Err(format!("an inet value must be 4 or 16 bytes, not {other}")),
        },
    };
    Ok(Some(text))
}

fn integer(value: &Value) -> Option<i64> {
    match value {
        Value::SmallInt(number) => Some(i64::from(*number)),
        Value::Integer(number) => Some(i64::from(*number)),
        Value::BigInt(number) => Some(*number),
        _ => value.to_text().and_then(|text| text.parse().ok()),
    }
}

fn mismatch(value: &Value, cql: CqlType) -> String {
    format!("the value {} does not fit the CQL type {}", value.to_text().unwrap_or_default(), cql.name())
}

/// `value` in the binary form of `cql`, or `None` for NULL.
pub(crate) fn encode_value(value: &Value, cql: CqlType) -> Result<Option<Vec<u8>>, String> {
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    let bytes = match cql {
        CqlType::Ascii | CqlType::Varchar => value.to_text().unwrap_or_default().into_bytes(),
        CqlType::Int => i32::try_from(integer(value).ok_or_else(|| mismatch(value, cql))?).map_err(|_| mismatch(value, cql))?.to_be_bytes().to_vec(),
        CqlType::BigInt | CqlType::Counter => integer(value).ok_or_else(|| mismatch(value, cql))?.to_be_bytes().to_vec(),
        CqlType::SmallInt => i16::try_from(integer(value).ok_or_else(|| mismatch(value, cql))?).map_err(|_| mismatch(value, cql))?.to_be_bytes().to_vec(),
        CqlType::TinyInt => i8::try_from(integer(value).ok_or_else(|| mismatch(value, cql))?).map_err(|_| mismatch(value, cql))?.to_be_bytes().to_vec(),
        CqlType::Boolean => match value {
            Value::Boolean(flag) => vec![u8::from(*flag)],
            _ => return Err(mismatch(value, cql)),
        },
        CqlType::Double => match value {
            Value::Double(number) => number.to_be_bytes().to_vec(),
            Value::Real(number) => f64::from(*number).to_be_bytes().to_vec(),
            _ => value.to_text().and_then(|text| text.parse::<f64>().ok()).ok_or_else(|| mismatch(value, cql))?.to_be_bytes().to_vec(),
        },
        CqlType::Float => match value {
            Value::Real(number) => number.to_be_bytes().to_vec(),
            _ => value.to_text().and_then(|text| text.parse::<f32>().ok()).ok_or_else(|| mismatch(value, cql))?.to_be_bytes().to_vec(),
        },
        CqlType::Decimal => {
            let (negative, digits, scale) = decimal_parts(&value.to_text().unwrap_or_default())?;
            let mut out = scale.to_be_bytes().to_vec();
            out.extend(varint_bytes(negative, magnitude_from_digits(&digits)));
            out
        }
        CqlType::Varint => {
            let (negative, digits, scale) = decimal_parts(&value.to_text().unwrap_or_default())?;
            if scale != 0 && digits[digits.len() - usize::try_from(scale).unwrap_or(0)..].bytes().any(|byte| byte != b'0') {
                return Err(mismatch(value, cql));
            }
            varint_bytes(negative, magnitude_from_digits(&digits[..digits.len() - usize::try_from(scale).unwrap_or(0)]))
        }
        CqlType::Uuid | CqlType::TimeUuid => match value {
            Value::Uuid(bytes) => bytes.to_vec(),
            _ => return Err(mismatch(value, cql)),
        },
        CqlType::Timestamp => match value {
            Value::TimestampTz(micros) | Value::Timestamp(micros) => (micros.div_euclid(1000) + UNIX_EPOCH_FROM_2000_MS).to_be_bytes().to_vec(),
            _ => return Err(mismatch(value, cql)),
        },
        CqlType::Date => match value {
            Value::Date(days) => u32::try_from(i64::from(*days) + UNIX_EPOCH_FROM_2000_DAYS + DATE_CENTER).map_err(|_| mismatch(value, cql))?.to_be_bytes().to_vec(),
            _ => return Err(mismatch(value, cql)),
        },
        CqlType::Time => match value {
            Value::Time(micros) => (micros * 1000).to_be_bytes().to_vec(),
            _ => return Err(mismatch(value, cql)),
        },
        CqlType::Blob => match value {
            Value::Bytea(bytes) => bytes.clone(),
            _ => value.to_text().unwrap_or_default().into_bytes(),
        },
        CqlType::Inet => match value.to_text().unwrap_or_default().parse::<IpAddr>() {
            Ok(IpAddr::V4(address)) => address.octets().to_vec(),
            Ok(IpAddr::V6(address)) => address.octets().to_vec(),
            Err(_) => return Err(mismatch(value, cql)),
        },
    };
    Ok(Some(bytes))
}

#[cfg(test)]
pub(crate) fn varint_round_trip(text: &str) -> String {
    let (negative, digits, _) = decimal_parts(text).unwrap();
    varint_text(&varint_bytes(negative, magnitude_from_digits(&digits)))
}
