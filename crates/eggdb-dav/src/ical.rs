//! The parts of iCalendar (RFC 5545) a CalDAV `time-range` filter (RFC 4791 section 9.9)
//! needs: unfolded lines, the components of a calendar object, their start, end or duration,
//! and their recurrences (`RRULE` and `RDATE`, less `EXDATE`), expanded only as far as the range.

const SECONDS_PER_DAY: i64 = 86_400;
const MOST_OCCURRENCES: usize = 100_000;
const MOST_COMPONENTS: usize = 10_000;

/// A UTC time in seconds since 1970, with whether it was written as a date only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Moment {
    /// Seconds since 1970-01-01T00:00:00Z.
    pub seconds: i64,
    /// Whether the value was a `DATE` rather than a `DATE-TIME`.
    pub date_only: bool,
}

/// A `time-range` filter: either bound may be open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TimeRange {
    /// Its inclusive start.
    pub start: Option<i64>,
    /// Its exclusive end.
    pub end: Option<i64>,
}

impl TimeRange {
    fn overlaps(self, start: i64, end: i64) -> bool {
        self.start.is_none_or(|from| end > from) && self.end.is_none_or(|until| start < until)
    }

    fn holds_instant(self, at: i64) -> bool {
        self.start.is_none_or(|from| at >= from) && self.end.is_none_or(|until| at < until)
    }
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let of_era = year - era * 400;
    let month_index = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = of_era * 365 + of_era / 4 - of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
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

fn days_in_month(year: i64, month: i64) -> i64 {
    days_from_civil(if month == 12 { year + 1 } else { year }, if month == 12 { 1 } else { month + 1 }, 1)
        - days_from_civil(year, month, 1)
}

fn digits(text: &str) -> Option<i64> {
    (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())).then(|| text.parse().ok()).flatten()
}

/// Reads an iCalendar `DATE` (`20260112`) or `DATE-TIME` (`20260112T093000`, with `Z` for UTC).
/// Times in a named zone, and floating times, are read as UTC.
pub fn parse_moment(text: &str) -> Option<Moment> {
    let text = text.trim();
    let (date, time) = match text.split_once('T') {
        Some((date, time)) => (date, Some(time.strip_suffix('Z').unwrap_or(time))),
        None => (text, None),
    };
    if date.len() != 8 {
        return None;
    }
    let (year, month, day) = (digits(&date[..4])?, digits(&date[4..6])?, digits(&date[6..])?);
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let Some(time) = time else {
        return Some(Moment {
            seconds: days * SECONDS_PER_DAY,
            date_only: true,
        });
    };
    if time.len() != 6 {
        return None;
    }
    let (hour, minute, second) = (digits(&time[..2])?, digits(&time[2..4])?, digits(&time[4..])?);
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some(Moment {
        seconds: days * SECONDS_PER_DAY + hour * 3600 + minute * 60 + second,
        date_only: false,
    })
}

/// Reads an iCalendar `DURATION` (`PT1H30M`, `P1D`, `-P2W`) as seconds.
pub fn parse_duration(text: &str) -> Option<i64> {
    let text = text.trim();
    let (sign, rest) = match text.as_bytes().first() {
        Some(b'-') => (-1, &text[1..]),
        Some(b'+') => (1, &text[1..]),
        _ => (1, text),
    };
    let rest = rest.strip_prefix('P')?;
    let mut total: i64 = 0;
    let mut number = String::new();
    let mut in_time = false;
    let mut any = false;
    for character in rest.chars() {
        match character {
            '0'..='9' => number.push(character),
            'T' if number.is_empty() && !in_time => in_time = true,
            unit => {
                let value = digits(&number)?;
                number.clear();
                let seconds = match (unit, in_time) {
                    ('W', false) => 7 * SECONDS_PER_DAY,
                    ('D', false) => SECONDS_PER_DAY,
                    ('H', true) => 3600,
                    ('M', true) => 60,
                    ('S', true) => 1,
                    _ => return None,
                };
                total = total.checked_add(value.checked_mul(seconds)?)?;
                any = true;
            }
        }
    }
    (any && number.is_empty()).then_some(sign * total)
}

/// The content lines of an iCalendar text, unfolded: a line that starts with a space or a tab
/// continues the one before it.
pub fn unfolded(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        match (line.strip_prefix(' ').or_else(|| line.strip_prefix('\t')), lines.last_mut()) {
            (Some(continued), Some(last)) => last.push_str(continued),
            _ => lines.push(line.to_owned()),
        }
    }
    lines
}

/// One property line: its upper-cased name, its parameters and its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Property {
    /// The name, upper case.
    pub name: String,
    /// The parameters, names upper case.
    pub parameters: Vec<(String, String)>,
    /// The value.
    pub value: String,
}

fn property_of(line: &str) -> Option<Property> {
    let mut quoted = false;
    let colon = line.char_indices().find_map(|(at, character)| match character {
        '"' => {
            quoted = !quoted;
            None
        }
        ':' if !quoted => Some(at),
        _ => None,
    })?;
    let (head, value) = (&line[..colon], &line[colon + 1..]);
    let mut parts = head.split(';');
    let name = parts.next()?.trim().to_ascii_uppercase();
    let parameters = parts
        .filter_map(|parameter| parameter.split_once('='))
        .map(|(key, value)| (key.trim().to_ascii_uppercase(), value.trim_matches('"').to_owned()))
        .collect();
    Some(Property {
        name,
        parameters,
        value: value.to_owned(),
    })
}

/// A component (`VEVENT`, `VTODO`, `VJOURNAL`, `VFREEBUSY`, `VALARM` and the rest) with its own
/// properties, not its sub-components'.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    /// The name, upper case.
    pub name: String,
    /// Its properties, in order.
    pub properties: Vec<Property>,
}

impl Component {
    fn first(&self, name: &str) -> Option<&Property> {
        self.properties.iter().find(|property| property.name == name)
    }

    fn moment(&self, name: &str) -> Option<Moment> {
        self.first(name).and_then(|property| parse_moment(&property.value))
    }

    fn moments(&self, name: &str) -> Vec<i64> {
        self.properties
            .iter()
            .filter(|property| property.name == name)
            .flat_map(|property| property.value.split(','))
            .filter_map(|value| parse_moment(value).map(|moment| moment.seconds))
            .collect()
    }
}

/// Every component of a calendar object, nested ones included, in order of their `BEGIN`.
pub fn components(text: &str) -> Vec<Component> {
    let mut open: Vec<Component> = Vec::new();
    let mut done = Vec::new();
    for line in unfolded(text) {
        let Some(property) = property_of(&line) else {
            continue;
        };
        match property.name.as_str() {
            "BEGIN" => {
                if open.len() + done.len() >= MOST_COMPONENTS {
                    break;
                }
                open.push(Component {
                    name: property.value.trim().to_ascii_uppercase(),
                    properties: Vec::new(),
                });
            }
            "END" => {
                if let Some(component) = open.pop() {
                    done.push(component);
                }
            }
            _ => {
                if let Some(current) = open.last_mut() {
                    current.properties.push(property);
                }
            }
        }
    }
    done
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Frequency {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    frequency: Frequency,
    interval: i64,
    count: Option<usize>,
    until: Option<i64>,
    weekdays: Vec<i64>,
}

fn weekday_number(code: &str) -> Option<i64> {
    Some(match code {
        "MO" => 0,
        "TU" => 1,
        "WE" => 2,
        "TH" => 3,
        "FR" => 4,
        "SA" => 5,
        "SU" => 6,
        _ => return None,
    })
}

fn weekday_of(days: i64) -> i64 {
    (days + 3).rem_euclid(7)
}

enum ParsedRule {
    Known(Rule),
    Unknown,
}

fn parse_rule(text: &str) -> ParsedRule {
    let mut frequency = None;
    let mut rule = Rule {
        frequency: Frequency::Daily,
        interval: 1,
        count: None,
        until: None,
        weekdays: Vec::new(),
    };
    for part in text.split(';') {
        let Some((key, value)) = part.split_once('=') else {
            return ParsedRule::Unknown;
        };
        match key.trim().to_ascii_uppercase().as_str() {
            "FREQ" => {
                frequency = Some(match value.trim().to_ascii_uppercase().as_str() {
                    "DAILY" => Frequency::Daily,
                    "WEEKLY" => Frequency::Weekly,
                    "MONTHLY" => Frequency::Monthly,
                    "YEARLY" => Frequency::Yearly,
                    _ => return ParsedRule::Unknown,
                });
            }
            "INTERVAL" => match digits(value.trim()).filter(|interval| *interval > 0) {
                Some(interval) => rule.interval = interval,
                None => return ParsedRule::Unknown,
            },
            "COUNT" => match digits(value.trim()).and_then(|count| usize::try_from(count).ok()) {
                Some(count) => rule.count = Some(count),
                None => return ParsedRule::Unknown,
            },
            "UNTIL" => match parse_moment(value) {
                Some(moment) => {
                    rule.until = Some(if moment.date_only { moment.seconds + SECONDS_PER_DAY - 1 } else { moment.seconds })
                }
                None => return ParsedRule::Unknown,
            },
            "BYDAY" => {
                for code in value.split(',') {
                    match weekday_number(code.trim().to_ascii_uppercase().as_str()) {
                        Some(day) => rule.weekdays.push(day),
                        None => return ParsedRule::Unknown,
                    }
                }
            }
            "WKST" => {}
            _ => return ParsedRule::Unknown,
        }
    }
    match frequency {
        Some(found) if found == Frequency::Weekly || rule.weekdays.is_empty() => {
            rule.frequency = found;
            ParsedRule::Known(rule)
        }
        _ => ParsedRule::Unknown,
    }
}

fn period_starts(rule: &Rule, first: i64, stop_at: i64) -> Vec<i64> {
    let day = first.div_euclid(SECONDS_PER_DAY);
    let time = first.rem_euclid(SECONDS_PER_DAY);
    let (year, month, month_day) = civil_from_days(day);
    let mut starts = Vec::new();
    let mut produced = 0usize;
    let mut step: i64 = 0;
    while starts.len() < MOST_OCCURRENCES && step <= MOST_OCCURRENCES as i64 * 4 {
        let candidates: Vec<i64> = match rule.frequency {
            Frequency::Daily => vec![day + step * rule.interval],
            Frequency::Weekly if rule.weekdays.is_empty() => vec![day + step * rule.interval * 7],
            Frequency::Weekly => {
                let week_start = day - weekday_of(day) + step * rule.interval * 7;
                let mut days: Vec<i64> = rule.weekdays.iter().map(|weekday| week_start + weekday).collect();
                days.sort_unstable();
                days.dedup();
                days.retain(|candidate| *candidate >= day);
                days
            }
            Frequency::Monthly => {
                let months = (month - 1) + step * rule.interval;
                let (target_year, target_month) = (year + months.div_euclid(12), months.rem_euclid(12) + 1);
                if month_day <= days_in_month(target_year, target_month) {
                    vec![days_from_civil(target_year, target_month, month_day)]
                } else {
                    Vec::new()
                }
            }
            Frequency::Yearly => {
                let target_year = year + step * rule.interval;
                if month_day <= days_in_month(target_year, month) {
                    vec![days_from_civil(target_year, month, month_day)]
                } else {
                    Vec::new()
                }
            }
        };
        step += 1;
        for candidate in candidates {
            let start = candidate * SECONDS_PER_DAY + time;
            if rule.until.is_some_and(|until| start > until) || rule.count.is_some_and(|count| produced >= count) {
                return starts;
            }
            produced += 1;
            if start >= stop_at {
                return starts;
            }
            starts.push(start);
        }
    }
    starts
}

/// Whether `component` (one with `DTSTART`, `DTEND` or `DURATION`, `DUE` for to-dos, and maybe
/// `RRULE`, `RDATE` and `EXDATE`) has an instance overlapping `range`, by RFC 4791 section 9.9.
/// A recurrence rule this reader does not follow (`BYMONTHDAY`, `BYSETPOS` and the like) counts
/// as overlapping, so a client gets the object and filters it itself.
pub fn overlaps(component: &Component, range: TimeRange) -> bool {
    let start = component.moment("DTSTART");
    let end = component.moment("DTEND").or_else(|| component.moment("DUE"));
    let duration = component.first("DURATION").and_then(|property| parse_duration(&property.value));
    let Some(start) = start else {
        return match end {
            Some(due) => {
                range.start.is_none_or(|from| from < due.seconds) && range.end.is_none_or(|until| until >= due.seconds)
            }
            None => true,
        };
    };
    let length = match (end, duration) {
        (Some(end), _) => Some((end.seconds - start.seconds).max(0)),
        (None, Some(duration)) => Some(duration.max(0)),
        (None, None) if start.date_only => Some(SECONDS_PER_DAY),
        (None, None) => None,
    };
    let instance_holds = |at: i64| match length {
        Some(0) => range.holds_instant(at),
        Some(length) => range.overlaps(at, at + length),
        None => range.holds_instant(at),
    };
    let excluded = component.moments("EXDATE");
    let mut starts = vec![start.seconds];
    if let Some(rule) = component.first("RRULE") {
        match parse_rule(&rule.value) {
            ParsedRule::Known(rule) => {
                let stop_at = range.end.unwrap_or(i64::MAX);
                starts = period_starts(&rule, start.seconds, stop_at);
                if starts.is_empty() && start.seconds < stop_at {
                    starts.push(start.seconds);
                }
            }
            ParsedRule::Unknown => return true,
        }
    }
    starts.extend(component.moments("RDATE"));
    starts.into_iter().filter(|at| !excluded.contains(at)).any(instance_holds)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> i64 {
        parse_moment(text).unwrap().seconds
    }

    fn range(start: &str, end: &str) -> TimeRange {
        TimeRange {
            start: Some(at(start)),
            end: Some(at(end)),
        }
    }

    fn event(lines: &str) -> Component {
        components(&format!("BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\n{lines}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"))
            .into_iter()
            .find(|component| component.name == "VEVENT")
            .unwrap()
    }

    #[test]
    fn values_parse_and_lines_unfold() {
        assert_eq!(at("19700102T000000Z"), 86_400);
        assert_eq!(parse_moment("20260229"), None);
        assert_eq!(parse_moment("20240229").map(|moment| moment.date_only), Some(true));
        assert_eq!(parse_duration("PT1H30M"), Some(5400));
        assert_eq!(parse_duration("-P1W"), Some(-604_800));
        assert_eq!(parse_duration("P1H"), None);
        assert_eq!(unfolded("SUMMARY:a\r\n  b\r\nX:y"), vec!["SUMMARY:a b".to_owned(), "X:y".to_owned()]);
    }

    #[test]
    fn single_events_overlap_by_rfc_4791() {
        let meeting = event("DTSTART:20260112T090000Z\r\nDTEND:20260112T100000Z");
        assert!(overlaps(&meeting, range("20260112T000000Z", "20260113T000000Z")));
        assert!(!overlaps(&meeting, range("20260112T100000Z", "20260113T000000Z")));
        let all_day = event("DTSTART;VALUE=DATE:20260112");
        assert!(overlaps(&all_day, range("20260112T230000Z", "20260113T000000Z")));
        assert!(!overlaps(&all_day, range("20260113T000000Z", "20260114T000000Z")));
        let open = TimeRange {
            start: Some(at("20260101T000000Z")),
            end: None,
        };
        assert!(overlaps(&event("DTSTART:20260112T090000Z\r\nDURATION:PT15M"), open));
    }

    #[test]
    fn recurrences_expand_only_as_far_as_the_range() {
        let weekly = event("DTSTART:20260105T090000Z\r\nDURATION:PT1H\r\nRRULE:FREQ=WEEKLY;BYDAY=MO,WE;COUNT=6\r\nEXDATE:20260114T090000Z");
        assert!(overlaps(&weekly, range("20260107T000000Z", "20260108T000000Z")));
        assert!(!overlaps(&weekly, range("20260114T000000Z", "20260115T000000Z")));
        assert!(overlaps(&weekly, range("20260121T000000Z", "20260122T000000Z")));
        assert!(!overlaps(&weekly, range("20260126T000000Z", "20260127T000000Z")));
        let monthly = event("DTSTART:20260131T090000Z\r\nDTEND:20260131T100000Z\r\nRRULE:FREQ=MONTHLY");
        assert!(!overlaps(&monthly, range("20260201T000000Z", "20260301T000000Z")));
        assert!(overlaps(&monthly, range("20260331T000000Z", "20260401T000000Z")));
        let unknown = event("DTSTART:20260101T090000Z\r\nRRULE:FREQ=MONTHLY;BYSETPOS=-1;BYDAY=FR");
        assert!(overlaps(&unknown, range("20300101T000000Z", "20300102T000000Z")));
    }
}
