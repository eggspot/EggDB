use super::compile::{SECONDS_PER_DAY, weekday_of, year_of};
use crate::datetime;

const LONGEST_ABBREVIATION: usize = 10;
const SHORTEST_ABBREVIATION: usize = 3;
const LONGEST_OFFSET_HOURS: i64 = 24;
const LONGEST_RULE_HOURS: i64 = 167;
const DEFAULT_CHANGE_TIME: i32 = 2 * 3600;
const DAYS_BEFORE_MONTH: [u16; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Abbreviation {
    bytes: [u8; LONGEST_ABBREVIATION],
    length: u8,
}

impl Abbreviation {
    fn new(text: &[u8]) -> Option<Self> {
        if !(SHORTEST_ABBREVIATION..=LONGEST_ABBREVIATION).contains(&text.len()) {
            return None;
        }
        let mut bytes = [0; LONGEST_ABBREVIATION];
        bytes.get_mut(..text.len())?.copy_from_slice(text);
        Some(Self {
            bytes,
            length: u8::try_from(text.len()).ok()?,
        })
    }

    pub(super) fn as_str(&self) -> &str {
        self.bytes
            .get(..usize::from(self.length))
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .unwrap_or("")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DayRule {
    JulianWithoutLeapDay(u16),
    FromZero(u16),
    Weekday { month: u8, week: u8, weekday: u8 },
}

impl DayRule {
    fn day(self, year: i64) -> i64 {
        match self {
            Self::JulianWithoutLeapDay(number) => {
                let ordinal = number.saturating_sub(1);
                let month = DAYS_BEFORE_MONTH
                    .iter()
                    .rposition(|&before| before <= ordinal)
                    .unwrap_or(0);
                let before = DAYS_BEFORE_MONTH.get(month).copied().unwrap_or(0);
                let month_number = i64::try_from(month).unwrap_or(0) + 1;
                datetime::days_from_civil(year, month_number, i64::from(ordinal - before) + 1)
            }
            Self::FromZero(number) => datetime::days_from_civil(year, 1, 1) + i64::from(number),
            Self::Weekday {
                month,
                week,
                weekday,
            } => {
                let month = i64::from(month);
                let weekday = i64::from(weekday);
                if week == 5 {
                    let last = datetime::days_from_civil(
                        year,
                        month,
                        datetime::days_in_month(year, month),
                    );
                    last - (weekday_of(last) - weekday).rem_euclid(7)
                } else {
                    let first = datetime::days_from_civil(year, month, 1) + (i64::from(week) - 1) * 7;
                    first + (weekday - weekday_of(first)).rem_euclid(7)
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Change {
    day: DayRule,
    time: i32,
}

impl Change {
    fn local_seconds(self, year: i64) -> i64 {
        self.day.day(year) * SECONDS_PER_DAY + i64::from(self.time)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Daylight {
    abbreviation: Abbreviation,
    offset: i32,
    start: Change,
    end: Change,
}

/// A zone written as a POSIX `TZ` rule string (`EST5EDT,M3.2.0,M11.1.0`, `<+03>-3`): a
/// standard time, and optionally a daylight saving time with the rules that start and end it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PosixZone {
    standard: Abbreviation,
    offset: i32,
    daylight: Option<Daylight>,
}

/// One of a POSIX zone's two local times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PosixTime {
    pub(super) offset: i32,
    pub(super) is_dst: bool,
    pub(super) abbreviation: Abbreviation,
}

struct Reader<'t> {
    bytes: &'t [u8],
    at: usize,
}

impl Reader<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn take(&mut self, byte: u8) -> bool {
        let found = self.peek() == Some(byte);
        if found {
            self.at += 1;
        }
        found
    }

    fn done(&self) -> bool {
        self.at >= self.bytes.len()
    }

    fn abbreviation(&mut self) -> Option<Abbreviation> {
        if self.take(b'<') {
            let start = self.at;
            while self
                .peek()
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-'))
            {
                self.at += 1;
            }
            let text = self.bytes.get(start..self.at)?;
            if !self.take(b'>') {
                return None;
            }
            return Abbreviation::new(text);
        }
        let start = self.at;
        while self.peek().is_some_and(|byte| byte.is_ascii_alphabetic()) {
            self.at += 1;
        }
        Abbreviation::new(self.bytes.get(start..self.at)?)
    }

    fn number(&mut self, longest: usize) -> Option<i64> {
        let start = self.at;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) && self.at - start < longest {
            self.at += 1;
        }
        let digits = self.bytes.get(start..self.at)?;
        if digits.is_empty() {
            return None;
        }
        Some(
            digits
                .iter()
                .fold(0i64, |total, digit| total * 10 + i64::from(digit - b'0')),
        )
    }

    fn clock(&mut self, longest_hours: i64) -> Option<i64> {
        let sign = if self.take(b'-') {
            -1
        } else {
            self.take(b'+');
            1
        };
        let hours = self.number(3)?;
        let mut minutes = 0;
        let mut seconds = 0;
        if self.take(b':') {
            minutes = self.number(2)?;
            if self.take(b':') {
                seconds = self.number(2)?;
            }
        }
        if hours > longest_hours || minutes > 59 || seconds > 59 {
            return None;
        }
        Some(sign * (hours * 3600 + minutes * 60 + seconds))
    }

    fn east_offset(&mut self) -> Option<i32> {
        let west = self.clock(LONGEST_OFFSET_HOURS)?;
        let east = i32::try_from(-west).ok()?;
        datetime::valid_offset(east).then_some(east)
    }

    fn day_rule(&mut self) -> Option<DayRule> {
        if self.take(b'M') {
            let month = self.number(2)?;
            if !self.take(b'.') {
                return None;
            }
            let week = self.number(1)?;
            if !self.take(b'.') {
                return None;
            }
            let weekday = self.number(1)?;
            if !(1..=12).contains(&month) || !(1..=5).contains(&week) || weekday > 6 {
                return None;
            }
            return Some(DayRule::Weekday {
                month: u8::try_from(month).ok()?,
                week: u8::try_from(week).ok()?,
                weekday: u8::try_from(weekday).ok()?,
            });
        }
        if self.take(b'J') {
            let number = self.number(3)?;
            if !(1..=365).contains(&number) {
                return None;
            }
            return u16::try_from(number).ok().map(DayRule::JulianWithoutLeapDay);
        }
        let number = self.number(3)?;
        if number > 365 {
            return None;
        }
        u16::try_from(number).ok().map(DayRule::FromZero)
    }

    fn change(&mut self) -> Option<Change> {
        let day = self.day_rule()?;
        let time = if self.take(b'/') {
            i32::try_from(self.clock(LONGEST_RULE_HOURS)?).ok()?
        } else {
            DEFAULT_CHANGE_TIME
        };
        Some(Change { day, time })
    }
}

fn united_states_rules() -> (Change, Change) {
    (
        Change {
            day: DayRule::Weekday {
                month: 3,
                week: 2,
                weekday: 0,
            },
            time: DEFAULT_CHANGE_TIME,
        },
        Change {
            day: DayRule::Weekday {
                month: 11,
                week: 1,
                weekday: 0,
            },
            time: DEFAULT_CHANGE_TIME,
        },
    )
}

impl PosixZone {
    /// Reads `text` as a POSIX `TZ` string; `None` unless all of it is one. A daylight time
    /// without rules follows the United States' rules, as PostgreSQL documents.
    pub(super) fn read(text: &str) -> Option<Self> {
        let mut reader = Reader {
            bytes: text.trim().as_bytes(),
            at: 0,
        };
        let standard = reader.abbreviation()?;
        let offset = reader.east_offset()?;
        if reader.done() {
            return Some(Self {
                standard,
                offset,
                daylight: None,
            });
        }
        let abbreviation = reader.abbreviation()?;
        let daylight_offset = match reader.peek() {
            Some(b',') | None => offset.checked_add(3600)?,
            Some(_) => reader.east_offset()?,
        };
        if !datetime::valid_offset(daylight_offset) {
            return None;
        }
        let (start, end) = if reader.take(b',') {
            let start = reader.change()?;
            if !reader.take(b',') {
                return None;
            }
            (start, reader.change()?)
        } else {
            united_states_rules()
        };
        reader.done().then_some(Self {
            standard,
            offset,
            daylight: Some(Daylight {
                abbreviation,
                offset: daylight_offset,
                start,
                end,
            }),
        })
    }

    fn standard_time(&self) -> PosixTime {
        PosixTime {
            offset: self.offset,
            is_dst: false,
            abbreviation: self.standard,
        }
    }

    fn shifts_around(&self, daylight: &Daylight, seconds: i64) -> Vec<(i64, PosixTime)> {
        let daylight_time = PosixTime {
            offset: daylight.offset,
            is_dst: true,
            abbreviation: daylight.abbreviation,
        };
        let year = year_of(seconds);
        let mut shifts = Vec::with_capacity(6);
        for year in year.saturating_sub(1)..=year.saturating_add(1) {
            let start = daylight.start.local_seconds(year) - i64::from(self.offset);
            let end = daylight.end.local_seconds(year) - i64::from(daylight.offset);
            shifts.push((start, daylight_time));
            shifts.push((end, self.standard_time()));
        }
        shifts.sort_by_key(|&(at, _)| at);
        shifts
    }

    /// The local time in effect at `utc`, in seconds since 2000-01-01 UTC.
    pub(super) fn local_time_at(&self, utc: i64) -> PosixTime {
        let Some(daylight) = self.daylight.as_ref() else {
            return self.standard_time();
        };
        self.shifts_around(daylight, utc)
            .into_iter()
            .take_while(|&(at, _)| at <= utc)
            .last()
            .map_or_else(|| self.standard_time(), |(_, time)| time)
    }

    /// The offset of the local time `local`: a skipped time takes the offset from before the
    /// change and a repeated time the offset from after it.
    pub(super) fn offset_of_local(&self, local: i64) -> i32 {
        let Some(daylight) = self.daylight.as_ref() else {
            return self.offset;
        };
        let mut offset = self.offset;
        for (at, time) in self.shifts_around(daylight, local) {
            if at + i64::from(time.offset) <= local {
                offset = time.offset;
            } else {
                break;
            }
        }
        offset
    }
}

#[cfg(test)]
mod tests {
    use super::PosixZone;
    use crate::datetime;

    fn seconds(year: i64, month: i64, day: i64, hour: i64, minute: i64) -> i64 {
        datetime::days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60
    }

    fn zone(text: &str) -> PosixZone {
        PosixZone::read(text).unwrap_or_else(|| panic!("{text} is a POSIX zone"))
    }

    #[test]
    fn fixed_posix_zones_read_offsets_west_of_utc() {
        let moscow_like = zone("<+03>-3");
        let time = moscow_like.local_time_at(0);
        assert_eq!((time.offset, time.is_dst), (3 * 3600, false));
        assert_eq!(time.abbreviation.as_str(), "+03");
        assert_eq!(zone("EST5").local_time_at(0).offset, -5 * 3600);
        assert_eq!(zone("<-0330>3:30").offset_of_local(0), -(3 * 3600 + 1800));
    }

    #[test]
    fn rules_start_and_end_daylight_time() {
        let eastern = zone("EST5EDT,M3.2.0,M11.1.0");
        let before = eastern.local_time_at(seconds(2026, 3, 8, 6, 59));
        let after = eastern.local_time_at(seconds(2026, 3, 8, 7, 0));
        assert_eq!((before.offset, before.abbreviation.as_str()), (-5 * 3600, "EST"));
        assert_eq!((after.offset, after.abbreviation.as_str()), (-4 * 3600, "EDT"));
        assert!(after.is_dst);
        assert_eq!(eastern.local_time_at(seconds(2026, 11, 1, 5, 59)).offset, -4 * 3600);
        assert_eq!(eastern.local_time_at(seconds(2026, 11, 1, 6, 0)).offset, -5 * 3600);
        assert_eq!(eastern.offset_of_local(seconds(2026, 3, 8, 2, 30)), -5 * 3600);
        assert_eq!(eastern.offset_of_local(seconds(2026, 11, 1, 1, 30)), -5 * 3600);
        assert_eq!(eastern.offset_of_local(seconds(2026, 7, 1, 12, 0)), -4 * 3600);
    }

    #[test]
    fn a_daylight_time_without_rules_follows_the_united_states() {
        let pacific = zone("PST8PDT");
        assert_eq!(pacific.local_time_at(seconds(2026, 7, 1, 12, 0)).offset, -7 * 3600);
        assert_eq!(pacific.local_time_at(seconds(2026, 1, 1, 12, 0)).offset, -8 * 3600);
    }

    #[test]
    fn southern_rules_wrap_the_year_and_julian_days_ignore_leap_days() {
        let sydney = zone("AEST-10AEDT,M10.1.0,M4.1.0/3");
        assert_eq!(sydney.local_time_at(seconds(2026, 1, 15, 0, 0)).offset, 11 * 3600);
        assert_eq!(sydney.local_time_at(seconds(2026, 6, 15, 0, 0)).offset, 10 * 3600);
        let julian = zone("XST3XDT,J60/0,J300/0");
        assert_eq!(julian.local_time_at(seconds(2024, 3, 1, 4, 0)).offset, -2 * 3600);
        assert_eq!(julian.local_time_at(seconds(2024, 2, 29, 12, 0)).offset, -3 * 3600);
    }

    #[test]
    fn malformed_strings_are_not_zones() {
        for text in [
            "",
            "E5",
            "EST",
            "EST5EDT,M3.2.0",
            "EST5EDT,M13.2.0,M11.1.0",
            "EST5EDT,M3.6.0,M11.1.0",
            "EST5EDT,J0,J100",
            "EST5EDT,366,100",
            "EST25",
            "<+03-3",
            "<+0303030303030>-3",
            "EST5EDT,M3.2.0/168,M11.1.0",
            "EST5 extra",
        ] {
            assert!(PosixZone::read(text).is_none(), "{text}");
        }
    }
}
