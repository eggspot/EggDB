mod compile;

use crate::datetime::{self, MICROS_PER_SECOND};
use compile::{Compiled, LocalTime, SECONDS_PER_DAY, Source};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, PoisonError};

const DATABASE: &str = include_str!("time_zones/tzdata.zi");
const LONGEST_NAME: usize = 64;

fn source() -> &'static Source {
    static SOURCE: OnceLock<Source> = OnceLock::new();
    SOURCE.get_or_init(|| Source::read(DATABASE))
}

#[derive(Debug)]
pub(crate) struct Region {
    name: &'static str,
    compiled: Compiled,
}

impl Region {
    fn local_time_at(&self, utc: i64) -> Option<Cow<'_, LocalTime>> {
        if let Some(tail) = self.compiled.tail.as_ref().filter(|tail| utc >= tail.from) {
            return tail.local_time_at(utc).map(Cow::Owned);
        }
        let transitions = &self.compiled.transitions;
        let after = transitions.partition_point(|&(at, _)| at <= utc);
        let index = after
            .checked_sub(1)
            .and_then(|last| transitions.get(last))
            .map_or(0, |&(_, index)| index);
        self.compiled.types.get(index).map(Cow::Borrowed)
    }

    fn offset_at(&self, utc: i64) -> i32 {
        self.local_time_at(utc).map_or(0, |local| local.offset)
    }

    fn offset_of_local(&self, local: i64) -> i32 {
        if let Some(tail) = self
            .compiled
            .tail
            .as_ref()
            .filter(|tail| local >= tail.from + SECONDS_PER_DAY)
        {
            return i32::try_from(tail.offset_of_local(local)).unwrap_or(0);
        }
        let types = &self.compiled.types;
        let offset_of = |index: usize| types.get(index).map_or(0, |local| local.offset);
        let transitions = &self.compiled.transitions;
        let after = transitions
            .partition_point(|&(at, index)| at + i64::from(offset_of(index)) <= local);
        let index = after
            .checked_sub(1)
            .and_then(|last| transitions.get(last))
            .map_or(0, |&(_, index)| index);
        offset_of(index)
    }
}

fn region(name: &str) -> Option<&'static Region> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() > LONGEST_NAME {
        return None;
    }
    let source = source();
    let (canonical, first) = source.zone(trimmed)?;
    static REGIONS: OnceLock<Mutex<HashMap<&'static str, &'static Region>>> = OnceLock::new();
    let mut regions = REGIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(found) = regions.get(canonical) {
        return Some(found);
    }
    let compiled = compile::compile(source, first)?;
    let found: &'static Region = Box::leak(Box::new(Region {
        name: canonical,
        compiled,
    }));
    regions.insert(canonical, found);
    Some(found)
}

pub(crate) fn region_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = source().zone_names().collect();
    names.sort_unstable();
    names
}

#[derive(Debug, Clone, Copy)]
enum Zone {
    Fixed(i32),
    Region(&'static Region),
}

/// A time zone as a session's `TimeZone` setting or `AT TIME ZONE` names it: a fixed offset
/// from UTC, or a region of the IANA time zone database (`Europe/Paris`) whose offset follows
/// its daylight saving and historical rules.
#[derive(Debug, Clone, Copy)]
pub struct TimeZone(Zone);

impl PartialEq for TimeZone {
    fn eq(&self, other: &Self) -> bool {
        match (self.0, other.0) {
            (Zone::Fixed(left), Zone::Fixed(right)) => left == right,
            (Zone::Region(left), Zone::Region(right)) => std::ptr::eq(left, right),
            _ => false,
        }
    }
}

impl Eq for TimeZone {}

impl Default for TimeZone {
    fn default() -> Self {
        Self::UTC
    }
}

impl TimeZone {
    /// Coordinated Universal Time.
    pub const UTC: Self = Self(Zone::Fixed(0));

    /// A fixed offset of `offset_seconds` east of UTC, or `None` beyond 15:59:59 either way.
    pub fn fixed(offset_seconds: i32) -> Option<Self> {
        datetime::valid_offset(offset_seconds).then_some(Self(Zone::Fixed(offset_seconds)))
    }

    /// The zone `name` names, as PostgreSQL's `TimeZone` setting reads it: `UTC` and its
    /// spellings, a POSIX-style offset (`+03` is three hours west, `UTC-05:30` five and a half
    /// hours east), or a region or alias of the time zone database in any letter case
    /// (`Europe/Paris`, `us/eastern`); `None` when it names none of these.
    pub fn named(name: &str) -> Option<Self> {
        if let Some(offset) = datetime::time_zone_offset(name) {
            return Some(Self(Zone::Fixed(offset)));
        }
        region(name).map(|found| Self(Zone::Region(found)))
    }

    pub(crate) fn region(name: &str) -> Option<Self> {
        region(name).map(|found| Self(Zone::Region(found)))
    }

    pub(crate) fn region_name(self) -> Option<&'static str> {
        match self.0 {
            Zone::Fixed(_) => None,
            Zone::Region(found) => Some(found.name),
        }
    }

    pub(crate) fn fixed_offset(self) -> Option<i32> {
        match self.0 {
            Zone::Fixed(offset) => Some(offset),
            Zone::Region(_) => None,
        }
    }

    pub(crate) fn offset_at(self, utc_micros: i64) -> i32 {
        match self.0 {
            Zone::Fixed(offset) => offset,
            Zone::Region(found) => found.offset_at(utc_micros.div_euclid(MICROS_PER_SECOND)),
        }
    }

    pub(crate) fn offset_of_local(self, local_micros: i64) -> i32 {
        match self.0 {
            Zone::Fixed(offset) => offset,
            Zone::Region(found) => {
                found.offset_of_local(local_micros.div_euclid(MICROS_PER_SECOND))
            }
        }
    }

    pub(crate) fn abbreviation_at(self, utc_micros: i64) -> Option<String> {
        match self.0 {
            Zone::Fixed(_) => None,
            Zone::Region(found) => found
                .local_time_at(utc_micros.div_euclid(MICROS_PER_SECOND))
                .map(|local| local.abbreviation.to_string()),
        }
    }

    pub(crate) fn is_dst_at(self, utc_micros: i64) -> bool {
        match self.0 {
            Zone::Fixed(_) => false,
            Zone::Region(found) => found
                .local_time_at(utc_micros.div_euclid(MICROS_PER_SECOND))
                .is_some_and(|local| local.is_dst),
        }
    }

    pub(crate) fn utc_of_local(self, local_micros: i64) -> i128 {
        i128::from(local_micros)
            - i128::from(self.offset_of_local(local_micros)) * i128::from(MICROS_PER_SECOND)
    }

    pub(crate) fn local_of_utc(self, utc_micros: i64) -> i128 {
        i128::from(utc_micros)
            + i128::from(self.offset_at(utc_micros)) * i128::from(MICROS_PER_SECOND)
    }
}

#[cfg(test)]
mod tests {
    use super::TimeZone;
    use crate::datetime::{self, MICROS_PER_DAY, MICROS_PER_SECOND};

    fn utc(year: i64, month: i64, day: i64, hour: i64, minute: i64) -> i64 {
        datetime::days_from_civil(year, month, day) * MICROS_PER_DAY
            + (hour * 3600 + minute * 60) * MICROS_PER_SECOND
    }

    fn zone(name: &str) -> TimeZone {
        TimeZone::named(name).unwrap_or_else(|| panic!("{name} is in the database"))
    }

    fn database_version() -> &'static str {
        super::DATABASE
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("# version "))
            .map_or("", |version| version.split('-').next().unwrap_or(version))
    }

    #[test]
    fn the_database_is_the_one_the_docs_name() {
        assert_eq!(database_version(), "2026e");
        assert!(super::region_names().len() > 500);
    }

    #[test]
    fn regions_resolve_in_any_case_and_through_aliases() {
        assert_eq!(zone("europe/paris").region_name(), Some("Europe/Paris"));
        assert_eq!(zone("US/Eastern").region_name(), Some("US/Eastern"));
        assert_eq!(zone("UTC"), TimeZone::UTC);
        assert_eq!(zone("+03").fixed_offset(), Some(-3 * 3600));
        assert!(TimeZone::named("Mars/Olympus").is_none());
        assert!(TimeZone::named("").is_none());
        assert!(TimeZone::named(&"a/".repeat(100)).is_none());
    }

    #[test]
    fn paris_changes_on_the_last_sundays_of_march_and_october_at_one_utc() {
        let paris = zone("Europe/Paris");
        assert_eq!(paris.offset_at(utc(2026, 3, 29, 0, 59)), 3600);
        assert_eq!(paris.offset_at(utc(2026, 3, 29, 1, 0)), 7200);
        assert_eq!(paris.offset_at(utc(2026, 10, 25, 0, 59)), 7200);
        assert_eq!(paris.offset_at(utc(2026, 10, 25, 1, 0)), 3600);
        assert_eq!(paris.abbreviation_at(utc(2026, 7, 1, 12, 0)).as_deref(), Some("CEST"));
        assert_eq!(paris.abbreviation_at(utc(2026, 1, 1, 12, 0)).as_deref(), Some("CET"));
        assert!(paris.is_dst_at(utc(2026, 7, 1, 12, 0)));
        assert_eq!(paris.offset_at(utc(1900, 1, 1, 0, 0)), 561);
    }

    #[test]
    fn new_york_follows_the_2007_rules_and_the_older_ones() {
        let new_york = zone("America/New_York");
        assert_eq!(new_york.offset_at(utc(2026, 3, 8, 6, 59)), -5 * 3600);
        assert_eq!(new_york.offset_at(utc(2026, 3, 8, 7, 0)), -4 * 3600);
        assert_eq!(new_york.offset_at(utc(2026, 11, 1, 5, 59)), -4 * 3600);
        assert_eq!(new_york.offset_at(utc(2026, 11, 1, 6, 0)), -5 * 3600);
        assert_eq!(new_york.offset_at(utc(2000, 4, 2, 7, 0)), -4 * 3600);
        assert_eq!(new_york.offset_at(utc(2000, 4, 2, 6, 59)), -5 * 3600);
    }

    #[test]
    fn southern_and_half_hour_zones_follow_their_rules() {
        let sydney = zone("Australia/Sydney");
        assert_eq!(sydney.offset_at(utc(2026, 1, 15, 0, 0)), 11 * 3600);
        assert_eq!(sydney.offset_at(utc(2026, 7, 15, 0, 0)), 10 * 3600);
        let lord_howe = zone("Australia/Lord_Howe");
        assert_eq!(lord_howe.offset_at(utc(2026, 1, 15, 0, 0)), 11 * 3600);
        assert_eq!(lord_howe.offset_at(utc(2026, 7, 15, 0, 0)), 10 * 3600 + 1800);
        assert_eq!(zone("Asia/Kolkata").offset_at(utc(2026, 7, 15, 0, 0)), 5 * 3600 + 1800);
        assert_eq!(zone("Asia/Kathmandu").offset_at(utc(2026, 7, 15, 0, 0)), 5 * 3600 + 2700);
    }

    #[test]
    fn dublin_keeps_postgresql_s_summer_time_flag() {
        let dublin = zone("Europe/Dublin");
        assert!(dublin.is_dst_at(utc(2026, 7, 1, 12, 0)));
        assert_eq!(dublin.abbreviation_at(utc(2026, 7, 1, 12, 0)).as_deref(), Some("IST"));
        assert_eq!(dublin.abbreviation_at(utc(2026, 1, 1, 12, 0)).as_deref(), Some("GMT"));
    }

    #[test]
    fn rules_still_apply_after_the_listed_years() {
        let paris = zone("Europe/Paris");
        assert_eq!(paris.offset_at(utc(2100, 7, 1, 0, 0)), 7200);
        assert_eq!(paris.offset_at(utc(2100, 1, 1, 0, 0)), 3600);
        assert_eq!(paris.offset_at(utc(2100, 3, 28, 0, 59)), 3600);
        assert_eq!(paris.offset_at(utc(2100, 3, 28, 1, 0)), 7200);
        assert_eq!(paris.offset_of_local(utc(2100, 7, 1, 12, 0)), 7200);
    }

    #[test]
    fn local_times_in_a_gap_take_the_offset_before_and_repeated_ones_the_offset_after() {
        let new_york = zone("America/New_York");
        assert_eq!(new_york.offset_of_local(utc(2026, 3, 8, 1, 59)), -5 * 3600);
        assert_eq!(new_york.offset_of_local(utc(2026, 3, 8, 2, 30)), -5 * 3600);
        assert_eq!(new_york.offset_of_local(utc(2026, 3, 8, 3, 0)), -4 * 3600);
        assert_eq!(new_york.offset_of_local(utc(2026, 11, 1, 0, 59)), -4 * 3600);
        assert_eq!(new_york.offset_of_local(utc(2026, 11, 1, 1, 30)), -5 * 3600);
        assert_eq!(new_york.offset_of_local(utc(2026, 11, 1, 2, 0)), -5 * 3600);
    }

    #[test]
    fn every_region_compiles_and_answers_at_the_ends_of_time() {
        for name in super::region_names() {
            let found = zone(name);
            for instant in [datetime::MIN_TIMESTAMP, 0, datetime::END_TIMESTAMP - 1] {
                assert!(datetime::valid_offset(found.offset_at(instant)), "{name}");
                assert!(datetime::valid_offset(found.offset_of_local(instant)), "{name}");
            }
        }
    }
}
