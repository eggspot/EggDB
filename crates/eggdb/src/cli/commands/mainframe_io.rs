use super::Session;
use crate::cli::args::CommandName;
use crate::cli::failure::{Exit, Failure};
use crate::cli::render;
use eggdb_parquet::{Cell, Column, ColumnType};
use std::path::Path;

const MAX_RECORDS_BYTES: u64 = 1 << 30;
const MAX_COPYBOOK_BYTES: u64 = 4 << 20;
const MAX_VALUES: usize = 100_000_000;
const MAX_PICTURE_SYMBOLS: usize = 100_000;
const MAX_DIGITS: u32 = 38;
const MAX_INTEGER_DIGITS: u32 = 18;
const MAX_COLUMNS: usize = 1600;
const MAX_OCCURS: usize = 100_000;
const SEQUENCE_AREA: usize = 6;
const INDICATOR_COLUMN: usize = 6;
const PROGRAM_AREA_END: usize = 72;
const CONDITION_LEVEL: u32 = 88;
const RENAMES_LEVEL: u32 = 66;
const RECORD_DESCRIPTOR_LENGTH: usize = 4;
const EBCDIC_SPACE: u8 = 0x40;
const EBCDIC_PLUS: u8 = 0x4E;
const EBCDIC_MINUS: u8 = 0x60;
const EBCDIC_ZERO: u8 = 0xF0;
const CP037: [u8; 256] = [
    0x00, 0x01, 0x02, 0x03, 0x9C, 0x09, 0x86, 0x7F, 0x97, 0x8D, 0x8E, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
    0x10, 0x11, 0x12, 0x13, 0x9D, 0x85, 0x08, 0x87, 0x18, 0x19, 0x92, 0x8F, 0x1C, 0x1D, 0x1E, 0x1F,
    0x80, 0x81, 0x82, 0x83, 0x84, 0x0A, 0x17, 0x1B, 0x88, 0x89, 0x8A, 0x8B, 0x8C, 0x05, 0x06, 0x07,
    0x90, 0x91, 0x16, 0x93, 0x94, 0x95, 0x96, 0x04, 0x98, 0x99, 0x9A, 0x9B, 0x14, 0x15, 0x9E, 0x1A,
    0x20, 0xA0, 0xE2, 0xE4, 0xE0, 0xE1, 0xE3, 0xE5, 0xE7, 0xF1, 0xA2, 0x2E, 0x3C, 0x28, 0x2B, 0x7C,
    0x26, 0xE9, 0xEA, 0xEB, 0xE8, 0xED, 0xEE, 0xEF, 0xEC, 0xDF, 0x21, 0x24, 0x2A, 0x29, 0x3B, 0xAC,
    0x2D, 0x2F, 0xC2, 0xC4, 0xC0, 0xC1, 0xC3, 0xC5, 0xC7, 0xD1, 0xA6, 0x2C, 0x25, 0x5F, 0x3E, 0x3F,
    0xF8, 0xC9, 0xCA, 0xCB, 0xC8, 0xCD, 0xCE, 0xCF, 0xCC, 0x60, 0x3A, 0x23, 0x40, 0x27, 0x3D, 0x22,
    0xD8, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0xAB, 0xBB, 0xF0, 0xFD, 0xFE, 0xB1,
    0xB0, 0x6A, 0x6B, 0x6C, 0x6D, 0x6E, 0x6F, 0x70, 0x71, 0x72, 0xAA, 0xBA, 0xE6, 0xB8, 0xC6, 0xA4,
    0xB5, 0x7E, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0xA1, 0xBF, 0xD0, 0xDD, 0xDE, 0xAE,
    0x5E, 0xA3, 0xA5, 0xB7, 0xA9, 0xA7, 0xB6, 0xBC, 0xBD, 0xBE, 0x5B, 0x5D, 0xAF, 0xA8, 0xB4, 0xD7,
    0x7B, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0xAD, 0xF4, 0xF6, 0xF2, 0xF3, 0xF5,
    0x7D, 0x4A, 0x4B, 0x4C, 0x4D, 0x4E, 0x4F, 0x50, 0x51, 0x52, 0xB9, 0xFB, 0xFC, 0xF9, 0xFA, 0xFF,
    0x5C, 0xF7, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0xB2, 0xD4, 0xD6, 0xD2, 0xD3, 0xD5,
    0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0xB3, 0xDB, 0xDC, 0xD9, 0xDA, 0x9F,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Usage {
    Display,
    Packed,
    Binary,
    Float4,
    Float8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignPlace {
    Trailing,
    Leading,
    TrailingSeparate,
    LeadingSeparate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Picture {
    Alphanumeric(usize),
    Numeric { digits: u32, scale: u32, signed: bool },
    Edited(usize),
}

#[derive(Debug)]
struct Item {
    level: u32,
    name: String,
    picture: Option<Picture>,
    usage: Option<Usage>,
    occurs: usize,
    redefines: bool,
    sign: SignPlace,
    children: Vec<Item>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Text,
    Zoned { signed: bool, sign: SignPlace },
    Packed { signed: bool },
    Binary { signed: bool },
    Float,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Slot {
    name: String,
    offset: usize,
    length: usize,
    encoding: Encoding,
    column_type: ColumnType,
}

fn program_text(copybook: &str) -> String {
    let mut out = String::new();
    for line in copybook.lines() {
        let fixed = line.len() > INDICATOR_COLUMN
            && line.is_char_boundary(SEQUENCE_AREA)
            && line[..SEQUENCE_AREA]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b' ');
        let body = if fixed {
            if matches!(line.as_bytes()[INDICATOR_COLUMN], b'*' | b'/') {
                continue;
            }
            let end = line.len().min(PROGRAM_AREA_END);
            line.get(INDICATOR_COLUMN + 1..end).unwrap_or_default()
        } else {
            line
        };
        let body = body.split("*>").next().unwrap_or_default();
        if body.trim_start().starts_with('*') && !fixed {
            continue;
        }
        out.push_str(body);
        out.push('\n');
    }
    out
}

fn statements(program: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut characters = program.chars().peekable();
    while let Some(character) = characters.next() {
        match quote {
            Some(open) if character == open => quote = None,
            Some(_) => {}
            None if matches!(character, '\'' | '"') => quote = Some(character),
            None if character == '.'
                && characters.peek().is_none_or(|next| next.is_whitespace()) =>
            {
                found.push(std::mem::take(&mut current));
                continue;
            }
            None => {}
        }
        current.push(character);
    }
    if !current.trim().is_empty() {
        found.push(current);
    }
    found
}

fn words(statement: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for character in statement.chars() {
        match quote {
            Some(open) => {
                current.push(character);
                if character == open {
                    quote = None;
                }
            }
            None if character.is_whitespace() || character == ',' && current.is_empty() => {
                if !current.is_empty() {
                    found.push(std::mem::take(&mut current));
                }
            }
            None => {
                if matches!(character, '\'' | '"') {
                    quote = Some(character);
                }
                current.push(character);
            }
        }
    }
    if !current.is_empty() {
        found.push(current);
    }
    found
}

fn usage_of(word: &str) -> Option<Usage> {
    Some(match word {
        "DISPLAY" => Usage::Display,
        "COMP-3" | "COMPUTATIONAL-3" | "PACKED-DECIMAL" => Usage::Packed,
        "COMP" | "COMPUTATIONAL" | "COMP-4" | "COMPUTATIONAL-4" | "COMP-5" | "COMPUTATIONAL-5"
        | "BINARY" => Usage::Binary,
        "COMP-1" | "COMPUTATIONAL-1" => Usage::Float4,
        "COMP-2" | "COMPUTATIONAL-2" => Usage::Float8,
        _ => return None,
    })
}

fn is_clause(word: &str) -> bool {
    usage_of(word).is_some()
        || matches!(
            word,
            "PIC" | "PICTURE" | "USAGE" | "OCCURS" | "REDEFINES" | "VALUE" | "VALUES" | "SIGN"
                | "LEADING" | "TRAILING" | "SEPARATE" | "JUSTIFIED" | "JUST" | "SYNC"
                | "SYNCHRONIZED" | "BLANK" | "EXTERNAL" | "GLOBAL"
        )
}

fn picture_of(text: &str) -> Result<Picture, String> {
    let upper = text.to_ascii_uppercase();
    let mut symbols: Vec<char> = Vec::new();
    let mut characters = upper.chars().peekable();
    while let Some(symbol) = characters.next() {
        if symbol == '(' {
            let mut count = String::new();
            for digit in characters.by_ref() {
                if digit == ')' {
                    break;
                }
                count.push(digit);
            }
            let repeat: usize = count
                .trim()
                .parse()
                .map_err(|_| format!("PICTURE {text} has a bad repeat count"))?;
            let previous = *symbols
                .last()
                .ok_or_else(|| format!("PICTURE {text} starts with a repeat count"))?;
            if repeat == 0 || symbols.len() + repeat > MAX_PICTURE_SYMBOLS {
                return Err(format!("PICTURE {text} has a bad repeat count"));
            }
            symbols.extend(std::iter::repeat_n(previous, repeat - 1));
            continue;
        }
        symbols.push(symbol);
        if symbols.len() > MAX_PICTURE_SYMBOLS {
            return Err(format!("PICTURE {text} is too long"));
        }
    }
    if symbols.contains(&'P') {
        return Err(format!("PICTURE {text} uses P scaling, which is not supported"));
    }
    if symbols.iter().all(|symbol| matches!(symbol, 'S' | '9' | 'V')) {
        let point = symbols.iter().position(|symbol| *symbol == 'V');
        let digits = symbols.iter().filter(|symbol| **symbol == '9').count() as u32;
        let scale = point.map_or(0, |point| {
            symbols[point..].iter().filter(|symbol| **symbol == '9').count() as u32
        });
        if digits == 0 || digits > MAX_DIGITS {
            return Err(format!("PICTURE {text} has {digits} digits; 1 to {MAX_DIGITS} are read"));
        }
        return Ok(Picture::Numeric {
            digits,
            scale,
            signed: symbols.first() == Some(&'S'),
        });
    }
    if symbols.iter().all(|symbol| matches!(symbol, 'X' | 'A' | '9')) {
        return Ok(Picture::Alphanumeric(symbols.len()));
    }
    Ok(Picture::Edited(
        symbols.iter().filter(|symbol| !matches!(symbol, 'V' | 'S')).count(),
    ))
}

fn item_of(statement: &str) -> Result<Option<Item>, String> {
    let tokens: Vec<String> = words(statement)
        .into_iter()
        .map(|word| {
            if word.starts_with(['\'', '"']) {
                word
            } else {
                word.to_ascii_uppercase()
            }
        })
        .collect();
    let Some(first) = tokens.first() else {
        return Ok(None);
    };
    let level: u32 = first
        .parse()
        .map_err(|_| format!("{:?} does not start with a level number", statement.trim()))?;
    if level == CONDITION_LEVEL || level == RENAMES_LEVEL {
        return Ok(None);
    }
    let mut at = 1;
    let name = match tokens.get(1) {
        Some(word) if !is_clause(word) => {
            at = 2;
            word.clone()
        }
        _ => "FILLER".to_owned(),
    };
    let mut item = Item {
        level,
        name,
        picture: None,
        usage: None,
        occurs: 1,
        redefines: false,
        sign: SignPlace::Trailing,
        children: Vec::new(),
    };
    let mut separate = false;
    while let Some(word) = tokens.get(at) {
        at += 1;
        match word.as_str() {
            "PIC" | "PICTURE" => {
                if tokens.get(at).map(String::as_str) == Some("IS") {
                    at += 1;
                }
                let text = tokens.get(at).ok_or("PICTURE has no picture string")?;
                at += 1;
                item.picture = Some(picture_of(text)?);
            }
            "USAGE" | "IS" | "TIMES" | "CHARACTER" => {}
            "OCCURS" => {
                let count = tokens.get(at).ok_or("OCCURS has no count")?;
                at += 1;
                if tokens.get(at).map(String::as_str) == Some("TO") {
                    return Err(format!("{} OCCURS ... DEPENDING ON is not supported", item.name));
                }
                item.occurs = count
                    .parse::<usize>()
                    .ok()
                    .filter(|count| (1..=MAX_OCCURS).contains(count))
                    .ok_or_else(|| format!("{} has a bad OCCURS count", item.name))?;
            }
            "DEPENDING" => {
                return Err(format!("{} OCCURS ... DEPENDING ON is not supported", item.name));
            }
            "REDEFINES" => {
                item.redefines = true;
                at += 1;
            }
            "LEADING" => item.sign = SignPlace::Leading,
            "TRAILING" => item.sign = SignPlace::Trailing,
            "SEPARATE" => separate = true,
            "VALUE" | "VALUES" => break,
            other => {
                if let Some(usage) = usage_of(other) {
                    item.usage = Some(usage);
                }
            }
        }
    }
    if separate {
        item.sign = match item.sign {
            SignPlace::Leading | SignPlace::LeadingSeparate => SignPlace::LeadingSeparate,
            _ => SignPlace::TrailingSeparate,
        };
    }
    Ok(Some(item))
}

fn tree_of(items: Vec<Item>) -> Vec<Item> {
    let mut roots: Vec<Item> = Vec::new();
    let mut open: Vec<Item> = Vec::new();
    for item in items {
        while open.last().is_some_and(|parent| parent.level >= item.level) {
            close_one(&mut open, &mut roots);
        }
        open.push(item);
    }
    while !open.is_empty() {
        close_one(&mut open, &mut roots);
    }
    roots
}

fn close_one(open: &mut Vec<Item>, roots: &mut Vec<Item>) {
    if let Some(item) = open.pop() {
        match open.last_mut() {
            Some(parent) => parent.children.push(item),
            None => roots.push(item),
        }
    }
}

fn storage(picture: Picture, usage: Usage, sign: SignPlace) -> usize {
    match (picture, usage) {
        (_, Usage::Float4) => 4,
        (_, Usage::Float8) => 8,
        (Picture::Alphanumeric(length) | Picture::Edited(length), _) => length,
        (Picture::Numeric { digits, signed, .. }, Usage::Display) => {
            let separate = matches!(sign, SignPlace::LeadingSeparate | SignPlace::TrailingSeparate);
            digits as usize + usize::from(signed && separate)
        }
        (Picture::Numeric { digits, .. }, Usage::Packed) => digits as usize / 2 + 1,
        (Picture::Numeric { digits, .. }, Usage::Binary) => match digits {
            0..=4 => 2,
            5..=9 => 4,
            _ => 8,
        },
    }
}

fn size_of(item: &Item, inherited: Usage) -> Result<usize, String> {
    let usage = item.usage.unwrap_or(inherited);
    let one = if item.children.is_empty() {
        match (item.picture, usage) {
            (Some(picture), usage) => storage(picture, usage, item.sign),
            (None, Usage::Float4) => 4,
            (None, Usage::Float8) => 8,
            (None, _) => return Err(format!("{} has neither a PICTURE nor sub-items", item.name)),
        }
    } else {
        let mut total = 0usize;
        for child in item.children.iter().filter(|child| !child.redefines) {
            total = total
                .checked_add(size_of(child, usage)?)
                .ok_or("the record layout is too large")?;
        }
        total
    };
    one.checked_mul(item.occurs)
        .ok_or_else(|| "the record layout is too large".to_owned())
}

fn column_name(name: &str) -> String {
    name.to_ascii_lowercase().replace('-', "_")
}

fn slot_of(item: &Item, usage: Usage, name: String, offset: usize, length: usize) -> Result<Slot, String> {
    let (encoding, column_type) = match (item.picture, usage) {
        (_, Usage::Float4 | Usage::Float8) => (Encoding::Float, ColumnType::Double),
        (Some(Picture::Alphanumeric(_) | Picture::Edited(_)), Usage::Display) => {
            (Encoding::Text, ColumnType::Text)
        }
        (Some(Picture::Numeric { digits, scale, signed }), usage) => {
            let encoding = match usage {
                Usage::Packed => Encoding::Packed { signed },
                Usage::Binary => Encoding::Binary { signed },
                _ => Encoding::Zoned { signed, sign: item.sign },
            };
            let column_type = if scale == 0 && digits <= MAX_INTEGER_DIGITS {
                ColumnType::Int64
            } else {
                ColumnType::Decimal { precision: digits, scale }
            };
            (encoding, column_type)
        }
        _ => return Err(format!("{} is not numeric, so it cannot be COMP", item.name)),
    };
    Ok(Slot {
        name,
        offset,
        length,
        encoding,
        column_type,
    })
}

fn lay_out(item: &Item, start: usize, suffix: &str, inherited: Usage, slots: &mut Vec<Slot>) -> Result<(), String> {
    let usage = item.usage.unwrap_or(inherited);
    let total = size_of(item, inherited)?;
    let one = total / item.occurs;
    for occurrence in 0..item.occurs {
        let suffix = if item.occurs > 1 {
            format!("{suffix}_{}", occurrence + 1)
        } else {
            suffix.to_owned()
        };
        let at = start + occurrence * one;
        if item.children.is_empty() {
            if item.name != "FILLER" {
                let name = format!("{}{suffix}", column_name(&item.name));
                slots.push(slot_of(item, usage, name, at, one)?);
            }
            continue;
        }
        let mut offset = at;
        for child in &item.children {
            if child.redefines {
                continue;
            }
            lay_out(child, offset, &suffix, usage, slots)?;
            offset += size_of(child, usage)?;
        }
    }
    Ok(())
}

/// The fields of a COBOL copybook's first record (its first 01 level, or every item when it has
/// none), with their offsets: OCCURS repeated as `name_1`, `name_2`, REDEFINES and FILLER skipped,
/// level 88 conditions ignored. Returns the slots and the record's length.
fn layout_of(copybook: &str) -> Result<(Vec<Slot>, usize), String> {
    let mut items = Vec::new();
    for statement in statements(&program_text(copybook)) {
        if let Some(item) = item_of(&statement)? {
            items.push(item);
        }
    }
    let mut roots = tree_of(items);
    if roots.is_empty() {
        return Err("the copybook declares no items".to_owned());
    }
    let record = if roots[0].level == 1 {
        roots.swap_remove(0)
    } else {
        Item {
            level: 0,
            name: "RECORD".to_owned(),
            picture: None,
            usage: None,
            occurs: 1,
            redefines: false,
            sign: SignPlace::Trailing,
            children: roots,
        }
    };
    let length = size_of(&record, Usage::Display)?;
    let mut slots = Vec::new();
    lay_out(&record, 0, "", Usage::Display, &mut slots)?;
    if slots.is_empty() {
        return Err("the copybook's record has no named fields".to_owned());
    }
    if slots.len() > MAX_COLUMNS {
        return Err(format!("the record has {} fields; a table holds at most {MAX_COLUMNS}", slots.len()));
    }
    let mut seen: Vec<String> = Vec::new();
    for slot in &mut slots {
        let base = slot.name.clone();
        let mut suffix = 2;
        while seen.contains(&slot.name) {
            slot.name = format!("{base}_{suffix}");
            suffix += 1;
        }
        seen.push(slot.name.clone());
    }
    Ok((slots, length))
}

fn ebcdic_text(bytes: &[u8]) -> String {
    let text: String = bytes
        .iter()
        .map(|byte| char::from(CP037[usize::from(*byte)]))
        .collect();
    text.trim_end_matches([' ', '\0']).to_owned()
}

fn blank(bytes: &[u8]) -> bool {
    bytes.iter().all(|byte| *byte == 0) || bytes.iter().all(|byte| *byte == EBCDIC_SPACE)
}

fn negative_zone(nibble: u8) -> bool {
    matches!(nibble, 0x0B | 0x0D)
}

fn zoned(bytes: &[u8], signed: bool, sign: SignPlace) -> Result<i128, String> {
    let (digits, negative) = match (signed, sign) {
        (true, SignPlace::LeadingSeparate) => {
            let (first, rest) = bytes.split_first().ok_or("an empty number")?;
            (rest, *first == EBCDIC_MINUS)
        }
        (true, SignPlace::TrailingSeparate) => {
            let (last, rest) = bytes.split_last().ok_or("an empty number")?;
            (rest, *last == EBCDIC_MINUS)
        }
        (true, SignPlace::Leading) => (bytes, bytes.first().is_some_and(|byte| negative_zone(byte >> 4))),
        (true, SignPlace::Trailing) => (bytes, bytes.last().is_some_and(|byte| negative_zone(byte >> 4))),
        (false, _) => (bytes, false),
    };
    let mut value: i128 = 0;
    for byte in digits {
        let digit = if *byte == EBCDIC_SPACE { 0 } else { byte & 0x0F };
        if digit > 9 || (*byte != EBCDIC_SPACE && byte >> 4 < 0x0A && *byte != EBCDIC_ZERO + digit) {
            return Err(format!("byte {byte:#04x} is not a zoned decimal digit"));
        }
        value = value * 10 + i128::from(digit);
    }
    Ok(if negative { -value } else { value })
}

fn packed(bytes: &[u8]) -> Result<i128, String> {
    let mut value: i128 = 0;
    let count = bytes.len() * 2;
    for index in 0..count - 1 {
        let byte = bytes[index / 2];
        let nibble = if index % 2 == 0 { byte >> 4 } else { byte & 0x0F };
        if nibble > 9 {
            return Err(format!("byte {byte:#04x} is not packed decimal"));
        }
        value = value * 10 + i128::from(nibble);
    }
    let sign = bytes[bytes.len() - 1] & 0x0F;
    if sign < 0x0A {
        return Err(format!("packed decimal sign {sign:#x} is not a sign"));
    }
    Ok(if negative_zone(sign) { -value } else { value })
}

fn binary(bytes: &[u8], signed: bool) -> i128 {
    let bits = bytes.iter().fold(0u64, |held, byte| (held << 8) | u64::from(*byte));
    let width = bytes.len() * 8;
    if signed && width < 64 && bits >> (width - 1) & 1 == 1 {
        i128::from(bits) - (1i128 << width)
    } else if signed {
        i128::from(bits as i64)
    } else {
        i128::from(bits)
    }
}

fn ibm_float(bytes: &[u8]) -> f64 {
    let bits = bytes.iter().fold(0u64, |held, byte| (held << 8) | u64::from(*byte));
    let width = bytes.len() * 8;
    let negative = bits >> (width - 1) & 1 == 1;
    let exponent = ((bits >> (width - 8)) & 0x7F) as i32 - 64;
    let fraction_bits = width - 8;
    let fraction = (bits & ((1u64 << fraction_bits) - 1)) as f64 / (1u64 << fraction_bits) as f64;
    let value = fraction * 16f64.powi(exponent);
    if negative { -value } else { value }
}

fn cell_of(slot: &Slot, record: &[u8]) -> Result<Cell, String> {
    let Some(bytes) = record.get(slot.offset..slot.offset + slot.length) else {
        return Ok(Cell::Null);
    };
    let number = match slot.encoding {
        Encoding::Text => return Ok(Cell::Bytes(ebcdic_text(bytes).into_bytes())),
        Encoding::Float => return Ok(Cell::Float(ibm_float(bytes))),
        _ if blank(bytes) => return Ok(Cell::Null),
        Encoding::Zoned { signed, sign } => zoned(bytes, signed, sign)?,
        Encoding::Packed { .. } => packed(bytes)?,
        Encoding::Binary { signed } => binary(bytes, signed),
    };
    Ok(match slot.column_type {
        ColumnType::Int64 => Cell::Int(
            i64::try_from(number).map_err(|_| format!("{number} does not fit field {}", slot.name))?,
        ),
        _ => Cell::Decimal(number),
    })
}

const LARGE_BLOCK_FLAG: u32 = 0x8000_0000;
const SEGMENT_COMPLETE: u8 = 0;
const SEGMENT_FIRST: u8 = 1;
const SEGMENT_LAST: u8 = 2;
const SEGMENT_MIDDLE: u8 = 3;

fn variable_records(data: &[u8]) -> Option<Vec<std::borrow::Cow<'_, [u8]>>> {
    let mut records = Vec::new();
    let mut spanned: Option<Vec<u8>> = None;
    let mut at = 0;
    while at < data.len() {
        let descriptor = data.get(at..at + RECORD_DESCRIPTOR_LENGTH)?;
        if descriptor[3] != 0 || descriptor[2] > SEGMENT_MIDDLE {
            return None;
        }
        let size = usize::from(u16::from_be_bytes([descriptor[0], descriptor[1]]));
        let body = data.get(at + RECORD_DESCRIPTOR_LENGTH..at.checked_add(size)?).filter(|_| size >= RECORD_DESCRIPTOR_LENGTH)?;
        match (descriptor[2], spanned.as_mut()) {
            (SEGMENT_COMPLETE, None) => records.push(std::borrow::Cow::Borrowed(body)),
            (SEGMENT_FIRST, None) => spanned = Some(body.to_vec()),
            (SEGMENT_MIDDLE, Some(joined)) => joined.extend_from_slice(body),
            (SEGMENT_LAST, Some(joined)) => {
                joined.extend_from_slice(body);
                records.push(std::borrow::Cow::Owned(spanned.take()?));
            }
            _ => return None,
        }
        at += size;
    }
    spanned.is_none().then_some(records)
}

fn blocked_records(data: &[u8]) -> Option<Vec<std::borrow::Cow<'_, [u8]>>> {
    let mut blocks = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let word = u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?);
        let size = if word & LARGE_BLOCK_FLAG != 0 {
            usize::try_from(word & !LARGE_BLOCK_FLAG).ok()?
        } else if word & 0xFFFF == 0 {
            usize::try_from(word >> 16).ok()?
        } else {
            return None;
        };
        let body = data.get(at + 4..at.checked_add(size)?).filter(|_| size > 4)?;
        blocks.push(body);
        at += size;
    }
    let mut records = Vec::new();
    let mut joined: Vec<u8> = Vec::new();
    let mut spanning = false;
    for block in blocks {
        let mut inner = 0;
        while inner < block.len() {
            let descriptor = block.get(inner..inner + RECORD_DESCRIPTOR_LENGTH)?;
            if descriptor[3] != 0 || descriptor[2] > SEGMENT_MIDDLE {
                return None;
            }
            let size = usize::from(u16::from_be_bytes([descriptor[0], descriptor[1]]));
            let body = block.get(inner + RECORD_DESCRIPTOR_LENGTH..inner.checked_add(size)?).filter(|_| size >= RECORD_DESCRIPTOR_LENGTH)?;
            match (descriptor[2], spanning) {
                (SEGMENT_COMPLETE, false) => records.push(std::borrow::Cow::Borrowed(body)),
                (SEGMENT_FIRST, false) => {
                    joined = body.to_vec();
                    spanning = true;
                }
                (SEGMENT_MIDDLE, true) => joined.extend_from_slice(body),
                (SEGMENT_LAST, true) => {
                    joined.extend_from_slice(body);
                    records.push(std::borrow::Cow::Owned(std::mem::take(&mut joined)));
                    spanning = false;
                }
                _ => return None,
            }
            inner += size;
        }
    }
    (!spanning).then_some(records)
}

fn records_of(data: &[u8], length: usize) -> Result<Vec<std::borrow::Cow<'_, [u8]>>, String> {
    if length == 0 {
        return Err("the copybook's record is empty".to_owned());
    }
    if data.len() % length == 0 {
        return Ok(data.chunks(length).map(std::borrow::Cow::Borrowed).collect());
    }
    blocked_records(data)
        .filter(|records| !records.is_empty())
        .or_else(|| variable_records(data))
        .ok_or_else(|| {
            format!(
                "it is neither whole {length}-byte records nor variable records (RECFM V, VB or VBS, as a VSAM or sequential data set is unloaded) with record and block descriptor words"
            )
        })
}

/// The records of an EBCDIC (code page 037) data set as table columns, laid out by `copybook`:
/// text fields decoded and right-trimmed, zoned, packed (COMP-3) and binary (COMP, COMP-4, COMP-5)
/// numbers as integers or decimals, COMP-1 and COMP-2 hexadecimal floats as doubles, and numbers
/// of all spaces or low-values as null. Fixed-length records, or variable ones each led by a
/// record descriptor word, blocked behind block descriptor words or spanned over segments, as
/// IDCAMS `REPRO` and IEBGENER unload VSAM and sequential data sets (D839).
pub(super) fn columns_of(copybook: &str, data: &[u8]) -> Result<Vec<Column>, String> {
    let (slots, length) = layout_of(copybook)?;
    let records = records_of(data, length)?;
    if records.len().saturating_mul(slots.len()) > MAX_VALUES {
        return Err(format!("it holds more than {MAX_VALUES} values, the most one import reads"));
    }
    let mut columns: Vec<Column> = slots
        .iter()
        .map(|slot| Column {
            name: slot.name.clone(),
            column_type: slot.column_type,
            nullable: true,
            cells: Vec::with_capacity(records.len()),
        })
        .collect();
    for (index, record) in records.iter().enumerate() {
        for (slot, column) in slots.iter().zip(columns.iter_mut()) {
            let cell = cell_of(slot, record)
                .map_err(|error| format!("record {}, field {}: {error}", index + 1, slot.name))?;
            column.cells.push(cell);
        }
    }
    Ok(columns)
}

impl Session<'_> {
    pub(super) fn import_mainframe(&mut self, copybook: &Path, source: &Path) -> Result<Exit, Failure> {
        let usage = |message: String| Failure::usage(message, Some(CommandName::Import));
        let shown = render::text(source.display().to_string().as_bytes());
        let shown_copybook = render::text(copybook.display().to_string().as_bytes());
        let table = self.request.csv.table.clone().unwrap_or_default();
        for (path, shown, limit) in [
            (copybook, &shown_copybook, MAX_COPYBOOK_BYTES),
            (source, &shown, MAX_RECORDS_BYTES),
        ] {
            let size = std::fs::metadata(path)
                .map_err(|error| usage(format!("cannot read {shown}: {error}")))?
                .len();
            if size > limit {
                return Err(usage(format!("{shown} is larger than eggdb import reads at once")));
            }
        }
        let layout = std::fs::read(copybook)
            .map_err(|error| usage(format!("cannot read {shown_copybook}: {error}")))?;
        let layout = String::from_utf8_lossy(&layout);
        let data = std::fs::read(source)
            .map_err(|error| usage(format!("cannot read {shown}: {error}")))?;
        let columns = columns_of(&layout, &data).map_err(|error| {
            Failure::database(format!("cannot import {shown}: {error}"), Exit::DatabaseError)
        })?;
        self.import_columns(&shown, &table, &columns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COPYBOOK: &str = "      * customer record
       01  CUSTOMER-RECORD.
           05  CUST-ID          PIC 9(5).
           05  CUST-NAME        PIC X(6).
           05  FILLER           PIC X(2).
           05  BALANCE          PIC S9(5)V99 COMP-3.
           05  VISITS           PIC S9(4) COMP OCCURS 2 TIMES.
           05  OLD-ID REDEFINES CUST-ID PIC X(5).
           05  RATE             PIC S9(3)V9 SIGN LEADING SEPARATE.
               88  NO-RATE      VALUE ZERO.
";

    #[test]
    fn copybook_lays_out_fields() {
        let (slots, length) = layout_of(COPYBOOK).unwrap();
        let names: Vec<&str> = slots.iter().map(|slot| slot.name.as_str()).collect();
        assert_eq!(names, vec!["cust_id", "cust_name", "balance", "visits_1", "visits_2", "rate"]);
        assert_eq!(length, 5 + 6 + 2 + 4 + 4 + 5);
        assert_eq!(slots[2].offset, 13);
        assert_eq!(slots[2].column_type, ColumnType::Decimal { precision: 7, scale: 2 });
        assert_eq!(slots[5].offset, 21);
    }

    #[test]
    fn ebcdic_record_decodes() {
        let mut record = vec![0xF0, 0xF0, 0xF0, 0xF4, 0xF2];
        record.extend([0xC1, 0x81, 0x40, 0x40, 0x40, 0x40]);
        record.extend([0x40, 0x40]);
        record.extend([0x01, 0x23, 0x45, 0x6D]);
        record.extend([0x00, 0x07, 0xFF, 0xFE]);
        record.extend([EBCDIC_MINUS, 0xF0, 0xF1, 0xF2, 0xF5]);
        let columns = columns_of(COPYBOOK, &record).unwrap();
        assert_eq!(columns[0].cells, vec![Cell::Int(42)]);
        assert_eq!(columns[1].cells, vec![Cell::Bytes(b"Aa".to_vec())]);
        assert_eq!(columns[2].cells, vec![Cell::Decimal(-123_456)]);
        assert_eq!(columns[3].cells, vec![Cell::Int(7)]);
        assert_eq!(columns[4].cells, vec![Cell::Int(-2)]);
        assert_eq!(columns[5].cells, vec![Cell::Decimal(-125)]);
        assert_eq!(ibm_float(&[0x41, 0x10, 0x00, 0x00]), 1.0);
        assert_eq!(ibm_float(&[0xC2, 0x76, 0xA0, 0x00]), -118.625);
    }

    #[test]
    fn blocked_and_spanned_variable_records_are_unblocked() {
        let record = |body: &[u8], segment: u8| {
            let mut out = ((body.len() + 4) as u16).to_be_bytes().to_vec();
            out.extend([segment, 0]);
            out.extend_from_slice(body);
            out
        };
        let unblocked = [record(b"abc", 0), record(b"de", 1), record(b"fg", 2)].concat();
        let found = variable_records(&unblocked).unwrap();
        assert_eq!(found.iter().map(|record| record.to_vec()).collect::<Vec<_>>(), vec![b"abc".to_vec(), b"defg".to_vec()]);
        let inner = [record(b"abc", 0), record(b"xy", 0)].concat();
        let mut blocked = ((inner.len() + 4) as u16).to_be_bytes().to_vec();
        blocked.extend([0, 0]);
        blocked.extend_from_slice(&inner);
        let found = blocked_records(&blocked).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(&*found[1], b"xy");
        let mut large = (0x8000_0000u32 | (inner.len() as u32 + 4)).to_be_bytes().to_vec();
        large.extend_from_slice(&inner);
        assert_eq!(blocked_records(&large).unwrap().len(), 2);
        assert!(variable_records(&record(b"de", 1)).is_none());
        assert!(records_of(&[1, 2, 3], 2).is_err());
    }
}
