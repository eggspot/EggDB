use super::TimeZone;

const LONGEST_ABBREVIATION: usize = 6;

#[derive(Debug, Clone, Copy)]
enum Meaning {
    Standard(i32),
    Daylight(i32),
    Region(&'static str),
}

/// One row of `pg_timezone_abbrevs`: an abbreviation, its offset east of UTC in seconds and
/// whether it names a daylight saving time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AbbreviationRow {
    pub(crate) abbreviation: &'static str,
    pub(crate) offset: i32,
    pub(crate) is_dst: bool,
}

const DEFAULT_SET: [(&str, Meaning); 194] = [
    ("ACDT", Meaning::Daylight(37800)),
    ("ACSST", Meaning::Daylight(37800)),
    ("ACST", Meaning::Standard(34200)),
    ("ACT", Meaning::Standard(-18000)),
    ("ACWST", Meaning::Standard(31500)),
    ("ADT", Meaning::Daylight(-10800)),
    ("AEDT", Meaning::Daylight(39600)),
    ("AESST", Meaning::Daylight(39600)),
    ("AEST", Meaning::Standard(36000)),
    ("AFT", Meaning::Standard(16200)),
    ("AKDT", Meaning::Daylight(-28800)),
    ("AKST", Meaning::Standard(-32400)),
    ("ALMST", Meaning::Daylight(25200)),
    ("ALMT", Meaning::Standard(21600)),
    ("AMST", Meaning::Region("Asia/Yerevan")),
    ("AMT", Meaning::Standard(-14400)),
    ("ANAST", Meaning::Region("Asia/Anadyr")),
    ("ANAT", Meaning::Region("Asia/Anadyr")),
    ("ARST", Meaning::Region("America/Argentina/Buenos_Aires")),
    ("ART", Meaning::Region("America/Argentina/Buenos_Aires")),
    ("AST", Meaning::Standard(-14400)),
    ("AWSST", Meaning::Daylight(32400)),
    ("AWST", Meaning::Standard(28800)),
    ("AZOST", Meaning::Daylight(0)),
    ("AZOT", Meaning::Standard(-3600)),
    ("AZST", Meaning::Region("Asia/Baku")),
    ("AZT", Meaning::Region("Asia/Baku")),
    ("BDST", Meaning::Daylight(7200)),
    ("BDT", Meaning::Standard(21600)),
    ("BNT", Meaning::Standard(28800)),
    ("BORT", Meaning::Standard(28800)),
    ("BOT", Meaning::Standard(-14400)),
    ("BRA", Meaning::Standard(-10800)),
    ("BRST", Meaning::Daylight(-7200)),
    ("BRT", Meaning::Standard(-10800)),
    ("BST", Meaning::Daylight(3600)),
    ("BTT", Meaning::Standard(21600)),
    ("CADT", Meaning::Daylight(37800)),
    ("CAST", Meaning::Standard(34200)),
    ("CCT", Meaning::Standard(28800)),
    ("CDT", Meaning::Daylight(-18000)),
    ("CEST", Meaning::Daylight(7200)),
    ("CET", Meaning::Standard(3600)),
    ("CETDST", Meaning::Daylight(7200)),
    ("CHADT", Meaning::Daylight(49500)),
    ("CHAST", Meaning::Standard(45900)),
    ("CHUT", Meaning::Standard(36000)),
    ("CKT", Meaning::Region("Pacific/Rarotonga")),
    ("CLST", Meaning::Daylight(-10800)),
    ("CLT", Meaning::Region("America/Santiago")),
    ("COT", Meaning::Standard(-18000)),
    ("CST", Meaning::Standard(-21600)),
    ("CXT", Meaning::Standard(25200)),
    ("DAVT", Meaning::Region("Antarctica/Davis")),
    ("DDUT", Meaning::Standard(36000)),
    ("EASST", Meaning::Region("Pacific/Easter")),
    ("EAST", Meaning::Region("Pacific/Easter")),
    ("EAT", Meaning::Standard(10800)),
    ("EDT", Meaning::Daylight(-14400)),
    ("EEST", Meaning::Daylight(10800)),
    ("EET", Meaning::Standard(7200)),
    ("EETDST", Meaning::Daylight(10800)),
    ("EGST", Meaning::Daylight(0)),
    ("EGT", Meaning::Standard(-3600)),
    ("EST", Meaning::Standard(-18000)),
    ("FET", Meaning::Standard(10800)),
    ("FJST", Meaning::Daylight(46800)),
    ("FJT", Meaning::Standard(43200)),
    ("FKST", Meaning::Region("Atlantic/Stanley")),
    ("FKT", Meaning::Region("Atlantic/Stanley")),
    ("FNST", Meaning::Daylight(-3600)),
    ("FNT", Meaning::Standard(-7200)),
    ("GALT", Meaning::Standard(-21600)),
    ("GAMT", Meaning::Standard(-32400)),
    ("GEST", Meaning::Region("Asia/Tbilisi")),
    ("GET", Meaning::Region("Asia/Tbilisi")),
    ("GFT", Meaning::Standard(-10800)),
    ("GILT", Meaning::Standard(43200)),
    ("GMT", Meaning::Standard(0)),
    ("GYT", Meaning::Region("America/Guyana")),
    ("HKT", Meaning::Standard(28800)),
    ("HST", Meaning::Standard(-36000)),
    ("ICT", Meaning::Standard(25200)),
    ("IDT", Meaning::Daylight(10800)),
    ("IOT", Meaning::Region("Indian/Chagos")),
    ("IRKST", Meaning::Region("Asia/Irkutsk")),
    ("IRKT", Meaning::Region("Asia/Irkutsk")),
    ("IRT", Meaning::Standard(12600)),
    ("IST", Meaning::Standard(7200)),
    ("JAYT", Meaning::Standard(32400)),
    ("JST", Meaning::Standard(32400)),
    ("KDT", Meaning::Daylight(36000)),
    ("KGST", Meaning::Daylight(21600)),
    ("KGT", Meaning::Region("Asia/Bishkek")),
    ("KOST", Meaning::Region("Pacific/Kosrae")),
    ("KRAST", Meaning::Region("Asia/Krasnoyarsk")),
    ("KRAT", Meaning::Region("Asia/Krasnoyarsk")),
    ("KST", Meaning::Standard(32400)),
    ("LHDT", Meaning::Region("Australia/Lord_Howe")),
    ("LHST", Meaning::Standard(37800)),
    ("LIGT", Meaning::Standard(36000)),
    ("LINT", Meaning::Region("Pacific/Kiritimati")),
    ("LKT", Meaning::Region("Asia/Colombo")),
    ("MAGST", Meaning::Region("Asia/Magadan")),
    ("MAGT", Meaning::Region("Asia/Magadan")),
    ("MART", Meaning::Standard(-34200)),
    ("MAWT", Meaning::Region("Antarctica/Mawson")),
    ("MDT", Meaning::Daylight(-21600)),
    ("MEST", Meaning::Daylight(7200)),
    ("MESZ", Meaning::Daylight(7200)),
    ("MET", Meaning::Standard(3600)),
    ("METDST", Meaning::Daylight(7200)),
    ("MEZ", Meaning::Standard(3600)),
    ("MHT", Meaning::Standard(43200)),
    ("MMT", Meaning::Standard(23400)),
    ("MPT", Meaning::Standard(36000)),
    ("MSD", Meaning::Daylight(14400)),
    ("MSK", Meaning::Region("Europe/Moscow")),
    ("MST", Meaning::Standard(-25200)),
    ("MUST", Meaning::Daylight(18000)),
    ("MUT", Meaning::Standard(14400)),
    ("MVT", Meaning::Standard(18000)),
    ("MYT", Meaning::Standard(28800)),
    ("NDT", Meaning::Daylight(-9000)),
    ("NFT", Meaning::Standard(-12600)),
    ("NOVST", Meaning::Region("Asia/Novosibirsk")),
    ("NOVT", Meaning::Region("Asia/Novosibirsk")),
    ("NPT", Meaning::Standard(20700)),
    ("NST", Meaning::Standard(-12600)),
    ("NUT", Meaning::Region("Pacific/Niue")),
    ("NZDT", Meaning::Daylight(46800)),
    ("NZST", Meaning::Standard(43200)),
    ("NZT", Meaning::Standard(43200)),
    ("OMSST", Meaning::Region("Asia/Omsk")),
    ("OMST", Meaning::Region("Asia/Omsk")),
    ("PDT", Meaning::Daylight(-25200)),
    ("PET", Meaning::Standard(-18000)),
    ("PETST", Meaning::Region("Asia/Kamchatka")),
    ("PETT", Meaning::Region("Asia/Kamchatka")),
    ("PGT", Meaning::Standard(36000)),
    ("PHT", Meaning::Standard(28800)),
    ("PKST", Meaning::Daylight(21600)),
    ("PKT", Meaning::Standard(18000)),
    ("PMDT", Meaning::Daylight(-7200)),
    ("PMST", Meaning::Standard(-10800)),
    ("PONT", Meaning::Standard(39600)),
    ("PST", Meaning::Standard(-28800)),
    ("PWT", Meaning::Standard(32400)),
    ("PYST", Meaning::Daylight(-10800)),
    ("PYT", Meaning::Region("America/Asuncion")),
    ("RET", Meaning::Standard(14400)),
    ("SADT", Meaning::Daylight(37800)),
    ("SAST", Meaning::Standard(7200)),
    ("SCT", Meaning::Standard(14400)),
    ("SGT", Meaning::Region("Asia/Singapore")),
    ("TAHT", Meaning::Standard(-36000)),
    ("TFT", Meaning::Standard(18000)),
    ("TJT", Meaning::Standard(18000)),
    ("TKT", Meaning::Region("Pacific/Fakaofo")),
    ("TMT", Meaning::Region("Asia/Ashgabat")),
    ("TOT", Meaning::Standard(46800)),
    ("TRUT", Meaning::Standard(36000)),
    ("UCT", Meaning::Standard(0)),
    ("ULAST", Meaning::Daylight(32400)),
    ("ULAT", Meaning::Region("Asia/Ulaanbaatar")),
    ("UT", Meaning::Standard(0)),
    ("UTC", Meaning::Standard(0)),
    ("UYST", Meaning::Daylight(-7200)),
    ("UYT", Meaning::Standard(-10800)),
    ("UZST", Meaning::Daylight(21600)),
    ("UZT", Meaning::Standard(18000)),
    ("VET", Meaning::Region("America/Caracas")),
    ("VLAST", Meaning::Region("Asia/Vladivostok")),
    ("VLAT", Meaning::Region("Asia/Vladivostok")),
    ("VOLT", Meaning::Region("Europe/Volgograd")),
    ("VUT", Meaning::Standard(39600)),
    ("WADT", Meaning::Daylight(28800)),
    ("WAKT", Meaning::Standard(43200)),
    ("WAST", Meaning::Standard(25200)),
    ("WAT", Meaning::Standard(3600)),
    ("WDT", Meaning::Daylight(32400)),
    ("WET", Meaning::Standard(0)),
    ("WETDST", Meaning::Daylight(3600)),
    ("WFT", Meaning::Standard(43200)),
    ("WGST", Meaning::Daylight(-7200)),
    ("WGT", Meaning::Standard(-10800)),
    ("XJT", Meaning::Standard(21600)),
    ("YAKST", Meaning::Region("Asia/Yakutsk")),
    ("YAKT", Meaning::Region("Asia/Yakutsk")),
    ("YAPT", Meaning::Standard(36000)),
    ("YEKST", Meaning::Daylight(21600)),
    ("YEKT", Meaning::Region("Asia/Yekaterinburg")),
    ("Z", Meaning::Standard(0)),
    ("ZULU", Meaning::Standard(0)),
];

fn meaning(name: &str) -> Option<Meaning> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() > LONGEST_ABBREVIATION {
        return None;
    }
    let upper = trimmed.to_ascii_uppercase();
    DEFAULT_SET
        .binary_search_by(|&(abbreviation, _)| abbreviation.cmp(upper.as_str()))
        .ok()
        .and_then(|index| DEFAULT_SET.get(index))
        .map(|&(_, meaning)| meaning)
}

/// The zone a time zone abbreviation of PostgreSQL's `Default` set names, in any letter case:
/// a fixed offset (`PST`, `CEST`), or the region whose offset an abbreviation that changed
/// meaning over time follows (`MSK` follows `Europe/Moscow`).
pub(crate) fn abbreviation_zone(name: &str) -> Option<TimeZone> {
    match meaning(name)? {
        Meaning::Standard(offset) | Meaning::Daylight(offset) => TimeZone::fixed(offset),
        Meaning::Region(region) => TimeZone::region(region),
    }
}

/// Every abbreviation of the `Default` set as `pg_timezone_abbrevs` lists it at `now_micros`.
pub(crate) fn abbreviation_rows(now_micros: i64) -> Vec<AbbreviationRow> {
    DEFAULT_SET
        .iter()
        .filter_map(|&(abbreviation, meaning)| {
            let (offset, is_dst) = match meaning {
                Meaning::Standard(offset) => (offset, false),
                Meaning::Daylight(offset) => (offset, true),
                Meaning::Region(region) => {
                    let zone = TimeZone::region(region)?;
                    (zone.offset_at(now_micros), zone.is_dst_at(now_micros))
                }
            };
            Some(AbbreviationRow {
                abbreviation,
                offset,
                is_dst,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_SET, abbreviation_rows, abbreviation_zone};
    use crate::time_zones::TimeZone;

    #[test]
    fn the_set_is_sorted_so_lookups_find_every_entry() {
        assert!(DEFAULT_SET.windows(2).all(|pair| pair[0].0 < pair[1].0));
        for (abbreviation, _) in DEFAULT_SET {
            assert!(abbreviation_zone(abbreviation).is_some(), "{abbreviation}");
        }
    }

    #[test]
    fn abbreviations_read_in_any_case_as_postgresql_s_default_set() {
        let hours = |hours: i32| TimeZone::fixed(hours * 3600);
        assert_eq!(abbreviation_zone("PST"), hours(-8));
        assert_eq!(abbreviation_zone("est"), hours(-5));
        assert_eq!(abbreviation_zone("Cet"), hours(1));
        assert_eq!(abbreviation_zone("CEST"), hours(2));
        assert_eq!(abbreviation_zone("JST"), hours(9));
        assert_eq!(abbreviation_zone("UTC"), hours(0));
        assert_eq!(abbreviation_zone("NPT"), TimeZone::fixed(20_700));
        assert_eq!(abbreviation_zone("MSK"), TimeZone::region("Europe/Moscow"));
        assert_eq!(abbreviation_zone("XYZ"), None);
        assert_eq!(abbreviation_zone("Europe/Paris"), None);
    }

    #[test]
    fn rows_flag_daylight_abbreviations() {
        let rows = abbreviation_rows(0);
        assert_eq!(rows.len(), DEFAULT_SET.len());
        let row = |name: &str| rows.iter().find(|row| row.abbreviation == name).copied();
        assert_eq!(
            row("PDT").map(|row| (row.offset, row.is_dst)),
            Some((-7 * 3600, true))
        );
        assert_eq!(
            row("PST").map(|row| (row.offset, row.is_dst)),
            Some((-8 * 3600, false))
        );
        assert_eq!(row("MSK").map(|row| row.offset), Some(3 * 3600));
    }
}
