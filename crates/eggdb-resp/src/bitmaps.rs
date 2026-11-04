use crate::commands::{Context, NOT_INTEGER, SYNTAX, WRONG_TYPE, integer, load, reading, save, writing};
use crate::protocol::Reply;
use crate::value::{Data, Entry};

const NAMES: &[&str] = &[
    "setbit",
    "getbit",
    "bitcount",
    "bitpos",
    "bitop",
    "bitfield",
    "bitfield_ro",
];

const BIT_OFFSET: &str = "ERR bit offset is not an integer or out of range";
const BIT_VALUE: &str = "ERR bit is not an integer or out of range";
const BIT_ARGUMENT: &str = "ERR The bit argument must be 1 or 0.";
const BITFIELD_TYPE: &str = "ERR Invalid bitfield type. Use something like i16 u8. Note that u64 is not supported but i64 is.";
const OVERFLOW_TYPE: &str = "ERR Invalid OVERFLOW type specified";
const READ_ONLY_FIELDS: &str = "ERR BITFIELD_RO only supports the GET subcommand";
const NOT_SINGLE_SOURCE: &str = "ERR BITOP NOT must be called with a single source key.";

const MAX_STRING_BITS: u64 = 512 * 1024 * 1024 * 8;

pub(crate) fn is_known(name: &str) -> bool {
    NAMES.contains(&name)
}

pub(crate) fn execute(
    context: &Context<'_>,
    db: u32,
    name: &str,
    args: &[Vec<u8>],
) -> Option<Reply> {
    Some(match (name, args.len()) {
        ("setbit", 3) => setbit(context, db, args),
        ("getbit", 2) => getbit(context, db, args),
        ("bitcount", count) if count >= 1 => bitcount(context, db, args),
        ("bitpos", count) if count >= 2 => bitpos(context, db, args),
        ("bitop", count) if count >= 3 => bitop(context, db, args),
        ("bitfield", count) if count >= 1 => bitfield(context, db, args, false),
        ("bitfield_ro", count) if count >= 1 => bitfield(context, db, args, true),
        _ => return None,
    })
}

fn owned_string(entry: Option<Entry>) -> Result<(Vec<u8>, Option<u64>), String> {
    match entry {
        None => Ok((Vec::new(), None)),
        Some(Entry {
            data: Data::String(bytes),
            expires_at_ms,
        }) => Ok((bytes, expires_at_ms)),
        Some(_) => Err(WRONG_TYPE.to_owned()),
    }
}

fn borrowed_string(entry: Option<&Entry>) -> Result<Option<&[u8]>, String> {
    match entry.map(|entry| &entry.data) {
        None => Ok(None),
        Some(Data::String(bytes)) => Ok(Some(bytes.as_slice())),
        Some(_) => Err(WRONG_TYPE.to_owned()),
    }
}

fn bit_offset(bytes: &[u8]) -> Result<u64, String> {
    match integer(bytes) {
        Ok(offset) if offset >= 0 && (offset as u64) < MAX_STRING_BITS => Ok(offset as u64),
        _ => Err(BIT_OFFSET.to_owned()),
    }
}

fn byte_index(offset: u64) -> Option<usize> {
    usize::try_from(offset / 8).ok()
}

fn bit_mask(offset: u64) -> u8 {
    0x80 >> (offset % 8)
}

fn bit_at(value: &[u8], offset: u64) -> bool {
    byte_index(offset)
        .and_then(|index| value.get(index))
        .is_some_and(|byte| byte & bit_mask(offset) != 0)
}

fn set_bit(value: &mut [u8], offset: u64, on: bool) {
    if let Some(byte) = byte_index(offset).and_then(|index| value.get_mut(index)) {
        if on {
            *byte |= bit_mask(offset);
        } else {
            *byte &= !bit_mask(offset);
        }
    }
}

fn grow_to_hold_bit(context: &Context<'_>, value: &mut Vec<u8>, offset: u64) -> Result<(), String> {
    let length = byte_index(offset)
        .and_then(|index| index.checked_add(1))
        .ok_or_else(|| BIT_OFFSET.to_owned())?;
    if length > value.len() {
        context.check_value_room(length)?;
        value.resize(length, 0);
    }
    Ok(())
}

fn setbit(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    let offset = match bit_offset(&args[1]) {
        Ok(offset) => offset,
        Err(error) => return Reply::Error(error),
    };
    let on = match integer(&args[2]) {
        Ok(0) => false,
        Ok(1) => true,
        _ => return Reply::Error(BIT_VALUE.to_owned()),
    };
    writing(context, db, |keys| {
        let (mut value, expires_at_ms) = owned_string(load(keys, &args[0], now)?)?;
        grow_to_hold_bit(context, &mut value, offset)?;
        let was = bit_at(&value, offset);
        set_bit(&mut value, offset, on);
        save(
            keys,
            &args[0],
            &Entry {
                data: Data::String(value),
                expires_at_ms,
            },
        )?;
        Ok(Reply::Integer(i64::from(was)))
    })
}

fn getbit(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    let offset = match bit_offset(&args[1]) {
        Ok(offset) => offset,
        Err(error) => return Reply::Error(error),
    };
    reading(context, db, |keys| {
        let entry = load(keys, &args[0], now)?;
        let value = borrowed_string(entry.as_ref())?.unwrap_or(&[]);
        Ok(Reply::Integer(i64::from(bit_at(value, offset))))
    })
}

fn counts_bits(unit: &[u8]) -> Result<bool, String> {
    match String::from_utf8_lossy(unit).to_ascii_lowercase().as_str() {
        "byte" => Ok(false),
        "bit" => Ok(true),
        _ => Err(SYNTAX.to_owned()),
    }
}

fn from_the_end(total: i64, index: i64) -> i64 {
    if index < 0 {
        total.saturating_add(index)
    } else {
        index
    }
}

fn clamped_range(total: i64, start: i64, end: i64) -> Option<(u64, u64)> {
    if start < 0 && end < 0 && start > end {
        return None;
    }
    let start = from_the_end(total, start).max(0);
    let end = from_the_end(total, end).max(0).min(total.saturating_sub(1));
    (start <= end).then_some((start as u64, end as u64))
}

struct BitRange {
    start: i64,
    end: Option<i64>,
    in_bits: bool,
}

impl BitRange {
    fn parse(args: &[Vec<u8>]) -> Result<Self, String> {
        let number = |at: usize| -> Result<Option<i64>, String> {
            args.get(at).map(|bytes| integer(bytes)).transpose()
        };
        let start = number(0)?.unwrap_or(0);
        let end = number(1)?;
        let in_bits = match args.get(2) {
            Some(unit) => counts_bits(unit)?,
            None => false,
        };
        Ok(Self {
            start,
            end,
            in_bits,
        })
    }

    fn bits_of(&self, length: usize) -> Option<(u64, u64)> {
        let length = length as i64;
        let total = if self.in_bits {
            length.saturating_mul(8)
        } else {
            length
        };
        let (from, to) = clamped_range(total, self.start, self.end.unwrap_or(-1))?;
        Some(if self.in_bits {
            (from, to)
        } else {
            (from * 8, to * 8 + 7)
        })
    }
}

fn byte_mask(first_bit: u64, last_bit: u64) -> u8 {
    (0xFF_u8 >> first_bit) & (0xFF_u8 << (7 - last_bit))
}

fn ones_between(value: &[u8], from: u64, to: u64) -> u64 {
    let (Some(first), Some(last)) = (byte_index(from), byte_index(to)) else {
        return 0;
    };
    let byte = |index: usize| value.get(index).copied().unwrap_or(0);
    if first == last {
        return u64::from((byte(first) & byte_mask(from % 8, to % 8)).count_ones());
    }
    let edges = (byte(first) & byte_mask(from % 8, 7)).count_ones()
        + (byte(last) & byte_mask(0, to % 8)).count_ones();
    let middle: u64 = value
        .get(first + 1..last)
        .unwrap_or(&[])
        .iter()
        .map(|byte| u64::from(byte.count_ones()))
        .sum();
    u64::from(edges) + middle
}

fn bitcount(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    if !matches!(args.len(), 1 | 3 | 4) {
        return Reply::Error(SYNTAX.to_owned());
    }
    let range = match BitRange::parse(&args[1..]) {
        Ok(range) => range,
        Err(error) => return Reply::Error(error),
    };
    reading(context, db, |keys| {
        let entry = load(keys, &args[0], now)?;
        let value = borrowed_string(entry.as_ref())?.unwrap_or(&[]);
        let count = match range.bits_of(value.len()) {
            Some((from, to)) => ones_between(value, from, to),
            None => 0,
        };
        Ok(Reply::Integer(count as i64))
    })
}

fn first_bit(value: &[u8], from: u64, to: u64, wanted: bool) -> Option<u64> {
    let skipped = if wanted { 0x00 } else { 0xFF };
    let mut at = from;
    while at <= to {
        if at.is_multiple_of(8) && at + 7 <= to {
            let byte = byte_index(at)
                .and_then(|index| value.get(index))
                .copied()
                .unwrap_or(0);
            if byte == skipped {
                at += 8;
                continue;
            }
        }
        if bit_at(value, at) == wanted {
            return Some(at);
        }
        at += 1;
    }
    None
}

fn bitpos(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    if args.len() > 5 {
        return Reply::Error(SYNTAX.to_owned());
    }
    let wanted = match integer(&args[1]) {
        Ok(0) => false,
        Ok(1) => true,
        Ok(_) => return Reply::Error(BIT_ARGUMENT.to_owned()),
        Err(error) => return Reply::Error(error),
    };
    let range = match BitRange::parse(&args[2..]) {
        Ok(range) => range,
        Err(error) => return Reply::Error(error),
    };
    reading(context, db, |keys| {
        let entry = load(keys, &args[0], now)?;
        let Some(value) = borrowed_string(entry.as_ref())? else {
            return Ok(Reply::Integer(if wanted { -1 } else { 0 }));
        };
        let Some((from, to)) = range.bits_of(value.len()) else {
            return Ok(Reply::Integer(-1));
        };
        Ok(Reply::Integer(match first_bit(value, from, to, wanted) {
            Some(position) => position as i64,
            None if !wanted && range.end.is_none() => (to + 1) as i64,
            None => -1,
        }))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BitOperation {
    And,
    Or,
    Xor,
    Not,
    Diff,
    Diff1,
    AndOr,
    One,
}

impl BitOperation {
    fn parse(word: &[u8]) -> Option<Self> {
        Some(
            match String::from_utf8_lossy(word).to_ascii_lowercase().as_str() {
                "and" => Self::And,
                "or" => Self::Or,
                "xor" => Self::Xor,
                "not" => Self::Not,
                "diff" => Self::Diff,
                "diff1" => Self::Diff1,
                "andor" => Self::AndOr,
                "one" => Self::One,
                _ => return None,
            },
        )
    }

    fn needs_two_sources(self) -> Option<&'static str> {
        match self {
            Self::Diff => Some("DIFF"),
            Self::Diff1 => Some("DIFF1"),
            Self::AndOr => Some("ANDOR"),
            _ => None,
        }
    }

    fn combine(self, bytes: &[u8]) -> u8 {
        let first = bytes.first().copied().unwrap_or(0);
        let rest = bytes.get(1..).unwrap_or(&[]);
        let any_of_rest = rest.iter().fold(0u8, |all, byte| all | byte);
        match self {
            Self::And => bytes.iter().fold(0xFF_u8, |all, byte| all & byte),
            Self::Or => first | any_of_rest,
            Self::Xor => bytes.iter().fold(0u8, |all, byte| all ^ byte),
            Self::Not => !first,
            Self::Diff => first & !any_of_rest,
            Self::Diff1 => !first & any_of_rest,
            Self::AndOr => first & any_of_rest,
            Self::One => {
                let (mut once, mut more) = (0u8, 0u8);
                for byte in bytes {
                    more |= once & byte;
                    once |= byte;
                }
                once & !more
            }
        }
    }
}

fn bitop(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    let Some(operation) = BitOperation::parse(&args[0]) else {
        return Reply::Error(SYNTAX.to_owned());
    };
    let (destination, sources) = (&args[1], &args[2..]);
    if operation == BitOperation::Not && sources.len() != 1 {
        return Reply::Error(NOT_SINGLE_SOURCE.to_owned());
    }
    if let Some(word) = operation.needs_two_sources()
        && sources.len() < 2
    {
        return Reply::Error(format!(
            "ERR BITOP {word} must be called with at least two source keys."
        ));
    }
    writing(context, db, |keys| {
        let mut values = Vec::with_capacity(sources.len());
        for key in sources {
            values.push(owned_string(load(keys, key, now)?)?.0);
        }
        let length = values.iter().map(Vec::len).max().unwrap_or(0);
        if length == 0 {
            keys.delete(destination)?;
            return Ok(Reply::Integer(0));
        }
        context.check_value_room(length)?;
        let mut column = vec![0u8; values.len()];
        let mut result = Vec::with_capacity(length);
        for index in 0..length {
            for (slot, value) in column.iter_mut().zip(&values) {
                *slot = value.get(index).copied().unwrap_or(0);
            }
            result.push(operation.combine(&column));
        }
        save(
            keys,
            destination,
            &Entry {
                data: Data::String(result),
                expires_at_ms: None,
            },
        )?;
        Ok(Reply::Integer(length as i64))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overflow {
    Wrap,
    Saturate,
    Fail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Field {
    signed: bool,
    bits: u32,
    offset: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldOperation {
    Get(Field),
    Set(Field, i64, Overflow),
    IncrBy(Field, i64, Overflow),
}

impl Field {
    fn parse(kind: &[u8], offset: &[u8]) -> Result<Self, String> {
        let (signed, digits) = match kind.split_first() {
            Some((b'i', digits)) => (true, digits),
            Some((b'u', digits)) => (false, digits),
            _ => return Err(BITFIELD_TYPE.to_owned()),
        };
        let widest = if signed { 64 } else { 63 };
        let bits = std::str::from_utf8(digits)
            .ok()
            .and_then(|text| text.parse::<u32>().ok())
            .filter(|bits| (1..=widest).contains(bits))
            .ok_or_else(|| BITFIELD_TYPE.to_owned())?;
        let (in_fields, number) = match offset.strip_prefix(b"#") {
            Some(number) => (true, number),
            None => (false, offset),
        };
        let index = integer(number)
            .ok()
            .and_then(|index| u64::try_from(index).ok())
            .ok_or_else(|| BIT_OFFSET.to_owned())?;
        let offset = if in_fields {
            index.checked_mul(u64::from(bits))
        } else {
            Some(index)
        }
        .filter(|offset| *offset < MAX_STRING_BITS)
        .ok_or_else(|| BIT_OFFSET.to_owned())?;
        Ok(Self {
            signed,
            bits,
            offset,
        })
    }

    fn last_bit(self) -> u64 {
        self.offset + u64::from(self.bits) - 1
    }

    fn read(self, value: &[u8]) -> i64 {
        let mut raw = 0u64;
        for bit in 0..u64::from(self.bits) {
            raw = (raw << 1) | u64::from(bit_at(value, self.offset + bit));
        }
        if self.signed && self.bits < 64 {
            let shift = 64 - self.bits;
            ((raw << shift) as i64) >> shift
        } else {
            raw as i64
        }
    }

    fn write(self, value: &mut [u8], field: i128) {
        let raw = field as u64;
        let bits = u64::from(self.bits);
        for bit in 0..bits {
            set_bit(value, self.offset + bit, (raw >> (bits - 1 - bit)) & 1 == 1);
        }
    }

    fn fitted(self, wanted: i128, overflow: Overflow) -> Option<i128> {
        let span = 1_i128 << self.bits;
        let (lowest, highest) = if self.signed {
            (-(span / 2), span / 2 - 1)
        } else {
            (0, span - 1)
        };
        if (lowest..=highest).contains(&wanted) {
            return Some(wanted);
        }
        match overflow {
            Overflow::Fail => None,
            Overflow::Saturate => Some(wanted.clamp(lowest, highest)),
            Overflow::Wrap => Some((wanted - lowest).rem_euclid(span) + lowest),
        }
    }
}

fn parse_overflow(word: &[u8]) -> Result<Overflow, String> {
    match String::from_utf8_lossy(word).to_ascii_lowercase().as_str() {
        "wrap" => Ok(Overflow::Wrap),
        "sat" => Ok(Overflow::Saturate),
        "fail" => Ok(Overflow::Fail),
        _ => Err(OVERFLOW_TYPE.to_owned()),
    }
}

fn parse_field_operations(args: &[Vec<u8>], read_only: bool) -> Result<Vec<FieldOperation>, String> {
    let mut overflow = Overflow::Wrap;
    let mut operations = Vec::new();
    let mut at = 0;
    while let Some(word) = args.get(at) {
        let rest = &args[at + 1..];
        match String::from_utf8_lossy(word).to_ascii_lowercase().as_str() {
            "get" if rest.len() >= 2 => {
                operations.push(FieldOperation::Get(Field::parse(&rest[0], &rest[1])?));
                at += 3;
            }
            subcommand @ ("set" | "incrby") if rest.len() >= 3 => {
                let field = Field::parse(&rest[0], &rest[1])?;
                let number = integer(&rest[2]).map_err(|_| NOT_INTEGER.to_owned())?;
                if read_only {
                    return Err(READ_ONLY_FIELDS.to_owned());
                }
                operations.push(if subcommand == "set" {
                    FieldOperation::Set(field, number, overflow)
                } else {
                    FieldOperation::IncrBy(field, number, overflow)
                });
                at += 4;
            }
            "overflow" if !rest.is_empty() => {
                overflow = parse_overflow(&rest[0])?;
                at += 2;
            }
            _ => return Err(SYNTAX.to_owned()),
        }
    }
    Ok(operations)
}

fn apply_field_operation(value: &mut [u8], operation: FieldOperation) -> Reply {
    match operation {
        FieldOperation::Get(field) => Reply::Integer(field.read(value)),
        FieldOperation::Set(field, number, overflow) => {
            let old = field.read(value);
            let wanted = if field.signed {
                i128::from(number)
            } else {
                i128::from(number as u64)
            };
            match field.fitted(wanted, overflow) {
                Some(stored) => {
                    field.write(value, stored);
                    Reply::Integer(old)
                }
                None => Reply::null(),
            }
        }
        FieldOperation::IncrBy(field, by, overflow) => {
            let wanted = i128::from(field.read(value)) + i128::from(by);
            match field.fitted(wanted, overflow) {
                Some(stored) => {
                    field.write(value, stored);
                    Reply::Integer(stored as i64)
                }
                None => Reply::null(),
            }
        }
    }
}

fn bitfield(context: &Context<'_>, db: u32, args: &[Vec<u8>], read_only: bool) -> Reply {
    let now = context.now_ms;
    let key = &args[0];
    let operations = match parse_field_operations(&args[1..], read_only) {
        Ok(operations) => operations,
        Err(error) => return Reply::Error(error),
    };
    let last_written_bit = operations
        .iter()
        .filter_map(|operation| match operation {
            FieldOperation::Get(_) => None,
            FieldOperation::Set(field, ..) | FieldOperation::IncrBy(field, ..) => {
                Some(field.last_bit())
            }
        })
        .max();
    let Some(last_written_bit) = last_written_bit else {
        return reading(context, db, |keys| {
            let entry = load(keys, key, now)?;
            let value = borrowed_string(entry.as_ref())?.unwrap_or(&[]);
            Ok(Reply::Array(Some(
                operations
                    .iter()
                    .map(|operation| match operation {
                        FieldOperation::Get(field) => Reply::Integer(field.read(value)),
                        _ => Reply::null(),
                    })
                    .collect(),
            )))
        });
    };
    writing(context, db, |keys| {
        let (mut value, expires_at_ms) = owned_string(load(keys, key, now)?)?;
        grow_to_hold_bit(context, &mut value, last_written_bit)?;
        let replies = operations
            .iter()
            .map(|operation| apply_field_operation(&mut value, *operation))
            .collect();
        save(
            keys,
            key,
            &Entry {
                data: Data::String(value),
                expires_at_ms,
            },
        )?;
        Ok(Reply::Array(Some(replies)))
    })
}
