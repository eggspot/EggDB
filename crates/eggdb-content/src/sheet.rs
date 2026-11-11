use crate::text::decode_entities;
use crate::zip::write_stored;
use crate::{ContentError, ZipArchive};

const MAX_COLUMNS: usize = 16_384;
const MAX_ROWS: usize = 1_048_576;
const SLOTS_PER_SHEET_BYTE: usize = 8;
const MICROS_PER_DAY: i64 = 86_400_000_000;
const DAYS_FROM_1899_12_30_TO_1970: i64 = 25_569;
const DAYS_FROM_1904_01_01_TO_1970: i64 = 24_107;
const FIRST_SERIAL_AFTER_FALSE_LEAP_DAY: f64 = 61.0;
const FALSE_LEAP_DAY_SERIAL: f64 = 60.0;
const MAX_SHEET_NAME: usize = 31;
const DATE_STYLE: usize = 1;
const TIMESTAMP_STYLE: usize = 2;

const CONTENT_TYPES: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>",
    "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">",
    "<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>",
    "<Default Extension=\"xml\" ContentType=\"application/xml\"/>",
    "<Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>",
    "<Override PartName=\"/xl/worksheets/sheet1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>",
    "<Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>",
    "</Types>"
);
const ROOT_RELATIONSHIPS: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>",
    "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
    "<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/>",
    "</Relationships>"
);
const WORKBOOK_RELATIONSHIPS: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>",
    "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
    "<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet1.xml\"/>",
    "<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>",
    "</Relationships>"
);
const WORKBOOK_HEAD: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>",
    "<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" ",
    "xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">",
    "<sheets><sheet name=\""
);
const WORKBOOK_TAIL: &str = "\" sheetId=\"1\" r:id=\"rId1\"/></sheets></workbook>";
const STYLES: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>",
    "<styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">",
    "<fonts count=\"1\"><font><sz val=\"11\"/><name val=\"Calibri\"/></font></fonts>",
    "<fills count=\"2\"><fill><patternFill patternType=\"none\"/></fill><fill><patternFill patternType=\"gray125\"/></fill></fills>",
    "<borders count=\"1\"><border><left/><right/><top/><bottom/><diagonal/></border></borders>",
    "<cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs>",
    "<cellXfs count=\"3\">",
    "<xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\"/>",
    "<xf numFmtId=\"14\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyNumberFormat=\"1\"/>",
    "<xf numFmtId=\"22\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyNumberFormat=\"1\"/>",
    "</cellXfs>",
    "<cellStyles count=\"1\"><cellStyle name=\"Normal\" xfId=\"0\" builtinId=\"0\"/></cellStyles>",
    "</styleSheet>"
);
const SHEET_HEAD: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>",
    "<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData>"
);
const SHEET_TAIL: &str = "</sheetData></worksheet>";

/// One cell of a worksheet.
#[derive(Debug, Clone, PartialEq)]
pub enum SheetCell {
    /// No value.
    Empty,
    /// A number, as the decimal text the sheet stores.
    Number(String),
    /// `TRUE` or `FALSE`.
    Boolean(bool),
    /// Text, including formula results and error values such as `#DIV/0!`.
    Text(String),
    /// A date-formatted whole number: days since 1970-01-01.
    Date(i64),
    /// A date-formatted number with a time of day: microseconds since 1970-01-01.
    Timestamp(i64),
}

struct Tag<'x> {
    name: &'x str,
    attributes: &'x str,
    closing: bool,
    empty: bool,
    start: usize,
    end: usize,
}

fn unreadable(message: &str) -> ContentError {
    ContentError(format!("not a readable Excel workbook: {message}"))
}

fn local_name(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

fn next_tag(xml: &str, from: usize) -> Option<Tag<'_>> {
    let start = from + xml.get(from..)?.find('<')?;
    let end = start + xml.get(start..)?.find('>')?;
    let inner = &xml[start + 1..end];
    let closing = inner.starts_with('/');
    let empty = inner.ends_with('/');
    let inner = inner.trim_start_matches('/').trim_end_matches('/');
    let name_end = inner.find(char::is_whitespace).unwrap_or(inner.len());
    Some(Tag {
        name: local_name(&inner[..name_end]),
        attributes: &inner[name_end..],
        closing,
        empty,
        start,
        end: end + 1,
    })
}

fn attribute<'x>(attributes: &'x str, wanted: &str) -> Option<&'x str> {
    let mut rest = attributes;
    loop {
        let trimmed = rest.trim_start();
        let equals = trimmed.find('=')?;
        let name = trimmed[..equals].trim();
        let after = trimmed[equals + 1..].trim_start();
        let quote = after.chars().next().filter(|quote| matches!(quote, '"' | '\''))?;
        let close = after[1..].find(quote)?;
        if local_name(name) == wanted {
            return Some(&after[1..1 + close]);
        }
        rest = &after[2 + close..];
    }
}

fn starts_escape(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= 7
        && bytes[0] == b'_'
        && bytes[1] == b'x'
        && bytes[2..6].iter().all(u8::is_ascii_hexdigit)
        && bytes[6] == b'_'
}

fn decoded(raw: &str) -> String {
    let mut entities = String::with_capacity(raw.len());
    decode_entities(raw, &mut entities);
    if !entities.contains("_x") {
        return entities;
    }
    let mut out = String::with_capacity(entities.len());
    let mut rest = entities.as_str();
    while let Some(at) = rest.find("_x") {
        out.push_str(&rest[..at]);
        let candidate = &rest[at..];
        let character = starts_escape(candidate)
            .then(|| u32::from_str_radix(&candidate[2..6], 16).ok())
            .flatten()
            .and_then(char::from_u32);
        match character {
            Some(character) => {
                out.push(character);
                rest = &candidate[7..];
            }
            None => {
                out.push_str("_x");
                rest = &candidate[2..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn text_until_tag(xml: &str, from: usize) -> (String, usize) {
    let stop = xml[from..].find('<').map_or(xml.len(), |offset| from + offset);
    (decoded(&xml[from..stop]), stop)
}

fn closing_of(xml: &str, from: usize, name: &str) -> usize {
    let mut at = from;
    while let Some(tag) = next_tag(xml, at) {
        if tag.closing && tag.name == name {
            return tag.start;
        }
        at = tag.end;
    }
    xml.len()
}

fn joined_text(xml: &str) -> String {
    let mut out = String::new();
    let mut at = 0;
    let mut phonetic = 0usize;
    while let Some(tag) = next_tag(xml, at) {
        at = tag.end;
        match (tag.name, tag.closing, tag.empty) {
            ("rPh", false, false) => phonetic += 1,
            ("rPh", true, _) => phonetic = phonetic.saturating_sub(1),
            ("t", false, false) if phonetic == 0 => {
                let (text, stop) = text_until_tag(xml, at);
                out.push_str(&text);
                at = stop;
            }
            _ => {}
        }
    }
    out
}

fn element_text(xml: &str, wanted: &str) -> Option<String> {
    let mut at = 0;
    while let Some(tag) = next_tag(xml, at) {
        at = tag.end;
        if tag.name == wanted && !tag.closing {
            if tag.empty {
                return Some(String::new());
            }
            return Some(text_until_tag(xml, at).0);
        }
    }
    None
}

fn shared_strings(xml: &str) -> Vec<String> {
    let mut strings = Vec::new();
    let mut at = 0;
    while let Some(tag) = next_tag(xml, at) {
        at = tag.end;
        if tag.name != "si" || tag.closing {
            continue;
        }
        if tag.empty {
            strings.push(String::new());
            continue;
        }
        let close = closing_of(xml, at, "si");
        strings.push(joined_text(&xml[at..close]));
        at = close;
    }
    strings
}

fn format_kind(code: &str) -> Option<bool> {
    let mut plain = String::new();
    let mut characters = code.chars();
    let mut quoted = false;
    let mut bracketed = false;
    while let Some(character) = characters.next() {
        match character {
            '"' => quoted = !quoted,
            '\\' if !quoted => {
                characters.next();
            }
            '[' if !quoted => bracketed = true,
            ']' if !quoted => bracketed = false,
            _ if quoted || bracketed => {}
            other => plain.push(other.to_ascii_lowercase()),
        }
    }
    let section = plain.split(';').next().unwrap_or_default();
    let date = section.contains('y') || section.contains('d');
    let time = section.contains('h') || section.contains('s');
    match (date, time) {
        (false, false) => None,
        (true, false) => Some(false),
        (_, true) => Some(true),
    }
}

fn builtin_format_kind(id: u32) -> Option<bool> {
    match id {
        14..=17 => Some(false),
        18..=22 | 45..=47 => Some(true),
        _ => None,
    }
}

fn date_styles(xml: &str) -> Vec<Option<bool>> {
    let mut custom: Vec<(u32, Option<bool>)> = Vec::new();
    let mut styles = Vec::new();
    let mut in_cell_formats = false;
    let mut at = 0;
    while let Some(tag) = next_tag(xml, at) {
        at = tag.end;
        match (tag.name, tag.closing) {
            ("numFmt", false) => {
                let id = attribute(tag.attributes, "numFmtId").and_then(|id| id.parse().ok());
                let code = attribute(tag.attributes, "formatCode").map(decoded);
                if let (Some(id), Some(code)) = (id, code) {
                    custom.push((id, format_kind(&code)));
                }
            }
            ("cellXfs", closing) => in_cell_formats = !closing && !tag.empty,
            ("xf", false) if in_cell_formats => {
                let id = attribute(tag.attributes, "numFmtId")
                    .and_then(|id| id.parse::<u32>().ok())
                    .unwrap_or(0);
                let kind = custom
                    .iter()
                    .find(|(known, _)| *known == id)
                    .map_or_else(|| builtin_format_kind(id), |(_, kind)| *kind);
                styles.push(kind);
            }
            _ => {}
        }
    }
    styles
}

fn date_cell(serial: f64, has_time: bool, epoch: i64) -> SheetCell {
    let adjusted = if epoch == DAYS_FROM_1899_12_30_TO_1970
        && (1.0..FALSE_LEAP_DAY_SERIAL).contains(&serial)
    {
        serial + 1.0
    } else {
        serial
    };
    let micros = ((adjusted - epoch as f64) * MICROS_PER_DAY as f64).round();
    if !micros.is_finite() || micros.abs() > 9.0e18 {
        return SheetCell::Number(serial.to_string());
    }
    let micros = micros as i64;
    if !has_time && micros.rem_euclid(MICROS_PER_DAY) == 0 {
        SheetCell::Date(micros.div_euclid(MICROS_PER_DAY))
    } else {
        SheetCell::Timestamp(micros)
    }
}

fn column_of(reference: &str) -> Option<usize> {
    let mut column = 0usize;
    let mut letters = 0;
    for letter in reference.bytes().take_while(u8::is_ascii_alphabetic) {
        letters += 1;
        if letters > 3 {
            return None;
        }
        column = column * 26 + usize::from(letter.to_ascii_uppercase() - b'A' + 1);
    }
    (letters > 0).then(|| column - 1)
}

fn take_slots(left: &mut usize, wanted: usize) -> Result<(), ContentError> {
    *left = left
        .checked_sub(wanted)
        .ok_or_else(|| unreadable("its cell references reach far beyond its contents"))?;
    Ok(())
}

struct Lookup<'s> {
    shared: &'s [String],
    styles: &'s [Option<bool>],
    epoch: i64,
}

fn cell_value(attributes: &str, content: &str, lookup: &Lookup<'_>) -> Result<SheetCell, ContentError> {
    let value = element_text(content, "v");
    Ok(match attribute(attributes, "t").unwrap_or("n") {
        "s" => {
            let Some(value) = value else {
                return Ok(SheetCell::Empty);
            };
            let index: usize = value
                .trim()
                .parse()
                .map_err(|_| unreadable("a shared string index is not a number"))?;
            SheetCell::Text(
                lookup
                    .shared
                    .get(index)
                    .cloned()
                    .ok_or_else(|| unreadable("a shared string index is out of range"))?,
            )
        }
        "inlineStr" => SheetCell::Text(joined_text(content)),
        "b" => match value.as_deref().map(str::trim) {
            Some("1" | "true") => SheetCell::Boolean(true),
            Some(_) => SheetCell::Boolean(false),
            None => SheetCell::Empty,
        },
        "str" | "e" | "d" => value.map_or(SheetCell::Empty, SheetCell::Text),
        _ => {
            let Some(value) = value else {
                return Ok(SheetCell::Empty);
            };
            let text = value.trim();
            let serial: f64 = text
                .parse()
                .map_err(|_| unreadable("a number cell holds no number"))?;
            let style = attribute(attributes, "s")
                .and_then(|index| index.parse::<usize>().ok())
                .and_then(|index| lookup.styles.get(index).copied())
                .flatten();
            match style {
                Some(has_time) => date_cell(serial, has_time, lookup.epoch),
                None => SheetCell::Number(text.to_owned()),
            }
        }
    })
}

fn sheet_rows(xml: &str, lookup: &Lookup<'_>) -> Result<Vec<Vec<SheetCell>>, ContentError> {
    let mut rows: Vec<Vec<SheetCell>> = Vec::new();
    let mut slots_left = xml.len().saturating_mul(SLOTS_PER_SHEET_BYTE);
    let mut current: Option<usize> = None;
    let mut at = 0;
    while let Some(tag) = next_tag(xml, at) {
        at = tag.end;
        if tag.closing {
            if tag.name == "row" {
                current = None;
            }
            continue;
        }
        match tag.name {
            "row" => {
                let number = match attribute(tag.attributes, "r") {
                    Some(text) => text
                        .trim()
                        .parse::<usize>()
                        .ok()
                        .filter(|number| (1..=MAX_ROWS).contains(number))
                        .ok_or_else(|| unreadable("a row number is out of range"))?,
                    None => rows.len() + 1,
                };
                if number <= rows.len() {
                    return Err(unreadable("its rows are out of order"));
                }
                take_slots(&mut slots_left, number - rows.len())?;
                rows.resize_with(number, Vec::new);
                current = (!tag.empty).then_some(number - 1);
            }
            "c" => {
                let content = if tag.empty {
                    ""
                } else {
                    let close = closing_of(xml, at, "c");
                    let content = &xml[at..close];
                    at = close;
                    content
                };
                let Some(row) = current else {
                    continue;
                };
                let cells = &mut rows[row];
                let column = match attribute(tag.attributes, "r") {
                    Some(reference) => column_of(reference)
                        .ok_or_else(|| unreadable("a cell reference has no column"))?,
                    None => cells.len(),
                };
                if column >= MAX_COLUMNS {
                    return Err(unreadable("a cell lies beyond column XFD"));
                }
                if column < cells.len() {
                    return Err(unreadable("its cells are out of order"));
                }
                let cell = cell_value(tag.attributes, content, lookup)?;
                if cell == SheetCell::Empty {
                    continue;
                }
                take_slots(&mut slots_left, column + 1 - cells.len())?;
                cells.resize(column, SheetCell::Empty);
                cells.push(cell);
            }
            _ => {}
        }
    }
    Ok(rows)
}

fn part(archive: &ZipArchive<'_>, name: &str, budget: &mut usize) -> Result<Option<String>, ContentError> {
    let Some(bytes) = archive.read(name, *budget)? else {
        return Ok(None);
    };
    *budget = budget.saturating_sub(bytes.len());
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| unreadable(&format!("{name} is not UTF-8 text")))
}

struct Relationship {
    id: String,
    kind: String,
    target: String,
}

fn relationships(xml: &str) -> Vec<Relationship> {
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(tag) = next_tag(xml, at) {
        at = tag.end;
        if tag.name != "Relationship" || tag.closing {
            continue;
        }
        if let (Some(id), Some(kind), Some(target)) = (
            attribute(tag.attributes, "Id"),
            attribute(tag.attributes, "Type"),
            attribute(tag.attributes, "Target"),
        ) {
            found.push(Relationship {
                id: id.to_owned(),
                kind: kind.to_owned(),
                target: decoded(target),
            });
        }
    }
    found
}

fn part_path(target: &str) -> String {
    match target.strip_prefix('/') {
        Some(absolute) => absolute.to_owned(),
        None => format!("xl/{target}"),
    }
}

/// The rows of the first worksheet of an `.xlsx` workbook (ECMA-376 SpreadsheetML): row and
/// column positions kept, so a skipped row is an empty row and a skipped cell an empty cell.
/// Shared and inline strings become text, date-formatted numbers dates or timestamps (in the
/// workbook's 1900 or 1904 date system), and formulas their cached values. It decompresses at
/// most `limit` bytes over all the parts it reads.
pub fn read_sheet(file: &[u8], limit: usize) -> Result<Vec<Vec<SheetCell>>, ContentError> {
    let archive = ZipArchive::new(file)?;
    let mut budget = limit;
    let workbook = part(&archive, "xl/workbook.xml", &mut budget)?
        .ok_or_else(|| unreadable("it has no xl/workbook.xml"))?;
    let links = match part(&archive, "xl/_rels/workbook.xml.rels", &mut budget)? {
        Some(xml) => relationships(&xml),
        None => Vec::new(),
    };
    let mut epoch = DAYS_FROM_1899_12_30_TO_1970;
    let mut first_sheet = None;
    let mut at = 0;
    while let Some(tag) = next_tag(&workbook, at) {
        at = tag.end;
        match tag.name {
            "workbookPr" if matches!(attribute(tag.attributes, "date1904"), Some("1" | "true")) => {
                epoch = DAYS_FROM_1904_01_01_TO_1970;
            }
            "sheet" if first_sheet.is_none() && !tag.closing => {
                first_sheet = attribute(tag.attributes, "id").map(str::to_owned);
            }
            _ => {}
        }
    }
    let linked = |ending: &str| {
        links
            .iter()
            .find(|link| link.kind.ends_with(ending))
            .map(|link| part_path(&link.target))
    };
    let sheet_path = first_sheet
        .and_then(|id| links.iter().find(|link| link.id == id))
        .map_or_else(|| "xl/worksheets/sheet1.xml".to_owned(), |link| part_path(&link.target));
    let shared_path = linked("/sharedStrings").unwrap_or_else(|| "xl/sharedStrings.xml".to_owned());
    let styles_path = linked("/styles").unwrap_or_else(|| "xl/styles.xml".to_owned());
    let shared = part(&archive, &shared_path, &mut budget)?
        .map(|xml| shared_strings(&xml))
        .unwrap_or_default();
    let styles = part(&archive, &styles_path, &mut budget)?
        .map(|xml| date_styles(&xml))
        .unwrap_or_default();
    let sheet = part(&archive, &sheet_path, &mut budget)?
        .ok_or_else(|| unreadable(&format!("it has no worksheet at {sheet_path}")))?;
    sheet_rows(
        &sheet,
        &Lookup {
            shared: &shared,
            styles: &styles,
            epoch,
        },
    )
}

fn escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (index, character) in text.char_indices() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\r' => out.push_str("&#13;"),
            '_' if starts_escape(&text[index..]) => out.push_str("_x005F_"),
            '\t' | '\n' => out.push(character),
            control if (control as u32) < 0x20 || matches!(control, '\u{fffe}' | '\u{ffff}') => {
                out.push_str(&format!("_x{:04X}_", control as u32));
            }
            other => out.push(other),
        }
    }
    out
}

fn column_letters(mut index: usize) -> String {
    let mut letters = Vec::new();
    loop {
        letters.push(char::from(b'A' + (index % 26) as u8));
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    letters.iter().rev().collect()
}

fn sheet_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|character| match character {
            '[' | ']' | ':' | '*' | '?' | '/' | '\\' => '_',
            other => other,
        })
        .take(MAX_SHEET_NAME)
        .collect();
    let cleaned = cleaned.trim_matches('\'');
    if cleaned.is_empty() {
        "Sheet1".to_owned()
    } else {
        cleaned.to_owned()
    }
}

fn serial_of_days(days: i64) -> i64 {
    let serial = days.saturating_add(DAYS_FROM_1899_12_30_TO_1970);
    if (2..FIRST_SERIAL_AFTER_FALSE_LEAP_DAY as i64).contains(&serial) {
        serial - 1
    } else {
        serial
    }
}

fn push_cell(sheet: &mut String, reference: &str, cell: &SheetCell) {
    match cell {
        SheetCell::Empty => {}
        SheetCell::Number(text) => {
            sheet.push_str(&format!("<c r=\"{reference}\"><v>{}</v></c>", escaped(text)));
        }
        SheetCell::Boolean(flag) => {
            sheet.push_str(&format!("<c r=\"{reference}\" t=\"b\"><v>{}</v></c>", u8::from(*flag)));
        }
        SheetCell::Text(text) => {
            sheet.push_str(&format!(
                "<c r=\"{reference}\" t=\"inlineStr\"><is><t xml:space=\"preserve\">{}</t></is></c>",
                escaped(text)
            ));
        }
        SheetCell::Date(days) => {
            sheet.push_str(&format!(
                "<c r=\"{reference}\" s=\"{DATE_STYLE}\"><v>{}</v></c>",
                serial_of_days(*days)
            ));
        }
        SheetCell::Timestamp(micros) => {
            let days = micros.div_euclid(MICROS_PER_DAY);
            let of_day = micros.rem_euclid(MICROS_PER_DAY) as f64 / MICROS_PER_DAY as f64;
            sheet.push_str(&format!(
                "<c r=\"{reference}\" s=\"{TIMESTAMP_STYLE}\"><v>{}</v></c>",
                serial_of_days(days) as f64 + of_day
            ));
        }
    }
}

/// An `.xlsx` workbook (ECMA-376 SpreadsheetML) with one worksheet named after `name` holding
/// `rows`: text as inline strings, dates and timestamps as date-formatted serial numbers in the
/// 1900 date system, every part stored uncompressed.
pub fn write_sheet(name: &str, rows: &[Vec<SheetCell>]) -> Result<Vec<u8>, ContentError> {
    if rows.len() > MAX_ROWS {
        return Err(ContentError(format!(
            "an Excel worksheet holds at most {MAX_ROWS} rows, not {}",
            rows.len()
        )));
    }
    let mut sheet = String::from(SHEET_HEAD);
    for (row_index, row) in rows.iter().enumerate() {
        if row.len() > MAX_COLUMNS {
            return Err(ContentError(format!(
                "an Excel worksheet holds at most {MAX_COLUMNS} columns, not {}",
                row.len()
            )));
        }
        let number = row_index + 1;
        sheet.push_str(&format!("<row r=\"{number}\">"));
        for (column, cell) in row.iter().enumerate() {
            push_cell(&mut sheet, &format!("{}{number}", column_letters(column)), cell);
        }
        sheet.push_str("</row>");
    }
    sheet.push_str(SHEET_TAIL);
    let workbook = format!("{WORKBOOK_HEAD}{}{WORKBOOK_TAIL}", escaped(&sheet_name(name)));
    write_stored(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", ROOT_RELATIONSHIPS.as_bytes()),
        ("xl/workbook.xml", workbook.as_bytes()),
        ("xl/_rels/workbook.xml.rels", WORKBOOK_RELATIONSHIPS.as_bytes()),
        ("xl/styles.xml", STYLES.as_bytes()),
        ("xl/worksheets/sheet1.xml", sheet.as_bytes()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_sheet_reads_back() {
        let rows = vec![
            vec![SheetCell::Text("id".into()), SheetCell::Text("name & <tag>".into())],
            vec![SheetCell::Number("1".into()), SheetCell::Text(" a_x0041_\u{1}".into())],
            vec![SheetCell::Empty, SheetCell::Boolean(true)],
            vec![SheetCell::Date(19_000), SheetCell::Timestamp(1_641_038_400_000_000)],
        ];
        let file = write_sheet("my:table", &rows).unwrap();
        assert_eq!(read_sheet(&file, 1 << 20).unwrap(), rows);
    }

    #[test]
    fn columns_and_formats_are_read() {
        assert_eq!(column_of("A1"), Some(0));
        assert_eq!(column_of("AA7"), Some(26));
        assert_eq!(column_of("XFD1"), Some(16_383));
        assert_eq!(column_letters(16_383), "XFD");
        assert_eq!(format_kind("yyyy-mm-dd"), Some(false));
        assert_eq!(format_kind("[$-409]h:mm AM/PM"), Some(true));
        assert_eq!(format_kind("#,##0.00\" days\""), None);
        assert_eq!(date_cell(1.0, false, DAYS_FROM_1899_12_30_TO_1970), SheetCell::Date(-25_567));
        assert_eq!(date_cell(61.0, false, DAYS_FROM_1899_12_30_TO_1970), SheetCell::Date(-25_508));
    }
}
