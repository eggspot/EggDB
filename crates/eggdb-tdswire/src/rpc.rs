//! Reading remote procedure call (RPC) requests, as [MS-TDS] 2.2.6.6 defines them: one or more
//! calls, each naming its procedure (by name, or by the id of a system procedure such as
//! `sp_executesql`) and carrying parameters, each with its TYPE_INFO ([MS-TDS] 2.2.5.4) and its
//! value decoded into an engine value. Like [`crate::messages`], every reader takes untrusted
//! bytes and returns an error, never a panic.

use crate::messages::{MalformedMessage, read_all_headers};
use eggdb_sql::{Numeric, Value};

/// The id of `sp_executesql`, which runs a parameterized batch once.
pub const SP_EXECUTESQL: u16 = 10;
/// The id of `sp_prepare`, which prepares a parameterized batch and returns its handle.
pub const SP_PREPARE: u16 = 11;
/// The id of `sp_execute`, which runs a prepared batch by its handle.
pub const SP_EXECUTE: u16 = 12;
/// The id of `sp_prepexec`, which prepares a parameterized batch and runs it at once.
pub const SP_PREPEXEC: u16 = 13;
/// The id of `sp_unprepare`, which forgets a prepared batch.
pub const SP_UNPREPARE: u16 = 15;
/// The most parameters one call may carry, as SQL Server allows.
pub const MAX_PARAMETERS: usize = 2100;

const PROCEDURE_ID: u16 = 0xFFFF;
const MAX_PROCEDURE_NAME_UNITS: usize = 1024;
const BATCH_FLAG: u8 = 0xFF;
const NO_EXEC_FLAG: u8 = 0xFE;
const BY_REFERENCE: u8 = 0x01;
const DEFAULT_VALUE: u8 = 0x02;
const ENCRYPTED: u8 = 0x08;
const MAX_LENGTH: u16 = 0xFFFF;
const NULL_LONG_LENGTH: u32 = 0xFFFF_FFFF;
const PLP_NULL: u64 = u64::MAX;
const PLP_UNKNOWN_LENGTH: u64 = u64::MAX - 1;
const UTF8_COLLATION: u32 = 0x0400_0000;
const MAX_DECIMAL_PRECISION: u8 = 38;
const MAX_TIME_SCALE: u8 = 7;
const MICROS_PER_DAY: i64 = 86_400_000_000;
const DAYS_FROM_YEAR_ONE_TO_2000: i64 = 730_119;
const LAST_DAY_FROM_YEAR_ONE: i64 = 3_652_058;
const DAYS_FROM_1900_TO_2000: i64 = 36_524;
const DATETIME_TICKS_PER_DAY: i64 = 300 * 86_400;
const FIRST_DATETIME_DAY: i64 = -53_690;
const LAST_DATETIME_DAY: i64 = 2_958_463;
const MINUTES_PER_DAY: i64 = 1440;
const MICROS_PER_MINUTE: i64 = 60_000_000;
const MONEY_SCALE: u8 = 4;
const MAX_OFFSET_MINUTES: i16 = 840;

const WINDOWS_1252_HIGH: [char; 32] = [
    '\u{20AC}', '\u{0081}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008D}', '\u{017D}', '\u{008F}',
    '\u{0090}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{0153}', '\u{009D}', '\u{017E}', '\u{0178}',
];

/// Why an RPC request could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcError {
    /// The bytes do not follow [MS-TDS]; the server ends the connection with a protocol error.
    Malformed(MalformedMessage),
    /// A parameter has a TDS type EggDB does not read yet (`sql_variant`, `xml`, user-defined
    /// types, table-valued parameters and the types older than TDS 7.2), named by its byte.
    UnsupportedType(u8),
    /// A parameter is encrypted (Always Encrypted), which EggDB does not offer.
    EncryptedParameter,
    /// A call carries more than [`MAX_PARAMETERS`] parameters.
    TooManyParameters,
}

fn malformed(reason: &'static str) -> RpcError {
    RpcError::Malformed(MalformedMessage { reason })
}

fn ends_early() -> RpcError {
    malformed("the RPC request ends early")
}

/// The procedure an RPC call names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Procedure {
    /// A system procedure named by its id, such as [`SP_EXECUTESQL`].
    Id(u16),
    /// A procedure named by its name, as the client wrote it.
    Name(String),
}

/// The system procedures EggDB runs over RPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemProcedure {
    /// `sp_executesql`.
    ExecuteSql,
    /// `sp_prepare`.
    Prepare,
    /// `sp_execute`.
    Execute,
    /// `sp_prepexec`.
    PrepareExecute,
    /// `sp_unprepare`.
    Unprepare,
}

impl SystemProcedure {
    /// The procedure's name, as SQL Server spells it.
    pub fn name(self) -> &'static str {
        match self {
            Self::ExecuteSql => "sp_executesql",
            Self::Prepare => "sp_prepare",
            Self::Execute => "sp_execute",
            Self::PrepareExecute => "sp_prepexec",
            Self::Unprepare => "sp_unprepare",
        }
    }
}

impl Procedure {
    /// The system procedure this call names, by id or by name (in any case, bracketed or
    /// not, with or without a schema or database before it), or `None` for any other one.
    pub fn system_procedure(&self) -> Option<SystemProcedure> {
        match self {
            Self::Id(SP_EXECUTESQL) => Some(SystemProcedure::ExecuteSql),
            Self::Id(SP_PREPARE) => Some(SystemProcedure::Prepare),
            Self::Id(SP_EXECUTE) => Some(SystemProcedure::Execute),
            Self::Id(SP_PREPEXEC) => Some(SystemProcedure::PrepareExecute),
            Self::Id(SP_UNPREPARE) => Some(SystemProcedure::Unprepare),
            Self::Id(_) => None,
            Self::Name(name) => {
                let last = name.rsplit('.').next().unwrap_or(name);
                let bare = last
                    .trim_matches(|character| character == '[' || character == ']')
                    .to_ascii_lowercase();
                [
                    SystemProcedure::ExecuteSql,
                    SystemProcedure::Prepare,
                    SystemProcedure::Execute,
                    SystemProcedure::PrepareExecute,
                    SystemProcedure::Unprepare,
                ]
                .into_iter()
                .find(|procedure| procedure.name() == bare)
            }
        }
    }

    /// How errors and the log name the procedure.
    pub fn shown(&self) -> String {
        match (self.system_procedure(), self) {
            (Some(procedure), Self::Id(_)) => procedure.name().to_owned(),
            (_, Self::Name(name)) => name.clone(),
            (None, Self::Id(id)) => format!("system procedure {id}"),
        }
    }
}

/// One parameter of an RPC call.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcParameter {
    /// The parameter's name with its `@`, or empty when the client passes it by position.
    pub name: String,
    /// Whether the client asked for the parameter's value back: an output parameter.
    pub output: bool,
    /// Whether the client asked for the parameter's default value.
    pub uses_default: bool,
    /// The value, as the engine value of its TDS type: integers, `bit` as `boolean`,
    /// `decimal`, `numeric` and `money` as `numeric`, `datetime`, `smalldatetime` and
    /// `datetime2` as `timestamp`, `datetimeoffset` as `timestamptz` (its instant), the
    /// character types as `text` and the binary ones as `bytea`.
    pub value: Value,
    /// The parameter's TYPE_INFO and value exactly as sent, kept for output parameters so the
    /// server can send them back in a RETURNVALUE token.
    pub sent: Option<Vec<u8>>,
}

/// One call of an RPC request.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcedureCall {
    /// The procedure called.
    pub procedure: Procedure,
    /// The option flags (`fWithRecomp`, `fNoMetaData`, `fReuseMetaData`), accepted and ignored.
    pub option_flags: u16,
    /// The parameters, in the order sent.
    pub parameters: Vec<RpcParameter>,
}

/// An RPC request: its calls, run in order, and the transaction descriptor of its headers.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcRequest {
    /// The transaction descriptor the client sent, read but not enforced in this version.
    pub transaction_descriptor: u64,
    /// The calls to run. A call sent after a NoExec flag is read and left out.
    pub calls: Vec<ProcedureCall>,
}

struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Reader<'b> {
    fn take(&mut self, count: usize) -> Result<&'b [u8], RpcError> {
        let end = self
            .at
            .checked_add(count)
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(ends_early)?;
        let taken = self.bytes.get(self.at..end).ok_or_else(ends_early)?;
        self.at = end;
        Ok(taken)
    }

    fn byte(&mut self) -> Result<u8, RpcError> {
        self.take(1)?.first().copied().ok_or_else(ends_early)
    }

    fn u16(&mut self) -> Result<u16, RpcError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, RpcError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64, RpcError> {
        let bytes = self.take(8)?;
        let mut eight = [0u8; 8];
        eight.copy_from_slice(bytes);
        Ok(u64::from_le_bytes(eight))
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }
}

fn utf16_text(bytes: &[u8], reason: &'static str) -> Result<String, RpcError> {
    if !bytes.len().is_multiple_of(2) {
        return Err(malformed(reason));
    }
    let units = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair));
    char::decode_utf16(units)
        .collect::<Result<String, _>>()
        .map_err(|_| malformed(reason))
}

fn windows_1252_text(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&byte| match byte {
            0x80..=0x9F => WINDOWS_1252_HIGH[usize::from(byte - 0x80)],
            other => char::from(other),
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Utf16,
    Utf8,
    Windows1252,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Null,
    Integer { width: u8 },
    Bit,
    Float { width: u8 },
    Money,
    DateTime,
    Decimal { scale: u8 },
    Date,
    Time { scale: u8 },
    DateTime2 { scale: u8 },
    DateTimeOffset { scale: u8 },
    Guid,
    Text(Encoding),
    Binary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Length {
    Fixed(usize),
    Byte,
    UnsignedShort,
    Long,
    PartiallyLengthPrefixed,
}

fn width(reader: &mut Reader<'_>, allowed: &[u8]) -> Result<u8, RpcError> {
    let width = reader.byte()?;
    if allowed.contains(&width) {
        Ok(width)
    } else {
        Err(malformed(
            "a parameter's TYPE_INFO has a length its type does not take",
        ))
    }
}

fn scale(reader: &mut Reader<'_>) -> Result<u8, RpcError> {
    let scale = reader.byte()?;
    if scale > MAX_TIME_SCALE {
        return Err(malformed("a time parameter's scale is above 7"));
    }
    Ok(scale)
}

fn character_encoding(reader: &mut Reader<'_>, unicode: bool) -> Result<Encoding, RpcError> {
    let collation = reader.take(5)?;
    let info = u32::from_le_bytes([collation[0], collation[1], collation[2], collation[3]]);
    Ok(if unicode {
        Encoding::Utf16
    } else if info & UTF8_COLLATION != 0 {
        Encoding::Utf8
    } else {
        Encoding::Windows1252
    })
}

fn read_type_info(reader: &mut Reader<'_>) -> Result<(Shape, Length), RpcError> {
    let kind = reader.byte()?;
    Ok(match kind {
        0x1F => (Shape::Null, Length::Fixed(0)),
        0x30 => (Shape::Integer { width: 1 }, Length::Fixed(1)),
        0x34 => (Shape::Integer { width: 2 }, Length::Fixed(2)),
        0x38 => (Shape::Integer { width: 4 }, Length::Fixed(4)),
        0x7F => (Shape::Integer { width: 8 }, Length::Fixed(8)),
        0x32 => (Shape::Bit, Length::Fixed(1)),
        0x3B => (Shape::Float { width: 4 }, Length::Fixed(4)),
        0x3E => (Shape::Float { width: 8 }, Length::Fixed(8)),
        0x7A => (Shape::Money, Length::Fixed(4)),
        0x3C => (Shape::Money, Length::Fixed(8)),
        0x3A => (Shape::DateTime, Length::Fixed(4)),
        0x3D => (Shape::DateTime, Length::Fixed(8)),
        0x26 => (
            Shape::Integer {
                width: width(reader, &[1, 2, 4, 8])?,
            },
            Length::Byte,
        ),
        0x68 => {
            width(reader, &[1])?;
            (Shape::Bit, Length::Byte)
        }
        0x6D => (
            Shape::Float {
                width: width(reader, &[4, 8])?,
            },
            Length::Byte,
        ),
        0x6E => {
            width(reader, &[4, 8])?;
            (Shape::Money, Length::Byte)
        }
        0x6F => {
            width(reader, &[4, 8])?;
            (Shape::DateTime, Length::Byte)
        }
        0x6A | 0x6C | 0x37 | 0x3F => {
            width(reader, &[5, 9, 13, 17])?;
            let precision = reader.byte()?;
            let scale = reader.byte()?;
            if !(1..=MAX_DECIMAL_PRECISION).contains(&precision) || scale > precision {
                return Err(malformed(
                    "a decimal parameter's precision or scale is out of range",
                ));
            }
            (Shape::Decimal { scale }, Length::Byte)
        }
        0x24 => {
            width(reader, &[16])?;
            (Shape::Guid, Length::Byte)
        }
        0x28 => (Shape::Date, Length::Byte),
        0x29 => (
            Shape::Time {
                scale: scale(reader)?,
            },
            Length::Byte,
        ),
        0x2A => (
            Shape::DateTime2 {
                scale: scale(reader)?,
            },
            Length::Byte,
        ),
        0x2B => (
            Shape::DateTimeOffset {
                scale: scale(reader)?,
            },
            Length::Byte,
        ),
        0xE7 | 0xEF | 0xA7 | 0xAF => {
            let longest = reader.u16()?;
            let encoding = character_encoding(reader, matches!(kind, 0xE7 | 0xEF))?;
            let length = match (longest, kind) {
                (MAX_LENGTH, 0xE7 | 0xA7) => Length::PartiallyLengthPrefixed,
                (MAX_LENGTH, _) => {
                    return Err(malformed(
                        "a fixed-length character parameter has no length",
                    ));
                }
                _ => Length::UnsignedShort,
            };
            (Shape::Text(encoding), length)
        }
        0xA5 | 0xAD => {
            let longest = reader.u16()?;
            let length = match (longest, kind) {
                (MAX_LENGTH, 0xA5) => Length::PartiallyLengthPrefixed,
                (MAX_LENGTH, _) => {
                    return Err(malformed("a fixed-length binary parameter has no length"));
                }
                _ => Length::UnsignedShort,
            };
            (Shape::Binary, length)
        }
        0x63 | 0x23 => {
            reader.u32()?;
            let encoding = character_encoding(reader, kind == 0x63)?;
            (Shape::Text(encoding), Length::Long)
        }
        0x22 => {
            reader.u32()?;
            (Shape::Binary, Length::Long)
        }
        other => return Err(RpcError::UnsupportedType(other)),
    })
}

fn read_data(reader: &mut Reader<'_>, length: Length) -> Result<Option<Vec<u8>>, RpcError> {
    Ok(match length {
        Length::Fixed(0) => None,
        Length::Fixed(count) => Some(reader.take(count)?.to_vec()),
        Length::Byte => match reader.byte()? {
            0 => None,
            count => Some(reader.take(usize::from(count))?.to_vec()),
        },
        Length::UnsignedShort => match reader.u16()? {
            MAX_LENGTH => None,
            count => Some(reader.take(usize::from(count))?.to_vec()),
        },
        Length::Long => match reader.u32()? {
            NULL_LONG_LENGTH => None,
            count => Some(reader.take(count as usize)?.to_vec()),
        },
        Length::PartiallyLengthPrefixed => {
            let total = reader.u64()?;
            if total == PLP_NULL {
                return Ok(None);
            }
            let mut data = Vec::new();
            loop {
                let chunk = reader.u32()? as usize;
                if chunk == 0 {
                    break;
                }
                data.extend_from_slice(reader.take(chunk)?);
            }
            if total != PLP_UNKNOWN_LENGTH && total != data.len() as u64 {
                return Err(malformed(
                    "a PLP parameter's chunks do not add up to its length",
                ));
            }
            Some(data)
        }
    })
}

fn little_endian(bytes: &[u8]) -> u128 {
    bytes
        .iter()
        .rev()
        .fold(0u128, |number, &byte| (number << 8) | u128::from(byte))
}

fn signed(bytes: &[u8]) -> Result<i64, RpcError> {
    Ok(match bytes.len() {
        1 => i64::from(bytes[0]),
        2 => i64::from(i16::from_le_bytes([bytes[0], bytes[1]])),
        4 => i64::from(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
        8 => {
            let mut eight = [0u8; 8];
            eight.copy_from_slice(bytes);
            i64::from_le_bytes(eight)
        }
        _ => return Err(malformed("an integer parameter has an odd length")),
    })
}

fn integer_value(number: i64, width: u8) -> Result<Value, RpcError> {
    let out_of_range = || malformed("an integer parameter does not fit its type");
    Ok(match width {
        1 | 2 => Value::SmallInt(i16::try_from(number).map_err(|_| out_of_range())?),
        4 => Value::Integer(i32::try_from(number).map_err(|_| out_of_range())?),
        _ => Value::BigInt(number),
    })
}

fn numeric(negative: bool, units: u128, scale: u8) -> Result<Value, RpcError> {
    let digits = units.to_string();
    let scale = usize::from(scale);
    let padded = format!("{digits:0>width$}", width = scale + 1);
    let (whole, fraction) = padded.split_at(padded.len() - scale);
    let sign = if negative && units != 0 { "-" } else { "" };
    let text = if scale == 0 {
        format!("{sign}{whole}")
    } else {
        format!("{sign}{whole}.{fraction}")
    };
    Numeric::parse(&text)
        .map(Value::Numeric)
        .map_err(|_| malformed("a decimal parameter is not a number"))
}

fn time_micros(bytes: &[u8], scale: u8) -> Result<i64, RpcError> {
    if !(3..=5).contains(&bytes.len()) {
        return Err(malformed("a time parameter has an odd length"));
    }
    let units = little_endian(bytes) as u64;
    let micros = if scale >= MAX_TIME_SCALE {
        units / 10
    } else {
        units.saturating_mul(10u64.pow(6 - u32::from(scale)))
    };
    i64::try_from(micros)
        .ok()
        .filter(|micros| *micros < MICROS_PER_DAY)
        .ok_or_else(|| malformed("a time parameter is past midnight"))
}

fn days_since_2000(bytes: &[u8]) -> Result<i64, RpcError> {
    let days = little_endian(bytes) as i64;
    if days > LAST_DAY_FROM_YEAR_ONE {
        return Err(malformed("a date parameter is after 9999-12-31"));
    }
    Ok(days - DAYS_FROM_YEAR_ONE_TO_2000)
}

fn split_date_time(data: &[u8], tail: usize) -> Result<(&[u8], &[u8]), RpcError> {
    let time_length = data
        .len()
        .checked_sub(3 + tail)
        .ok_or_else(|| malformed("a date and time parameter is too short"))?;
    let time = data.get(..time_length).ok_or_else(ends_early)?;
    let date = data
        .get(time_length..time_length + 3)
        .ok_or_else(ends_early)?;
    Ok((time, date))
}

fn guid(data: &[u8]) -> Result<Value, RpcError> {
    let mut uuid = [0u8; 16];
    if data.len() != 16 {
        return Err(malformed("a uniqueidentifier parameter is not 16 bytes"));
    }
    uuid.copy_from_slice(data);
    uuid[..4].reverse();
    uuid[4..6].reverse();
    uuid[6..8].reverse();
    Ok(Value::Uuid(uuid))
}

fn decode(shape: Shape, data: Option<&[u8]>) -> Result<Value, RpcError> {
    let Some(data) = data else {
        return Ok(Value::Null);
    };
    Ok(match shape {
        Shape::Null => Value::Null,
        Shape::Integer { width } => integer_value(signed(data)?, width)?,
        Shape::Bit => match data {
            [flag] => Value::Boolean(*flag != 0),
            _ => return Err(malformed("a bit parameter is not one byte")),
        },
        Shape::Float { width } => match (width, data.len()) {
            (4, 4) => Value::Real(f32::from_le_bytes([data[0], data[1], data[2], data[3]])),
            (_, 4) => Value::Double(f64::from(f32::from_le_bytes([
                data[0], data[1], data[2], data[3],
            ]))),
            (8, 8) => {
                let mut eight = [0u8; 8];
                eight.copy_from_slice(data);
                Value::Double(f64::from_le_bytes(eight))
            }
            _ => return Err(malformed("a float parameter has an odd length")),
        },
        Shape::Money => {
            let units = match data.len() {
                8 => {
                    let high = i64::from(i32::from_le_bytes([data[0], data[1], data[2], data[3]]));
                    let low = i64::from(u32::from_le_bytes([data[4], data[5], data[6], data[7]]));
                    (high << 32) | low
                }
                4 => signed(data)?,
                _ => return Err(malformed("a money parameter has an odd length")),
            };
            numeric(units < 0, u128::from(units.unsigned_abs()), MONEY_SCALE)?
        }
        Shape::DateTime => match data.len() {
            8 => {
                let days = i64::from(i32::from_le_bytes([data[0], data[1], data[2], data[3]]));
                let ticks = i64::from(u32::from_le_bytes([data[4], data[5], data[6], data[7]]));
                if ticks >= DATETIME_TICKS_PER_DAY {
                    return Err(malformed("a datetime parameter is past midnight"));
                }
                if !(FIRST_DATETIME_DAY..=LAST_DATETIME_DAY).contains(&days) {
                    return Err(malformed("a datetime parameter is outside 1753 to 9999"));
                }
                let micros = (ticks * 10_000 + 1) / 3;
                Value::Timestamp((days - DAYS_FROM_1900_TO_2000) * MICROS_PER_DAY + micros)
            }
            4 => {
                let days = i64::from(u16::from_le_bytes([data[0], data[1]]));
                let minutes = i64::from(u16::from_le_bytes([data[2], data[3]]));
                if minutes >= MINUTES_PER_DAY {
                    return Err(malformed("a smalldatetime parameter is past midnight"));
                }
                Value::Timestamp(
                    (days - DAYS_FROM_1900_TO_2000) * MICROS_PER_DAY + minutes * MICROS_PER_MINUTE,
                )
            }
            _ => return Err(malformed("a datetime parameter has an odd length")),
        },
        Shape::Decimal { scale } => {
            let Some((&sign, magnitude)) = data.split_first() else {
                return Err(ends_early());
            };
            if magnitude.len() > 16 {
                return Err(malformed("a decimal parameter is longer than 17 bytes"));
            }
            numeric(sign == 0, little_endian(magnitude), scale)?
        }
        Shape::Date => {
            if data.len() != 3 {
                return Err(malformed("a date parameter is not 3 bytes"));
            }
            let days = days_since_2000(data)?;
            Value::Date(
                i32::try_from(days).map_err(|_| malformed("a date parameter is out of range"))?,
            )
        }
        Shape::Time { scale } => Value::Time(time_micros(data, scale)?),
        Shape::DateTime2 { scale } => {
            let (time, date) = split_date_time(data, 0)?;
            Value::Timestamp(days_since_2000(date)? * MICROS_PER_DAY + time_micros(time, scale)?)
        }
        Shape::DateTimeOffset { scale } => {
            let (time, date) = split_date_time(data, 2)?;
            let offset = data
                .get(data.len() - 2..)
                .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                .ok_or_else(ends_early)?;
            if !(-MAX_OFFSET_MINUTES..=MAX_OFFSET_MINUTES).contains(&offset) {
                return Err(malformed(
                    "a datetimeoffset parameter's offset is out of range",
                ));
            }
            Value::TimestampTz(days_since_2000(date)? * MICROS_PER_DAY + time_micros(time, scale)?)
        }
        Shape::Guid => guid(data)?,
        Shape::Text(Encoding::Utf16) => Value::Text(utf16_text(
            data,
            "a character parameter is not valid UTF-16",
        )?),
        Shape::Text(Encoding::Utf8) => Value::Text(
            String::from_utf8(data.to_vec())
                .map_err(|_| malformed("a character parameter is not valid UTF-8"))?,
        ),
        Shape::Text(Encoding::Windows1252) => Value::Text(windows_1252_text(data)),
        Shape::Binary => Value::Bytea(data.to_vec()),
    })
}

fn read_parameter(reader: &mut Reader<'_>) -> Result<RpcParameter, RpcError> {
    let name_units = usize::from(reader.byte()?);
    let name = utf16_text(
        reader.take(name_units * 2)?,
        "a parameter name is not valid UTF-16",
    )?;
    let status = reader.byte()?;
    if status & ENCRYPTED != 0 {
        return Err(RpcError::EncryptedParameter);
    }
    let type_start = reader.at;
    let (shape, length) = read_type_info(reader)?;
    let data = read_data(reader, length)?;
    let output = status & BY_REFERENCE != 0;
    let sent = if output {
        reader.bytes.get(type_start..reader.at).map(<[u8]>::to_vec)
    } else {
        None
    };
    Ok(RpcParameter {
        name,
        output,
        uses_default: status & DEFAULT_VALUE != 0,
        value: decode(shape, data.as_deref())?,
        sent,
    })
}

fn read_call(reader: &mut Reader<'_>) -> Result<ProcedureCall, RpcError> {
    let length = reader.u16()?;
    let procedure = if length == PROCEDURE_ID {
        Procedure::Id(reader.u16()?)
    } else {
        let units = usize::from(length);
        if units == 0 || units > MAX_PROCEDURE_NAME_UNITS {
            return Err(malformed("an RPC procedure name is empty or too long"));
        }
        Procedure::Name(utf16_text(
            reader.take(units * 2)?,
            "an RPC procedure name is not valid UTF-16",
        )?)
    };
    let option_flags = reader.u16()?;
    let mut parameters = Vec::new();
    while let Some(next) = reader.peek() {
        if next == BATCH_FLAG || next == NO_EXEC_FLAG {
            break;
        }
        if parameters.len() == MAX_PARAMETERS {
            return Err(RpcError::TooManyParameters);
        }
        parameters.push(read_parameter(reader)?);
    }
    Ok(ProcedureCall {
        procedure,
        option_flags,
        parameters,
    })
}

/// Reads an RPC request's body ([MS-TDS] 2.2.6.6): its ALL_HEADERS, as
/// [`crate::messages::parse_sql_batch`] reads them, then calls separated by the batch flag
/// (0xFF) or the NoExec flag (0xFE, whose next call is read and not run), a trailing flag
/// allowed. Each call names its procedure by a UTF-16 name or by 0xFFFF and a system procedure
/// id, then its option flags and its parameters, each a name, a status byte, a TYPE_INFO and
/// a value.
pub fn parse_rpc_request(body: &[u8]) -> Result<RpcRequest, RpcError> {
    let (transaction_descriptor, start) = read_all_headers(body).map_err(RpcError::Malformed)?;
    let mut reader = Reader {
        bytes: body,
        at: start,
    };
    let mut calls = Vec::new();
    let mut runs = true;
    loop {
        let call = read_call(&mut reader)?;
        if runs {
            calls.push(call);
        }
        match reader.peek() {
            None => break,
            Some(flag) => {
                reader.at += 1;
                runs = flag == BATCH_FLAG;
            }
        }
        if reader.peek().is_none() {
            break;
        }
    }
    Ok(RpcRequest {
        transaction_descriptor,
        calls,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        Procedure, RpcError, SP_EXECUTE, SP_EXECUTESQL, SystemProcedure, parse_rpc_request,
    };
    use eggdb_sql::{Numeric, Value};

    fn utf16(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn headers() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&22u32.to_le_bytes());
        body.extend_from_slice(&18u32.to_le_bytes());
        body.extend_from_slice(&2u16.to_le_bytes());
        body.extend_from_slice(&9u64.to_le_bytes());
        body.extend_from_slice(&1u32.to_le_bytes());
        body
    }

    fn parameter(name: &str, status: u8, type_and_value: &[u8]) -> Vec<u8> {
        let mut out = vec![name.encode_utf16().count() as u8];
        out.extend(utf16(name));
        out.push(status);
        out.extend_from_slice(type_and_value);
        out
    }

    fn nvarchar(text: &str) -> Vec<u8> {
        let mut out = vec![0xE7, 0x40, 0x1F, 0x09, 0x04, 0xD0, 0x00, 0x34];
        let bytes = utf16(text);
        out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        out.extend(bytes);
        out
    }

    fn call_by_id(id: u16, parameters: &[Vec<u8>]) -> Vec<u8> {
        let mut out = vec![0xFF, 0xFF];
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        for parameter in parameters {
            out.extend_from_slice(parameter);
        }
        out
    }

    fn sample() -> Vec<u8> {
        let mut plp = vec![0xE7, 0xFF, 0xFF, 0x09, 0x04, 0xD0, 0x00, 0x34];
        let long = utf16("long");
        plp.extend_from_slice(&(long.len() as u64).to_le_bytes());
        plp.extend_from_slice(&3u32.to_le_bytes());
        plp.extend_from_slice(&long[..3]);
        plp.extend_from_slice(&5u32.to_le_bytes());
        plp.extend_from_slice(&long[3..]);
        plp.extend_from_slice(&0u32.to_le_bytes());
        let mut body = headers();
        body.extend(call_by_id(
            SP_EXECUTESQL,
            &[
                parameter("", 0, &nvarchar("SELECT @p1")),
                parameter("", 0, &nvarchar("@p1 int")),
                parameter("@p1", 1, &[0x26, 4, 4, 0xFE, 0xFF, 0xFF, 0xFF]),
                parameter("@p2", 0, &[0x6A, 5, 5, 2, 0, 0xE2, 0x04, 0, 0]),
                parameter("@p3", 0, &[0x26, 8, 0]),
                parameter("@p4", 0, &plp),
                parameter("@p5", 0, &[0x6F, 8, 8, 0xAC, 0x8E, 0, 0, 0x2C, 0x01, 0, 0]),
                parameter(
                    "@p6",
                    0,
                    &[
                        0x24, 16, 16, 0xFF, 0x19, 0x96, 0x6F, 0x86, 0x8B, 0x11, 0xD0, 0xB4, 0x2D,
                        0x00, 0xC0, 0x4F, 0xC9, 0x64, 0xFF,
                    ],
                ),
                parameter("@p7", 0, &[0x2A, 3, 7, 0, 0, 0, 0, 0x07, 0x24, 0x0B]),
                parameter(
                    "@p8",
                    0,
                    &[0xA7, 10, 0, 0x09, 0x04, 0xD0, 0x00, 0x34, 2, 0, 0x80, b'a'],
                ),
                parameter("@p9", 0, &[0x6E, 8, 8, 0, 0, 0, 0, 0x10, 0x27, 0, 0]),
            ],
        ));
        body.push(0xFF);
        body.extend(call_by_id(
            SP_EXECUTE,
            &[parameter("", 0, &[0x38, 3, 0, 0, 0])],
        ));
        body.push(0xFF);
        body
    }

    #[test]
    fn calls_parameters_and_their_types_read_as_engine_values() {
        let request = parse_rpc_request(&sample()).unwrap();
        assert_eq!(request.transaction_descriptor, 9);
        assert_eq!(request.calls.len(), 2);
        let call = &request.calls[0];
        assert_eq!(call.procedure, Procedure::Id(SP_EXECUTESQL));
        assert_eq!(
            call.procedure.system_procedure(),
            Some(SystemProcedure::ExecuteSql)
        );
        let values: Vec<&Value> = call
            .parameters
            .iter()
            .map(|parameter| &parameter.value)
            .collect();
        assert_eq!(values[0], &Value::Text("SELECT @p1".into()));
        assert_eq!(values[2], &Value::Integer(-2));
        assert_eq!(
            values[3],
            &Value::Numeric(Numeric::parse("-12.50").unwrap())
        );
        assert_eq!(values[4], &Value::Null);
        assert_eq!(values[5], &Value::Text("long".into()));
        assert_eq!(values[6], &Value::Timestamp(1_000_000));
        assert_eq!(
            values[7],
            &Value::Uuid([
                0x6F, 0x96, 0x19, 0xFF, 0x8B, 0x86, 0xD0, 0x11, 0xB4, 0x2D, 0x00, 0xC0, 0x4F, 0xC9,
                0x64, 0xFF
            ])
        );
        assert_eq!(values[8], &Value::Timestamp(0));
        assert_eq!(values[9], &Value::Text("\u{20AC}a".into()));
        assert_eq!(
            values[10],
            &Value::Numeric(Numeric::parse("1.0000").unwrap())
        );
        assert!(call.parameters[2].output);
        assert_eq!(
            call.parameters[2].sent.as_deref(),
            Some(&[0x26, 4, 4, 0xFE, 0xFF, 0xFF, 0xFF][..])
        );
        assert!(call.parameters[3].sent.is_none());
        assert_eq!(call.parameters[2].name, "@p1");
        assert_eq!(request.calls[1].parameters[0].value, Value::Integer(3));
    }

    #[test]
    fn names_of_system_procedures_are_found_in_any_spelling() {
        for name in [
            "sp_executesql",
            "SP_EXECUTESQL",
            "sys.sp_executesql",
            "[master]..[sp_executesql]",
        ] {
            assert_eq!(
                Procedure::Name(name.into()).system_procedure(),
                Some(SystemProcedure::ExecuteSql),
                "{name}"
            );
        }
        assert_eq!(
            Procedure::Name("dbo.orders_by_day".into()).system_procedure(),
            None
        );
        assert_eq!(Procedure::Id(2).system_procedure(), None);
        assert_eq!(Procedure::Id(SP_EXECUTE).shown(), "sp_execute");
    }

    #[test]
    fn unsupported_types_encryption_and_truncation_are_errors_never_panics() {
        let mut variant = headers();
        variant.extend(call_by_id(
            SP_EXECUTESQL,
            &[parameter("", 0, &[0x62, 0, 0, 0, 0])],
        ));
        assert_eq!(
            parse_rpc_request(&variant),
            Err(RpcError::UnsupportedType(0x62))
        );
        let mut encrypted = headers();
        encrypted.extend(call_by_id(
            SP_EXECUTESQL,
            &[parameter("", 0x08, &[0x26, 4, 0])],
        ));
        assert_eq!(
            parse_rpc_request(&encrypted),
            Err(RpcError::EncryptedParameter)
        );
        let mut bad_width = headers();
        bad_width.extend(call_by_id(
            SP_EXECUTESQL,
            &[parameter("", 0, &[0x26, 3, 0])],
        ));
        assert!(matches!(
            parse_rpc_request(&bad_width),
            Err(RpcError::Malformed(_))
        ));
        let mut late_time = headers();
        late_time.extend(call_by_id(
            SP_EXECUTESQL,
            &[parameter("", 0, &[0x29, 0, 3, 0x80, 0x51, 0x01])],
        ));
        assert!(matches!(
            parse_rpc_request(&late_time),
            Err(RpcError::Malformed(_))
        ));
        let whole = sample();
        for cut in 0..whole.len() {
            let _ = parse_rpc_request(&whole[..cut]);
        }
        let mut no_exec = headers();
        no_exec.extend(call_by_id(SP_EXECUTE, &[]));
        no_exec.push(0xFE);
        no_exec.extend(call_by_id(SP_EXECUTE, &[]));
        assert_eq!(parse_rpc_request(&no_exec).unwrap().calls.len(), 1);
    }
}
