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
            Element::Dropped
        }
        Some(Token::Word(kind) | Token::Quoted(kind))
            if kind.eq_ignore_ascii_case("timestamp")
                || kind.eq_ignore_ascii_case("rowversion") =>
        {
            notes.push(DifferenceNote::new(
                format!("{table}.{column}"),
                "rowversion column not kept",
            ));
            Element::Dropped
        }
        _ => Element::Kept,
    }
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
