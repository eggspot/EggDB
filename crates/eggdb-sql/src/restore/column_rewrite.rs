use super::classify_sqlserver::sqlserver_name_key;
use super::script::DifferenceNote;
use super::words::{Token, Words, plain_qualified_name};
use crate::error::{SqlError, SqlState};
use std::collections::HashMap;
use std::ops::Range;

const MAX_ALIAS_TYPES: usize = 10_000;
const BIGINT_DECIMAL_DIGITS: u32 = 18;
const DEFAULT_DECIMAL_PRECISION: u32 = 18;
const ELEMENT_WORDS: [&str; 7] = [
    "constraint",
    "primary",
    "unique",
    "check",
    "foreign",
    "index",
    "period",
];
const IDENTITY_NOT_FIRST_KEY: &str = "IDENTITY not kept: EggDB needs an identity column to be the first column of the table's primary key; new rows must give its value";
const XML_AS_TEXT: &str = "xml column kept as nvarchar(max): its values are text here, without XML methods or XML schema collections";

pub(crate) struct TableRewrite {
    pub(crate) text: String,
    pub(crate) notes: Vec<DifferenceNote>,
    pub(crate) identity_not_kept: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AliasType {
    pub(crate) base: String,
    base_name: String,
    base_arguments: Option<String>,
    pub(crate) not_null: bool,
}

#[derive(Debug, Default)]
pub(crate) struct AliasTypes {
    types: HashMap<String, AliasType>,
}

impl AliasTypes {
    pub(crate) fn learn(&mut self, text: &str) {
        let Some((name, alias)) = alias_type_definition(text) else {
            return;
        };
        let key = sqlserver_name_key(&name);
        if self.types.len() < MAX_ALIAS_TYPES || self.types.contains_key(&key) {
            self.types.insert(key, alias);
        }
    }

    fn get(&self, name: &str) -> Option<&AliasType> {
        if self.types.is_empty() {
            return None;
        }
        self.types.get(&sqlserver_name_key(name))
    }
}

struct Spanned {
    token: Token,
    range: Range<usize>,
}

struct TypeReference {
    name: String,
    range: Range<usize>,
    arguments: Option<Range<usize>>,
    next: usize,
}

enum Element {
    Kept,
    Changed(String),
    Dropped,
}

pub(crate) fn rewritten_table(text: &str, aliases: &AliasTypes) -> Option<TableRewrite> {
    let table = created_table(text)?;
    let mut rewrite = TableRewrite {
        text: String::new(),
        notes: Vec::new(),
        identity_not_kept: false,
    };
    let body = rebuilt_body(text, |element| {
        rewritten_element(element, &table, aliases, &mut rewrite)
    })?;
    rewrite.text = body;
    Some(rewrite)
}

pub(crate) fn without_refused_identity(text: &str, error: &SqlError) -> Option<TableRewrite> {
    if error.state() != SqlState::INVALID_TABLE_DEFINITION || error.vendor_code() != Some(40517) {
        return None;
    }
    let table = created_table(text)?;
    let mut notes = Vec::new();
    let body = rebuilt_body(text, |element| {
        let tokens = spanned_tokens(element);
        let Some(column) = column_name(&tokens) else {
            return Element::Kept;
        };
        let Some(at) = tokens
            .iter()
            .skip(1)
            .position(|spanned| spanned.token.is_word("identity"))
            .map(|offset| offset + 1)
        else {
            return Element::Kept;
        };
        let Some(range) = identity_range(&tokens, at) else {
            return Element::Kept;
        };
        notes.push(DifferenceNote::new(
            format!("{table}.{column}"),
            IDENTITY_NOT_FIRST_KEY,
        ));
        Element::Changed(applied(element, &[(range, String::new())]))
    })?;
    Some(TableRewrite {
        text: body,
        notes,
        identity_not_kept: true,
    })
}

pub(crate) fn alias_type_definition(text: &str) -> Option<(String, AliasType)> {
    let mut head = Words::sqlserver(text);
    if !(head.next()?.is_word("create") && head.next()?.is_word("type")) {
        return None;
    }
    let tokens = spanned_tokens(text);
    let plain: Vec<Token> = tokens.iter().map(|spanned| spanned.token.clone()).collect();
    let mut at = 2;
    let name = plain_qualified_name(&plain, &mut at)?;
    if !tokens.get(at)?.token.is_word("from") {
        return None;
    }
    let base = type_reference(&tokens, at + 1)?;
    let not_null = tokens
        .get(base.next..)
        .unwrap_or_default()
        .windows(2)
        .any(|pair| pair[0].token.is_word("not") && pair[1].token.is_word("null"));
    let alias = AliasType {
        base: text.get(base.range.clone())?.to_owned(),
        base_arguments: base
            .arguments
            .and_then(|range| text.get(range).map(str::to_owned)),
        base_name: base.name,
        not_null,
    };
    Some((name, alias))
}

fn created_table(text: &str) -> Option<String> {
    let head: Vec<Token> = Words::sqlserver(text).take(8).collect();
    if !(head.first()?.is_word("create") && head.get(1)?.is_word("table")) {
        return None;
    }
    let mut at = 2;
    plain_qualified_name(&head, &mut at)
}

fn rebuilt_body(text: &str, mut element_of: impl FnMut(&str) -> Element) -> Option<String> {
    let (body, parts) = table_body(text)?;
    let mut kept: Vec<String> = Vec::new();
    let mut changed = false;
    for part in parts {
        let element = text.get(part)?;
        match element_of(element) {
            Element::Kept => kept.push(element.to_owned()),
            Element::Changed(rewritten) => {
                changed = true;
                kept.push(rewritten);
            }
            Element::Dropped => changed = true,
        }
    }
    if !changed || kept.is_empty() {
        return None;
    }
    let mut rewritten = String::with_capacity(text.len());
    rewritten.push_str(text.get(..body.start)?);
    rewritten.push_str(&kept.join(","));
    rewritten.push_str(text.get(body.end..)?);
    Some(rewritten)
}

fn column_name(tokens: &[Spanned]) -> Option<&str> {
    let first = tokens.first()?;
    if let Token::Word(word) = &first.token
        && ELEMENT_WORDS.contains(&word.as_str())
    {
        return None;
    }
    first.token.plain_name_part()
}

fn rewritten_element(
    element: &str,
    table: &str,
    aliases: &AliasTypes,
    rewrite: &mut TableRewrite,
) -> Element {
    let tokens = spanned_tokens(element);
    let Some(column) = column_name(&tokens) else {
        return Element::Kept;
    };
    let object = format!("{table}.{column}");
    if tokens.get(1).is_some_and(|spanned| spanned.token.is_word("as")) {
        rewrite
            .notes
            .push(DifferenceNote::new(object, "computed column not kept"));
        return Element::Dropped;
    }
    let Some(declared) = type_reference(&tokens, 1) else {
        return Element::Kept;
    };
    if matches!(declared.name.as_str(), "timestamp" | "rowversion") {
        rewrite
            .notes
            .push(DifferenceNote::new(object, "rowversion column not kept"));
        return Element::Dropped;
    }
    let rest = tokens.get(declared.next..).unwrap_or_default();
    let alias = aliases.get(&declared.name);
    let (base_name, base_arguments, mut replacement) = match alias {
        Some(alias) => (
            alias.base_name.clone(),
            alias.base_arguments.clone(),
            Some(alias.base.clone()),
        ),
        None => (
            declared.name.clone(),
            declared
                .arguments
                .clone()
                .and_then(|range| element.get(range).map(str::to_owned)),
            None,
        ),
    };
    if matches!(base_name.as_str(), "xml" | "sys.xml") {
        replacement = Some("nvarchar(max)".to_owned());
        rewrite
            .notes
            .push(DifferenceNote::new(object.clone(), XML_AS_TEXT));
    }
    let mut identity_removed = None;
    if matches!(base_name.as_str(), "decimal" | "numeric" | "dec")
        && let Some(offset) = rest
            .iter()
            .position(|spanned| spanned.token.is_word("identity"))
    {
        let (precision, scale) = decimal_arguments(base_arguments.as_deref());
        if scale == 0 && precision <= BIGINT_DECIMAL_DIGITS {
            replacement = Some("bigint".to_owned());
            rewrite.notes.push(DifferenceNote::new(
                object,
                format!("IDENTITY of numeric({precision}, 0) kept as a bigint column"),
            ));
        } else if let Some(range) = identity_range(&tokens, declared.next + offset) {
            identity_removed = Some(range);
            rewrite.identity_not_kept = true;
            rewrite.notes.push(DifferenceNote::new(
                object,
                format!(
                    "IDENTITY not kept: EggDB has no IDENTITY on numeric({precision}, {scale}); new rows must give its value"
                ),
            ));
        }
    }
    if let Some(alias) = alias
        && alias.not_null
        && !rest.iter().any(|spanned| spanned.token.is_word("null"))
        && let Some(text) = &mut replacement
    {
        text.push_str(" NOT NULL");
    }
    let mut edits = Vec::new();
    if let Some(text) = replacement {
        edits.push((declared.range, text));
    }
    if let Some(range) = identity_removed {
        edits.push((range, String::new()));
    }
    if edits.is_empty() {
        return Element::Kept;
    }
    Element::Changed(applied(element, &edits))
}

fn decimal_arguments(arguments: Option<&str>) -> (u32, u32) {
    let Some(arguments) = arguments else {
        return (DEFAULT_DECIMAL_PRECISION, 0);
    };
    let mut parts = arguments.split(',').map(|part| part.trim().parse::<u32>());
    let precision = match parts.next() {
        Some(Ok(precision)) => precision,
        _ => u32::MAX,
    };
    let scale = match parts.next() {
        Some(Ok(scale)) => scale,
        Some(Err(_)) => u32::MAX,
        None => 0,
    };
    (precision, scale)
}

fn identity_range(tokens: &[Spanned], at: usize) -> Option<Range<usize>> {
    let identity = tokens.get(at)?;
    let end = match tokens.get(at + 1) {
        Some(next) if next.token.is_symbol(b'(') => tokens.get(closing(tokens, at + 1)?)?.range.end,
        _ => identity.range.end,
    };
    Some(identity.range.start..end)
}

fn applied(text: &str, edits: &[(Range<usize>, String)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (range, replacement) in edits {
        if range.start < at {
            continue;
        }
        out.push_str(text.get(at..range.start).unwrap_or(""));
        out.push_str(replacement);
        at = range.end;
    }
    out.push_str(text.get(at..).unwrap_or(""));
    out
}

fn type_reference(tokens: &[Spanned], at: usize) -> Option<TypeReference> {
    let first = tokens.get(at)?;
    let mut name = first.token.plain_name_part()?.to_lowercase();
    let start = first.range.start;
    let mut end = first.range.end;
    let mut next = at + 1;
    while tokens
        .get(next)
        .is_some_and(|spanned| spanned.token.is_symbol(b'.'))
    {
        let Some(part) = tokens.get(next + 1) else {
            break;
        };
        let Some(text) = part.token.plain_name_part() else {
            break;
        };
        name.push('.');
        name.push_str(&text.to_lowercase());
        end = part.range.end;
        next += 2;
    }
    let mut arguments = None;
    if tokens
        .get(next)
        .is_some_and(|spanned| spanned.token.is_symbol(b'('))
    {
        let close = closing(tokens, next)?;
        arguments = Some(tokens.get(next)?.range.end..tokens.get(close)?.range.start);
        end = tokens.get(close)?.range.end;
        next = close + 1;
    }
    Some(TypeReference {
        name,
        range: start..end,
        arguments,
        next,
    })
}

fn closing(tokens: &[Spanned], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, spanned) in tokens.iter().enumerate().skip(open) {
        if spanned.token.is_symbol(b'(') {
            depth += 1;
        } else if spanned.token.is_symbol(b')') {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

fn spanned_tokens(text: &str) -> Vec<Spanned> {
    let mut words = Words::sqlserver(text);
    let mut tokens = Vec::new();
    loop {
        let before = words.offset();
        let Some(token) = words.next() else {
            return tokens;
        };
        let end = words.offset();
        let start = after_blanks(text, before).min(end);
        tokens.push(Spanned {
            token,
            range: start..end,
        });
    }
}

fn after_blanks(text: &str, mut at: usize) -> usize {
    let bytes = text.as_bytes();
    loop {
        match (bytes.get(at), bytes.get(at + 1)) {
            (Some(byte), _) if byte.is_ascii_whitespace() => at += 1,
            (Some(b'-'), Some(b'-')) => {
                while bytes.get(at).is_some_and(|&byte| byte != b'\n') {
                    at += 1;
                }
            }
            (Some(b'/'), Some(b'*')) => {
                let mut depth = 1usize;
                at += 2;
                while depth > 0 && at < bytes.len() {
                    if bytes.get(at) == Some(&b'/') && bytes.get(at + 1) == Some(&b'*') {
                        depth += 1;
                        at += 2;
                    } else if bytes.get(at) == Some(&b'*') && bytes.get(at + 1) == Some(&b'/') {
                        depth -= 1;
                        at += 2;
                    } else {
                        at += 1;
                    }
                }
            }
            _ => return at,
        }
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
