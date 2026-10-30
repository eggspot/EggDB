use super::column_rewrite;
use super::script::DifferenceNote;
use crate::error::{SqlError, SqlState};
use std::ops::Range;

const IDENTITY_NOT_KEPT: &str = "identity not kept: values restored, new rows need explicit values";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DisabledElement {
    pub(crate) table: String,
    pub(crate) name: String,
    pub(crate) clause: String,
    pub(crate) in_column: bool,
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
    if matches!(
        lexemes.get(end).map(|lexeme| &lexeme.piece),
        Some(Piece::Open)
    ) {
        let depth = lexemes[end].depth;
        end += 1;
        while end < lexemes.len()
            && !(lexemes[end].piece == Piece::Close && lexemes[end].depth == depth)
        {
            end += 1;
        }
        end += 1;
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
            end += 1;
        }
    }
    let from = lexemes[start].span.start;
    let to = lexemes
        .get(end.saturating_sub(1))
        .map_or(element.span.end, |lexeme| lexeme.span.end);
    Some(from..to)
}

fn is_virtual(element: &Element) -> bool {
    let lexemes = &element.lexemes;
    if lexemes
        .iter()
        .any(|lexeme| is_word(Some(lexeme), "identity"))
    {
        return false;
    }
    lexemes.windows(2).any(|pair| {
        pair[0].depth == 1 && is_word(Some(&pair[0]), "as") && matches!(pair[1].piece, Piece::Open)
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

struct ColumnConstraint {
    span: Range<usize>,
    name: String,
    kind_at: usize,
    disable: Range<usize>,
}

fn is_constraint_kind(lexeme: Option<&Lexeme>) -> bool {
    ["check", "references", "unique", "primary"]
        .iter()
        .any(|word| is_word(lexeme, word))
}

fn starts_column_part(lexemes: &[Lexeme], at: usize) -> bool {
    let lexeme = &lexemes[at];
    if lexeme.depth != 1 {
        return false;
    }
    let previous = at.checked_sub(1).and_then(|before| lexemes.get(before));
    let after_constraint_name = at
        .checked_sub(2)
        .is_some_and(|before| is_word(lexemes.get(before), "constraint"));
    if is_constraint_kind(Some(lexeme)) {
        return !after_constraint_name;
    }
    if is_word(Some(lexeme), "not") {
        return is_word(lexemes.get(at + 1), "null");
    }
    if is_word(Some(lexeme), "null") {
        return !["not", "set", "default", "on"]
            .iter()
            .any(|word| is_word(previous, word));
    }
    ["constraint", "default", "generated", "collate", "visible", "invisible"]
        .iter()
        .any(|word| is_word(Some(lexeme), word))
}

fn disabled_column_constraints(element: &Element) -> Vec<ColumnConstraint> {
    let lexemes = &element.lexemes;
    let starts: Vec<usize> = (1..lexemes.len())
        .filter(|&at| starts_column_part(lexemes, at))
        .collect();
    let mut found = Vec::new();
    for (number, &start) in starts.iter().enumerate() {
        let end = starts.get(number + 1).copied().unwrap_or(lexemes.len());
        let named = is_word(lexemes.get(start), "constraint");
        let kind_at = if named { start + 2 } else { start };
        if kind_at >= end || !is_constraint_kind(lexemes.get(kind_at)) {
            continue;
        }
        let part = &lexemes[start..end];
        let Some(disable) = disable_span(part, 1) else {
            continue;
        };
        let name = if named {
            name_of(lexemes.get(start + 1)).unwrap_or_default()
        } else {
            String::new()
        };
        found.push(ColumnConstraint {
            span: lexemes[start].span.start..lexemes[end - 1].span.end,
            name,
            kind_at,
            disable,
        });
    }
    found
}

fn without_span(text: &str, from: usize, to: usize, removed: &Range<usize>) -> String {
    if removed.start < from || removed.end > to {
        return text.get(from..to).unwrap_or("").trim().to_owned();
    }
    let before = text.get(from..removed.start).unwrap_or("");
    let after = text.get(removed.end..to).unwrap_or("");
    format!("{}{}", before.trim_end(), after).trim().to_owned()
}

fn table_level_clause(text: &str, element: &Element, constraint: &ColumnConstraint) -> String {
    let lexemes = &element.lexemes;
    let column = lexemes
        .first()
        .and_then(|lexeme| text.get(lexeme.span.clone()))
        .unwrap_or("");
    let kind = &lexemes[constraint.kind_at];
    let named = text
        .get(constraint.span.start..kind.span.start)
        .unwrap_or("");
    let end = constraint.span.end;
    let rest_after = |at: usize| {
        let from = lexemes.get(at).map_or(end, |lexeme| lexeme.span.end);
        without_span(text, from, end, &constraint.disable)
    };
    let joined = |head: String, rest: String| {
        if rest.is_empty() {
            head
        } else {
            format!("{head} {rest}")
        }
    };
    let body = if is_word(Some(kind), "references") {
        format!(
            "FOREIGN KEY ({column}) {}",
            without_span(text, kind.span.start, end, &constraint.disable)
        )
    } else if is_word(Some(kind), "unique") {
        joined(format!("UNIQUE ({column})"), rest_after(constraint.kind_at))
    } else if is_word(Some(kind), "primary") {
        let key_at = if is_word(lexemes.get(constraint.kind_at + 1), "key") {
            constraint.kind_at + 1
        } else {
            constraint.kind_at
        };
        joined(format!("PRIMARY KEY ({column})"), rest_after(key_at))
    } else {
        without_span(text, kind.span.start, end, &constraint.disable)
    };
    format!("{named}{body}")
}

fn without_spans(text: &str, whole: &Range<usize>, removed: &mut [Range<usize>]) -> String {
    removed.sort_by_key(|span| span.start);
    let mut out = String::new();
    let mut cursor = whole.start;
    for span in removed.iter() {
        if span.start < cursor || span.end > whole.end {
            continue;
        }
        out.push_str(text.get(cursor..span.start).unwrap_or("").trim_end());
        cursor = span.end;
    }
    out.push_str(text.get(cursor..whole.end).unwrap_or(""));
    out
}

struct RewrittenElements {
    kept: Vec<String>,
    notes: Vec<DifferenceNote>,
    disabled: Vec<DisabledElement>,
    changed: bool,
}

fn elements_between(lexemes: &[Lexeme], open: usize, close: usize) -> Vec<Element> {
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
    elements
}

fn rewritten_elements(
    text: &str,
    lexemes: &[Lexeme],
    open: usize,
    close: usize,
    table: &str,
    table_name: &str,
) -> RewrittenElements {
    let elements = elements_between(lexemes, open, close);
    let mut rewritten = RewrittenElements {
        kept: Vec::new(),
        notes: Vec::new(),
        disabled: Vec::new(),
        changed: false,
    };
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
                rewritten.disabled.push(DisabledElement {
                    table: table.to_owned(),
                    name,
                    clause: format!("{}{}", before.trim_end(), after).trim().to_owned(),
                    in_column: false,
                });
                rewritten.changed = true;
                continue;
            }
            rewritten.kept.push(element_text.to_owned());
            continue;
        }
        let column = name_of(first).unwrap_or_default();
        if is_virtual(element) {
            rewritten.notes.push(DifferenceNote::new(
                format!("{table_name}.{column}"),
                "virtual column not kept: Oracle computes it, so it is left out here",
            ));
            rewritten.changed = true;
            continue;
        }
        let mut removed = Vec::new();
        for constraint in disabled_column_constraints(element) {
            rewritten.disabled.push(DisabledElement {
                table: table.to_owned(),
                name: constraint.name.clone(),
                clause: table_level_clause(text, element, &constraint),
                in_column: true,
            });
            removed.push(constraint.span);
        }
        if removed.is_empty() {
            rewritten.kept.push(element_text.to_owned());
            continue;
        }
        rewritten.changed = true;
        rewritten
            .kept
            .push(without_spans(text, &element.span, &mut removed));
    }
    rewritten
}

fn closing_paren(lexemes: &[Lexeme], open: usize) -> Option<usize> {
    lexemes
        .iter()
        .skip(open + 1)
        .position(|lexeme| lexeme.piece == Piece::Close && lexeme.depth == 0)
        .map(|offset| offset + open + 1)
}

struct CreatedTable {
    table: String,
    table_name: String,
    open: usize,
    close: usize,
}

fn created_table(text: &str, lexemes: &[Lexeme]) -> Option<CreatedTable> {
    let table_at = lexemes
        .iter()
        .position(|lexeme| lexeme.depth == 0 && is_word(Some(lexeme), "table"))?;
    let open = lexemes
        .iter()
        .skip(table_at)
        .position(|lexeme| lexeme.piece == Piece::Open && lexeme.depth == 0)?
        + table_at;
    let table_start = lexemes.get(table_at + 1)?.span.start;
    let table = text
        .get(table_start..lexemes[open].span.start)?
        .trim()
        .to_owned();
    let close = closing_paren(lexemes, open)?;
    let table_name = lexemes[table_at + 1..open]
        .iter()
        .rev()
        .find_map(|lexeme| name_of(Some(lexeme)))
        .unwrap_or_default();
    Some(CreatedTable {
        table,
        table_name,
        open,
        close,
    })
}

fn with_elements(
    text: &str,
    lexemes: &[Lexeme],
    open: usize,
    close: usize,
    kept: &[String],
) -> Option<String> {
    let head = text.get(..lexemes[open].span.end)?;
    let tail = text.get(lexemes[close].span.start..)?;
    Some(format!("{head}{}{tail}", kept.join(", ")))
}

pub(crate) fn rewritten_table(text: &str) -> Option<TableRewrite> {
    let lexemes = lexemes(text);
    let created = created_table(text, &lexemes)?;
    let rewritten = rewritten_elements(
        text,
        &lexemes,
        created.open,
        created.close,
        &created.table,
        &created.table_name,
    );
    if !rewritten.changed || rewritten.kept.is_empty() {
        return None;
    }
    Some(TableRewrite {
        text: with_elements(text, &lexemes, created.open, created.close, &rewritten.kept)?,
        notes: rewritten.notes,
        disabled: rewritten.disabled,
    })
}

pub(crate) fn without_refused_identity(
    text: &str,
    error: &SqlError,
) -> Option<column_rewrite::TableRewrite> {
    let refused = error.state() == SqlState::FEATURE_NOT_SUPPORTED
        || error.state() == SqlState::INVALID_TABLE_DEFINITION;
    if !refused {
        return None;
    }
    let lexemes = lexemes(text);
    let created = created_table(text, &lexemes)?;
    let mut kept = Vec::new();
    let mut notes = Vec::new();
    for element in elements_between(&lexemes, created.open, created.close) {
        let element_text = text.get(element.span.clone()).unwrap_or("");
        let identity = if starts_constraint(element.lexemes.first()) {
            None
        } else {
            identity_span(&element)
        };
        let Some(span) = identity else {
            kept.push(element_text.to_owned());
            continue;
        };
        let column = name_of(element.lexemes.first()).unwrap_or_default();
        notes.push(DifferenceNote::new(
            format!("{}.{column}", created.table_name),
            IDENTITY_NOT_KEPT,
        ));
        kept.push(without_spans(text, &element.span, &mut [span]));
    }
    if notes.is_empty() {
        return None;
    }
    Some(column_rewrite::TableRewrite {
        text: with_elements(text, &lexemes, created.open, created.close, &kept)?,
        notes,
        identity_not_kept: true,
    })
}

pub(crate) fn rewritten_added(text: &str) -> Option<TableRewrite> {
    let lexemes = lexemes(text);
    if !is_word(lexemes.first(), "alter") || !is_word(lexemes.get(1), "table") {
        return None;
    }
    let add_at = lexemes
        .iter()
        .position(|lexeme| lexeme.depth == 0 && is_word(Some(lexeme), "add"))?;
    let open = add_at + 1;
    if lexemes.get(open)?.piece != Piece::Open || add_at < 3 {
        return None;
    }
    let close = closing_paren(&lexemes, open)?;
    let table = text
        .get(lexemes[2].span.start..lexemes[add_at - 1].span.end)?
        .trim()
        .to_owned();
    let table_name = lexemes[2..add_at]
        .iter()
        .rev()
        .find_map(|lexeme| name_of(Some(lexeme)))
        .unwrap_or_default();
    let rewritten = rewritten_elements(text, &lexemes, open, close, &table, &table_name);
    if !rewritten.changed {
        return None;
    }
    let text = if rewritten.kept.is_empty() {
        String::new()
    } else {
        with_elements(text, &lexemes, open, close, &rewritten.kept)?
    };
    Some(TableRewrite {
        text,
        notes: rewritten.notes,
        disabled: rewritten.disabled,
    })
}

pub(crate) fn without_disable(text: &str) -> Option<String> {
    let lexemes = lexemes(text);
    let span = disable_span(&lexemes, 0)?;
    let before = text.get(..span.start)?;
    let after = text.get(span.end..)?;
    Some(format!("{}{}", before.trim_end(), after))
}
