use super::script::DifferenceNote;
use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DisabledElement {
    pub(crate) table: String,
    pub(crate) name: String,
    pub(crate) clause: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableRewrite {
    pub(crate) text: String,
    pub(crate) notes: Vec<DifferenceNote>,
    pub(crate) disabled: Vec<DisabledElement>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Word(String),
    Quoted(String),
    Open,
    Close,
    Comma,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Lexeme {
    piece: Piece,
    span: Range<usize>,
    depth: u32,
}

const IDENTITY_OPTION_WORDS: [&str; 20] = [
    "minvalue",
    "maxvalue",
    "nominvalue",
    "nomaxvalue",
    "increment",
    "by",
    "start",
    "with",
    "limit",
    "value",
    "cache",
    "nocache",
    "order",
    "noorder",
    "cycle",
    "nocycle",
    "keep",
    "nokeep",
    "scale",
    "noscale",
];

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$' | b'#') || byte >= 0x80
}

fn lexemes(text: &str) -> Vec<Lexeme> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0u32;
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        let next = bytes.get(at + 1).copied();
        let start = at;
        match byte {
            b'-' if next == Some(b'-') => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
                continue;
            }
            b'/' if next == Some(b'*') => {
                at += 2;
                while at < bytes.len() && !(bytes[at] == b'*' && bytes.get(at + 1) == Some(&b'/')) {
                    at += 1;
                }
                at = (at + 2).min(bytes.len());
                continue;
            }
            b'\'' => {
                at += 1;
                while at < bytes.len() {
                    if bytes[at] == b'\'' {
                        if bytes.get(at + 1) == Some(&b'\'') {
                            at += 2;
                            continue;
                        }
                        break;
                    }
                    at += 1;
                }
                at = (at + 1).min(bytes.len());
                out.push(Lexeme {
                    piece: Piece::Other,
                    span: start..at,
                    depth,
                });
                continue;
            }
            b'"' => {
                at += 1;
                while at < bytes.len() && bytes[at] != b'"' {
                    at += 1;
                }
                let name = text.get(start + 1..at).unwrap_or("");
                at = (at + 1).min(bytes.len());
                out.push(Lexeme {
                    piece: Piece::Quoted(crate::lexer::swap_uniform_case(name)),
                    span: start..at,
                    depth,
                });
                continue;
            }
            b'(' => {
                out.push(Lexeme {
                    piece: Piece::Open,
                    span: start..start + 1,
                    depth,
                });
                depth += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                out.push(Lexeme {
                    piece: Piece::Close,
                    span: start..start + 1,
                    depth,
                });
            }
            b',' => out.push(Lexeme {
                piece: Piece::Comma,
                span: start..start + 1,
                depth,
            }),
            byte if is_name_byte(byte) => {
                while at < bytes.len() && is_name_byte(bytes[at]) {
                    at += 1;
                }
                out.push(Lexeme {
                    piece: Piece::Word(text[start..at].to_ascii_lowercase()),
                    span: start..at,
                    depth,
                });
                continue;
            }
            byte if byte.is_ascii_whitespace() => {}
            _ => out.push(Lexeme {
                piece: Piece::Other,
                span: start..start + 1,
                depth,
            }),
        }
        at += 1;
    }
    out
}

fn is_word(lexeme: Option<&Lexeme>, word: &str) -> bool {
    matches!(lexeme.map(|lexeme| &lexeme.piece), Some(Piece::Word(found)) if found == word)
}

fn name_of(lexeme: Option<&Lexeme>) -> Option<String> {
    match lexeme.map(|lexeme| &lexeme.piece) {
        Some(Piece::Word(word) | Piece::Quoted(word)) => Some(word.clone()),
        _ => None,
    }
}

fn starts_constraint(lexeme: Option<&Lexeme>) -> bool {
    ["constraint", "primary", "unique", "foreign", "check"]
        .iter()
        .any(|word| is_word(lexeme, word))
}

struct Element {
    span: Range<usize>,
    lexemes: Vec<Lexeme>,
}

fn identity_span(element: &Element) -> Option<Range<usize>> {
    let lexemes = &element.lexemes;
    let start = lexemes
        .iter()
        .position(|lexeme| lexeme.depth == 1 && is_word(Some(lexeme), "generated"))?;
    let identity = lexemes
        .iter()
        .skip(start)
        .position(|lexeme| is_word(Some(lexeme), "identity"))?
        + start;
    let mut end = identity + 1;
    let mut options = false;
    if matches!(lexemes.get(end).map(|lexeme| &lexeme.piece), Some(Piece::Open)) {
        let depth = lexemes[end].depth;
        end += 1;
        while end < lexemes.len()
            && !(lexemes[end].piece == Piece::Close && lexemes[end].depth == depth)
        {
            end += 1;
        }
        end += 1;
        options = true;
    } else {
        while let Some(lexeme) = lexemes.get(end) {
            let option = match &lexeme.piece {
                Piece::Word(word) => {
                    IDENTITY_OPTION_WORDS.contains(&word.as_str())
                        || word.chars().all(|character| character.is_ascii_digit())
                }
                Piece::Other => lexeme.span.len() == 1,
                _ => false,
            };
            if !option {
                break;
            }
            options = true;
            end += 1;
        }
    }
    if !options {
        return None;
    }
    let from = lexemes[start].span.start;
    let to = lexemes
        .get(end.saturating_sub(1))
        .map_or(element.span.end, |lexeme| lexeme.span.end);
    Some(from..to)
}

fn is_virtual(element: &Element) -> bool {
    let lexemes = &element.lexemes;
    if lexemes.iter().any(|lexeme| is_word(Some(lexeme), "identity")) {
        return false;
    }
    lexemes.windows(2).any(|pair| {
        pair[0].depth == 1
            && is_word(Some(&pair[0]), "as")
            && matches!(pair[1].piece, Piece::Open)
    })
}

fn disable_span(lexemes: &[Lexeme], depth: u32) -> Option<Range<usize>> {
    let at = lexemes
        .iter()
        .position(|lexeme| lexeme.depth == depth && is_word(Some(lexeme), "disable"))?;
    let mut end = at + 1;
    while is_word(lexemes.get(end), "validate") || is_word(lexemes.get(end), "novalidate") {
        end += 1;
    }
    Some(lexemes[at].span.start..lexemes[end - 1].span.end)
}

pub(crate) fn rewritten_table(text: &str) -> Option<TableRewrite> {
    let lexemes = lexemes(text);
    let table_at = lexemes
        .iter()
        .position(|lexeme| lexeme.depth == 0 && is_word(Some(lexeme), "table"))?;
    let open = lexemes
        .iter()
        .skip(table_at)
        .position(|lexeme| lexeme.piece == Piece::Open && lexeme.depth == 0)?
        + table_at;
    let table_start = lexemes.get(table_at + 1)?.span.start;
    let table = text.get(table_start..lexemes[open].span.start)?.trim().to_owned();
    let close = lexemes
        .iter()
        .skip(open + 1)
        .position(|lexeme| lexeme.piece == Piece::Close && lexeme.depth == 0)?
        + open
        + 1;
    let mut elements = Vec::new();
    let mut element_start = open + 1;
    for index in open + 1..=close {
        let lexeme = &lexemes[index];
        let ends = index == close || (lexeme.piece == Piece::Comma && lexeme.depth == 1);
        if !ends {
            continue;
        }
        let parts: Vec<Lexeme> = lexemes[element_start..index].to_vec();
        let span_start = lexemes[element_start.min(index)].span.start;
        let span_end = if index > element_start {
            lexemes[index - 1].span.end
        } else {
            span_start
        };
        let span_start = if element_start == index {
            lexeme.span.start
        } else {
            span_start
        };
        elements.push(Element {
            span: span_start..span_end,
            lexemes: parts,
        });
        element_start = index + 1;
    }
    let table_name = lexemes[table_at + 1..open]
        .iter()
        .rev()
        .find_map(|lexeme| name_of(Some(lexeme)))
        .unwrap_or_default();
    let mut kept: Vec<String> = Vec::new();
    let mut notes = Vec::new();
    let mut disabled = Vec::new();
    let mut changed = false;
    for element in &elements {
        let element_text = text.get(element.span.clone()).unwrap_or("");
        let first = element.lexemes.first();
        if starts_constraint(first) {
            if let Some(span) = disable_span(&element.lexemes, 1) {
                let name = if is_word(first, "constraint") {
                    name_of(element.lexemes.get(1)).unwrap_or_default()
                } else {
                    String::new()
                };
                let before = text.get(element.span.start..span.start).unwrap_or("");
                let after = text.get(span.end..element.span.end).unwrap_or("");
                disabled.push(DisabledElement {
                    table: table.clone(),
                    name,
                    clause: format!("{}{}", before.trim_end(), after).trim().to_owned(),
                });
                changed = true;
                continue;
            }
            kept.push(element_text.to_owned());
            continue;
        }
        let column = name_of(first).unwrap_or_default();
        if is_virtual(element) {
            notes.push(DifferenceNote::new(
                format!("{table_name}.{column}"),
                "virtual column not kept: Oracle computes it, so it is left out here",
            ));
            changed = true;
            continue;
        }
        if let Some(span) = identity_span(element) {
            let before = text.get(element.span.start..span.start).unwrap_or("");
            let after = text.get(span.end..element.span.end).unwrap_or("");
            kept.push(format!("{}{}", before.trim_end(), after));
            notes.push(DifferenceNote::new(
                format!("{table_name}.{column}"),
                "identity not kept: values restored, new rows need explicit values",
            ));
            changed = true;
            continue;
        }
        kept.push(element_text.to_owned());
    }
    if !changed || kept.is_empty() {
        return None;
    }
    let head = text.get(..lexemes[open].span.end)?;
    let tail = text.get(lexemes[close].span.start..)?;
    Some(TableRewrite {
        text: format!("{head}{}{tail}", kept.join(", ")),
        notes,
        disabled,
    })
}

pub(crate) fn without_disable(text: &str) -> Option<String> {
    let lexemes = lexemes(text);
    let span = disable_span(&lexemes, 0)?;
    let before = text.get(..span.start)?;
    let after = text.get(span.end..)?;
    Some(format!("{}{}", before.trim_end(), after))
}
