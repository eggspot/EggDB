use crate::error::{SqlError, SqlResult, SqlState};
use crate::json::{Json, parse_document};

const PAGE_WIDTH: f64 = 595.0;
const PAGE_HEIGHT: f64 = 842.0;
const MARGIN: f64 = 40.0;
const FONT_SIZE: f64 = 9.0;
const TITLE_SIZE: f64 = 14.0;
const LINE_HEIGHT: f64 = 12.0;
const CHARACTER_WIDTH: f64 = 0.5;
const MOST_ROWS: usize = 100_000;
const MOST_COLUMNS: usize = 64;

fn invalid(message: impl std::fmt::Display) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, format!("eggdb_pdf_table: {message}"))
}

fn cell_text(value: &Json) -> String {
    match value {
        Json::Null => String::new(),
        Json::Boolean(flag) => flag.to_string(),
        Json::Number(number) | Json::String(number) => number.clone(),
        other => crate::json::normalized_value(other).unwrap_or_default(),
    }
}

fn pdf_string(text: &str, most_characters: usize) -> String {
    let mut out = String::from("(");
    for character in text.chars().take(most_characters) {
        match character {
            '(' | ')' | '\\' => {
                out.push('\\');
                out.push(character);
            }
            printable if (' '..='~').contains(&printable) => out.push(printable),
            latin if (u32::from(latin) >= 0xA0 && u32::from(latin) <= 0xFF) => out.push_str(&format!("\\{:03o}", u32::from(latin))),
            _ => out.push('?'),
        }
    }
    out.push(')');
    out
}

fn text_line(content: &mut String, x: f64, y: f64, size: f64, text: &str, most_characters: usize) {
    content.push_str(&format!("BT /F1 {size} Tf {x:.1} {y:.1} Td {} Tj ET\n", pdf_string(text, most_characters)));
}

/// `eggdb_pdf_table(title, rows)`: a PDF of `rows` (a JSON array of objects, their keys in order
/// giving the columns) as a table on A4 pages in Helvetica, the title on the first page and the
/// heading row on every page. Text outside Latin-1 shows as `?`; long cells are cut to fit.
pub(crate) fn table_pdf(title: &str, rows: &str) -> SqlResult<Vec<u8>> {
    let document = parse_document(rows).map_err(|error| invalid(error.message))?;
    let Json::Array(items) = &document.value else {
        return Err(invalid("rows must be a JSON array of objects"));
    };
    if items.len() > MOST_ROWS {
        return Err(invalid(format!("a report may hold at most {MOST_ROWS} rows")));
    }
    let mut columns: Vec<String> = Vec::new();
    for item in items {
        if let Json::Object(members) = &item.value {
            for (name, _) in members {
                if !columns.contains(name) && columns.len() < MOST_COLUMNS {
                    columns.push(name.clone());
                }
            }
        }
    }
    let column_width = (PAGE_WIDTH - 2.0 * MARGIN) / columns.len().max(1) as f64;
    let most_characters = ((column_width - 4.0) / (FONT_SIZE * CHARACTER_WIDTH)).max(1.0) as usize;
    let mut pages: Vec<String> = Vec::new();
    let mut content = String::new();
    let mut y = PAGE_HEIGHT - MARGIN;
    let heading = |content: &mut String, y: &mut f64| {
        for (index, column) in columns.iter().enumerate() {
            text_line(content, MARGIN + index as f64 * column_width, *y, FONT_SIZE, column, most_characters);
        }
        content.push_str(&format!("{MARGIN} {:.1} m {:.1} {:.1} l S\n", *y - 3.0, PAGE_WIDTH - MARGIN, *y - 3.0));
        *y -= LINE_HEIGHT * 1.5;
    };
    text_line(&mut content, MARGIN, y, TITLE_SIZE, title, 80);
    y -= LINE_HEIGHT * 2.0;
    heading(&mut content, &mut y);
    for item in items {
        if y < MARGIN {
            pages.push(std::mem::take(&mut content));
            y = PAGE_HEIGHT - MARGIN;
            heading(&mut content, &mut y);
        }
        let Json::Object(members) = &item.value else {
            continue;
        };
        for (index, column) in columns.iter().enumerate() {
            let value = members.iter().find(|(name, _)| name == column).map_or(String::new(), |(_, node)| cell_text(&node.value));
            text_line(&mut content, MARGIN + index as f64 * column_width, y, FONT_SIZE, &value, most_characters);
        }
        y -= LINE_HEIGHT;
    }
    pages.push(content);
    let font_object = 3 + 2 * pages.len();
    let mut objects: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        format!(
            "<< /Type /Pages /Kids [{}] /Count {} >>",
            (0..pages.len()).map(|page| format!("{} 0 R", 3 + 2 * page)).collect::<Vec<_>>().join(" "),
            pages.len()
        ),
    ];
    for (page, stream) in pages.iter().enumerate() {
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {PAGE_WIDTH} {PAGE_HEIGHT}] /Resources << /Font << /F1 {font_object} 0 R >> >> /Contents {} 0 R >>",
            4 + 2 * page
        ));
        objects.push(format!("<< /Length {} >>\nstream\n{stream}endstream", stream.len()));
    }
    objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_owned());
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (index, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
    }
    let table_at = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{table_at}\n%%EOF\n", objects.len() + 1).as_bytes());
    crate::memory::check_value_bytes(out.len())?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_become_a_pdf_with_its_cross_reference_table() {
        let pdf = table_pdf("Stock (low)", r#"[{"item":"eggs","quantity":3},{"item":"milk","quantity":40}]"#).unwrap();
        let text = String::from_utf8_lossy(&pdf);
        assert!(text.starts_with("%PDF-1.4"));
        assert!(text.contains("(Stock \\(low\\)) Tj"));
        assert!(text.contains("(eggs) Tj") && text.contains("(40) Tj"));
        assert!(text.trim_end().ends_with("%%EOF"));
    }
}
