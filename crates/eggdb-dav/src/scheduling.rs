//! CalDAV scheduling (RFC 6638) for the server's one user (D1077): the schedule inbox and
//! outbox, free-busy queries `POST`ed to the outbox, iTIP messages (RFC 5546) put in the inbox
//! applied to the user's calendar, and the iTIP messages the user's own invitations need queued
//! in the outbox for a gateway to deliver.

use crate::ical::{self, TimeRange};

const MOST_FREE_BUSY_SECONDS: i64 = 366 * 86_400;

/// An iCalendar object's lines, unfolded.
pub(crate) fn lines(text: &str) -> Vec<String> {
    ical::unfolded(text).into_iter().filter(|line| !line.is_empty()).collect()
}

/// Lines folded at 75 octets and joined with CRLF, as RFC 5545 §3.1 writes them.
pub(crate) fn folded(lines: &[String]) -> String {
    let mut out = String::new();
    for line in lines {
        let mut rest = line.as_str();
        let mut first = true;
        while !rest.is_empty() {
            let room = if first { 75 } else { 74 };
            let mut cut = rest.len().min(room);
            while !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            if !first {
                out.push(' ');
            }
            out.push_str(&rest[..cut]);
            out.push_str("\r\n");
            rest = &rest[cut..];
            first = false;
        }
    }
    out
}

fn name_of(line: &str) -> String {
    line.split([';', ':']).next().unwrap_or_default().to_ascii_uppercase()
}

fn value_of(line: &str) -> &str {
    let mut quoted = false;
    for (at, character) in line.char_indices() {
        match character {
            '"' => quoted = !quoted,
            ':' if !quoted => return &line[at + 1..],
            _ => {}
        }
    }
    ""
}

fn parameter<'l>(line: &'l str, wanted: &str) -> Option<&'l str> {
    let head = &line[..line.len() - value_of(line).len()];
    head.trim_end_matches(':').split(';').skip(1).find_map(|part| {
        let (key, value) = part.split_once('=')?;
        key.eq_ignore_ascii_case(wanted).then(|| value.trim_matches('"'))
    })
}

fn with_parameter(line: &str, wanted: &str, value: &str) -> String {
    let rest = value_of(line);
    let head = &line[..line.len() - rest.len()];
    let head = head.trim_end_matches(':');
    let mut parts: Vec<String> = head.split(';').map(str::to_owned).collect();
    parts.retain(|part| part.split_once('=').is_none_or(|(key, _)| !key.eq_ignore_ascii_case(wanted)));
    parts.push(format!("{wanted}={value}"));
    format!("{}:{rest}", parts.join(";"))
}

/// The value of the first property `name` in `lines`.
pub(crate) fn first<'l>(lines: &'l [String], name: &str) -> Option<&'l str> {
    lines.iter().find(|line| name_of(line) == name).map(|line| value_of(line))
}

/// Whether `address` (`mailto:...`) is the user's: its local part is the user name.
pub(crate) fn is_user(address: &str, user: &str) -> bool {
    let address = address.trim();
    let mail = address.get(..7).filter(|scheme| scheme.eq_ignore_ascii_case("mailto:")).map_or(address, |_| &address[7..]);
    mail.split('@').next().is_some_and(|local| local.eq_ignore_ascii_case(user))
}

/// What an iTIP message put in the inbox does to the user's calendar object with the same UID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Applied {
    /// Store this text as the object (a new invitation or an updated one).
    Replace(String),
    /// Nothing to do.
    Nothing,
}

/// Applies the iTIP message `message` to `existing` (the user's object with its UID, if any).
pub(crate) fn apply(message: &str, existing: Option<&str>) -> Applied {
    let incoming = lines(message);
    let method = first(&incoming, "METHOD").unwrap_or_default().to_ascii_uppercase();
    let without_method: Vec<String> = incoming.iter().filter(|line| name_of(line) != "METHOD").cloned().collect();
    match (method.as_str(), existing) {
        ("REQUEST" | "PUBLISH", _) => Applied::Replace(folded(&without_method)),
        ("REPLY", Some(existing)) => {
            let replies: Vec<(String, String)> = incoming
                .iter()
                .filter(|line| name_of(line) == "ATTENDEE")
                .filter_map(|line| Some((value_of(line).to_ascii_lowercase(), parameter(line, "PARTSTAT")?.to_owned())))
                .collect();
            let updated: Vec<String> = lines(existing)
                .into_iter()
                .map(|line| {
                    let address = value_of(&line).to_ascii_lowercase();
                    match replies.iter().find(|(replied, _)| name_of(&line) == "ATTENDEE" && *replied == address) {
                        Some((_, status)) => with_parameter(&line, "PARTSTAT", status),
                        None => line,
                    }
                })
                .collect();
            Applied::Replace(folded(&updated))
        }
        ("CANCEL", Some(existing)) => {
            let mut updated = Vec::new();
            let mut in_event = false;
            for line in lines(existing) {
                match line.to_ascii_uppercase().as_str() {
                    "BEGIN:VEVENT" | "BEGIN:VTODO" => in_event = true,
                    "END:VEVENT" | "END:VTODO" if in_event => {
                        updated.push("STATUS:CANCELLED".to_owned());
                        in_event = false;
                    }
                    _ if in_event && name_of(&line) == "STATUS" => continue,
                    _ => {}
                }
                updated.push(line);
            }
            Applied::Replace(folded(&updated))
        }
        _ => Applied::Nothing,
    }
}

/// The iTIP message (`METHOD:REQUEST` or `CANCEL`) the user's own object `text` needs sent to
/// its attendees, or `None` when the user does not organize it or it has no other attendee.
pub(crate) fn outgoing(text: &str, user: &str, method: &str) -> Option<(String, Vec<String>)> {
    let found = lines(text);
    if !first(&found, "ORGANIZER").is_some_and(|organizer| is_user(organizer, user)) {
        return None;
    }
    let recipients: Vec<String> = found
        .iter()
        .filter(|line| name_of(line) == "ATTENDEE")
        .map(|line| value_of(line).to_owned())
        .filter(|address| !is_user(address, user))
        .collect();
    if recipients.is_empty() {
        return None;
    }
    let mut message: Vec<String> = Vec::with_capacity(found.len() + 1);
    for line in found {
        if name_of(&line) == "METHOD" {
            continue;
        }
        let begins_calendar = line.eq_ignore_ascii_case("BEGIN:VCALENDAR");
        message.push(line);
        if begins_calendar {
            message.push(format!("METHOD:{method}"));
        }
    }
    if method == "CANCEL" {
        message = message
            .into_iter()
            .map(|line| if name_of(&line) == "STATUS" { "STATUS:CANCELLED".to_owned() } else { line })
            .collect();
    }
    Some((folded(&message), recipients))
}

fn civil(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

/// `seconds` since 1970 as an iCalendar UTC date-time, `20261113T090000Z`.
pub(crate) fn utc_stamp(seconds: i64) -> String {
    let (year, month, day) = civil(seconds.div_euclid(86_400));
    let of_day = seconds.rem_euclid(86_400);
    format!("{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z", of_day / 3600, of_day / 60 % 60, of_day % 60)
}

/// The busy periods of the calendar objects `objects` within `range`: every occurrence of an
/// event that is neither transparent nor cancelled.
pub(crate) fn busy(objects: &[String], range: TimeRange) -> Vec<(i64, i64)> {
    let mut periods = Vec::new();
    for text in objects {
        for component in ical::components(&ical::expanded(text, range)) {
            if component.name != "VEVENT" {
                continue;
            }
            let value = |name: &str| component.properties.iter().find(|property| property.name == name).map(|property| property.value.clone());
            if value("TRANSP").is_some_and(|transparency| transparency.eq_ignore_ascii_case("TRANSPARENT"))
                || value("STATUS").is_some_and(|status| status.eq_ignore_ascii_case("CANCELLED"))
            {
                continue;
            }
            let Some(start) = value("DTSTART").and_then(|text| ical::parse_moment(&text)).map(|moment| moment.seconds) else { continue };
            let end = value("DTEND")
                .and_then(|text| ical::parse_moment(&text))
                .map(|moment| moment.seconds)
                .or_else(|| value("DURATION").and_then(|text| ical::parse_duration(&text)).map(|length| start + length))
                .unwrap_or(start + 86_400);
            let (start, end) = (start.max(range.start.unwrap_or(start)), end.min(range.end.unwrap_or(end)));
            if end > start {
                periods.push((start, end));
            }
        }
    }
    periods.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (start, end) in periods {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// The free-busy question in a `POST` to the outbox: its range, organizer, attendees and UID.
pub(crate) struct FreeBusyRequest {
    pub(crate) range: (i64, i64),
    pub(crate) organizer: String,
    pub(crate) attendees: Vec<String>,
    pub(crate) uid: String,
}

/// Reads a `METHOD:REQUEST` `VFREEBUSY` (RFC 6638 §5 and RFC 5546 §3.3.2); `None` for anything
/// else or a range longer than a year.
pub(crate) fn free_busy_request(text: &str) -> Option<FreeBusyRequest> {
    let found = lines(text);
    if !first(&found, "METHOD")?.eq_ignore_ascii_case("REQUEST") || !found.iter().any(|line| line.eq_ignore_ascii_case("BEGIN:VFREEBUSY")) {
        return None;
    }
    let start = ical::parse_moment(first(&found, "DTSTART")?)?.seconds;
    let end = ical::parse_moment(first(&found, "DTEND")?)?.seconds;
    if end <= start || end - start > MOST_FREE_BUSY_SECONDS {
        return None;
    }
    Some(FreeBusyRequest {
        range: (start, end),
        organizer: first(&found, "ORGANIZER").unwrap_or_default().to_owned(),
        attendees: found.iter().filter(|line| name_of(line) == "ATTENDEE").map(|line| value_of(line).to_owned()).collect(),
        uid: first(&found, "UID").unwrap_or_default().to_owned(),
    })
}

/// The `METHOD:REPLY` `VFREEBUSY` answering `request` for `attendee` with `periods`.
pub(crate) fn free_busy_reply(request: &FreeBusyRequest, attendee: &str, periods: &[(i64, i64)], now: i64) -> String {
    let mut out = vec![
        "BEGIN:VCALENDAR".to_owned(),
        "VERSION:2.0".to_owned(),
        "PRODID:-//EggDB//CalDAV//EN".to_owned(),
        "METHOD:REPLY".to_owned(),
        "BEGIN:VFREEBUSY".to_owned(),
        format!("UID:{}", request.uid),
        format!("DTSTAMP:{}", utc_stamp(now)),
        format!("DTSTART:{}", utc_stamp(request.range.0)),
        format!("DTEND:{}", utc_stamp(request.range.1)),
        format!("ORGANIZER:{}", request.organizer),
        format!("ATTENDEE:{attendee}"),
    ];
    out.extend(periods.iter().map(|(start, end)| format!("FREEBUSY;FBTYPE=BUSY:{}/{}", utc_stamp(*start), utc_stamp(*end))));
    out.push("END:VFREEBUSY".to_owned());
    out.push("END:VCALENDAR".to_owned());
    folded(&out)
}
