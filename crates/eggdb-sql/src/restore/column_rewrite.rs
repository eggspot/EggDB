use super::script::DifferenceNote;
use super::words::{Token, Words, plain_qualified_name};
use std::ops::Range;

pub(crate) fn without_unsupported_columns(text: &str) -> Option<(String, Vec<DifferenceNote>)> {
    let head: Vec<Token> = Words::sqlserver(text).take(8).collect();
    if !(head.first()?.is_word("create") && head.get(1)?.is_word("table")) {
        return None;
    }
    let mut at = 2;
    let table = plain_qualified_name(&head, &mut at)?;
    let (body, parts) = table_body(text)?;
    let mut notes = Vec::new();
    let mut kept: Vec<String> = Vec::new();
    let mut changed = false;
    for part in parts {
        let element = &text[part];
        match rewritten_element(element, &table, &mut notes) {
            Element::Kept => kept.push(element.to_owned()),
            Element::Rewritten(rewritten) => {
                kept.push(rewritten);
                changed = true;
            }
            Element::Dropped => changed = true,
        }
    }
    if !changed || kept.is_empty() {
        return None;
    }
    let mut rewritten = String::with_capacity(text.len());
    rewritten.push_str(&text[..body.start]);
    rewritten.push_str(&kept.join(","));
    rewritten.push_str(&text[body.end..]);
    Some((rewritten, notes))
}

enum Element {
    Kept,
    Rewritten(String),
    Dropped,
}

fn rewritten_element(element: &str, table: &str, notes: &mut Vec<DifferenceNote>) -> Element {
    let tokens: Vec<Token> = Words::sqlserver(element).take(4).collect();
    let Some(first) = tokens.first() else {
        return Element::Kept;
    };
    if let Token::Word(word) = first
        && matches!(
            word.as_str(),
            "constraint" | "primary" | "unique" | "check" | "foreign" | "index" | "period"
        )
    {
        return Element::Kept;
    }
    let Some(column) = first.plain_name_part() else {
        return Element::Kept;
    };
    match tokens.get(1) {
        Some(token) if token.is_word("as") => {
            notes.push(DifferenceNote::new(
                format!("{table}.{column}"),
                "computed column not kept",
            ));
            return Element::Dropped;
        }
        Some(Token::Word(kind) | Token::Quoted(kind))
            if kind.eq_ignore_ascii_case("timestamp")
                || kind.eq_ignore_ascii_case("rowversion") =>
        {
            notes.push(DifferenceNote::new(
                format!("{table}.{column}"),
                "rowversion column not kept",
            ));
            return Element::Dropped;
        }
        _ => {}
    }
    match identity_arguments(element) {
        Some((range, shown)) if shown != "1, 1" => {
            notes.push(DifferenceNote::new(
                format!("{table}.{column}"),
                format!("IDENTITY({shown}) restored as IDENTITY(1, 1); new values continue after the highest stored one"),
            ));
            let mut rewritten = String::with_capacity(element.len());
            rewritten.push_str(&element[..range.start]);
            rewritten.push_str("(1, 1)");
            rewritten.push_str(&element[range.end..]);
            Element::Rewritten(rewritten)
        }
        _ => Element::Kept,
    }
}

fn identity_arguments(element: &str) -> Option<(Range<usize>, String)> {
    let lower = element.to_ascii_lowercase();
    let mut from = 0;
    while let Some(offset) = lower.get(from..)?.find("identity") {
        let at = from + offset;
        let before = at.checked_sub(1).map(|index| lower.as_bytes()[index]);
        from = at + "identity".len();
        if before.is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'[') {
            continue;
        }
        let rest = &element[from..];
        let open = rest.len() - rest.trim_start().len();
        if !rest[open..].starts_with('(') {
            continue;
        }
        let close = rest[open..].find(')')?;
        let inside = &rest[open + 1..open + close];
        let arguments: Vec<&str> = inside.split(',').map(str::trim).collect();
        if arguments.len() != 2 || !arguments.iter().all(|argument| is_signed_integer(argument)) {
            return None;
        }
        let shown = format!("{}, {}", arguments[0], arguments[1]);
        return Some((from + open..from + open + close + 1, shown));
    }
    None
}

fn is_signed_integer(text: &str) -> bool {
    let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
    !digits.is_empty() && digits.len() <= 20 && digits.bytes().all(|byte| byte.is_ascii_digit())
}

fn table_body(text: &str) -> Option<(Range<usize>, Vec<Range<usize>>)> {
    let bytes = text.as_bytes();
    let mut at = 0;
    let mut depth = 0u32;
    let mut start = None;
    let mut part_start = 0;
    let mut parts = Vec::new();
    while at < bytes.len() {
        let byte = bytes[at];
        let next = bytes.get(at + 1).copied();
        match byte {
            b'\'' | b'[' | b'"' => {
                let close = if byte == b'[' { b']' } else { byte };
                at += 1;
                while at < bytes.len() {
                    if bytes[at] == close {
                        if bytes.get(at + 1) == Some(&close) {
                            at += 2;
                            continue;
                        }
                        break;
                    }
                    at += 1;
                }
            }
            b'-' if next == Some(b'-') => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            b'/' if next == Some(b'*') => {
                let mut nested = 1u32;
                at += 2;
                while at < bytes.len() && nested > 0 {
                    if bytes[at] == b'/' && bytes.get(at + 1) == Some(&b'*') {
                        nested += 1;
                        at += 1;
                    } else if bytes[at] == b'*' && bytes.get(at + 1) == Some(&b'/') {
                        nested -= 1;
                        at += 1;
                    }
                    at += 1;
                }
                continue;
            }
            b'(' => {
                depth += 1;
                if depth == 1 && start.is_none() {
                    start = Some(at + 1);
                    part_start = at + 1;
                }
            }
            b')' => {
                if depth == 1
                    && let Some(body_start) = start
                {
                    parts.push(part_start..at);
                    return Some((body_start..at, parts));
                }
                depth = depth.saturating_sub(1);
            }
            b',' if depth == 1 => {
                parts.push(part_start..at);
                part_start = at + 1;
            }
            _ => {}
        }
        at += 1;
    }
    None
}
