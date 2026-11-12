use eggdb_sql::{OutputColumn, QueryResult, SqlType, Value};
use std::collections::BTreeMap;

const COLUMN_SHORTEST: usize = "col".len();
const CLEAR_SHORTEST: usize = "cl".len();
const LONGEST_TEXT_WIDTH: usize = 60_000;

/// How SQL*Plus shows one column, set by `COLUMN name option ...`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ColumnFormat {
    heading: Option<String>,
    format: Option<Format>,
    null_text: Option<String>,
    hidden: bool,
    wrap: Wrap,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Wrap {
    #[default]
    Wrapped,
    WordWrapped,
    Truncated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Format {
    Text { width: usize },
    Number(NumberMask),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NumberMask {
    written: String,
    integer_digits: usize,
    fraction_digits: usize,
    grouped: bool,
    dollar: bool,
    zero_padded: bool,
}

/// The column formats an `eggdb shell` session holds, by upper-case column name.
pub(super) type ColumnFormats = BTreeMap<String, ColumnFormat>;

/// Whether `word` names SQL*Plus's `COLUMN` command, in full or abbreviated.
pub(super) fn is_column_command(word: &str) -> bool {
    word.len() >= COLUMN_SHORTEST && "column".starts_with(word)
}

/// Whether `word` and `rest` are SQL*Plus's `CLEAR COLUMNS`.
pub(super) fn is_clear_columns(word: &str, rest: &str) -> bool {
    let target = rest.trim().to_ascii_lowercase();
    word.len() >= CLEAR_SHORTEST
        && "clear".starts_with(word)
        && target.len() >= COLUMN_SHORTEST
        && "columns".starts_with(target.as_str())
}

fn words(text: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(&next) = chars.peek() {
        if next.is_whitespace() {
            chars.next();
            continue;
        }
        if next == '\'' || next == '"' {
            chars.next();
            let mut word = String::new();
            loop {
                match chars.next() {
                    Some(c) if c == next && chars.peek() == Some(&next) => {
                        chars.next();
                        word.push(c);
                    }
                    Some(c) if c == next => break,
                    Some(c) => word.push(c),
                    None => return Err("SP2-0758: quoted text is not closed".to_owned()),
                }
            }
            words.push(word);
            continue;
        }
        let mut word = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() {
                break;
            }
            word.push(c);
            chars.next();
        }
        words.push(word);
    }
    Ok(words)
}

fn option_named(word: &str, option: &str, shortest: usize) -> bool {
    let word = word.to_ascii_lowercase();
    word.len() >= shortest && option.starts_with(word.as_str())
}

fn parse_format(text: &str) -> Result<Format, String> {
    let invalid = || format!("SP2-0246: Illegal FORMAT string \"{text}\"");
    let lower = text.to_ascii_lowercase();
    if let Some(width) = lower.strip_prefix('a') {
        let width: usize = width.parse().map_err(|_| invalid())?;
        if width == 0 || width > LONGEST_TEXT_WIDTH {
            return Err(invalid());
        }
        return Ok(Format::Text { width });
    }
    let body = lower.strip_prefix('$').unwrap_or(&lower);
    if body.is_empty()
        || !body.chars().all(|c| matches!(c, '9' | '0' | ',' | '.'))
        || body.matches('.').count() > 1
        || body.starts_with(',')
    {
        return Err(invalid());
    }
    let (integer, fraction) = body.split_once('.').unwrap_or((body, ""));
    if fraction.contains(',') {
        return Err(invalid());
    }
    Ok(Format::Number(NumberMask {
        written: text.to_owned(),
        integer_digits: integer.chars().filter(|c| *c != ',').count(),
        fraction_digits: fraction.len(),
        grouped: integer.contains(','),
        dollar: lower.starts_with('$'),
        zero_padded: integer.starts_with('0'),
    }))
}

fn shown(name: &str, format: &ColumnFormat) -> String {
    let mut text = format!("COLUMN   {name} {}\n", if format.hidden { "OFF" } else { "ON" });
    if let Some(heading) = &format.heading {
        text.push_str(&format!("HEADING  '{heading}'\n"));
    }
    match &format.format {
        Some(Format::Text { width }) => text.push_str(&format!("FORMAT   a{width}\n")),
        Some(Format::Number(mask)) => text.push_str(&format!("FORMAT   {}\n", mask.written)),
        None => {}
    }
    if let Some(null_text) = &format.null_text {
        text.push_str(&format!("NULL     '{null_text}'\n"));
    }
    if format.hidden {
        text.push_str("NOPRINT\n");
    }
    match format.wrap {
        Wrap::Wrapped => text.push_str("wrap\n"),
        Wrap::WordWrapped => text.push_str("word_wrap\n"),
        Wrap::Truncated => text.push_str("truncate\n"),
    }
    text
}

/// Runs `COLUMN arguments`: lists every format, lists one column's, or sets options on one.
/// Returns the text to print.
pub(super) fn column_command(arguments: &str, formats: &mut ColumnFormats) -> Result<String, String> {
    let words = words(arguments)?;
    let Some((name, options)) = words.split_first() else {
        return Ok(formats
            .iter()
            .map(|(name, format)| shown(name, format))
            .collect::<Vec<_>>()
            .join("\n"));
    };
    let key = name.to_uppercase();
    if options.is_empty() {
        return match formats.get(&key) {
            Some(format) => Ok(shown(&key, format)),
            None => Err(format!("SP2-0046: COLUMN '{name}' not defined")),
        };
    }
    let mut format = formats.get(&key).cloned().unwrap_or_default();
    let mut at = 0;
    while at < options.len() {
        let option = &options[at];
        let value = options.get(at + 1);
        let needs_value = |what: &str| format!("SP2-0080: no COLUMN {what} specified");
        if option_named(option, "format", 3) {
            let value = value.ok_or_else(|| needs_value("FORMAT"))?;
            format.format = Some(parse_format(value)?);
            at += 2;
        } else if option_named(option, "heading", 3) {
            let value = value.ok_or_else(|| needs_value("HEADING"))?;
            format.heading = Some(value.replace('|', " "));
            at += 2;
        } else if option_named(option, "null", 3) {
            let value = value.ok_or_else(|| needs_value("NULL"))?;
            format.null_text = Some(value.clone());
            at += 2;
        } else if option_named(option, "noprint", 5) {
            format.hidden = true;
            at += 1;
        } else if option_named(option, "print", 3) {
            format.hidden = false;
            at += 1;
        } else if option_named(option, "wrapped", 3) {
            format.wrap = Wrap::Wrapped;
            at += 1;
        } else if option_named(option, "word_wrapped", 3) {
            format.wrap = Wrap::WordWrapped;
            at += 1;
        } else if option_named(option, "truncated", 3) {
            format.wrap = Wrap::Truncated;
            at += 1;
        } else if option_named(option, "clear", 3) {
            format = ColumnFormat::default();
            at += 1;
        } else if option_named(option, "on", 2) || option_named(option, "off", 3) {
            at += 1;
        } else {
            return Err(format!("SP2-0158: unknown COLUMN option \"{option}\""));
        }
    }
    if format == ColumnFormat::default() {
        formats.remove(&key);
    } else {
        formats.insert(key, format);
    }
    Ok(String::new())
}

fn decimal_text(value: &Value) -> Option<String> {
    match value {
        Value::SmallInt(_)
        | Value::Integer(_)
        | Value::BigInt(_)
        | Value::Real(_)
        | Value::Double(_)
        | Value::Numeric(_) => value.to_text(),
        _ => None,
    }
}

fn rounded(text: &str, fraction_digits: usize) -> Option<(bool, String, String)> {
    let number: f64 = text.parse().ok()?;
    if !number.is_finite() {
        return None;
    }
    let negative = number < 0.0;
    let fixed = format!("{:.*}", fraction_digits, number.abs());
    let (integer, fraction) = fixed.split_once('.').unwrap_or((fixed.as_str(), ""));
    Some((negative, integer.to_owned(), fraction.to_owned()))
}

fn grouped(digits: &str) -> String {
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (position, digit) in digits.chars().enumerate() {
        if position > 0 && (digits.len() - position) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn masked(mask: &NumberMask, text: &str) -> String {
    let width = mask.written.len() + 1;
    let overflow = "#".repeat(width);
    let Some((negative, integer, fraction)) = rounded(text, mask.fraction_digits) else {
        return overflow;
    };
    let integer = if integer == "0" && mask.integer_digits == 0 {
        String::new()
    } else {
        integer
    };
    if integer.len() > mask.integer_digits {
        return overflow;
    }
    let integer = if mask.zero_padded {
        format!("{integer:0>width$}", width = mask.integer_digits)
    } else if integer.is_empty() && mask.fraction_digits == 0 {
        "0".to_owned()
    } else {
        integer
    };
    let integer = if mask.grouped { grouped(&integer) } else { integer };
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if mask.dollar {
        out.push('$');
    }
    out.push_str(&integer);
    if mask.fraction_digits > 0 {
        out.push('.');
        out.push_str(&fraction);
    }
    format!("{out:>width$}")
}

fn cut_to(text: &str, width: usize) -> String {
    let kept: String = text.chars().take(width).collect();
    format!("{kept:<width$}")
}

fn wrapped_lines(text: &str, width: usize, wrap: Wrap) -> Vec<String> {
    let width = width.max(1);
    if wrap == Wrap::Truncated {
        return vec![cut_to(text.lines().next().unwrap_or_default(), width)];
    }
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let characters: Vec<char> = paragraph.chars().collect();
        if characters.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut start = 0;
        while start < characters.len() {
            let mut end = (start + width).min(characters.len());
            if wrap == Wrap::WordWrapped
                && end < characters.len()
                && let Some(space) = characters[start..end].iter().rposition(|character| *character == ' ')
                && space > 0
            {
                end = start + space;
            }
            lines.push(characters[start..end].iter().collect::<String>().trim_end().to_owned());
            start = end;
            if wrap == Wrap::WordWrapped {
                while start < characters.len() && characters[start] == ' ' {
                    start += 1;
                }
            }
        }
    }
    lines.into_iter().map(|line| format!("{line:<width$}")).collect()
}

fn text_width(format: &ColumnFormat) -> Option<usize> {
    match &format.format {
        Some(Format::Text { width }) => Some(*width),
        _ => None,
    }
}

fn shown_value(value: &Value, format: &ColumnFormat) -> Value {
    if matches!(value, Value::Null) {
        return match &format.null_text {
            Some(text) => Value::Text(text.clone()),
            None => Value::Null,
        };
    }
    match &format.format {
        Some(Format::Text { width }) => {
            Value::Text(cut_to(&value.to_text().unwrap_or_default(), *width))
        }
        Some(Format::Number(mask)) => match decimal_text(value) {
            Some(text) => Value::Text(masked(mask, &text)),
            None => value.clone(),
        },
        None => value.clone(),
    }
}

/// `result` as SQL*Plus shows it under `formats`: columns set `NOPRINT` left out, headings
/// replaced, and values laid out by their `FORMAT` and `NULL` text.
pub(super) fn formatted(result: &QueryResult, formats: &ColumnFormats) -> QueryResult {
    let mut shown = result.clone();
    let Some(columns) = &result.columns else {
        return shown;
    };
    if formats.is_empty() {
        return shown;
    }
    let settings: Vec<Option<&ColumnFormat>> = columns
        .iter()
        .map(|column| formats.get(&column.name.to_uppercase()))
        .collect();
    let kept: Vec<usize> = settings
        .iter()
        .enumerate()
        .filter(|(_, setting)| !setting.is_some_and(|format| format.hidden))
        .map(|(position, _)| position)
        .collect();
    shown.columns = Some(
        kept.iter()
            .map(|&position| {
                let column = &columns[position];
                match settings[position] {
                    Some(format) => OutputColumn {
                        name: format.heading.clone().unwrap_or_else(|| column.name.clone()),
                        sql_type: if format.format.is_some() || format.null_text.is_some() {
                            Some(SqlType::Text)
                        } else {
                            column.sql_type
                        },
                    },
                    None => column.clone(),
                }
            })
            .collect(),
    );
    let mut rows = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        let mut first: Vec<Value> = Vec::with_capacity(kept.len());
        let mut continued: Vec<Vec<String>> = Vec::with_capacity(kept.len());
        for &position in &kept {
            let value = row.get(position).cloned().unwrap_or(Value::Null);
            let setting = settings[position];
            let lines = match (setting, setting.and_then(text_width)) {
                (Some(format), Some(width)) if !matches!(value, Value::Null) => {
                    wrapped_lines(&value.to_text().unwrap_or_default(), width, format.wrap)
                }
                _ => Vec::new(),
            };
            match lines.split_first() {
                Some((head, rest)) => {
                    first.push(Value::Text(head.clone()));
                    continued.push(rest.to_vec());
                }
                None => {
                    first.push(match setting {
                        Some(format) => shown_value(&value, format),
                        None => value,
                    });
                    continued.push(Vec::new());
                }
            }
        }
        let extra = continued.iter().map(Vec::len).max().unwrap_or(0);
        rows.push(first);
        for line in 0..extra {
            rows.push(
                continued
                    .iter()
                    .map(|lines| Value::Text(lines.get(line).cloned().unwrap_or_default()))
                    .collect(),
            );
        }
    }
    shown.rows = rows;
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result() -> QueryResult {
        let columns = vec![
            OutputColumn {
                name: "ENAME".to_owned(),
                sql_type: Some(SqlType::Text),
            },
            OutputColumn {
                name: "SAL".to_owned(),
                sql_type: Some(SqlType::Double),
            },
            OutputColumn {
                name: "SECRET".to_owned(),
                sql_type: Some(SqlType::Text),
            },
        ];
        QueryResult {
            tag: "SELECT 2".to_owned(),
            columns: Some(columns),
            rows: vec![
                vec![
                    Value::Text("KING".to_owned()),
                    Value::Double(5000.0),
                    Value::Text("x".to_owned()),
                ],
                vec![Value::Null, Value::Double(1234567.891), Value::Null],
            ],
            notices: Vec::new(),
            warnings: Vec::new(),
            last_insert_id: None,
            last_insert_rowid: None,
            last_identity_value: None,
            info_messages: Vec::new(),
        }
    }

    #[test]
    fn column_sets_lists_and_clears_formats() {
        let mut formats = ColumnFormats::new();
        assert_eq!(
            column_command("ename heading 'Employee|Name' format a6", &mut formats),
            Ok(String::new())
        );
        let listed = column_command("ENAME", &mut formats).expect("listed");
        assert!(listed.contains("HEADING  'Employee Name'"), "{listed}");
        assert!(listed.contains("FORMAT   a6"), "{listed}");
        assert!(column_command("nope", &mut formats).is_err());
        assert!(column_command("sal format 9x9", &mut formats).is_err());
        assert!(column_command("sal justify left", &mut formats).is_err());
        column_command("ename clear", &mut formats).expect("cleared");
        assert!(formats.is_empty());
        assert!(is_column_command("col"));
        assert!(!is_column_command("co"));
        assert!(is_clear_columns("cl", "col"));
    }

    #[test]
    fn formats_shape_headings_values_nulls_and_hidden_columns() {
        let mut formats = ColumnFormats::new();
        column_command("ename heading Who format a3 null '(none)'", &mut formats).expect("ename");
        column_command("sal format $9,999.99", &mut formats).expect("sal");
        column_command("secret noprint", &mut formats).expect("secret");
        let shown = formatted(&result(), &formats);
        let names: Vec<&str> = shown
            .columns
            .as_ref()
            .expect("columns")
            .iter()
            .map(|column| column.name.as_str())
            .collect();
        assert_eq!(names, vec!["Who", "SAL"]);
        assert_eq!(shown.rows[0], vec![
            Value::Text("KIN".to_owned()),
            Value::Text(" $5,000.00".to_owned())
        ]);
        assert_eq!(shown.rows[1], vec![
            Value::Text("(none)".to_owned()),
            Value::Text("##########".to_owned())
        ]);
    }

    #[test]
    fn text_columns_wrap_by_characters_or_words_or_truncate() {
        assert_eq!(wrapped_lines("abcdefgh", 3, Wrap::Wrapped), vec!["abc", "def", "gh "]);
        assert_eq!(wrapped_lines("one two three", 7, Wrap::WordWrapped), vec!["one two", "three  "]);
        assert_eq!(wrapped_lines("abcdefgh", 3, Wrap::Truncated), vec!["abc"]);
        let mut formats = ColumnFormats::new();
        column_command("ename format a2", &mut formats).unwrap();
        let shown = formatted(&result(), &formats);
        assert!(shown.rows.len() > result().rows.len());
    }

    #[test]
    fn number_masks_round_pad_and_group() {
        let mask = |text: &str| match parse_format(text) {
            Ok(Format::Number(mask)) => mask,
            other => panic!("{text}: {other:?}"),
        };
        assert_eq!(masked(&mask("999.9"), "12.34"), "  12.3");
        assert_eq!(masked(&mask("0999"), "7"), " 0007");
        assert_eq!(masked(&mask("99,999"), "-1234"), " -1,234");
        assert_eq!(masked(&mask("99"), "123"), "###");
        assert_eq!(masked(&mask(".99"), "0.456"), " .46");
    }
}
