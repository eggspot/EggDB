use crate::error::{SqlError, SqlResult, SqlState};

const MOST_DEPTH: usize = 16;
const MOST_LINES: usize = 200_000;
const STRUCTURED_VCARD_PROPERTIES: [&str; 4] = ["n", "adr", "org", "gender"];

fn invalid(format: &str, message: impl std::fmt::Display) -> SqlError {
    SqlError::new(SqlState::INVALID_TEXT_REPRESENTATION, format!("invalid {format}: {message}"))
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    crate::json::write_string(text, &mut out);
    out
}

fn unfolded(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        match line.chars().next() {
            Some(' ' | '\t') if !lines.is_empty() => {
                if let Some(last) = lines.last_mut() {
                    last.push_str(&line[1..]);
                }
            }
            _ if line.is_empty() => {}
            _ => lines.push(line.to_owned()),
        }
    }
    lines
}

fn unescaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn split_unescaped(text: &str, separator: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for character in text.chars() {
        if escaped {
            current.push('\\');
            current.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == separator {
            parts.push(unescaped(&std::mem::take(&mut current)));
        } else {
            current.push(character);
        }
    }
    parts.push(unescaped(&current));
    parts
}

struct Property {
    name: String,
    parameters: Vec<(String, String)>,
    value: String,
}

fn property(line: &str, format: &str) -> SqlResult<Property> {
    let mut quoted = false;
    let mut colon = None;
    for (at, character) in line.char_indices() {
        match character {
            '"' => quoted = !quoted,
            ':' if !quoted => {
                colon = Some(at);
                break;
            }
            _ => {}
        }
    }
    let colon = colon.ok_or_else(|| invalid(format, format!("a line has no ':' ({line})")))?;
    let (head, value) = (&line[..colon], &line[colon + 1..]);
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for character in head.chars() {
        match character {
            '"' => quoted = !quoted,
            ';' if !quoted => parts.push(std::mem::take(&mut current)),
            other => current.push(other),
        }
    }
    parts.push(current);
    let mut parts = parts.into_iter();
    let name = parts.next().unwrap_or_default();
    let name = name.rsplit('.').next().unwrap_or(&name).to_ascii_lowercase();
    if name.is_empty() {
        return Err(invalid(format, "a property has no name"));
    }
    let parameters = parts
        .map(|parameter| match parameter.split_once('=') {
            Some((key, value)) => (key.to_ascii_lowercase(), value.to_owned()),
            None => (parameter.to_ascii_lowercase(), String::new()),
        })
        .collect();
    Ok(Property { name, parameters, value: value.to_owned() })
}

fn property_json(property: &Property, structured: bool) -> String {
    let parameters: Vec<String> = property
        .parameters
        .iter()
        .map(|(key, value)| format!("{}:{}", json_string(key), json_string(value)))
        .collect();
    let value = if structured {
        format!("[{}]", split_unescaped(&property.value, ';').iter().map(|part| json_string(part)).collect::<Vec<_>>().join(","))
    } else {
        json_string(&unescaped(&property.value))
    };
    format!("[{},{{{}}},\"unknown\",{value}]", json_string(&property.name), parameters.join(","))
}

fn component(lines: &[String], at: &mut usize, name: &str, depth: usize, format: &str, vcard: bool) -> SqlResult<String> {
    if depth > MOST_DEPTH {
        return Err(invalid(format, format!("components nest deeper than {MOST_DEPTH}")));
    }
    let mut properties = Vec::new();
    let mut components = Vec::new();
    while *at < lines.len() {
        let line = &lines[*at];
        *at += 1;
        let found = property(line, format)?;
        match found.name.as_str() {
            "begin" => {
                let inner = found.value.trim().to_ascii_lowercase();
                components.push(component(lines, at, &inner, depth + 1, format, vcard)?);
            }
            "end" => {
                let closed = found.value.trim().to_ascii_lowercase();
                if closed != name {
                    return Err(invalid(format, format!("END:{} closes {}", found.value.trim(), name.to_ascii_uppercase())));
                }
                return Ok(if vcard {
                    format!("[{},[{}]]", json_string(name), properties.join(","))
                } else {
                    format!("[{},[{}],[{}]]", json_string(name), properties.join(","), components.join(","))
                });
            }
            _ => {
                let structured = vcard && STRUCTURED_VCARD_PROPERTIES.contains(&found.name.as_str());
                properties.push(property_json(&found, structured));
            }
        }
    }
    Err(invalid(format, format!("{} is not closed by END", name.to_ascii_uppercase())))
}

fn parse(text: &str, expected: &str, format: &str, vcard: bool) -> SqlResult<String> {
    let lines = unfolded(text);
    if lines.len() > MOST_LINES {
        return Err(invalid(format, format!("more than {MOST_LINES} lines")));
    }
    let mut at = 0;
    let mut objects = Vec::new();
    while at < lines.len() {
        let first = property(&lines[at], format)?;
        at += 1;
        let name = first.value.trim().to_ascii_lowercase();
        if first.name != "begin" || name != expected {
            return Err(invalid(format, format!("expected BEGIN:{}", expected.to_ascii_uppercase())));
        }
        objects.push(component(&lines, &mut at, &name, 0, format, vcard)?);
    }
    Ok(match objects.len() {
        1 => objects.remove(0),
        _ => format!("[{}]", objects.join(",")),
    })
}

/// An iCalendar object (RFC 5545) as jCal (RFC 7265): `["vcalendar", [properties], [components]]`,
/// each property `[name, {parameters}, "unknown", value]` with its text unescaped; several
/// calendars in one text come back as an array of them.
pub(crate) fn icalendar(text: &str) -> SqlResult<String> {
    parse(text, "vcalendar", "iCalendar", false)
}

/// A vCard (RFC 6350) as jCard (RFC 7095): `["vcard", [properties]]`, with `N`, `ADR`, `ORG` and
/// `GENDER` split into their `;`-separated parts; several cards come back as an array.
pub(crate) fn vcard(text: &str) -> SqlResult<String> {
    parse(text, "vcard", "vCard", true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendars_and_cards_become_jcal_and_jcard() {
        let calendar = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nSUMMARY:Team\\, weekly\r\nDTSTART;TZID=Asia/Ho_Chi_Minh:20261112T090000\r\nDESCRIPTION:Line one\\nand a long\r\n  folded line\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let parsed = icalendar(calendar).unwrap();
        assert!(parsed.starts_with("[\"vcalendar\",[[\"version\",{},\"unknown\",\"2.0\"]],[[\"vevent\""), "{parsed}");
        assert!(parsed.contains("[\"summary\",{},\"unknown\",\"Team, weekly\"]"), "{parsed}");
        assert!(parsed.contains("{\"tzid\":\"Asia/Ho_Chi_Minh\"}"), "{parsed}");
        assert!(parsed.contains("and a long folded line"), "{parsed}");
        assert!(crate::json::normalize(&parsed).is_ok());
        let card = vcard("BEGIN:VCARD\nVERSION:4.0\nFN:Nguyen Van A\nN:Nguyen;Van A;;;\nEND:VCARD\n").unwrap();
        assert!(card.contains("[\"n\",{},\"unknown\",[\"Nguyen\",\"Van A\",\"\",\"\",\"\"]]"), "{card}");
        assert!(icalendar("BEGIN:VCALENDAR\nBEGIN:VEVENT\nEND:VCALENDAR\n").is_err());
    }
}
