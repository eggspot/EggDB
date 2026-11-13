//! The text form of a column value that `pgoutput` sent in its type's binary send format
//! (`binary 'true'`), for the built-in types EggDB maps, arrays, ranges and multiranges of them,
//! network and geometric types, and the source's own enums and domains.

use super::ReplicationError;
use std::collections::HashMap;

const BOOL: u32 = 16;
const BYTEA: u32 = 17;
const INT8: u32 = 20;
const INT2: u32 = 21;
const INT4: u32 = 23;
const TEXT: u32 = 25;
const JSON: u32 = 114;
const FLOAT4: u32 = 700;
const FLOAT8: u32 = 701;
const BPCHAR: u32 = 1042;
const VARCHAR: u32 = 1043;
const DATE: u32 = 1082;
const TIME: u32 = 1083;
const TIMESTAMP: u32 = 1114;
const TIMESTAMPTZ: u32 = 1184;
const INTERVAL: u32 = 1186;
const TIMETZ: u32 = 1266;
const NUMERIC: u32 = 1700;
const UUID: u32 = 2950;
const JSONB: u32 = 3802;
const CIDR: u32 = 650;
const INET: u32 = 869;
const MACADDR: u32 = 829;
const MACADDR8: u32 = 774;
const POINT: u32 = 600;
const LSEG: u32 = 601;
const PATH: u32 = 602;
const BOX: u32 = 603;
const POLYGON: u32 = 604;
const LINE: u32 = 628;
const CIRCLE: u32 = 718;

const DECODED: &[u32] = &[
    BOOL,
    BYTEA,
    INT8,
    INT2,
    INT4,
    TEXT,
    JSON,
    FLOAT4,
    FLOAT8,
    BPCHAR,
    VARCHAR,
    DATE,
    TIME,
    TIMESTAMP,
    TIMESTAMPTZ,
    INTERVAL,
    TIMETZ,
    NUMERIC,
    UUID,
    JSONB,
    CIDR,
    INET,
    MACADDR,
    MACADDR8,
    POINT,
    LSEG,
    PATH,
    BOX,
    POLYGON,
    LINE,
    CIRCLE,
];

const BUILTIN_ARRAYS: &[(u32, u32)] = &[
    (1000, BOOL),
    (1001, BYTEA),
    (1005, INT2),
    (1007, INT4),
    (1016, INT8),
    (1009, TEXT),
    (199, JSON),
    (1021, FLOAT4),
    (1022, FLOAT8),
    (1014, BPCHAR),
    (1015, VARCHAR),
    (1182, DATE),
    (1183, TIME),
    (1115, TIMESTAMP),
    (1185, TIMESTAMPTZ),
    (1187, INTERVAL),
    (1270, TIMETZ),
    (1231, NUMERIC),
    (2951, UUID),
    (3807, JSONB),
    (651, CIDR),
    (1041, INET),
    (1040, MACADDR),
    (775, MACADDR8),
    (1017, POINT),
    (1018, LSEG),
    (1019, PATH),
    (1020, BOX),
    (1027, POLYGON),
    (629, LINE),
    (719, CIRCLE),
    (3905, 3904),
    (3907, 3906),
    (3909, 3908),
    (3911, 3910),
    (3913, 3912),
    (3927, 3926),
];

const BUILTIN_RANGES: &[(u32, u32)] = &[
    (3904, INT4),
    (3906, NUMERIC),
    (3908, TIMESTAMP),
    (3910, TIMESTAMPTZ),
    (3912, DATE),
    (3926, INT8),
];

const BUILTIN_MULTIRANGES: &[(u32, u32)] = &[
    (4451, 3904),
    (4532, 3906),
    (4533, 3908),
    (4534, 3910),
    (4535, 3912),
    (4536, 3926),
];

const MAX_TYPE_DEPTH: usize = 8;
const MAX_ARRAY_DIMENSIONS: usize = 6;
const PGSQL_AF_INET: u8 = 2;
const PGSQL_AF_INET6: u8 = 3;
const RANGE_EMPTY: u8 = 0x01;
const RANGE_LOWER_INCLUSIVE: u8 = 0x02;
const RANGE_UPPER_INCLUSIVE: u8 = 0x04;
const RANGE_LOWER_INFINITE: u8 = 0x08;
const RANGE_UPPER_INFINITE: u8 = 0x10;

const MICROS_PER_DAY: i64 = 86_400_000_000;
const DAYS_FROM_CIVIL_EPOCH_TO_POSTGRES_EPOCH: i64 = 10_957;
const NUMERIC_POSITIVE: u16 = 0x0000;
const NUMERIC_NEGATIVE: u16 = 0x4000;
const NUMERIC_NAN: u16 = 0xC000;
const NUMERIC_POSITIVE_INFINITY: u16 = 0xD000;
const NUMERIC_NEGATIVE_INFINITY: u16 = 0xF000;
const NUMERIC_BASE: u16 = 10_000;
const JSONB_VERSION: u8 = 1;

/// Whether a value of this built-in type, sent in its binary send format, can be read back
/// into text.
pub fn decodes_binary(type_oid: u32) -> bool {
    BinaryTypes::default().decodes(type_oid)
}

/// What one of the source's own types is, as its catalog (`pg_type`, `pg_range`) says, so that
/// its binary values can be read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserType {
    /// An enum: its binary form is the label.
    Enum,
    /// A domain: its binary form is its base type's.
    Domain {
        /// The base type's OID.
        base: u32,
    },
    /// An array of `element`.
    Array {
        /// The element type's OID.
        element: u32,
    },
    /// A range of `subtype`.
    Range {
        /// The subtype's OID.
        subtype: u32,
    },
    /// A multirange of the range type `range`.
    Multirange {
        /// The range type's OID.
        range: u32,
    },
}

/// The types whose binary values can be read back: the built-in ones, their arrays, ranges
/// and multiranges, and the source's own types added with [`BinaryTypes::add`].
#[derive(Debug, Clone, Default)]
pub struct BinaryTypes {
    user: HashMap<u32, UserType>,
}

fn builtin_kind(type_oid: u32) -> Option<UserType> {
    let find = |table: &[(u32, u32)]| {
        table
            .iter()
            .find(|(oid, _)| *oid == type_oid)
            .map(|(_, inner)| *inner)
    };
    find(BUILTIN_ARRAYS)
        .map(|element| UserType::Array { element })
        .or_else(|| find(BUILTIN_RANGES).map(|subtype| UserType::Range { subtype }))
        .or_else(|| find(BUILTIN_MULTIRANGES).map(|range| UserType::Multirange { range }))
}

fn too_deep(type_oid: u32) -> ReplicationError {
    ReplicationError(format!(
        "the type {type_oid} nests other types more than {MAX_TYPE_DEPTH} deep"
    ))
}

impl BinaryTypes {
    /// Only the built-in types.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one of the source's own types.
    pub fn add(&mut self, type_oid: u32, kind: UserType) {
        self.user.insert(type_oid, kind);
    }

    fn kind(&self, type_oid: u32) -> Option<UserType> {
        self.user
            .get(&type_oid)
            .copied()
            .or_else(|| builtin_kind(type_oid))
    }

    /// Whether a value of this type, sent in its binary send format, can be read back.
    pub fn decodes(&self, type_oid: u32) -> bool {
        self.decodes_at(type_oid, 0)
    }

    fn decodes_at(&self, type_oid: u32, depth: usize) -> bool {
        if depth > MAX_TYPE_DEPTH {
            return false;
        }
        if DECODED.contains(&type_oid) {
            return true;
        }
        match self.kind(type_oid) {
            Some(UserType::Enum) => true,
            Some(UserType::Domain { base: inner })
            | Some(UserType::Array { element: inner })
            | Some(UserType::Range { subtype: inner })
            | Some(UserType::Multirange { range: inner }) => self.decodes_at(inner, depth + 1),
            None => false,
        }
    }

    /// The text form PostgreSQL's input function accepts for a value of `type_oid` sent in
    /// its binary send format.
    pub fn text(&self, type_oid: u32, bytes: &[u8]) -> Result<String, ReplicationError> {
        self.text_at(type_oid, bytes, 0)
    }

    fn text_at(
        &self,
        type_oid: u32,
        bytes: &[u8],
        depth: usize,
    ) -> Result<String, ReplicationError> {
        if depth > MAX_TYPE_DEPTH {
            return Err(too_deep(type_oid));
        }
        if DECODED.contains(&type_oid) {
            return scalar_text(type_oid, bytes);
        }
        match self.kind(type_oid) {
            Some(UserType::Enum) => utf8(bytes, type_oid),
            Some(UserType::Domain { base }) => self.text_at(base, bytes, depth + 1),
            Some(UserType::Array { .. }) => self.array_text(type_oid, bytes, depth),
            Some(UserType::Range { subtype }) => self.range_text(subtype, bytes, depth),
            Some(UserType::Multirange { range }) => {
                self.multirange_text(type_oid, range, bytes, depth)
            }
            None => scalar_text(type_oid, bytes),
        }
    }

    fn array_text(
        &self,
        type_oid: u32,
        bytes: &[u8],
        depth: usize,
    ) -> Result<String, ReplicationError> {
        let mut reader = Reader::new(bytes, type_oid);
        let dimensions = usize::try_from(reader.i32()?).map_err(|_| malformed(type_oid))?;
        if dimensions > MAX_ARRAY_DIMENSIONS {
            return Err(malformed(type_oid));
        }
        reader.i32()?;
        let element = reader.u32()?;
        let mut lengths = Vec::with_capacity(dimensions);
        let mut lower_bounds = Vec::with_capacity(dimensions);
        let mut count = 1usize;
        for _ in 0..dimensions {
            let length = usize::try_from(reader.i32()?).map_err(|_| malformed(type_oid))?;
            lower_bounds.push(i64::from(reader.i32()?));
            count = count.checked_mul(length).ok_or_else(|| malformed(type_oid))?;
            lengths.push(length);
        }
        if dimensions == 0 {
            return Ok("{}".to_owned());
        }
        if count > reader.remaining() / 4 {
            return Err(malformed(type_oid));
        }
        let delimiter = if element == BOX { ';' } else { ',' };
        let mut elements = Vec::with_capacity(count);
        for _ in 0..count {
            elements.push(match reader.value()? {
                None => "NULL".to_owned(),
                Some(value) => array_element(&self.text_at(element, value, depth + 1)?, delimiter),
            });
        }
        if reader.remaining() != 0 {
            return Err(malformed(type_oid));
        }
        let mut text = String::new();
        if lower_bounds.iter().any(|bound| *bound != 1) {
            for (length, lower) in lengths.iter().zip(&lower_bounds) {
                text.push_str(&format!("[{lower}:{}]", lower + *length as i64 - 1));
            }
            text.push('=');
        }
        let mut next = 0;
        nest_array(&mut text, &lengths, &elements, &mut next, delimiter);
        Ok(text)
    }

    fn range_text(
        &self,
        subtype: u32,
        bytes: &[u8],
        depth: usize,
    ) -> Result<String, ReplicationError> {
        let mut reader = Reader::new(bytes, subtype);
        let flags = reader.u8()?;
        if flags & RANGE_EMPTY != 0 {
            return if reader.remaining() == 0 {
                Ok("empty".to_owned())
            } else {
                Err(malformed(subtype))
            };
        }
        let mut bound = |infinite: u8| -> Result<String, ReplicationError> {
            if flags & infinite != 0 {
                return Ok(String::new());
            }
            let value = reader.value()?.ok_or_else(|| malformed(subtype))?;
            Ok(range_bound(&self.text_at(subtype, value, depth + 1)?))
        };
        let lower = bound(RANGE_LOWER_INFINITE)?;
        let upper = bound(RANGE_UPPER_INFINITE)?;
        if reader.remaining() != 0 {
            return Err(malformed(subtype));
        }
        Ok(format!(
            "{}{lower},{upper}{}",
            if flags & RANGE_LOWER_INCLUSIVE != 0 { '[' } else { '(' },
            if flags & RANGE_UPPER_INCLUSIVE != 0 { ']' } else { ')' }
        ))
    }

    fn multirange_text(
        &self,
        type_oid: u32,
        range: u32,
        bytes: &[u8],
        depth: usize,
    ) -> Result<String, ReplicationError> {
        let mut reader = Reader::new(bytes, type_oid);
        let count = usize::try_from(reader.i32()?).map_err(|_| malformed(type_oid))?;
        if count > reader.remaining() / 4 {
            return Err(malformed(type_oid));
        }
        let mut ranges = Vec::with_capacity(count);
        for _ in 0..count {
            let value = reader.value()?.ok_or_else(|| malformed(type_oid))?;
            ranges.push(self.text_at(range, value, depth + 1)?);
        }
        if reader.remaining() != 0 {
            return Err(malformed(type_oid));
        }
        Ok(format!("{{{}}}", ranges.join(",")))
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    type_oid: u32,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8], type_oid: u32) -> Self {
        Self { bytes, type_oid }
    }

    fn remaining(&self) -> usize {
        self.bytes.len()
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], ReplicationError> {
        let (taken, rest) = self
            .bytes
            .split_at_checked(count)
            .ok_or_else(|| malformed(self.type_oid))?;
        self.bytes = rest;
        Ok(taken)
    }

    fn u8(&mut self) -> Result<u8, ReplicationError> {
        Ok(self.take(1)?[0])
    }

    fn i32(&mut self) -> Result<i32, ReplicationError> {
        Ok(i32::from_be_bytes(fixed(self.take(4)?, self.type_oid)?))
    }

    fn u32(&mut self) -> Result<u32, ReplicationError> {
        Ok(u32::from_be_bytes(fixed(self.take(4)?, self.type_oid)?))
    }

    fn f64(&mut self) -> Result<f64, ReplicationError> {
        Ok(f64::from_bits(u64::from_be_bytes(fixed(
            self.take(8)?,
            self.type_oid,
        )?)))
    }

    fn value(&mut self) -> Result<Option<&'a [u8]>, ReplicationError> {
        let length = self.i32()?;
        if length == -1 {
            return Ok(None);
        }
        let length = usize::try_from(length).map_err(|_| malformed(self.type_oid))?;
        self.take(length).map(Some)
    }

    fn finish(&self) -> Result<(), ReplicationError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(malformed(self.type_oid))
        }
    }
}

fn needs_quotes(text: &str, special: &[char]) -> bool {
    text.is_empty()
        || text.chars().any(|character| {
            special.contains(&character)
                || character.is_ascii_whitespace()
                || character == '\u{0B}'
        })
}

fn array_element(text: &str, delimiter: char) -> String {
    let special = ['"', '\\', '{', '}', delimiter];
    if !needs_quotes(text, &special) && !text.eq_ignore_ascii_case("NULL") {
        return text.to_owned();
    }
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for character in text.chars() {
        if character == '"' || character == '\\' {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted.push('"');
    quoted
}

fn range_bound(text: &str) -> String {
    let special = ['"', '\\', '(', ')', '[', ']', ','];
    if !needs_quotes(text, &special) {
        return text.to_owned();
    }
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for character in text.chars() {
        if character == '"' || character == '\\' {
            quoted.push(character);
        }
        quoted.push(character);
    }
    quoted.push('"');
    quoted
}

fn nest_array(
    text: &mut String,
    lengths: &[usize],
    elements: &[String],
    next: &mut usize,
    delimiter: char,
) {
    let Some((length, inner)) = lengths.split_first() else {
        if let Some(element) = elements.get(*next) {
            text.push_str(element);
        }
        *next += 1;
        return;
    };
    text.push('{');
    for index in 0..*length {
        if index > 0 {
            text.push(delimiter);
        }
        nest_array(text, inner, elements, next, delimiter);
    }
    text.push('}');
}

fn ipv6_text(address: &[u8; 16]) -> String {
    let groups: Vec<u16> = address
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_be_bytes(*pair))
        .collect();
    let mut best = (0usize, 0usize);
    let mut index = 0;
    while index < groups.len() {
        if groups[index] == 0 {
            let start = index;
            while index < groups.len() && groups[index] == 0 {
                index += 1;
            }
            if index - start > best.1 {
                best = (start, index - start);
            }
        } else {
            index += 1;
        }
    }
    let hex = |range: &[u16]| {
        range
            .iter()
            .map(|group| format!("{group:x}"))
            .collect::<Vec<_>>()
            .join(":")
    };
    if best.1 < 2 {
        return hex(&groups);
    }
    format!(
        "{}::{}",
        hex(&groups[..best.0]),
        hex(&groups[best.0 + best.1..])
    )
}

fn network_text(type_oid: u32, bytes: &[u8]) -> Result<String, ReplicationError> {
    let mut reader = Reader::new(bytes, type_oid);
    let family = reader.u8()?;
    let bits = reader.u8()?;
    reader.u8()?;
    let length = usize::from(reader.u8()?);
    let address = reader.take(length)?;
    reader.finish()?;
    let (text, maximum) = match (family, address.len()) {
        (PGSQL_AF_INET, 4) => (
            format!("{}.{}.{}.{}", address[0], address[1], address[2], address[3]),
            32,
        ),
        (PGSQL_AF_INET6, 16) => (ipv6_text(&fixed(address, type_oid)?), 128),
        _ => return Err(malformed(type_oid)),
    };
    if bits > maximum {
        return Err(malformed(type_oid));
    }
    Ok(if type_oid == CIDR || bits != maximum {
        format!("{text}/{bits}")
    } else {
        text
    })
}

fn mac_text(bytes: &[u8], length: usize, type_oid: u32) -> Result<String, ReplicationError> {
    if bytes.len() != length {
        return Err(malformed(type_oid));
    }
    Ok(bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":"))
}

fn point_text(reader: &mut Reader<'_>) -> Result<String, ReplicationError> {
    let x = reader.f64()?;
    let y = reader.f64()?;
    Ok(format!("({},{})", float_text(x), float_text(y)))
}

fn points_text(reader: &mut Reader<'_>, count: usize) -> Result<String, ReplicationError> {
    if count > reader.remaining() / 16 {
        return Err(malformed(reader.type_oid));
    }
    (0..count)
        .map(|_| point_text(reader))
        .collect::<Result<Vec<_>, _>>()
        .map(|points| points.join(","))
}

fn geometric_text(type_oid: u32, bytes: &[u8]) -> Result<String, ReplicationError> {
    let mut reader = Reader::new(bytes, type_oid);
    let text = match type_oid {
        POINT => point_text(&mut reader)?,
        LSEG => format!("[{}]", points_text(&mut reader, 2)?),
        BOX => points_text(&mut reader, 2)?,
        PATH => {
            let closed = reader.u8()? != 0;
            let count = usize::try_from(reader.i32()?).map_err(|_| malformed(type_oid))?;
            let points = points_text(&mut reader, count)?;
            if closed {
                format!("({points})")
            } else {
                format!("[{points}]")
            }
        }
        POLYGON => {
            let count = usize::try_from(reader.i32()?).map_err(|_| malformed(type_oid))?;
            format!("({})", points_text(&mut reader, count)?)
        }
        LINE => {
            let a = reader.f64()?;
            let b = reader.f64()?;
            let c = reader.f64()?;
            format!("{{{},{},{}}}", float_text(a), float_text(b), float_text(c))
        }
        CIRCLE => {
            let center = point_text(&mut reader)?;
            let radius = reader.f64()?;
            format!("<{center},{}>", float_text(radius))
        }
        _ => return Err(malformed(type_oid)),
    };
    reader.finish()?;
    Ok(text)
}

fn malformed(type_oid: u32) -> ReplicationError {
    ReplicationError(format!(
        "a binary value of type {type_oid} does not have that type's layout"
    ))
}

fn fixed<const N: usize>(bytes: &[u8], type_oid: u32) -> Result<[u8; N], ReplicationError> {
    bytes.try_into().map_err(|_| malformed(type_oid))
}

fn utf8(bytes: &[u8], type_oid: u32) -> Result<String, ReplicationError> {
    String::from_utf8(bytes.to_vec()).map_err(|_| malformed(type_oid))
}

fn float_text(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else if value.is_infinite() {
        (if value > 0.0 { "Infinity" } else { "-Infinity" }).to_owned()
    } else if value == 0.0 || (1e-5..1e16).contains(&value.abs()) {
        format!("{value}")
    } else {
        format!("{value:e}")
    }
}

fn float4_text(value: f32) -> String {
    if value.is_finite() && value != 0.0 && !(1e-5..1e16).contains(&value.abs()) {
        format!("{value:e}")
    } else if value.is_finite() {
        format!("{value}")
    } else {
        float_text(f64::from(value))
    }
}

fn civil_from_days(days_since_civil_epoch: i64) -> (i64, u32, u32) {
    let shifted = days_since_civil_epoch + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = (if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    }) as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn date_parts(days_since_postgres_epoch: i64) -> (String, bool) {
    let (year, month, day) =
        civil_from_days(days_since_postgres_epoch + DAYS_FROM_CIVIL_EPOCH_TO_POSTGRES_EPOCH);
    let before_christ = year <= 0;
    let shown_year = if before_christ { 1 - year } else { year };
    (
        format!("{shown_year:04}-{month:02}-{day:02}"),
        before_christ,
    )
}

fn time_of_day_text(micros: u64) -> String {
    let seconds = micros / 1_000_000;
    let fraction = micros % 1_000_000;
    let mut text = format!(
        "{:02}:{:02}:{:02}",
        seconds / 3_600,
        seconds / 60 % 60,
        seconds % 60
    );
    if fraction != 0 {
        let digits = format!("{fraction:06}");
        text.push('.');
        text.push_str(digits.trim_end_matches('0'));
    }
    text
}

fn timestamp_text(micros: i64, zone_suffix: &str) -> String {
    match micros {
        i64::MAX => "infinity".to_owned(),
        i64::MIN => "-infinity".to_owned(),
        _ => {
            let (date, before_christ) = date_parts(micros.div_euclid(MICROS_PER_DAY));
            let time = time_of_day_text(micros.rem_euclid(MICROS_PER_DAY) as u64);
            format!(
                "{date} {time}{zone_suffix}{}",
                if before_christ { " BC" } else { "" }
            )
        }
    }
}

fn zone_text(seconds_west_of_utc: i32) -> String {
    let east = -i64::from(seconds_west_of_utc);
    let sign = if east < 0 { '-' } else { '+' };
    let east = east.unsigned_abs();
    let mut text = format!("{sign}{:02}", east / 3_600);
    if east % 3_600 != 0 {
        text.push_str(&format!(":{:02}", east / 60 % 60));
        if east % 60 != 0 {
            text.push_str(&format!(":{:02}", east % 60));
        }
    }
    text
}

fn interval_text(bytes: &[u8]) -> Result<String, ReplicationError> {
    let bytes: [u8; 16] = fixed(bytes, INTERVAL)?;
    let micros = i64::from_be_bytes(bytes[..8].try_into().map_err(|_| malformed(INTERVAL))?);
    let days = i32::from_be_bytes(bytes[8..12].try_into().map_err(|_| malformed(INTERVAL))?);
    let months = i32::from_be_bytes(bytes[12..].try_into().map_err(|_| malformed(INTERVAL))?);
    let sign = if micros < 0 { "-" } else { "" };
    Ok(format!(
        "{months} mons {days} days {sign}{}",
        time_of_day_text(micros.unsigned_abs())
    ))
}

fn numeric_text(bytes: &[u8]) -> Result<String, ReplicationError> {
    let header: [u8; 8] = bytes
        .get(..8)
        .and_then(|header| header.try_into().ok())
        .ok_or_else(|| malformed(NUMERIC))?;
    let digit_count = usize::from(u16::from_be_bytes([header[0], header[1]]));
    let weight = i64::from(i16::from_be_bytes([header[2], header[3]]));
    let sign = u16::from_be_bytes([header[4], header[5]]);
    let scale = usize::from(u16::from_be_bytes([header[6], header[7]]) & 0x3FFF);
    let body = &bytes[8..];
    if body.len() != digit_count * 2 {
        return Err(malformed(NUMERIC));
    }
    let digits = body
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let digit = u16::from_be_bytes(*pair);
            (digit < NUMERIC_BASE)
                .then_some(digit)
                .ok_or_else(|| malformed(NUMERIC))
        })
        .collect::<Result<Vec<u16>, _>>()?;
    let mut text = match sign {
        NUMERIC_NAN => return Ok("NaN".to_owned()),
        NUMERIC_POSITIVE_INFINITY => return Ok("Infinity".to_owned()),
        NUMERIC_NEGATIVE_INFINITY => return Ok("-Infinity".to_owned()),
        NUMERIC_NEGATIVE => String::from("-"),
        NUMERIC_POSITIVE => String::new(),
        _ => return Err(malformed(NUMERIC)),
    };
    let digit_at = |index: i64| -> u16 {
        usize::try_from(index)
            .ok()
            .and_then(|index| digits.get(index).copied())
            .unwrap_or(0)
    };
    if weight < 0 {
        text.push('0');
    } else {
        text.push_str(&digit_at(0).to_string());
        for index in 1..=weight {
            text.push_str(&format!("{:04}", digit_at(index)));
        }
    }
    if scale > 0 {
        let mut fraction = String::with_capacity(scale + 4);
        let mut index = weight + 1;
        while fraction.len() < scale {
            fraction.push_str(&format!("{:04}", digit_at(index)));
            index += 1;
        }
        fraction.truncate(scale);
        text.push('.');
        text.push_str(&fraction);
    }
    Ok(text)
}

fn uuid_text(bytes: &[u8]) -> Result<String, ReplicationError> {
    let bytes: [u8; 16] = fixed(bytes, UUID)?;
    let mut text = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            text.push('-');
        }
        text.push_str(&format!("{byte:02x}"));
    }
    Ok(text)
}

fn bytea_text(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(2 + bytes.len() * 2);
    text.push_str("\\x");
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

/// The text form PostgreSQL's input function accepts for a value of the built-in `type_oid`
/// sent in its binary send format; an error for a type [`decodes_binary`] refuses or bytes that
/// do not have the type's layout.
pub fn binary_value_text(type_oid: u32, bytes: &[u8]) -> Result<String, ReplicationError> {
    BinaryTypes::default().text(type_oid, bytes)
}

fn scalar_text(type_oid: u32, bytes: &[u8]) -> Result<String, ReplicationError> {
    Ok(match type_oid {
        BOOL => match fixed::<1>(bytes, type_oid)? {
            [0] => "false".to_owned(),
            [1] => "true".to_owned(),
            _ => return Err(malformed(type_oid)),
        },
        BYTEA => bytea_text(bytes),
        INT2 => i16::from_be_bytes(fixed(bytes, type_oid)?).to_string(),
        INT4 => i32::from_be_bytes(fixed(bytes, type_oid)?).to_string(),
        INT8 => i64::from_be_bytes(fixed(bytes, type_oid)?).to_string(),
        TEXT | JSON | BPCHAR | VARCHAR => utf8(bytes, type_oid)?,
        JSONB => match bytes.split_first() {
            Some((&JSONB_VERSION, text)) => utf8(text, type_oid)?,
            _ => return Err(malformed(type_oid)),
        },
        FLOAT4 => float4_text(f32::from_bits(u32::from_be_bytes(fixed(bytes, type_oid)?))),
        FLOAT8 => float_text(f64::from_bits(u64::from_be_bytes(fixed(bytes, type_oid)?))),
        DATE => match i32::from_be_bytes(fixed(bytes, type_oid)?) {
            i32::MAX => "infinity".to_owned(),
            i32::MIN => "-infinity".to_owned(),
            days => {
                let (date, before_christ) = date_parts(i64::from(days));
                if before_christ {
                    format!("{date} BC")
                } else {
                    date
                }
            }
        },
        TIME => {
            let micros = i64::from_be_bytes(fixed(bytes, type_oid)?);
            if !(0..=MICROS_PER_DAY).contains(&micros) {
                return Err(malformed(type_oid));
            }
            time_of_day_text(micros as u64)
        }
        TIMETZ => {
            let bytes: [u8; 12] = fixed(bytes, type_oid)?;
            let micros = i64::from_be_bytes(bytes[..8].try_into().map_err(|_| malformed(TIMETZ))?);
            let zone = i32::from_be_bytes(bytes[8..].try_into().map_err(|_| malformed(TIMETZ))?);
            if !(0..=MICROS_PER_DAY).contains(&micros) {
                return Err(malformed(type_oid));
            }
            format!("{}{}", time_of_day_text(micros as u64), zone_text(zone))
        }
        TIMESTAMP => timestamp_text(i64::from_be_bytes(fixed(bytes, type_oid)?), ""),
        TIMESTAMPTZ => timestamp_text(i64::from_be_bytes(fixed(bytes, type_oid)?), "+00"),
        INTERVAL => interval_text(bytes)?,
        NUMERIC => numeric_text(bytes)?,
        UUID => uuid_text(bytes)?,
        INET | CIDR => network_text(type_oid, bytes)?,
        MACADDR => mac_text(bytes, 6, type_oid)?,
        MACADDR8 => mac_text(bytes, 8, type_oid)?,
        POINT | LSEG | PATH | BOX | POLYGON | LINE | CIRCLE => geometric_text(type_oid, bytes)?,
        _ => {
            return Err(ReplicationError(format!(
                "values of type {type_oid} arrived in binary, which follow cannot read; follow again to read them as text"
            )));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numeric(digits: &[u16], weight: i16, sign: u16, scale: u16) -> Vec<u8> {
        let mut bytes = (digits.len() as u16).to_be_bytes().to_vec();
        bytes.extend_from_slice(&weight.to_be_bytes());
        bytes.extend_from_slice(&sign.to_be_bytes());
        bytes.extend_from_slice(&scale.to_be_bytes());
        for digit in digits {
            bytes.extend_from_slice(&digit.to_be_bytes());
        }
        bytes
    }

    #[test]
    fn integers_booleans_and_text_read_back() {
        assert_eq!(
            binary_value_text(INT4, &(-42i32).to_be_bytes()).unwrap(),
            "-42"
        );
        assert_eq!(binary_value_text(INT2, &7i16.to_be_bytes()).unwrap(), "7");
        assert_eq!(
            binary_value_text(INT8, &i64::MIN.to_be_bytes()).unwrap(),
            "-9223372036854775808"
        );
        assert_eq!(binary_value_text(BOOL, &[1]).unwrap(), "true");
        assert_eq!(binary_value_text(TEXT, "it's".as_bytes()).unwrap(), "it's");
        assert_eq!(
            binary_value_text(JSONB, b"\x01{\"n\": 1}").unwrap(),
            "{\"n\": 1}"
        );
        assert_eq!(binary_value_text(BYTEA, &[0xDE, 0xAD]).unwrap(), "\\xdead");
    }

    #[test]
    fn numerics_keep_their_scale_and_sign() {
        assert_eq!(
            binary_value_text(NUMERIC, &numeric(&[12, 3400], 0, NUMERIC_POSITIVE, 2)).unwrap(),
            "12.34"
        );
        assert_eq!(
            binary_value_text(NUMERIC, &numeric(&[1, 2345, 6789], 1, NUMERIC_NEGATIVE, 4)).unwrap(),
            "-12345.6789"
        );
        assert_eq!(
            binary_value_text(NUMERIC, &numeric(&[5], -2, NUMERIC_POSITIVE, 8)).unwrap(),
            "0.00000005"
        );
        assert_eq!(
            binary_value_text(NUMERIC, &numeric(&[], 0, NUMERIC_POSITIVE, 2)).unwrap(),
            "0.00"
        );
        assert_eq!(
            binary_value_text(NUMERIC, &numeric(&[1], 2, NUMERIC_POSITIVE, 0)).unwrap(),
            "100000000"
        );
        assert_eq!(
            binary_value_text(NUMERIC, &numeric(&[], 0, NUMERIC_NAN, 0)).unwrap(),
            "NaN"
        );
        assert!(binary_value_text(NUMERIC, &numeric(&[10_000], 0, NUMERIC_POSITIVE, 0)).is_err());
        assert!(binary_value_text(NUMERIC, &numeric(&[1], 0, 0x1234, 0)).is_err());
    }

    #[test]
    fn dates_and_times_print_as_postgresql_reads_them() {
        assert_eq!(
            binary_value_text(DATE, &0i32.to_be_bytes()).unwrap(),
            "2000-01-01"
        );
        assert_eq!(
            binary_value_text(DATE, &9_497i32.to_be_bytes()).unwrap(),
            "2026-01-01"
        );
        assert_eq!(
            binary_value_text(DATE, &i32::MAX.to_be_bytes()).unwrap(),
            "infinity"
        );
        assert_eq!(
            binary_value_text(DATE, &(-730_120i32).to_be_bytes()).unwrap(),
            "0001-12-31 BC"
        );
        let at = 9_497i64 * MICROS_PER_DAY + 3_723_000_500;
        assert_eq!(
            binary_value_text(TIMESTAMPTZ, &at.to_be_bytes()).unwrap(),
            "2026-01-01 01:02:03.0005+00"
        );
        assert_eq!(
            binary_value_text(TIMESTAMP, &(-1i64).to_be_bytes()).unwrap(),
            "1999-12-31 23:59:59.999999"
        );
        let mut timetz = 3_600_000_000i64.to_be_bytes().to_vec();
        timetz.extend_from_slice(&(-19_800i32).to_be_bytes());
        assert_eq!(
            binary_value_text(TIMETZ, &timetz).unwrap(),
            "01:00:00+05:30"
        );
        let mut interval = (-5_000_000i64).to_be_bytes().to_vec();
        interval.extend_from_slice(&3i32.to_be_bytes());
        interval.extend_from_slice(&14i32.to_be_bytes());
        assert_eq!(
            binary_value_text(INTERVAL, &interval).unwrap(),
            "14 mons 3 days -00:00:05"
        );
    }

    #[test]
    fn floats_and_uuids_read_back() {
        assert_eq!(
            binary_value_text(FLOAT8, &1.5f64.to_be_bytes()).unwrap(),
            "1.5"
        );
        assert_eq!(
            binary_value_text(FLOAT8, &f64::INFINITY.to_be_bytes()).unwrap(),
            "Infinity"
        );
        assert_eq!(
            binary_value_text(FLOAT8, &1e300f64.to_be_bytes()).unwrap(),
            "1e300"
        );
        assert_eq!(
            binary_value_text(FLOAT4, &0.25f32.to_be_bytes()).unwrap(),
            "0.25"
        );
        let uuid: Vec<u8> = (0u8..16).collect();
        assert_eq!(
            binary_value_text(UUID, &uuid).unwrap(),
            "00010203-0405-0607-0809-0a0b0c0d0e0f"
        );
    }

    #[test]
    fn wrong_lengths_and_unknown_types_are_errors_not_panics() {
        for &type_oid in DECODED {
            for length in 0..20 {
                let bytes = vec![0xFFu8; length];
                let _ = binary_value_text(type_oid, &bytes);
            }
        }
        assert!(binary_value_text(INT4, &[0, 1]).is_err());
        assert!(binary_value_text(TIME, &(-1i64).to_be_bytes()).is_err());
        assert!(binary_value_text(JSONB, b"\x02{}").is_err());
        assert!(binary_value_text(142, &[0; 16]).is_err());
        assert!(!decodes_binary(142));
        assert!(decodes_binary(NUMERIC));
    }

    fn array(element: u32, lengths: &[(i32, i32)], values: &[Option<&[u8]>]) -> Vec<u8> {
        let mut bytes = (lengths.len() as i32).to_be_bytes().to_vec();
        bytes.extend_from_slice(&i32::from(values.iter().any(Option::is_none)).to_be_bytes());
        bytes.extend_from_slice(&element.to_be_bytes());
        for (length, lower) in lengths {
            bytes.extend_from_slice(&length.to_be_bytes());
            bytes.extend_from_slice(&lower.to_be_bytes());
        }
        for value in values {
            match value {
                None => bytes.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(value) => {
                    bytes.extend_from_slice(&(value.len() as i32).to_be_bytes());
                    bytes.extend_from_slice(value);
                }
            }
        }
        bytes
    }

    #[test]
    fn arrays_print_with_their_dimensions_quotes_and_nulls() {
        let one = 1i32.to_be_bytes();
        let two = 2i32.to_be_bytes();
        let three = 3i32.to_be_bytes();
        let four = 4i32.to_be_bytes();
        assert_eq!(
            binary_value_text(1007, &array(INT4, &[(3, 1)], &[Some(&one[..]), None, Some(&three[..])]))
                .unwrap(),
            "{1,NULL,3}"
        );
        assert_eq!(
            binary_value_text(
                1007,
                &array(INT4, &[(2, 1), (2, 1)], &[Some(&one[..]), Some(&two[..]), Some(&three[..]), Some(&four[..])])
            )
            .unwrap(),
            "{{1,2},{3,4}}"
        );
        assert_eq!(
            binary_value_text(1007, &array(INT4, &[(2, 0)], &[Some(&one[..]), Some(&two[..])])).unwrap(),
            "[0:1]={1,2}"
        );
        assert_eq!(
            binary_value_text(
                1009,
                &array(TEXT, &[(4, 1)], &[Some(&b"a b"[..]), Some(&b""[..]), Some(&b"null"[..]), Some(&b"q\"x"[..])])
            )
            .unwrap(),
            "{\"a b\",\"\",\"null\",\"q\\\"x\"}"
        );
        assert_eq!(binary_value_text(1007, &array(INT4, &[], &[])).unwrap(), "{}");
        let mut lying = array(INT4, &[(1, 1)], &[Some(&one[..])]);
        lying[12..16].copy_from_slice(&i32::MAX.to_be_bytes());
        assert!(binary_value_text(1007, &lying).is_err());
    }

    fn range(flags: u8, bounds: &[&[u8]]) -> Vec<u8> {
        let mut bytes = vec![flags];
        for bound in bounds {
            bytes.extend_from_slice(&(bound.len() as i32).to_be_bytes());
            bytes.extend_from_slice(bound);
        }
        bytes
    }

    #[test]
    fn ranges_and_multiranges_print_their_bounds() {
        let one = 1i32.to_be_bytes();
        let five = 5i32.to_be_bytes();
        let closed_open = range(RANGE_LOWER_INCLUSIVE, &[&one[..], &five[..]]);
        assert_eq!(binary_value_text(3904, &closed_open).unwrap(), "[1,5)");
        assert_eq!(binary_value_text(3904, &[RANGE_EMPTY]).unwrap(), "empty");
        assert_eq!(
            binary_value_text(3904, &range(RANGE_LOWER_INFINITE | RANGE_UPPER_INCLUSIVE, &[&five[..]]))
                .unwrap(),
            "(,5]"
        );
        let mut multirange = 2i32.to_be_bytes().to_vec();
        for _ in 0..2 {
            multirange.extend_from_slice(&(closed_open.len() as i32).to_be_bytes());
            multirange.extend_from_slice(&closed_open);
        }
        assert_eq!(binary_value_text(4451, &multirange).unwrap(), "{[1,5),[1,5)}");
        assert!(binary_value_text(3904, &range(0, &[&one[..]])).is_err());
    }

    #[test]
    fn enums_domains_and_their_arrays_read_through_the_source_types() {
        let mut types = BinaryTypes::new();
        types.add(70_000, UserType::Enum);
        types.add(70_001, UserType::Array { element: 70_000 });
        types.add(70_002, UserType::Domain { base: INT4 });
        types.add(70_003, UserType::Domain { base: 70_003 });
        assert!(types.decodes(70_001));
        assert!(!types.decodes(70_003));
        assert!(!decodes_binary(70_000));
        assert_eq!(types.text(70_000, b"happy").unwrap(), "happy");
        assert_eq!(
            types
                .text(70_001, &array(70_000, &[(2, 1)], &[Some(&b"sad"[..]), Some(&b"so so"[..])]))
                .unwrap(),
            "{sad,\"so so\"}"
        );
        assert_eq!(types.text(70_002, &7i32.to_be_bytes()).unwrap(), "7");
        assert!(types.text(70_003, &7i32.to_be_bytes()).is_err());
    }

    #[test]
    fn network_and_geometric_values_print_as_postgresql_reads_them() {
        assert_eq!(
            binary_value_text(INET, &[PGSQL_AF_INET, 32, 0, 4, 10, 0, 0, 1]).unwrap(),
            "10.0.0.1"
        );
        assert_eq!(
            binary_value_text(CIDR, &[PGSQL_AF_INET, 24, 1, 4, 10, 0, 0, 0]).unwrap(),
            "10.0.0.0/24"
        );
        let mut v6 = vec![PGSQL_AF_INET6, 64, 0, 16, 0x20, 0x01, 0x0d, 0xb8];
        v6.extend_from_slice(&[0; 11]);
        v6.push(1);
        assert_eq!(binary_value_text(INET, &v6).unwrap(), "2001:db8::1/64");
        assert_eq!(
            binary_value_text(MACADDR, &[8, 0, 0x2b, 1, 2, 3]).unwrap(),
            "08:00:2b:01:02:03"
        );
        let mut point = 1.5f64.to_be_bytes().to_vec();
        point.extend_from_slice(&(-2.0f64).to_be_bytes());
        assert_eq!(binary_value_text(POINT, &point).unwrap(), "(1.5,-2)");
        let mut circle = point.clone();
        circle.extend_from_slice(&3.0f64.to_be_bytes());
        assert_eq!(binary_value_text(CIRCLE, &circle).unwrap(), "<(1.5,-2),3>");
        let mut polygon = 2i32.to_be_bytes().to_vec();
        polygon.extend_from_slice(&point);
        polygon.extend_from_slice(&point);
        assert_eq!(
            binary_value_text(POLYGON, &polygon).unwrap(),
            "((1.5,-2),(1.5,-2))"
        );
        let mut path = vec![0u8];
        path.extend_from_slice(&polygon);
        assert_eq!(binary_value_text(PATH, &path).unwrap(), "[(1.5,-2),(1.5,-2)]");
        assert!(binary_value_text(INET, &[PGSQL_AF_INET, 40, 0, 4, 1, 2, 3, 4]).is_err());
        let mut huge = i32::MAX.to_be_bytes().to_vec();
        huge.extend_from_slice(&point);
        assert!(binary_value_text(POLYGON, &huge).is_err());
    }
}
