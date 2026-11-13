//! iMIP (RFC 6047): the invitations CalDAV scheduling queues in the user's outbox (D1077),
//! written as email messages for the mail store to deliver (D1079).

use crate::store::{self, Kind};
use eggdb_keyspace::Keyspace;

const RECIPIENTS: (&str, &str) = ("https://eggdb.dev/ns/dav", "recipients");
const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// An iTIP message waiting in the user's schedule outbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invitation {
    /// Where it waits.
    pub path: String,
    /// Its recipients' addresses, without `mailto:`.
    pub recipients: Vec<String>,
    /// The iCalendar object, with its `METHOD`.
    pub calendar: String,
}

/// Every invitation waiting in `user`'s outbox.
pub fn waiting_invitations(keyspace: &(impl Keyspace + ?Sized), user: &str) -> Result<Vec<Invitation>, String> {
    let outbox = format!("/calendars/{user}/outbox");
    let mut found = Vec::new();
    keyspace.read(0, &mut |keys| {
        found = store::children(keys, &outbox)?
            .into_iter()
            .filter(|resource| resource.kind == Kind::File)
            .filter_map(|resource| {
                let recipients = resource
                    .properties
                    .iter()
                    .find(|(namespace, name, _)| (namespace.as_str(), name.as_str()) == RECIPIENTS)?
                    .2
                    .split_whitespace()
                    .map(|address| address.get(..7).filter(|scheme| scheme.eq_ignore_ascii_case("mailto:")).map_or(address, |_| &address[7..]).to_owned())
                    .collect();
                Some(Invitation { path: resource.path, recipients, calendar: String::from_utf8_lossy(&resource.body).into_owned() })
            })
            .collect();
        Ok(())
    })?;
    Ok(found)
}

/// Removes a sent invitation from the outbox.
pub fn remove_invitation(keyspace: &(impl Keyspace + ?Sized), path: &str) -> Result<(), String> {
    keyspace.write(0, &mut |keys| store::remove(keys, path).map(drop))
}

fn rfc5322_date(seconds: u64) -> String {
    let seconds = i64::try_from(seconds).unwrap_or(0);
    let days = seconds.div_euclid(86_400);
    let (year, month, day) = crate::scheduling::civil(days);
    let of_day = seconds.rem_euclid(86_400);
    format!(
        "{}, {day:02} {} {year:04} {:02}:{:02}:{:02} +0000",
        DAYS[days.rem_euclid(7) as usize],
        MONTHS[(month - 1).clamp(0, 11) as usize],
        of_day / 3600,
        of_day / 60 % 60,
        of_day % 60
    )
}

fn header_safe(text: &str) -> String {
    text.chars().filter(|character| !character.is_control()).take(200).collect()
}

/// The email message carrying `invitation` from `from` (RFC 6047 §2): one `text/calendar` part
/// with the iTIP method in its media type, and a subject from the event's summary.
pub fn imip_message(from: &str, invitation: &Invitation, now_seconds: u64) -> Vec<u8> {
    let lines = crate::scheduling::lines(&invitation.calendar);
    let method = crate::scheduling::first(&lines, "METHOD").unwrap_or("REQUEST").to_ascii_uppercase();
    let summary = crate::scheduling::first(&lines, "SUMMARY").unwrap_or("Event").replace("\\,", ",").replace("\;", ";");
    let uid = crate::scheduling::first(&lines, "UID").unwrap_or("event");
    let subject = match method.as_str() {
        "CANCEL" => format!("Cancelled: {summary}"),
        "REPLY" => format!("Reply: {summary}"),
        _ => format!("Invitation: {summary}"),
    };
    let message_id: String = uid.chars().filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '.' | '_')).take(64).collect();
    let mut message = format!(
        "From: {}\r\nTo: {}\r\nSubject: {}\r\nDate: {}\r\nMessage-ID: <{message_id}-{now_seconds}@eggdb>\r\nMIME-Version: 1.0\r\nContent-Type: text/calendar; charset=utf-8; method={method}\r\nContent-Transfer-Encoding: 8bit\r\n\r\n",
        header_safe(from),
        header_safe(&invitation.recipients.join(", ")),
        header_safe(&subject),
        rfc5322_date(now_seconds)
    )
    .into_bytes();
    message.extend_from_slice(invitation.calendar.as_bytes());
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invitations_become_text_calendar_mail() {
        let invitation = Invitation {
            path: "/calendars/eggdb/outbox/m1-1-0.ics".to_owned(),
            recipients: vec!["ann@example.org".to_owned()],
            calendar: "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:m1\r\nSUMMARY:Plan\\, part 2\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n".to_owned(),
        };
        let message = String::from_utf8(imip_message("eggdb@example.org", &invitation, 1_794_560_400)).unwrap();
        assert!(message.contains("To: ann@example.org\r\n"), "{message}");
        assert!(message.contains("Subject: Invitation: Plan, part 2\r\n"), "{message}");
        assert!(message.contains("Content-Type: text/calendar; charset=utf-8; method=REQUEST\r\n"), "{message}");
        assert!(message.contains("Date: Fri, 13 Nov 2026 09:00:00 +0000\r\n"), "{message}");
        assert!(message.ends_with("END:VCALENDAR\r\n"));
    }
}
