use eggdb_sql::json::{Json, Node, parse_document};
use eggdb_sql::{SqlType, Value};
use std::cmp::Ordering;
use std::net::IpAddr;
use std::sync::{Mutex, PoisonError};

const MOST_INTERNED_TYPES: usize = 4096;
const MOST_COLLECTION_ITEMS: usize = 65_535;
const MOST_TYPE_DEPTH: usize = 16;
static INTERNED: Mutex<Vec<&'static CqlType>> = Mutex::new(Vec::new());
static INTERNED_TUPLES: Mutex<Vec<&'static [CqlType]>> = Mutex::new(Vec::new());
static INTERNED_USER_TYPES: Mutex<Vec<&'static UserType>> = Mutex::new(Vec::new());

/// A user-defined type: its keyspace, name and fields in order. A type named in a statement
/// but not yet looked up has an empty keyspace and no fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UserType {
    pub(crate) keyspace: String,
    pub(crate) name: String,
    pub(crate) fields: Vec<(String, CqlType)>,
}

impl UserType {
    pub(crate) fn is_resolved(&self) -> bool {
        !self.keyspace.is_empty()
    }
}

/// The one shared copy of a user-defined type.
pub(crate) fn interned_user_type(user_type: UserType) -> Result<&'static UserType, String> {
    let mut known = INTERNED_USER_TYPES.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(found) = known.iter().find(|found| ***found == user_type) {
        return Ok(*found);
    }
    if known.len() >= MOST_INTERNED_TYPES {
        return Err(format!("at most {MOST_INTERNED_TYPES} distinct user-defined types are supported"));
    }
    let leaked: &'static UserType = Box::leak(Box::new(user_type));
    known.push(leaked);
    Ok(leaked)
}

/// A user-defined type named in a statement, to be looked up in its keyspace.
pub(crate) fn unresolved_user_type(name: &str) -> Result<CqlType, String> {
    Ok(CqlType::Udt(interned_user_type(UserType {
        keyspace: String::new(),
        name: name.to_owned(),
        fields: Vec::new(),
    })?))
}

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
    List(&'static CqlType),
    Set(&'static CqlType),
    Map(&'static CqlType, &'static CqlType),
    Tuple(&'static [CqlType]),
    Udt(&'static UserType),
}

/// The one shared copy of `cql`, so collection types stay `Copy`; at most 4,096 distinct ones.
pub(crate) fn interned(cql: CqlType) -> Result<&'static CqlType, String> {
    let mut known = INTERNED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(found) = known.iter().find(|found| ***found == cql) {
        return Ok(*found);
    }
    if known.len() >= MOST_INTERNED_TYPES {
        return Err(format!("at most {MOST_INTERNED_TYPES} distinct collection types are supported"));
    }
    let leaked: &'static CqlType = Box::leak(Box::new(cql));
    known.push(leaked);
    Ok(leaked)
}

/// The one shared copy of a tuple's item types.
pub(crate) fn interned_tuple(items: Vec<CqlType>) -> Result<&'static [CqlType], String> {
    let mut known = INTERNED_TUPLES.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(found) = known.iter().find(|found| ***found == *items) {
        return Ok(*found);
    }
    if known.len() >= MOST_INTERNED_TYPES {
        return Err(format!("at most {MOST_INTERNED_TYPES} distinct tuple types are supported"));
    }
    let leaked: &'static [CqlType] = Box::leak(items.into_boxed_slice());
    known.push(leaked);
    Ok(leaked)
}

fn split_arguments(text: &str) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (at, character) in text.char_indices() {
        match character {
            '<' => depth += 1,
            '>' => depth = depth.checked_sub(1)?,
            ',' if depth == 0 => {
                parts.push(text[start..at].trim());
                start = at + 1;
            }
            _ => {}
        }
    }
    (depth == 0).then(|| {
        parts.push(text[start..].trim());
        parts
    })
}

fn parse_type(text: &str, depth: usize) -> Option<CqlType> {
    if depth > MOST_TYPE_DEPTH {
        return None;
    }
    let text = text.trim();
    let Some(open) = text.find('<') else {
        return CqlType::from_name(text);
    };
    let inner = text.strip_suffix('>')?.get(open + 1..)?;
    if text[..open].trim().eq_ignore_ascii_case("udt") {
        return parse_user_type(inner, depth);
    }
    let arguments = split_arguments(inner)?;
    let parsed: Vec<CqlType> = arguments.iter().map(|argument| parse_type(argument, depth + 1)).collect::<Option<_>>()?;
    match (text[..open].trim().to_ascii_lowercase().as_str(), parsed.as_slice()) {
        ("frozen", [inner]) => Some(*inner),
        ("list", [element]) => Some(CqlType::List(interned(*element).ok()?)),
        ("set", [element]) => Some(CqlType::Set(interned(*element).ok()?)),
        ("map", [key, value]) => Some(CqlType::Map(interned(*key).ok()?, interned(*value).ok()?)),
        ("tuple", items) if !items.is_empty() => Some(CqlType::Tuple(interned_tuple(items.to_vec()).ok()?)),
        _ => None,
    }
}

fn parse_user_type(inner: &str, depth: usize) -> Option<CqlType> {
    let arguments = split_arguments(inner)?;
    let (qualified, fields) = arguments.split_first()?;
    let (keyspace, name) = qualified.split_once('.')?;
    let mut parsed = Vec::with_capacity(fields.len());
    for field in fields {
        let (field_name, field_type) = field.trim().split_once(' ')?;
        parsed.push((field_name.to_owned(), parse_type(field_type, depth + 1)?));
    }
    Some(CqlType::Udt(
        interned_user_type(UserType {
            keyspace: keyspace.to_owned(),
            name: name.to_owned(),
            fields: parsed,
        })
        .ok()?,
    ))
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
        NAMES
            .iter()
            .find(|(known, _)| known.eq_ignore_ascii_case(name))
            .map(|(_, cql)| *cql)
    }

    /// The type written as CQL writes it: `int`, `list<text>`, `map<text, int>`,
    /// `tuple<int, text>`.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        parse_type(text, 0)
    }

    pub(crate) fn is_collection(self) -> bool {
        matches!(self, CqlType::List(_) | CqlType::Set(_) | CqlType::Map(..) | CqlType::Tuple(_) | CqlType::Udt(_))
    }

    /// The item types of a tuple or the field types of a user-defined type, in order.
    pub(crate) fn item_types(self) -> Vec<CqlType> {
        match self {
            CqlType::Tuple(items) => items.to_vec(),
            CqlType::Udt(user_type) => user_type.fields.iter().map(|(_, cql)| *cql).collect(),
            _ => Vec::new(),
        }
    }

    /// The type as `eggdb_cql` metadata keeps it: as [`CqlType::name`], except that a
    /// user-defined type carries its whole definition, `udt<ks.name, field type, ...>`, so
    /// reading it back needs no lookup.
    pub(crate) fn stored_name(self) -> String {
        match self {
            CqlType::List(element) => format!("list<{}>", element.stored_name()),
            CqlType::Set(element) => format!("set<{}>", element.stored_name()),
            CqlType::Map(key, value) => format!("map<{}, {}>", key.stored_name(), value.stored_name()),
            CqlType::Tuple(items) => format!("tuple<{}>", items.iter().map(|item| item.stored_name()).collect::<Vec<_>>().join(", ")),
            CqlType::Udt(user_type) => {
                let mut parts = vec![format!("{}.{}", user_type.keyspace, user_type.name)];
                parts.extend(user_type.fields.iter().map(|(name, cql)| format!("{name} {}", cql.stored_name())));
                format!("udt<{}>", parts.join(", "))
            }
            other => other.name(),
        }
    }

    pub(crate) fn name(self) -> String {
        match self {
            CqlType::Varchar => "text".to_owned(),
            CqlType::List(element) => format!("list<{}>", element.name()),
            CqlType::Set(element) => format!("set<{}>", element.name()),
            CqlType::Map(key, value) => format!("map<{}, {}>", key.name(), value.name()),
            CqlType::Tuple(items) => format!("tuple<{}>", items.iter().map(|item| item.name()).collect::<Vec<_>>().join(", ")),
            CqlType::Udt(user_type) => format!("frozen<{}>", user_type.name),
            other => NAMES
                .iter()
                .find(|(_, cql)| *cql == other)
                .map_or("text", |(name, _)| name)
                .to_owned(),
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
            CqlType::List(_) => 0x0020,
            CqlType::Map(..) => 0x0021,
            CqlType::Set(_) => 0x0022,
            CqlType::Tuple(_) => 0x0031,
            CqlType::Udt(_) => 0x0030,
        }
    }

    /// The SQL type a column of this type is stored as.
    pub(crate) fn sql(self) -> &'static str {
        match self {
            CqlType::Ascii | CqlType::Varchar | CqlType::Inet => "text",
            CqlType::List(_) | CqlType::Set(_) | CqlType::Map(..) | CqlType::Tuple(_) | CqlType::Udt(_) => "text",
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

const MOST_VARINT_BYTES: usize = 1024;
const MOST_DECIMAL_SCALE: usize = 4096;

fn checked_varint(bytes: &[u8]) -> Result<&[u8], String> {
    if bytes.len() > MOST_VARINT_BYTES {
        return Err(format!(
            "a varint may be at most {MOST_VARINT_BYTES} bytes, not {}",
            bytes.len()
        ));
    }
    Ok(bytes)
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
    if negative {
        format!("-{digits}")
    } else {
        digits
    }
}

fn decimal_parts(text: &str) -> Result<(bool, String, i32), String> {
    let (negative, unsigned) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    if whole.is_empty() && fraction.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|byte| byte.is_ascii_digit())
    {
        return Err(format!("{text} is not a decimal number CQL can carry"));
    }
    let scale =
        i32::try_from(fraction.len()).map_err(|_| "the decimal has too many digits".to_owned())?;
    Ok((negative, format!("{whole}{fraction}"), scale))
}

fn civil(days_since_1970: i64) -> (i64, i64, i64) {
    let z = days_since_1970 + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

fn fixed<const N: usize>(bytes: &[u8], cql: CqlType) -> Result<[u8; N], String> {
    bytes.try_into().map_err(|_| {
        format!(
            "a {} value must be {N} bytes, not {}",
            cql.name(),
            bytes.len()
        )
    })
}

fn float_text(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else if value.is_infinite() {
        if value > 0.0 {
            "Infinity".to_owned()
        } else {
            "-Infinity".to_owned()
        }
    } else {
        format!("{value:?}")
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn uuid_text(bytes: &[u8; 16]) -> String {
    let text = hex(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &text[..8],
        &text[8..12],
        &text[12..16],
        &text[16..20],
        &text[20..]
    )
}

/// A bound value in the text form a SQL parameter takes, or `None` for NULL.
pub(crate) fn parameter_text(bytes: Option<&[u8]>, cql: CqlType) -> Result<Option<String>, String> {
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    let text = match cql {
        CqlType::Ascii | CqlType::Varchar => {
            String::from_utf8(bytes.to_vec()).map_err(|_| "a text value is not UTF-8".to_owned())?
        }
        CqlType::Int => i32::from_be_bytes(fixed(bytes, cql)?).to_string(),
        CqlType::BigInt | CqlType::Counter => i64::from_be_bytes(fixed(bytes, cql)?).to_string(),
        CqlType::SmallInt => i16::from_be_bytes(fixed(bytes, cql)?).to_string(),
        CqlType::TinyInt => i8::from_be_bytes(fixed(bytes, cql)?).to_string(),
        CqlType::Boolean => (fixed::<1>(bytes, cql)?[0] != 0).to_string(),
        CqlType::Double => float_text(f64::from_be_bytes(fixed(bytes, cql)?)),
        CqlType::Float => float_text(f64::from(f32::from_be_bytes(fixed(bytes, cql)?))),
        CqlType::Varint => varint_text(checked_varint(bytes)?),
        CqlType::Decimal => {
            let scale = i32::from_be_bytes(fixed(
                bytes.get(..4).ok_or("a decimal value is cut short")?,
                cql,
            )?);
            let digits = varint_text(checked_varint(&bytes[4..])?);
            let (negative, digits) = match digits.strip_prefix('-') {
                Some(rest) => (true, rest.to_owned()),
                None => (false, digits),
            };
            let scale = usize::try_from(scale)
                .map_err(|_| "a decimal with a negative scale is not supported".to_owned())?;
            if scale > MOST_DECIMAL_SCALE {
                return Err(format!(
                    "a decimal's scale may be at most {MOST_DECIMAL_SCALE}, not {scale}"
                ));
            }
            let padded = format!("{digits:0>width$}", width = scale + 1);
            let (whole, fraction) = padded.split_at(padded.len() - scale);
            let sign = if negative { "-" } else { "" };
            if fraction.is_empty() {
                format!("{sign}{whole}")
            } else {
                format!("{sign}{whole}.{fraction}")
            }
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
            let (year, month, day) =
                civil(i64::from(u32::from_be_bytes(fixed(bytes, cql)?)) - DATE_CENTER);
            format!("{year:04}-{month:02}-{day:02}")
        }
        CqlType::Time => {
            let micros = i64::from_be_bytes(fixed(bytes, cql)?) / 1000;
            format!(
                "{:02}:{:02}:{:02}.{:06}",
                micros / 3_600_000_000,
                micros / 60_000_000 % 60,
                micros / 1_000_000 % 60,
                micros % 1_000_000
            )
        }
        CqlType::Blob => format!("\\x{}", hex(bytes)),
        CqlType::Inet => match bytes.len() {
            4 => IpAddr::from(fixed::<4>(bytes, cql)?).to_string(),
            16 => IpAddr::from(fixed::<16>(bytes, cql)?).to_string(),
            other => return Err(format!("an inet value must be 4 or 16 bytes, not {other}")),
        },
        CqlType::List(_) | CqlType::Set(_) | CqlType::Map(..) | CqlType::Tuple(_) | CqlType::Udt(_) => collection_json(bytes, cql, 0)?,
    };
    Ok(Some(text))
}

pub(crate) fn quote_json(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn element_json(bytes: &[u8], cql: CqlType, depth: usize) -> Result<String, String> {
    if cql.is_collection() {
        return collection_json(bytes, cql, depth + 1);
    }
    let text = parameter_text(Some(bytes), cql)?.unwrap_or_default();
    Ok(match cql {
        CqlType::Int | CqlType::BigInt | CqlType::Counter | CqlType::SmallInt | CqlType::TinyInt | CqlType::Varint | CqlType::Decimal => text,
        CqlType::Double | CqlType::Float if text.parse::<f64>().is_ok_and(f64::is_finite) => text,
        CqlType::Boolean => text,
        _ => quote_json(&text),
    })
}

struct Cursor<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Cursor<'b> {
    fn int(&mut self) -> Result<i32, String> {
        let raw = self.bytes.get(self.at..self.at + 4).ok_or("a collection value is cut short")?;
        self.at += 4;
        Ok(i32::from_be_bytes(raw.try_into().map_err(|_| "a collection value is cut short")?))
    }

    fn element(&mut self) -> Result<Option<&'b [u8]>, String> {
        let length = self.int()?;
        if length < 0 {
            return Ok(None);
        }
        let length = length as usize;
        let raw = self.bytes.get(self.at..self.at + length).ok_or("a collection element is cut short")?;
        self.at += length;
        Ok(Some(raw))
    }

    fn count(&mut self) -> Result<usize, String> {
        let count = self.int()?;
        usize::try_from(count)
            .ok()
            .filter(|count| *count <= MOST_COLLECTION_ITEMS)
            .ok_or_else(|| format!("a collection may hold at most {MOST_COLLECTION_ITEMS} items"))
    }
}

fn compare_json(left: &str, right: &str) -> Ordering {
    match (left.parse::<f64>(), right.parse::<f64>()) {
        (Ok(left), Ok(right)) => left.total_cmp(&right),
        _ => left.cmp(right),
    }
}

/// A collection's binary value as the JSON text a collection column stores: a list or set as
/// an array (a set sorted, without repeats), a map as an array of `[key, value]` pairs sorted
/// by key, a tuple as an array with `null` for missing items.
fn collection_json(bytes: &[u8], cql: CqlType, depth: usize) -> Result<String, String> {
    if depth > MOST_TYPE_DEPTH {
        return Err("collections are nested too deeply".to_owned());
    }
    let mut cursor = Cursor { bytes, at: 0 };
    let items: Vec<String> = match cql {
        CqlType::List(element) | CqlType::Set(element) => {
            let count = cursor.count()?;
            let mut items = Vec::with_capacity(count);
            for _ in 0..count {
                let raw = cursor.element()?.ok_or("a collection cannot hold null")?;
                items.push(element_json(raw, *element, depth)?);
            }
            if matches!(cql, CqlType::Set(_)) {
                items.sort_by(|left, right| compare_json(left, right));
                items.dedup();
            }
            items
        }
        CqlType::Map(key, value) => {
            let count = cursor.count()?;
            let mut pairs: Vec<(String, String)> = Vec::with_capacity(count);
            for _ in 0..count {
                let raw_key = cursor.element()?.ok_or("a map key cannot be null")?;
                let raw_value = cursor.element()?.ok_or("a map value cannot be null")?;
                pairs.push((element_json(raw_key, *key, depth)?, element_json(raw_value, *value, depth)?));
            }
            pairs.sort_by(|left, right| compare_json(&left.0, &right.0));
            pairs.dedup_by(|later, earlier| later.0 == earlier.0);
            pairs.into_iter().map(|(key, value)| format!("[{key},{value}]")).collect()
        }
        CqlType::Tuple(_) | CqlType::Udt(_) => {
            let types = cql.item_types();
            let mut items = Vec::with_capacity(types.len());
            for item in &types {
                items.push(match (cursor.at < bytes.len()).then(|| cursor.element()).transpose()?.flatten() {
                    Some(raw) => element_json(raw, *item, depth)?,
                    None => "null".to_owned(),
                });
            }
            items
        }
        _ => return Err(format!("{} is not a collection", cql.name())),
    };
    Ok(format!("[{}]", items.join(",")))
}

fn json_scalar_text(node: &Node) -> Result<String, String> {
    match &node.value {
        Json::String(text) | Json::Number(text) => Ok(text.clone()),
        Json::Boolean(flag) => Ok(flag.to_string()),
        Json::Null => Err("a collection cannot hold null".to_owned()),
        _ => Err("a collection item does not fit its type".to_owned()),
    }
}

fn push_element(out: &mut Vec<u8>, bytes: Option<Vec<u8>>) {
    match bytes {
        Some(bytes) => {
            out.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
            out.extend_from_slice(&bytes);
        }
        None => out.extend_from_slice(&(-1i32).to_be_bytes()),
    }
}

fn encode_json(node: &Node, cql: CqlType, depth: usize) -> Result<Vec<u8>, String> {
    if depth > MOST_TYPE_DEPTH {
        return Err("collections are nested too deeply".to_owned());
    }
    if !cql.is_collection() {
        return encode_text(&json_scalar_text(node)?, cql);
    }
    let Json::Array(items) = &node.value else {
        return Err(format!("a {} value must be a JSON array", cql.name()));
    };
    let mut out = Vec::new();
    match cql {
        CqlType::List(element) | CqlType::Set(element) => {
            out.extend_from_slice(&(items.len() as i32).to_be_bytes());
            for item in items {
                push_element(&mut out, Some(encode_json(item, *element, depth + 1)?));
            }
        }
        CqlType::Map(key, value) => {
            out.extend_from_slice(&(items.len() as i32).to_be_bytes());
            for item in items {
                let Json::Array(pair) = &item.value else {
                    return Err("a map entry must be a [key, value] pair".to_owned());
                };
                let [stored_key, stored_value] = pair.as_slice() else {
                    return Err("a map entry must be a [key, value] pair".to_owned());
                };
                push_element(&mut out, Some(encode_json(stored_key, *key, depth + 1)?));
                push_element(&mut out, Some(encode_json(stored_value, *value, depth + 1)?));
            }
        }
        CqlType::Tuple(_) | CqlType::Udt(_) => {
            for (at, item) in cql.item_types().iter().enumerate() {
                let bytes = match items.get(at) {
                    Some(node) if node.value != Json::Null => Some(encode_json(node, *item, depth + 1)?),
                    _ => None,
                };
                push_element(&mut out, bytes);
            }
        }
        _ => {}
    }
    Ok(out)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn date_days(text: &str) -> Option<i64> {
    let mut parts = text.splitn(3, '-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    ((1..=12).contains(&month) && (1..=31).contains(&day)).then(|| days_from_civil(year, month, day))
}

fn time_nanos(text: &str) -> Option<i64> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let mut parts = whole.split(':');
    let hour: i64 = parts.next()?.parse().ok()?;
    let minute: i64 = parts.next().unwrap_or("0").parse().ok()?;
    let second: i64 = parts.next().unwrap_or("0").parse().ok()?;
    if fraction.len() > 9 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let nanos: i64 = if fraction.is_empty() { 0 } else { format!("{fraction:0<9}").parse().ok()? };
    ((0..24).contains(&hour) && (0..60).contains(&minute) && (0..60).contains(&second))
        .then(|| ((hour * 60 + minute) * 60 + second) * 1_000_000_000 + nanos)
}

fn timestamp_millis(text: &str) -> Option<i64> {
    let text = text.trim();
    let (date, rest) = match text.find([' ', 'T']) {
        Some(at) => (&text[..at], text[at + 1..].trim()),
        None => (text, ""),
    };
    let days = date_days(date)?;
    let (time, offset) = match rest.find(['+', 'Z', 'z']).or_else(|| rest.rfind('-')) {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, ""),
    };
    let nanos = if time.is_empty() { 0 } else { time_nanos(time)? };
    let offset_seconds = match offset {
        "" | "Z" | "z" => 0,
        signed => {
            let sign = if signed.starts_with('-') { -1 } else { 1 };
            let digits = signed[1..].replace(':', "");
            let (hours, minutes) = match digits.len() {
                2 => (digits.parse::<i64>().ok()?, 0),
                4 => (digits[..2].parse::<i64>().ok()?, digits[2..].parse::<i64>().ok()?),
                _ => return None,
            };
            sign * (hours * 3600 + minutes * 60)
        }
    };
    Some((days * 86_400 - offset_seconds) * 1000 + nanos / 1_000_000)
}

fn hex_bytes(text: &str) -> Option<Vec<u8>> {
    let digits = text.strip_prefix("\\x").or_else(|| text.strip_prefix("0x")).unwrap_or(text);
    if digits.len() % 2 == 1 {
        return None;
    }
    (0..digits.len()).step_by(2).map(|at| u8::from_str_radix(digits.get(at..at + 2)?, 16).ok()).collect()
}

/// A value's text form (as a CQL literal or a collection's JSON item carries it) in the binary
/// form of `cql`.
pub(crate) fn encode_text(text: &str, cql: CqlType) -> Result<Vec<u8>, String> {
    let wrong = || format!("{text} does not fit the CQL type {}", cql.name());
    Ok(match cql {
        CqlType::Ascii | CqlType::Varchar => text.as_bytes().to_vec(),
        CqlType::Boolean => vec![u8::from(text.eq_ignore_ascii_case("true"))],
        CqlType::Uuid | CqlType::TimeUuid => {
            let bytes = hex_bytes(&text.replace('-', "")).filter(|bytes| bytes.len() == 16).ok_or_else(wrong)?;
            bytes
        }
        CqlType::Timestamp => timestamp_millis(text).ok_or_else(wrong)?.to_be_bytes().to_vec(),
        CqlType::Date => u32::try_from(date_days(text).ok_or_else(wrong)? + DATE_CENTER).map_err(|_| wrong())?.to_be_bytes().to_vec(),
        CqlType::Time => time_nanos(text).ok_or_else(wrong)?.to_be_bytes().to_vec(),
        CqlType::Blob => hex_bytes(text).ok_or_else(wrong)?,
        CqlType::List(_) | CqlType::Set(_) | CqlType::Map(..) | CqlType::Tuple(_) | CqlType::Udt(_) => {
            let node = parse_document(text).map_err(|error| error.message)?;
            encode_json(&node, cql, 0)?
        }
        _ => encode_value(&Value::Text(text.to_owned()), cql)?.unwrap_or_default(),
    })
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
    format!(
        "the value {} does not fit the CQL type {}",
        value.to_text().unwrap_or_default(),
        cql.name()
    )
}

/// One JSON element of a stored user-type value (a selected field) in the binary form of
/// `cql`, or `None` for NULL.
pub(crate) fn encode_json_element(value: &Value, cql: CqlType) -> Result<Option<Vec<u8>>, String> {
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    let node = parse_document(&value.to_text().unwrap_or_default()).map_err(|error| error.message)?;
    if node.value == Json::Null {
        return Ok(None);
    }
    encode_json(&node, cql, 0).map(Some)
}

/// `value` in the binary form of `cql`, or `None` for NULL.
pub(crate) fn encode_value(value: &Value, cql: CqlType) -> Result<Option<Vec<u8>>, String> {
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    let bytes = match cql {
        CqlType::Ascii | CqlType::Varchar => value.to_text().unwrap_or_default().into_bytes(),
        CqlType::Int => i32::try_from(integer(value).ok_or_else(|| mismatch(value, cql))?)
            .map_err(|_| mismatch(value, cql))?
            .to_be_bytes()
            .to_vec(),
        CqlType::BigInt | CqlType::Counter => integer(value)
            .ok_or_else(|| mismatch(value, cql))?
            .to_be_bytes()
            .to_vec(),
        CqlType::SmallInt => i16::try_from(integer(value).ok_or_else(|| mismatch(value, cql))?)
            .map_err(|_| mismatch(value, cql))?
            .to_be_bytes()
            .to_vec(),
        CqlType::TinyInt => i8::try_from(integer(value).ok_or_else(|| mismatch(value, cql))?)
            .map_err(|_| mismatch(value, cql))?
            .to_be_bytes()
            .to_vec(),
        CqlType::Boolean => match value {
            Value::Boolean(flag) => vec![u8::from(*flag)],
            _ => return Err(mismatch(value, cql)),
        },
        CqlType::Double => match value {
            Value::Double(number) => number.to_be_bytes().to_vec(),
            Value::Real(number) => f64::from(*number).to_be_bytes().to_vec(),
            _ => value
                .to_text()
                .and_then(|text| text.parse::<f64>().ok())
                .ok_or_else(|| mismatch(value, cql))?
                .to_be_bytes()
                .to_vec(),
        },
        CqlType::Float => match value {
            Value::Real(number) => number.to_be_bytes().to_vec(),
            _ => value
                .to_text()
                .and_then(|text| text.parse::<f32>().ok())
                .ok_or_else(|| mismatch(value, cql))?
                .to_be_bytes()
                .to_vec(),
        },
        CqlType::Decimal => {
            let (negative, digits, scale) = decimal_parts(&value.to_text().unwrap_or_default())?;
            let mut out = scale.to_be_bytes().to_vec();
            out.extend(varint_bytes(negative, magnitude_from_digits(&digits)));
            out
        }
        CqlType::Varint => {
            let (negative, digits, scale) = decimal_parts(&value.to_text().unwrap_or_default())?;
            if scale != 0
                && digits[digits.len() - usize::try_from(scale).unwrap_or(0)..]
                    .bytes()
                    .any(|byte| byte != b'0')
            {
                return Err(mismatch(value, cql));
            }
            varint_bytes(
                negative,
                magnitude_from_digits(
                    &digits[..digits.len() - usize::try_from(scale).unwrap_or(0)],
                ),
            )
        }
        CqlType::Uuid | CqlType::TimeUuid => match value {
            Value::Uuid(bytes) => bytes.to_vec(),
            _ => return Err(mismatch(value, cql)),
        },
        CqlType::Timestamp => match value {
            Value::TimestampTz(micros) | Value::Timestamp(micros) => (micros.div_euclid(1000)
                + UNIX_EPOCH_FROM_2000_MS)
                .to_be_bytes()
                .to_vec(),
            _ => return Err(mismatch(value, cql)),
        },
        CqlType::Date => match value {
            Value::Date(days) => {
                u32::try_from(i64::from(*days) + UNIX_EPOCH_FROM_2000_DAYS + DATE_CENTER)
                    .map_err(|_| mismatch(value, cql))?
                    .to_be_bytes()
                    .to_vec()
            }
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
        CqlType::List(_) | CqlType::Set(_) | CqlType::Map(..) | CqlType::Tuple(_) | CqlType::Udt(_) => {
            encode_text(&value.to_text().unwrap_or_default(), cql)?
        }
    };
    Ok(Some(bytes))
}

#[cfg(test)]
pub(crate) fn varint_round_trip(text: &str) -> String {
    let (negative, digits, _) = decimal_parts(text).unwrap();
    varint_text(&varint_bytes(negative, magnitude_from_digits(&digits)))
}
