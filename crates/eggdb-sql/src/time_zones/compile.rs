use crate::datetime;
use std::collections::HashMap;

pub(super) const SECONDS_PER_DAY: i64 = 86_400;
const LAST_LISTED_YEAR: i64 = 2037;
const EARLIEST_RULE_YEAR: i64 = 1800;
const MAX_YEAR: i64 = i64::MAX;
const MIN_YEAR: i64 = i64::MIN;
const MAX_LINKS_FOLLOWED: usize = 4;

#[derive(Debug)]
pub(super) struct Source {
    lines: Vec<&'static str>,
    zones: HashMap<String, (&'static str, usize)>,
    rules: HashMap<&'static str, Vec<usize>>,
}

impl Source {
    pub(super) fn read(text: &'static str) -> Self {
        let mut source = Self {
            lines: Vec::new(),
            zones: HashMap::new(),
            rules: HashMap::new(),
        };
        let mut links: Vec<(&'static str, &'static str)> = Vec::new();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("");
            if line.trim().is_empty() {
                continue;
            }
            let index = source.lines.len();
            source.lines.push(line);
            let mut fields = line.split_ascii_whitespace();
            let kind = fields.next().unwrap_or("");
            let name = fields.next().unwrap_or("");
            if kind.eq_ignore_ascii_case("z") || kind.eq_ignore_ascii_case("zone") {
                source
                    .zones
                    .insert(name.to_ascii_lowercase(), (name, index));
            } else if kind.eq_ignore_ascii_case("r") || kind.eq_ignore_ascii_case("rule") {
                source.rules.entry(name).or_default().push(index);
            } else if (kind.eq_ignore_ascii_case("l") || kind.eq_ignore_ascii_case("link"))
                && let Some(alias) = fields.next()
            {
                links.push((name, alias));
            }
        }
        for _ in 0..MAX_LINKS_FOLLOWED {
            let mut unresolved = Vec::new();
            for (target, alias) in links {
                match source.zones.get(&target.to_ascii_lowercase()) {
                    Some(&(_, first)) => {
                        source
                            .zones
                            .entry(alias.to_ascii_lowercase())
                            .or_insert((alias, first));
                    }
                    None => unresolved.push((target, alias)),
                }
            }
            if unresolved.is_empty() {
                break;
            }
            links = unresolved;
        }
        source
    }

    pub(super) fn zone(&self, name: &str) -> Option<(&'static str, usize)> {
        self.zones.get(&name.to_ascii_lowercase()).copied()
    }

    pub(super) fn zone_names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.zones.values().map(|&(name, _)| name)
    }

    fn zone_lines(&self, first: usize) -> Option<Vec<ZoneLine>> {
        let mut found = Vec::new();
        let mut at = first;
        loop {
            let line = *self.lines.get(at)?;
            let fields: Vec<&'static str> = line.split_ascii_whitespace().collect();
            let rest = if at == first {
                fields.get(2..)?
            } else {
                fields.as_slice()
            };
            let parsed = ZoneLine::read(rest)?;
            let ends = parsed.until.is_none();
            found.push(parsed);
            if ends {
                return Some(found);
            }
            at += 1;
        }
    }

    fn rules_named(&self, name: &str) -> Option<Vec<Rule>> {
        self.rules
            .get(name)?
            .iter()
            .map(|&index| {
                let fields: Vec<&'static str> =
                    self.lines.get(index)?.split_ascii_whitespace().collect();
                Rule::read(fields.get(2..)?)
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Clock {
    Wall,
    Standard,
    Universal,
}

#[derive(Debug, Clone, Copy)]
struct ClockTime {
    seconds: i64,
    clock: Clock,
}

impl ClockTime {
    fn read(text: &str) -> Option<Self> {
        let (number, clock) = match text.as_bytes().last()? {
            b'w' => (&text[..text.len() - 1], Clock::Wall),
            b's' => (&text[..text.len() - 1], Clock::Standard),
            b'u' | b'g' | b'z' => (&text[..text.len() - 1], Clock::Universal),
            _ => (text, Clock::Wall),
        };
        Some(Self {
            seconds: duration(number)?,
            clock,
        })
    }

    fn universal(self, local: i64, standard_offset: i64, save: i64) -> i64 {
        match self.clock {
            Clock::Universal => local,
            Clock::Standard => local - standard_offset,
            Clock::Wall => local - standard_offset - save,
        }
    }
}

pub(super) fn duration(text: &str) -> Option<i64> {
    if text == "-" {
        return Some(0);
    }
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, text),
    };
    let mut parts = digits.split(':');
    let part = |part: Option<&str>, limit: usize| -> Option<i64> {
        let part = part?;
        let whole = part.split('.').next()?;
        if whole.is_empty() || whole.len() > limit || !whole.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        whole.parse().ok()
    };
    let hours = part(parts.next(), 3)?;
    let minutes = match parts.next() {
        None => 0,
        some => part(some, 2)?,
    };
    let seconds = match parts.next() {
        None => 0,
        some => part(some, 2)?,
    };
    if parts.next().is_some() || minutes > 59 || seconds > 59 {
        return None;
    }
    Some(sign * (hours * 3600 + minutes * 60 + seconds))
}

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

const WEEKDAYS: [&str; 7] = [
    "sunday",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
];

fn abbreviated(word: &str, names: &[&str]) -> Option<i64> {
    let lower = word.to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    let mut matching = names
        .iter()
        .enumerate()
        .filter(|(_, name)| name.starts_with(lower.as_str()));
    let (index, _) = matching.next()?;
    if matching.next().is_some() {
        return None;
    }
    i64::try_from(index).ok()
}

#[derive(Debug, Clone, Copy)]
enum DaySpec {
    Fixed(i64),
    Last(i64),
    OnOrAfter(i64, i64),
    OnOrBefore(i64, i64),
}

impl DaySpec {
    fn read(text: &str) -> Option<Self> {
        if let Some(weekday) = text.strip_prefix("last") {
            return Some(Self::Last(abbreviated(weekday, &WEEKDAYS)?));
        }
        if let Some((weekday, day)) = text.split_once(">=") {
            return Some(Self::OnOrAfter(abbreviated(weekday, &WEEKDAYS)?, day_number(day)?));
        }
        if let Some((weekday, day)) = text.split_once("<=") {
            return Some(Self::OnOrBefore(abbreviated(weekday, &WEEKDAYS)?, day_number(day)?));
        }
        Some(Self::Fixed(day_number(text)?))
    }

    fn day(self, year: i64, month: i64) -> i64 {
        match self {
            Self::Fixed(day) => datetime::days_from_civil(year, month, day),
            Self::Last(weekday) => {
                let last = datetime::days_from_civil(year, month, datetime::days_in_month(year, month));
                last - (weekday_of(last) - weekday).rem_euclid(7)
            }
            Self::OnOrAfter(weekday, day) => {
                let base = datetime::days_from_civil(year, month, day);
                base + (weekday - weekday_of(base)).rem_euclid(7)
            }
            Self::OnOrBefore(weekday, day) => {
                let base = datetime::days_from_civil(year, month, day);
                base - (weekday_of(base) - weekday).rem_euclid(7)
            }
        }
    }
}

fn day_number(text: &str) -> Option<i64> {
    let day: i64 = text.parse().ok()?;
    (1..=31).contains(&day).then_some(day)
}

pub(super) fn weekday_of(days: i64) -> i64 {
    (days + 6).rem_euclid(7)
}

fn year_number(text: &str) -> Option<i64> {
    let year: i64 = text.parse().ok()?;
    (-10_000..=100_000).contains(&year).then_some(year)
}

#[derive(Debug, Clone)]
pub(super) struct Rule {
    from: i64,
    to: i64,
    month: i64,
    day: DaySpec,
    at: ClockTime,
    save: i64,
    letters: &'static str,
}

impl Rule {
    fn read(fields: &[&'static str]) -> Option<Self> {
        let [from, to, _, month, day, at, save, letters, ..] = fields else {
            return None;
        };
        let from = match year_number(from) {
            Some(year) => year,
            None if from.len() >= 2 && "minimum".starts_with(&from.to_ascii_lowercase()) => {
                MIN_YEAR
            }
            None => return None,
        };
        let lower_to = to.to_ascii_lowercase();
        let to = match year_number(to) {
            Some(year) => year,
            None if !lower_to.is_empty() && "only".starts_with(&lower_to) => from,
            None if lower_to.len() >= 2 && "maximum".starts_with(&lower_to) => MAX_YEAR,
            None => return None,
        };
        let save = match save.strip_suffix(['s', 'd']) {
            Some(number) => duration(number)?,
            None => duration(save)?,
        };
        Some(Self {
            from,
            to,
            month: abbreviated(month, &MONTHS)? + 1,
            day: DaySpec::read(day)?,
            at: ClockTime::read(at)?,
            save,
            letters: if *letters == "-" { "" } else { *letters },
        })
    }

    pub(super) fn continues(&self) -> bool {
        self.to == MAX_YEAR
    }
}

#[derive(Debug, Clone, Copy)]
struct Until {
    local: i64,
    at: ClockTime,
}

impl Until {
    fn read(fields: &[&str]) -> Option<Option<Self>> {
        let Some(year) = fields.first() else {
            return Some(None);
        };
        let year = year_number(year)?;
        let month = match fields.get(1) {
            Some(month) => abbreviated(month, &MONTHS)? + 1,
            None => 1,
        };
        let day = match fields.get(2) {
            Some(day) => DaySpec::read(day)?,
            None => DaySpec::Fixed(1),
        };
        let at = match fields.get(3) {
            Some(at) => ClockTime::read(at)?,
            None => ClockTime {
                seconds: 0,
                clock: Clock::Wall,
            },
        };
        Some(Some(Self {
            local: day.day(year, month) * SECONDS_PER_DAY + at.seconds,
            at,
        }))
    }

    fn universal(self, standard_offset: i64, save: i64) -> i64 {
        self.at.universal(self.local, standard_offset, save)
    }

    fn year(self) -> i64 {
        year_of(self.local)
    }
}

#[derive(Debug, Clone, Copy)]
enum ZoneRules {
    Fixed(i64),
    Named(&'static str),
}

#[derive(Debug, Clone, Copy)]
struct ZoneLine {
    offset: i64,
    rules: ZoneRules,
    format: &'static str,
    until: Option<Until>,
}

impl ZoneLine {
    fn read(fields: &[&'static str]) -> Option<Self> {
        let [offset, rules, format, until @ ..] = fields else {
            return None;
        };
        let rules = match duration(rules) {
            Some(save) => ZoneRules::Fixed(save),
            None => ZoneRules::Named(*rules),
        };
        Some(Self {
            offset: duration(offset)?,
            rules,
            format: *format,
            until: Until::read(until)?,
        })
    }
}

/// One of a zone's local times: its offset east of UTC in seconds, whether it is daylight
/// saving time, and its abbreviation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalTime {
    pub(crate) offset: i32,
    pub(crate) is_dst: bool,
    pub(crate) abbreviation: Box<str>,
}

fn local_time(standard_offset: i64, save: i64, letters: &str, format: &str) -> Option<LocalTime> {
    let offset = standard_offset + save;
    let is_dst = save != 0;
    let abbreviation = if let Some((standard, daylight)) = format.split_once('/') {
        if is_dst { daylight } else { standard }.to_owned()
    } else if format.contains("%s") {
        format.replace("%s", letters)
    } else if format.contains("%z") {
        format.replace("%z", &numeric_abbreviation(offset))
    } else {
        format.to_owned()
    };
    Some(LocalTime {
        offset: i32::try_from(offset).ok()?,
        is_dst,
        abbreviation: abbreviation.into_boxed_str(),
    })
}

fn numeric_abbreviation(offset: i64) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let magnitude = offset.abs();
    let (hours, minutes, seconds) = (magnitude / 3600, magnitude / 60 % 60, magnitude % 60);
    match (minutes, seconds) {
        (0, 0) => format!("{sign}{hours:02}"),
        (_, 0) => format!("{sign}{hours:02}{minutes:02}"),
        _ => format!("{sign}{hours:02}{minutes:02}{seconds:02}"),
    }
}

pub(super) fn year_of(seconds: i64) -> i64 {
    datetime::civil_from_days(seconds.div_euclid(SECONDS_PER_DAY)).0
}

fn year_start(year: i64) -> i64 {
    datetime::days_from_civil(year, 1, 1) * SECONDS_PER_DAY
}

#[derive(Debug, Clone, Copy)]
struct Change<'r> {
    local: i64,
    rule: &'r Rule,
}

fn changes_in_year(rules: &[Rule], year: i64) -> Vec<Change<'_>> {
    let mut changes: Vec<Change<'_>> = rules
        .iter()
        .filter(|rule| rule.from <= year && year <= rule.to)
        .map(|rule| Change {
            local: rule.day.day(year, rule.month) * SECONDS_PER_DAY + rule.at.seconds,
            rule,
        })
        .collect();
    changes.sort_by_key(|change| change.local);
    changes
}

#[derive(Debug, Clone)]
pub(super) struct Shift {
    pub(super) at: i64,
    pub(super) save: i64,
    pub(super) letters: &'static str,
}

pub(super) fn shifts_between(
    rules: &[Rule],
    standard_offset: i64,
    first_year: i64,
    last_year: i64,
) -> Vec<Shift> {
    let mut save = 0;
    let mut shifts = Vec::new();
    for year in first_year..=last_year {
        for change in changes_in_year(rules, year) {
            let at = change.rule.at.universal(change.local, standard_offset, save);
            save = change.rule.save;
            shifts.push(Shift {
                at,
                save,
                letters: change.rule.letters,
            });
        }
    }
    shifts
}

fn earliest_year(rules: &[Rule]) -> i64 {
    rules
        .iter()
        .map(|rule| rule.from)
        .min()
        .unwrap_or(EARLIEST_RULE_YEAR)
        .max(EARLIEST_RULE_YEAR)
}

fn standard_letters(rules: &[Rule]) -> &'static str {
    rules
        .iter()
        .filter(|rule| rule.save == 0)
        .min_by_key(|rule| rule.from)
        .map_or("", |rule| rule.letters)
}

fn state_at(start: Option<i64>, standard_offset: i64, rules: &[Rule]) -> (i64, &'static str) {
    let Some(start) = start else {
        return (0, standard_letters(rules));
    };
    let last_year = year_of(start) + 1;
    shifts_between(rules, standard_offset, earliest_year(rules), last_year)
        .into_iter()
        .take_while(|shift| shift.at <= start)
        .last()
        .map_or((0, standard_letters(rules)), |shift| (shift.save, shift.letters))
}

#[derive(Debug)]
pub(super) struct Tail {
    pub(super) from: i64,
    pub(super) offset: i64,
    pub(super) rules: Vec<Rule>,
    pub(super) format: &'static str,
}

impl Tail {
    fn shifts_around(&self, seconds: i64) -> Vec<Shift> {
        let year = year_of(seconds);
        shifts_between(&self.rules, self.offset, year - 2, year + 1)
    }

    pub(super) fn local_time_at(&self, utc: i64) -> Option<LocalTime> {
        let shifts = self.shifts_around(utc);
        let (save, letters) = shifts
            .iter()
            .take_while(|shift| shift.at <= utc)
            .last()
            .map_or((0, ""), |shift| (shift.save, shift.letters));
        local_time(self.offset, save, letters, self.format)
    }

    pub(super) fn offset_of_local(&self, local: i64) -> i64 {
        let shifts = self.shifts_around(local);
        let mut offset = self.offset;
        for shift in &shifts {
            let after = self.offset + shift.save;
            if shift.at + after <= local {
                offset = after;
            } else {
                break;
            }
        }
        offset
    }
}

#[derive(Debug)]
pub(super) struct Compiled {
    pub(super) types: Vec<LocalTime>,
    pub(super) transitions: Vec<(i64, usize)>,
    pub(super) tail: Option<Tail>,
}

impl Compiled {
    fn enter(&mut self, at: Option<i64>, local: LocalTime) {
        match at {
            None if self.types.is_empty() => self.types.push(local),
            None => {}
            Some(at) => self.change(at, local),
        }
    }

    fn change(&mut self, at: i64, local: LocalTime) {
        let current = self.transitions.last().map_or(0, |&(_, index)| index);
        if self.types.get(current) == Some(&local) {
            return;
        }
        let index = match self.types.iter().position(|known| *known == local) {
            Some(index) => index,
            None => {
                self.types.push(local);
                self.types.len() - 1
            }
        };
        match self.transitions.last_mut() {
            Some(last) if last.0 >= at => last.1 = index,
            _ => self.transitions.push((at, index)),
        }
        if self.transitions.len() >= 2 {
            let before = self.transitions.len() - 2;
            let same = self
                .transitions
                .get(before)
                .zip(self.transitions.last())
                .is_some_and(|(earlier, last)| earlier.1 == last.1);
            if same {
                self.transitions.pop();
            }
        }
    }
}

pub(super) fn compile(source: &Source, first: usize) -> Option<Compiled> {
    let lines = source.zone_lines(first)?;
    let mut compiled = Compiled {
        types: Vec::new(),
        transitions: Vec::new(),
        tail: None,
    };
    let mut start: Option<i64> = None;
    let count = lines.len();
    for (position, line) in lines.into_iter().enumerate() {
        let last_line = position + 1 == count;
        match line.rules {
            ZoneRules::Fixed(save) => {
                compiled.enter(start, local_time(line.offset, save, "", line.format)?);
                start = line.until.map(|until| until.universal(line.offset, save));
            }
            ZoneRules::Named(name) => {
                let rules = source.rules_named(name)?;
                let (mut save, mut letters) = state_at(start, line.offset, &rules);
                compiled.enter(start, local_time(line.offset, save, letters, line.format)?);
                let first_year = start.map_or_else(|| earliest_year(&rules), |start| year_of(start) - 1);
                let last_year = line.until.map_or(LAST_LISTED_YEAR, Until::year);
                'years: for year in first_year..=last_year {
                    for change in changes_in_year(&rules, year) {
                        let at = change.rule.at.universal(change.local, line.offset, save);
                        if start.is_some_and(|start| at <= start) {
                            continue;
                        }
                        if line
                            .until
                            .is_some_and(|until| at >= until.universal(line.offset, save))
                        {
                            break 'years;
                        }
                        save = change.rule.save;
                        letters = change.rule.letters;
                        compiled.change(at, local_time(line.offset, save, letters, line.format)?);
                    }
                }
                start = line.until.map(|until| until.universal(line.offset, save));
                if last_line && rules.iter().any(Rule::continues) {
                    compiled.tail = Some(Tail {
                        from: year_start(LAST_LISTED_YEAR + 1),
                        offset: line.offset,
                        rules,
                        format: line.format,
                    });
                }
            }
        }
    }
    (!compiled.types.is_empty()).then_some(compiled)
}
