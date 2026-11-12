//! Reading Internet messages (RFC 5322): the header section with folded lines unfolded, the
//! body's start, encoded words in headers (RFC 2047, base64 and quoted-printable in UTF-8,
//! US-ASCII and ISO-8859-1), address lists, and dates.

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];

/// Where the body starts: just after the first empty line, or the end when there is none.
pub fn body_start(raw: &[u8]) -> usize {
    raw.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|at| at + 4)
        .or_else(|| raw.windows(2).position(|window| window == b"\n\n").map(|at| at + 2))
        .unwrap_or(raw.len())
}

/// The header fields, in order, with folded lines joined.
pub fn headers(raw: &[u8]) -> Vec<(String, String)> {
    let section = String::from_utf8_lossy(&raw[..body_start(raw)]).into_owned();
    let mut fields: Vec<(String, String)> = Vec::new();
    for line in section.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            break;
        }
        if line.starts_with([' ', '\t']) {
            if let Some((_, value)) = fields.last_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            fields.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    fields
}

/// The first header field named `name` (any case), decoded.
pub fn header(raw: &[u8], name: &str) -> Option<String> {
    headers(raw)
        .into_iter()
        .find(|(found, _)| found.eq_ignore_ascii_case(name))
        .map(|(_, value)| decoded_words(&value))
}

fn base64_bytes(text: &str) -> Option<Vec<u8>> {
    eggdb_crypto::base64_decode(text.trim().as_bytes()).ok()
}

fn quoted_printable_word(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'_' => out.push(b' '),
            b'=' => {
                if let Some(byte) = text.get(at + 1..at + 3).and_then(|pair| u8::from_str_radix(pair, 16).ok()) {
                    out.push(byte);
                    at += 2;
                } else {
                    out.push(b'=');
                }
            }
            other => out.push(other),
        }
        at += 1;
    }
    out
}

pub(crate) fn text_in(charset: &str, bytes: &[u8]) -> Option<String> {
    match charset.to_ascii_lowercase().as_str() {
        "utf-8" | "utf8" | "us-ascii" => Some(String::from_utf8_lossy(bytes).into_owned()),
        "iso-8859-1" | "latin1" | "windows-1252" => Some(bytes.iter().map(|byte| char::from(*byte)).collect()),
        _ => None,
    }
}

/// `value` with its RFC 2047 encoded words (`=?charset?B?...?=`, `=?charset?Q?...?=`)
/// decoded; words in unknown charsets stay as written.
pub fn decoded_words(value: &str) -> String {
    let mut out = String::new();
    let mut rest = value;
    let mut last_was_word = false;
    while let Some(start) = rest.find("=?") {
        let before = &rest[..start];
        let after = &rest[start + 2..];
        let parts: Vec<&str> = after.splitn(3, '?').collect();
        let decoded = match parts.as_slice() {
            [charset, encoding, tail] => tail.find("?=").and_then(|end| {
                let text = &tail[..end];
                let bytes = match encoding.to_ascii_uppercase().as_str() {
                    "B" => base64_bytes(text)?,
                    "Q" => quoted_printable_word(text),
                    _ => return None,
                };
                let consumed = start + 2 + charset.len() + encoding.len() + 2 + end + 2;
                let charset = charset.split('*').next().unwrap_or(charset);
                Some((text_in(charset, &bytes)?, consumed))
            }),
            _ => None,
        };
        match decoded {
            Some((text, consumed)) => {
                if !(last_was_word && before.trim().is_empty()) {
                    out.push_str(before);
                }
                out.push_str(&text);
                rest = rest.get(consumed..).unwrap_or_default();
                last_was_word = true;
            }
            None => {
                out.push_str(&rest[..start + 2]);
                rest = after;
                last_was_word = false;
            }
        }
    }
    out.push_str(rest);
    out
}

/// One mailbox of an address list: a display name and the address.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Address {
    /// The display name, decoded, when there is one.
    pub name: Option<String>,
    /// The address, `local@domain`.
    pub email: String,
}

fn split_top_level(value: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut angle = 0usize;
    for character in value.chars() {
        match character {
            '"' => quoted = !quoted,
            '<' if !quoted => angle += 1,
            '>' if !quoted => angle = angle.saturating_sub(1),
            ',' if !quoted && angle == 0 => {
                parts.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(character);
    }
    parts.push(current);
    parts
}

/// The addresses of an address-list header value such as `"Ann" <ann@example.org>, bob@x`.
pub fn addresses(value: &str) -> Vec<Address> {
    let mut found = Vec::new();
    for part in split_top_level(value) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let part = part.split_once(':').filter(|(group, _)| !group.contains(['<', '@', '"'])).map_or(part, |(_, rest)| rest.trim_end_matches(';').trim());
        let (name, email) = match (part.rfind('<'), part.rfind('>')) {
            (Some(open), Some(close)) if open < close => {
                let name = part[..open].trim().trim_matches('"').trim();
                (
                    (!name.is_empty()).then(|| decoded_words(name)),
                    part[open + 1..close].trim().to_owned(),
                )
            }
            _ => {
                let email = part.split('(').next().unwrap_or(part).trim().to_owned();
                (None, email)
            }
        };
        if !email.is_empty() {
            found.push(Address { name, email });
        }
    }
    found
}

fn civil(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 } as u32;
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month = i64::from(month);
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// `seconds` since 1970 as an RFC 5322 date in UTC: `Thu, 12 Nov 2026 10:00:00 +0000`.
pub fn rfc5322_date(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    let (year, month, day) = civil(days);
    format!(
        "{}, {day:02} {} {year:04} {:02}:{:02}:{:02} +0000",
        WEEKDAYS[days.rem_euclid(7) as usize],
        MONTHS[(month - 1) as usize],
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// IMAP's `INTERNALDATE` form of `seconds`: `12-Nov-2026 10:00:00 +0000`.
pub fn imap_date(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    let (year, month, day) = civil(days);
    format!(
        "{day:02}-{}-{year:04} {:02}:{:02}:{:02} +0000",
        MONTHS[(month - 1) as usize],
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// `seconds` as RFC 3339 UTC, as JMAP writes dates: `2026-11-12T10:00:00Z`.
pub fn utc_date(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    let (year, month, day) = civil(days);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rest / 3600, rest % 3600 / 60, rest % 60)
}

/// An RFC 5322 date header value as seconds since 1970, when it reads as one.
pub fn parse_rfc5322_date(value: &str) -> Option<u64> {
    let value = value.split('(').next().unwrap_or(value);
    let value = value.split_once(',').map_or(value, |(_, rest)| rest);
    let parts: Vec<&str> = value.split_whitespace().collect();
    let [day, month, year, time, zone, ..] = parts.as_slice() else {
        return None;
    };
    let day: u32 = day.parse().ok()?;
    let month = MONTHS.iter().position(|name| name.eq_ignore_ascii_case(month))? as u32 + 1;
    let mut year: i64 = year.parse().ok()?;
    if year < 50 {
        year += 2000;
    } else if year < 1000 {
        year += 1900;
    }
    let mut clock = time.split(':');
    let hours: i64 = clock.next()?.parse().ok()?;
    let minutes: i64 = clock.next()?.parse().ok()?;
    let seconds: i64 = clock.next().map_or(Some(0), |part| part.parse().ok())?;
    let offset = match *zone {
        "UT" | "GMT" | "Z" => 0,
        zone if zone.len() == 5 && zone.is_ascii() && zone.starts_with(['+', '-']) => {
            let hours: i64 = zone[1..3].parse().ok()?;
            let minutes: i64 = zone[3..5].parse().ok()?;
            let total = hours * 3600 + minutes * 60;
            if zone.starts_with('-') { -total } else { total }
        }
        _ => 0,
    };
    let total = days_from_civil(year, month, day) * 86_400 + hours * 3600 + minutes * 60 + seconds - offset;
    u64::try_from(total).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MESSAGE: &[u8] = b"From: =?UTF-8?B?w4Fubg==?= <ann@example.org>\r\nTo: bob@example.org, \"Cid, Jr\" <cid@example.org>\r\nSubject: Hello\r\n  world =?ISO-8859-1?Q?caf=E9?=\r\nDate: Thu, 12 Nov 2026 10:00:00 +0100\r\n\r\nBody\r\n";

    #[test]
    fn headers_unfold_and_decode() {
        assert_eq!(header(MESSAGE, "subject").as_deref(), Some("Hello world café"));
        assert_eq!(&MESSAGE[body_start(MESSAGE)..], b"Body\r\n");
        let from = addresses(&header(MESSAGE, "From").unwrap_or_default());
        assert_eq!(from, vec![Address { name: Some("Ánn".to_owned()), email: "ann@example.org".to_owned() }]);
        let to = addresses(&header(MESSAGE, "To").unwrap_or_default());
        assert_eq!(to.len(), 2);
        assert_eq!(to[1].name.as_deref(), Some("Cid, Jr"));
    }

    #[test]
    fn dates_convert_both_ways() {
        let seconds = parse_rfc5322_date("Thu, 12 Nov 2026 10:00:00 +0100").unwrap();
        assert_eq!(rfc5322_date(seconds), "Thu, 12 Nov 2026 09:00:00 +0000");
        assert_eq!(imap_date(seconds), "12-Nov-2026 09:00:00 +0000");
        assert_eq!(utc_date(0), "1970-01-01T00:00:00Z");
        assert_eq!(parse_rfc5322_date("not a date"), None);
    }
}
