use crate::error::{DocumentError, DocumentResult};
use eggdb_bson::{Bson, Document};
use eggdb_sql::json::{Json, Node, parse_document};

const MAX_DEPTH: usize = 100;

fn bad(message: impl Into<String>) -> DocumentError {
    DocumentError::failed_to_parse(message)
}

fn member<'n>(members: &'n [(String, Node)], name: &str) -> Option<&'n Node> {
    members.iter().find(|(key, _)| key == name).map(|(_, node)| node)
}

fn string_member<'n>(members: &'n [(String, Node)], name: &str) -> DocumentResult<&'n str> {
    match member(members, name).map(|node| &node.value) {
        Some(Json::String(text)) => Ok(text),
        _ => Err(bad(format!("Extended JSON needs the string {name}"))),
    }
}

fn number_member(members: &[(String, Node)], name: &str) -> DocumentResult<u32> {
    match member(members, name).map(|node| &node.value) {
        Some(Json::Number(text)) => text.parse().map_err(|_| bad(format!("Extended JSON {name} is out of range"))),
        _ => Err(bad(format!("Extended JSON needs the number {name}"))),
    }
}

fn hex_bytes<const N: usize>(text: &str) -> DocumentResult<[u8; N]> {
    if text.len() != N * 2 || !text.is_ascii() {
        return Err(bad(format!("an object id needs {} hexadecimal digits", N * 2)));
    }
    let mut out = [0u8; N];
    for (at, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[at * 2..at * 2 + 2], 16).map_err(|_| bad("an object id is not hexadecimal"))?;
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

fn iso_milliseconds(text: &str) -> DocumentResult<i64> {
    let invalid = || bad(format!("'{text}' is not an ISO-8601 date"));
    let bytes = text.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' || bytes[13] != b':' || bytes[16] != b':' {
        return Err(invalid());
    }
    let field = |from: usize, to: usize| text.get(from..to).and_then(|part| part.parse::<i64>().ok()).ok_or_else(invalid);
    let (year, month, day) = (field(0, 4)?, field(5, 7)?, field(8, 10)?);
    let (hour, minute, second) = (field(11, 13)?, field(14, 16)?, field(17, 19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return Err(invalid());
    }
    let mut rest = &text[19..];
    let mut milliseconds = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return Err(invalid());
        }
        let padded = format!("{:0<3}", &fraction[..digits.min(3)]);
        milliseconds = padded.parse::<i64>().map_err(|_| invalid())?;
        rest = &fraction[digits..];
    }
    let offset_minutes = match rest {
        "Z" => 0,
        zone if zone.len() == 6 && (zone.starts_with('+') || zone.starts_with('-')) && &zone[3..4] == ":" => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let hours: i64 = zone[1..3].parse().map_err(|_| invalid())?;
            let minutes: i64 = zone[4..6].parse().map_err(|_| invalid())?;
            sign * (hours * 60 + minutes)
        }
        _ => return Err(invalid()),
    };
    let days = days_from_civil(year, month, day);
    Ok(((days * 24 + hour) * 60 + minute - offset_minutes) * 60_000 + second * 1_000 + milliseconds)
}

fn plain_number(text: &str) -> Bson {
    if let Ok(small) = text.parse::<i32>() {
        return Bson::Int32(small);
    }
    if let Ok(large) = text.parse::<i64>() {
        return Bson::Int64(large);
    }
    Bson::Double(text.parse().unwrap_or(f64::NAN))
}

fn special(members: &[(String, Node)], depth: usize) -> DocumentResult<Option<Bson>> {
    let Some((first, value)) = members.first() else {
        return Ok(None);
    };
    let text = |node: &Node| match &node.value {
        Json::String(text) => Ok(text.clone()),
        _ => Err(bad(format!("Extended JSON {first} needs a string"))),
    };
    Ok(Some(match first.as_str() {
        "$oid" => Bson::ObjectId(hex_bytes::<12>(&text(value)?)?),
        "$numberInt" => Bson::Int32(text(value)?.parse().map_err(|_| bad("$numberInt is not a 32-bit integer"))?),
        "$numberLong" => Bson::Int64(text(value)?.parse().map_err(|_| bad("$numberLong is not a 64-bit integer"))?),
        "$numberDouble" => Bson::Double(match text(value)?.as_str() {
            "Infinity" => f64::INFINITY,
            "-Infinity" => f64::NEG_INFINITY,
            "NaN" => f64::NAN,
            other => other.parse().map_err(|_| bad("$numberDouble is not a number"))?,
        }),
        "$numberDecimal" => return Err(DocumentError::not_implemented("decimal values in Extended JSON are not supported yet")),
        "$date" => Bson::DateTime(match &value.value {
            Json::String(iso) => iso_milliseconds(iso)?,
            Json::Number(number) => number.parse().map_err(|_| bad("$date is out of range"))?,
            Json::Object(inner) => string_member(inner, "$numberLong")?.parse().map_err(|_| bad("$date is out of range"))?,
            _ => return Err(bad("$date needs a string, a number or $numberLong")),
        }),
        "$binary" => match &value.value {
            Json::Object(inner) => {
                let bytes = eggdb_crypto::base64_decode(string_member(inner, "base64")?.as_bytes()).map_err(|_| bad("$binary is not base64"))?;
                let subtype = u8::from_str_radix(string_member(inner, "subType")?, 16).map_err(|_| bad("$binary subType is not hexadecimal"))?;
                Bson::Binary { subtype, bytes }
            }
            _ => return Err(bad("$binary needs base64 and subType")),
        },
        "$regularExpression" => match &value.value {
            Json::Object(inner) => Bson::Regex {
                pattern: string_member(inner, "pattern")?.to_owned(),
                options: string_member(inner, "options")?.to_owned(),
            },
            _ => return Err(bad("$regularExpression needs pattern and options")),
        },
        "$timestamp" => match &value.value {
            Json::Object(inner) => Bson::Timestamp { time: number_member(inner, "t")?, increment: number_member(inner, "i")? },
            _ => return Err(bad("$timestamp needs t and i")),
        },
        "$minKey" => Bson::MinKey,
        "$maxKey" => Bson::MaxKey,
        "$undefined" => Bson::Undefined,
        "$symbol" => Bson::Symbol(text(value)?),
        "$code" => match member(members, "$scope") {
            Some(scope) => match convert(scope, depth + 1)? {
                Bson::Document(scope) => Bson::JavaScriptWithScope { code: text(value)?, scope },
                _ => return Err(bad("$scope needs an object")),
            },
            None => Bson::JavaScript(text(value)?),
        },
        _ => return Ok(None),
    }))
}

fn convert(node: &Node, depth: usize) -> DocumentResult<Bson> {
    if depth > MAX_DEPTH {
        return Err(bad("Extended JSON is nested too deeply"));
    }
    Ok(match &node.value {
        Json::Null => Bson::Null,
        Json::Boolean(flag) => Bson::Boolean(*flag),
        Json::Number(text) => plain_number(text),
        Json::String(text) => Bson::String(text.clone()),
        Json::Array(items) => Bson::Array(items.iter().map(|item| convert(item, depth + 1)).collect::<DocumentResult<_>>()?),
        Json::Object(members) => match special(members, depth)? {
            Some(value) => value,
            None => Bson::Document(
                members
                    .iter()
                    .map(|(name, value)| Ok((name.clone(), convert(value, depth + 1)?)))
                    .collect::<DocumentResult<Vec<_>>>()?
                    .into_iter()
                    .collect(),
            ),
        },
    })
}

/// Reads a document written as MongoDB Extended JSON v2, canonical or relaxed, as
/// `mongoexport` and `mongodump`'s metadata files write it.
pub fn parse_extended_json(text: &str) -> DocumentResult<Document> {
    let node = parse_document(text).map_err(|error| bad(format!("invalid JSON at byte {}: {}", error.position, error.message)))?;
    match convert(&node, 0)? {
        Bson::Document(document) => Ok(document),
        other => Err(bad(format!("Extended JSON holds a {} where a document was expected", other.type_name()))),
    }
}
