mod collector;
mod framing;

#[cfg(test)]
mod tests;

pub use collector::{MAX_SYSLOG_MESSAGE_BYTES, SyslogCollector};

const MICROS_PER_SECOND: i64 = 1_000_000;
const SECONDS_PER_DAY: i64 = 86_400;
const USER_NOTICE: u8 = 13;
const BYTE_ORDER_MARK: &[u8] = b"\xEF\xBB\xBF";
const MONTHS: [&[u8; 3]; 12] = [
    b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec",
];
const MOST_HOSTNAME_BYTES: usize = 255;
const MOST_APP_NAME_BYTES: usize = 48;
const MOST_PROCID_BYTES: usize = 128;
const MOST_MSGID_BYTES: usize = 32;
const FUTURE_SLACK_MICROS: i64 = 31 * SECONDS_PER_DAY * MICROS_PER_SECOND;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Message {
    pub(crate) facility: u8,
    pub(crate) severity: u8,
    pub(crate) timestamp_micros: Option<i64>,
    pub(crate) hostname: Option<String>,
    pub(crate) app_name: Option<String>,
    pub(crate) procid: Option<String>,
    pub(crate) msgid: Option<String>,
    pub(crate) structured_data: Option<String>,
    pub(crate) message: String,
}

impl Message {
    fn of_priority(priority: u8, message: String) -> Self {
        Self {
            facility: priority / 8,
            severity: priority % 8,
            timestamp_micros: None,
            hostname: None,
            app_name: None,
            procid: None,
            msgid: None,
            structured_data: None,
            message,
        }
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace('\0', "\u{FFFD}")
}

fn trimmed(raw: &[u8]) -> &[u8] {
    let mut end = raw.len();
    while end > 0 && matches!(raw.get(end - 1), Some(b'\n' | b'\r' | b'\0')) {
        end -= 1;
    }
    raw.get(..end).unwrap_or_default()
}

/// Reads one syslog message as RFC 5424 when it says it is (`<PRI>1 `), and otherwise as RFC
/// 3164 does, which never fails: a message without a valid priority is `user.notice` (13)
/// with all of it as the text, and one without a timestamp keeps everything after the
/// priority as the text. 3164 timestamps carry no year or zone: they are read as UTC in the
/// year of `received_unix_micros`, or the year before when that would put them more than 31
/// days after it.
pub(crate) fn parse(raw: &[u8], received_unix_micros: i64) -> Message {
    let raw = trimmed(raw);
    match priority(raw) {
        Some((priority, rest)) => {
            rfc5424(priority, rest).unwrap_or_else(|| rfc3164(priority, rest, received_unix_micros))
        }
        None => Message::of_priority(USER_NOTICE, text(raw)),
    }
}

fn priority(raw: &[u8]) -> Option<(u8, &[u8])> {
    let rest = raw.strip_prefix(b"<")?;
    let close = rest.iter().take(4).position(|&byte| byte == b'>')?;
    let digits = rest.get(..close)?;
    let leading_zero = digits.len() > 1 && digits.first() == Some(&b'0');
    if leading_zero {
        return None;
    }
    let value = number(digits)?;
    if value > 191 {
        return None;
    }
    Some((u8::try_from(value).ok()?, rest.get(close + 1..)?))
}

fn number(digits: &[u8]) -> Option<i64> {
    if digits.is_empty() || digits.len() > 9 {
        return None;
    }
    digits.iter().try_fold(0_i64, |value, &digit| {
        digit
            .is_ascii_digit()
            .then(|| value * 10 + i64::from(digit - b'0'))
    })
}

fn printable(bytes: &[u8]) -> bool {
    bytes.iter().all(|byte| (33..=126).contains(byte))
}

fn header_field(input: &[u8], most: usize) -> Option<(Option<String>, &[u8])> {
    let space = input.iter().position(|&byte| byte == b' ')?;
    let token = input.get(..space)?;
    if token.is_empty() || token.len() > most || !printable(token) {
        return None;
    }
    let value = (token != b"-").then(|| text(token));
    Some((value, input.get(space + 1..)?))
}

fn rfc5424(priority: u8, rest: &[u8]) -> Option<Message> {
    let rest = rest.strip_prefix(b"1 ")?;
    let space = rest.iter().position(|&byte| byte == b' ')?;
    let timestamp = match rest.get(..space)? {
        b"-" => None,
        stamp => Some(rfc3339(stamp)?),
    };
    let rest = rest.get(space + 1..)?;
    let (hostname, rest) = header_field(rest, MOST_HOSTNAME_BYTES)?;
    let (app_name, rest) = header_field(rest, MOST_APP_NAME_BYTES)?;
    let (procid, rest) = header_field(rest, MOST_PROCID_BYTES)?;
    let (msgid, rest) = header_field(rest, MOST_MSGID_BYTES)?;
    let (structured_data, rest) = structured_data(rest)?;
    let body = if rest.is_empty() {
        rest
    } else {
        rest.strip_prefix(b" ")?
    };
    let body = body.strip_prefix(BYTE_ORDER_MARK).unwrap_or(body);
    Some(Message {
        timestamp_micros: timestamp,
        hostname,
        app_name,
        procid,
        msgid,
        structured_data,
        ..Message::of_priority(priority, text(body))
    })
}

fn structured_data(input: &[u8]) -> Option<(Option<String>, &[u8])> {
    if let Some(rest) = input.strip_prefix(b"-") {
        return Some((None, rest));
    }
    let mut at = 0;
    while input.get(at) == Some(&b'[') {
        at += 1;
        let mut quoted = false;
        loop {
            let byte = *input.get(at)?;
            at += 1;
            match (quoted, byte) {
                (true, b'\\') => {
                    input.get(at)?;
                    at += 1;
                }
                (_, b'"') => quoted = !quoted,
                (false, b']') => break,
                _ => {}
            }
        }
    }
    if at == 0 {
        return None;
    }
    Some((Some(text(input.get(..at)?)), input.get(at..)?))
}

fn digits_at(bytes: &[u8], from: usize, count: usize) -> Option<i64> {
    number(bytes.get(from..from + count)?)
}

fn rfc3339(stamp: &[u8]) -> Option<i64> {
    let separators = [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':')];
    if separators
        .iter()
        .any(|&(at, separator)| stamp.get(at) != Some(&separator))
    {
        return None;
    }
    let year = digits_at(stamp, 0, 4)?;
    let month = digits_at(stamp, 5, 2)?;
    let day = digits_at(stamp, 8, 2)?;
    let hour = digits_at(stamp, 11, 2)?;
    let minute = digits_at(stamp, 14, 2)?;
    let second = digits_at(stamp, 17, 2)?;
    let mut rest = stamp.get(19..)?;
    let mut fraction_micros = 0;
    if let Some(fraction) = rest.strip_prefix(b".") {
        let length = fraction
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if !(1..=6).contains(&length) {
            return None;
        }
        fraction_micros = number(fraction.get(..length)?)? * 10_i64.pow(6 - length as u32);
        rest = fraction.get(length..)?;
    }
    let offset_seconds = match rest {
        b"Z" => 0,
        [sign @ (b'+' | b'-'), zone @ ..] => {
            if zone.len() != 5 || zone.get(2) != Some(&b':') {
                return None;
            }
            let hours = digits_at(zone, 0, 2)?;
            let minutes = digits_at(zone, 3, 2)?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            let seconds = hours * 3600 + minutes * 60;
            if *sign == b'-' { -seconds } else { seconds }
        }
        _ => return None,
    };
    let seconds = civil_seconds(year, month, day, hour, minute, second)? - offset_seconds;
    Some(seconds * MICROS_PER_SECOND + fraction_micros)
}

fn leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_from_march = (month + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn year_of_days(days: i64) -> i64 {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let year = year_of_era + era * 400;
    if month_from_march >= 10 {
        year + 1
    } else {
        year
    }
}

fn civil_seconds(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
) -> Option<i64> {
    let valid = (1..=12).contains(&month)
        && (1..=days_in_month(year, month)).contains(&day)
        && hour <= 23
        && minute <= 59
        && second <= 59;
    valid.then(|| {
        days_from_civil(year, month, day) * SECONDS_PER_DAY + hour * 3600 + minute * 60 + second
    })
}

fn bsd_timestamp(content: &[u8], received_unix_micros: i64) -> Option<(i64, &[u8])> {
    let stamp = content.get(..15)?;
    let month_name = stamp.get(..3)?;
    let month = MONTHS
        .iter()
        .position(|name| name.as_slice() == month_name)?;
    let month = i64::try_from(month).ok()? + 1;
    if stamp.get(3) != Some(&b' ')
        || stamp.get(6) != Some(&b' ')
        || stamp.get(9) != Some(&b':')
        || stamp.get(12) != Some(&b':')
    {
        return None;
    }
    let day = match stamp.get(4..6)? {
        [b' ', digit] => number(&[*digit])?,
        digits => number(digits)?,
    };
    let hour = digits_at(stamp, 7, 2)?;
    let minute = digits_at(stamp, 10, 2)?;
    let second = digits_at(stamp, 13, 2)?;
    let rest = content.get(15..)?;
    let rest = if rest.is_empty() {
        rest
    } else {
        rest.strip_prefix(b" ")?
    };
    let received_year =
        year_of_days(received_unix_micros.div_euclid(MICROS_PER_SECOND * SECONDS_PER_DAY));
    let at_year = |year| {
        civil_seconds(year, month, day, hour, minute, second)
            .map(|seconds| seconds * MICROS_PER_SECOND)
    };
    let micros = match at_year(received_year) {
        Some(micros) if micros <= received_unix_micros.saturating_add(FUTURE_SLACK_MICROS) => {
            micros
        }
        _ => at_year(received_year - 1)?,
    };
    Some((micros, rest))
}

fn rfc3164(priority: u8, content: &[u8], received_unix_micros: i64) -> Message {
    let (timestamp, hostname, rest) = match bsd_timestamp(content, received_unix_micros) {
        Some((micros, rest)) => match bsd_hostname(rest) {
            Some((hostname, rest)) => (Some(micros), Some(hostname), rest),
            None => (Some(micros), None, rest),
        },
        None => (None, None, content),
    };
    let (app_name, procid, body) = match tag(rest) {
        Some((app_name, procid, body)) => (Some(app_name), procid, body),
        None => (None, None, rest),
    };
    Message {
        timestamp_micros: timestamp,
        hostname,
        app_name,
        procid,
        ..Message::of_priority(priority, text(body))
    }
}

fn bsd_hostname(rest: &[u8]) -> Option<(String, &[u8])> {
    let space = rest.iter().position(|&byte| byte == b' ')?;
    let token = rest.get(..space)?;
    let looks_like_a_tag = token.ends_with(b":") || token.contains(&b'[');
    if token.is_empty()
        || token.len() > MOST_HOSTNAME_BYTES
        || !printable(token)
        || looks_like_a_tag
    {
        return None;
    }
    Some((text(token), rest.get(space + 1..)?))
}

fn tag(rest: &[u8]) -> Option<(String, Option<String>, &[u8])> {
    let end = rest
        .iter()
        .take(MOST_APP_NAME_BYTES + 1)
        .position(|&byte| matches!(byte, b'[' | b':' | b' '))?;
    let name = rest.get(..end)?;
    if name.is_empty() || !printable(name) {
        return None;
    }
    let mut after = rest.get(end..)?;
    let mut procid = None;
    if let Some(inside) = after.strip_prefix(b"[") {
        let close = inside
            .iter()
            .take(MOST_PROCID_BYTES + 1)
            .position(|&byte| byte == b']')?;
        let id = inside.get(..close)?;
        if id.is_empty() || !printable(id) {
            return None;
        }
        procid = Some(text(id));
        after = inside.get(close + 1..)?;
    }
    let body = after.strip_prefix(b":")?;
    let body = body.strip_prefix(b" ").unwrap_or(body);
    Some((text(name), procid, body))
}
