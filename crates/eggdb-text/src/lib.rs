//! EggDB's Unicode text handling (D233), from tables generated out of the Unicode Character
//! Database by `scripts/unicode/generate.py`: the four normalization forms of UAX #15
//! ([`normalize`]), full case folding ([`case_fold`]), and comparisons that ignore case
//! ([`compare_case_insensitive`]) or case and accents ([`compare_accent_insensitive`]), with
//! [`primary_key`] for indexing what the latter treats as equal.

mod tables;

use std::cmp::Ordering;
use tables::{CASE_FOLDING, CASE_FOLDING_DATA, COMBINING_CLASSES, COMPOSITIONS, DECOMPOSITION_DATA, DECOMPOSITIONS};

/// The Unicode version the tables come from.
pub const UNICODE_VERSION: &str = tables::UNICODE_VERSION;

const HANGUL_S_BASE: u32 = 0xAC00;
const HANGUL_L_BASE: u32 = 0x1100;
const HANGUL_V_BASE: u32 = 0x1161;
const HANGUL_T_BASE: u32 = 0x11A7;
const HANGUL_L_COUNT: u32 = 19;
const HANGUL_V_COUNT: u32 = 21;
const HANGUL_T_COUNT: u32 = 28;
const HANGUL_N_COUNT: u32 = HANGUL_V_COUNT * HANGUL_T_COUNT;
const HANGUL_S_COUNT: u32 = HANGUL_L_COUNT * HANGUL_N_COUNT;

/// A Unicode normalization form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Form {
    /// Canonical decomposition followed by canonical composition.
    Nfc,
    /// Canonical decomposition.
    Nfd,
    /// Compatibility decomposition followed by canonical composition.
    Nfkc,
    /// Compatibility decomposition.
    Nfkd,
}

impl Form {
    /// The form named `name` (`NFC`, `NFD`, `NFKC` or `NFKD`, any case).
    pub fn named(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "NFC" => Some(Form::Nfc),
            "NFD" => Some(Form::Nfd),
            "NFKC" => Some(Form::Nfkc),
            "NFKD" => Some(Form::Nfkd),
            _ => None,
        }
    }
}

/// The canonical combining class of `character`, 0 for a starter.
pub fn combining_class(character: char) -> u8 {
    let code = u32::from(character);
    COMBINING_CLASSES.binary_search_by_key(&code, |(entry, _)| *entry).map_or(0, |at| COMBINING_CLASSES[at].1)
}

fn decompose_into(code: u32, compatibility: bool, out: &mut Vec<u32>) {
    if (HANGUL_S_BASE..HANGUL_S_BASE + HANGUL_S_COUNT).contains(&code) {
        let index = code - HANGUL_S_BASE;
        out.push(HANGUL_L_BASE + index / HANGUL_N_COUNT);
        out.push(HANGUL_V_BASE + (index % HANGUL_N_COUNT) / HANGUL_T_COUNT);
        let trailing = index % HANGUL_T_COUNT;
        if trailing != 0 {
            out.push(HANGUL_T_BASE + trailing);
        }
        return;
    }
    match DECOMPOSITIONS.binary_search_by_key(&code, |(entry, ..)| *entry) {
        Ok(at) if compatibility || !DECOMPOSITIONS[at].3 => {
            let (_, offset, length, _) = DECOMPOSITIONS[at];
            for part in &DECOMPOSITION_DATA[usize::from(offset)..usize::from(offset) + usize::from(length)] {
                decompose_into(*part, compatibility, out);
            }
        }
        _ => out.push(code),
    }
}

fn class_of(code: u32) -> u8 {
    char::from_u32(code).map_or(0, combining_class)
}

fn decompose(text: &str, compatibility: bool) -> Vec<u32> {
    let mut out = Vec::with_capacity(text.len());
    for character in text.chars() {
        decompose_into(u32::from(character), compatibility, &mut out);
    }
    let mut start = 0;
    while start < out.len() {
        if class_of(out[start]) == 0 {
            start += 1;
            continue;
        }
        let end = start + out[start..].iter().take_while(|code| class_of(**code) != 0).count();
        out[start..end].sort_by_key(|code| class_of(*code));
        start = end;
    }
    out
}

fn compose_pair(first: u32, second: u32) -> Option<u32> {
    if (HANGUL_L_BASE..HANGUL_L_BASE + HANGUL_L_COUNT).contains(&first) && (HANGUL_V_BASE..HANGUL_V_BASE + HANGUL_V_COUNT).contains(&second) {
        return Some(HANGUL_S_BASE + ((first - HANGUL_L_BASE) * HANGUL_V_COUNT + (second - HANGUL_V_BASE)) * HANGUL_T_COUNT);
    }
    if (HANGUL_S_BASE..HANGUL_S_BASE + HANGUL_S_COUNT).contains(&first)
        && (first - HANGUL_S_BASE) % HANGUL_T_COUNT == 0
        && (HANGUL_T_BASE + 1..HANGUL_T_BASE + HANGUL_T_COUNT).contains(&second)
    {
        return Some(first + (second - HANGUL_T_BASE));
    }
    COMPOSITIONS.binary_search_by_key(&(first, second), |(left, right, _)| (*left, *right)).ok().map(|at| COMPOSITIONS[at].2)
}

fn compose(codes: &[u32]) -> Vec<u32> {
    let mut result: Vec<u32> = Vec::with_capacity(codes.len());
    let mut starter: Option<usize> = None;
    let mut last_class: Option<u8> = None;
    for &code in codes {
        let class = class_of(code);
        if let Some(at) = starter {
            let reachable = match last_class {
                None => true,
                Some(previous) => previous != 0 && previous < class,
            };
            if reachable && let Some(composite) = compose_pair(result[at], code) {
                result[at] = composite;
                continue;
            }
        }
        if class == 0 {
            starter = Some(result.len());
            last_class = None;
        } else {
            last_class = Some(class);
        }
        result.push(code);
    }
    result
}

fn to_string(codes: &[u32]) -> String {
    codes.iter().filter_map(|code| char::from_u32(*code)).collect()
}

/// `text` in normalization form `form`.
pub fn normalize(text: &str, form: Form) -> String {
    if text.is_ascii() {
        return text.to_owned();
    }
    match form {
        Form::Nfd => to_string(&decompose(text, false)),
        Form::Nfkd => to_string(&decompose(text, true)),
        Form::Nfc => to_string(&compose(&decompose(text, false))),
        Form::Nfkc => to_string(&compose(&decompose(text, true))),
    }
}

/// Whether `text` is already in normalization form `form`.
pub fn is_normalized(text: &str, form: Form) -> bool {
    text.is_ascii() || normalize(text, form) == text
}

/// `text` with full case folding applied (CaseFolding.txt, statuses C and F), the form two
/// strings share when they differ only by case.
pub fn case_fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        let code = u32::from(character);
        match CASE_FOLDING.binary_search_by_key(&code, |(entry, ..)| *entry) {
            Ok(at) => {
                let (_, offset, length) = CASE_FOLDING[at];
                out.extend(CASE_FOLDING_DATA[usize::from(offset)..usize::from(offset) + usize::from(length)].iter().filter_map(|code| char::from_u32(*code)));
            }
            Err(_) => out.push(character),
        }
    }
    out
}

/// The key two strings share when they differ only by case, accents and compatibility forms:
/// case-folded, compatibility-decomposed, with combining marks removed.
pub fn primary_key(text: &str) -> String {
    to_string(&decompose(&case_fold(text), true).into_iter().filter(|code| class_of(*code) == 0 && !is_mark(*code)).collect::<Vec<_>>())
}

fn is_mark(code: u32) -> bool {
    matches!(code, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F)
}

/// Orders `left` and `right` ignoring case (then by their canonical forms, then their code
/// points, so only identical strings are equal).
pub fn compare_case_insensitive(left: &str, right: &str) -> Ordering {
    case_fold(&normalize(left, Form::Nfd))
        .cmp(&case_fold(&normalize(right, Form::Nfd)))
        .then_with(|| normalize(left, Form::Nfd).cmp(&normalize(right, Form::Nfd)))
        .then_with(|| left.cmp(right))
}

/// Orders `left` and `right` by [`primary_key`] first, so `résumé`, `Resume` and `RESUME` sort
/// together, then case-insensitively, then exactly.
pub fn compare_accent_insensitive(left: &str, right: &str) -> Ordering {
    primary_key(left).cmp(&primary_key(right)).then_with(|| compare_case_insensitive(left, right))
}

#[cfg(test)]
mod tests;
