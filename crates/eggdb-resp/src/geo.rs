use crate::aggregates::{expiry, sorted_of};
use crate::commands::{
    Context, NOT_INTEGER, SYNTAX, WRONG_TYPE, float, integer, load, reading, save, writing,
};
use crate::geohash::{self, Point};
use crate::keyspace::KeysRead;
use crate::protocol::Reply;
use crate::value::{Data, Entry, SortedSet};

const NAMES: &[&str] = &[
    "geoadd",
    "geodist",
    "geohash",
    "geopos",
    "geosearch",
    "geosearchstore",
    "georadius",
    "georadius_ro",
    "georadiusbymember",
    "georadiusbymember_ro",
];

const UNSUPPORTED_UNIT: &str = "ERR unsupported unit provided. please use M, KM, FT, MI";
const MEMBER_NOT_FOUND: &str = "ERR could not decode requested zset member";
const ANY_WITHOUT_COUNT: &str = "ERR the ANY argument requires COUNT argument";
const COUNT_NOT_POSITIVE: &str = "ERR COUNT must be > 0";
const NEGATIVE_RADIUS: &str = "ERR radius cannot be negative";
const NEGATIVE_BOX: &str = "ERR height or width cannot be negative";

pub(crate) fn is_known(name: &str) -> bool {
    NAMES.contains(&name)
}

pub(crate) fn execute(
    context: &Context<'_>,
    db: u32,
    name: &str,
    args: &[Vec<u8>],
) -> Option<Reply> {
    let form = match (name, args.len()) {
        ("geoadd", count) if count >= 4 => return Some(geoadd(context, db, args)),
        ("geodist", count) if count >= 3 => return Some(geodist(context, db, args)),
        ("geohash", count) if count >= 1 => return Some(geohash_command(context, db, args)),
        ("geopos", count) if count >= 1 => return Some(geopos(context, db, args)),
        ("georadius", count) if count >= 5 => SearchForm::Radius { read_only: false },
        ("georadius_ro", count) if count >= 5 => SearchForm::Radius { read_only: true },
        ("georadiusbymember", count) if count >= 4 => {
            SearchForm::RadiusByMember { read_only: false }
        }
        ("georadiusbymember_ro", count) if count >= 4 => {
            SearchForm::RadiusByMember { read_only: true }
        }
        ("geosearch", count) if count >= 6 => SearchForm::Search,
        ("geosearchstore", count) if count >= 7 => SearchForm::SearchStore,
        _ => return None,
    };
    Some(search(context, db, form, name, args))
}

fn coordinate_text(value: f64) -> Vec<u8> {
    let text = format!("{value:.17}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed == "-0" {
        b"0".to_vec()
    } else {
        trimmed.as_bytes().to_vec()
    }
}

fn distance_text(meters: f64, unit_meters: f64) -> Vec<u8> {
    format!("{:.4}", meters / unit_meters).into_bytes()
}

fn coordinates_reply(point: Point) -> Reply {
    Reply::Array(Some(vec![
        Reply::DoubleWithText(point.longitude, coordinate_text(point.longitude)),
        Reply::DoubleWithText(point.latitude, coordinate_text(point.latitude)),
    ]))
}

fn unit_meters(unit: &[u8]) -> Result<f64, String> {
    match String::from_utf8_lossy(unit).to_ascii_lowercase().as_str() {
        "m" => Ok(1.0),
        "km" => Ok(1000.0),
        "ft" => Ok(0.3048),
        "mi" => Ok(1609.34),
        _ => Err(UNSUPPORTED_UNIT.to_owned()),
    }
}

fn point_from(longitude: &[u8], latitude: &[u8]) -> Result<Point, String> {
    let point = Point {
        longitude: float(longitude)?,
        latitude: float(latitude)?,
    };
    if point.is_indexable() {
        Ok(point)
    } else {
        Err(format!(
            "ERR invalid longitude,latitude pair {:.6},{:.6}",
            point.longitude, point.latitude
        ))
    }
}

fn number_named(bytes: &[u8], what: &str) -> Result<f64, String> {
    float(bytes).map_err(|_| format!("ERR need numeric {what}"))
}

fn member_point(set: &SortedSet, member: &[u8]) -> Option<Point> {
    set.score(member).map(geohash::decode_score)
}

fn geoadd(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    let (mut only_new, mut only_existing, mut count_changed) = (false, false, false);
    let mut at = 1;
    while let Some(word) = args.get(at) {
        match String::from_utf8_lossy(word).to_ascii_lowercase().as_str() {
            "nx" => only_new = true,
            "xx" => only_existing = true,
            "ch" => count_changed = true,
            _ => break,
        }
        at += 1;
    }
    let triples = &args[at..];
    if triples.is_empty() || !triples.len().is_multiple_of(3) || (only_new && only_existing) {
        return Reply::Error(SYNTAX.to_owned());
    }
    let mut placed = Vec::with_capacity(triples.len() / 3);
    for triple in triples.chunks(3) {
        match point_from(&triple[0], &triple[1]) {
            Ok(point) => placed.push((geohash::score_of(point), triple[2].clone())),
            Err(error) => return Reply::Error(error),
        }
    }
    writing(context, db, |keys| {
        let entry = load(keys, &args[0], now)?;
        let mut set = sorted_of(entry.as_ref())?;
        let (mut added, mut changed) = (0, 0);
        for (score, member) in placed {
            let existing = set.score(&member);
            if (only_new && existing.is_some()) || (only_existing && existing.is_none()) {
                continue;
            }
            match existing {
                None => added += 1,
                Some(old) if old != score => changed += 1,
                Some(_) => {}
            }
            set.insert(member, score);
        }
        save(
            keys,
            &args[0],
            &Entry {
                data: Data::SortedSet(set),
                expires_at_ms: expiry(entry.as_ref()),
            },
        )?;
        Ok(Reply::Integer(if count_changed {
            added + changed
        } else {
            added
        }))
    })
}

fn geodist(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    if args.len() > 4 {
        return Reply::Error(SYNTAX.to_owned());
    }
    let now = context.now_ms;
    let unit = match args.get(3).map(|unit| unit_meters(unit)).transpose() {
        Ok(unit) => unit.unwrap_or(1.0),
        Err(error) => return Reply::Error(error),
    };
    reading(context, db, |keys| {
        let set = sorted_of(load(keys, &args[0], now)?.as_ref())?;
        Ok(
            match (member_point(&set, &args[1]), member_point(&set, &args[2])) {
                (Some(from), Some(to)) => {
                    Reply::bulk(distance_text(geohash::distance_meters(from, to), unit))
                }
                _ => Reply::null(),
            },
        )
    })
}

fn geohash_command(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    reading(context, db, |keys| {
        let set = sorted_of(load(keys, &args[0], now)?.as_ref())?;
        Ok(Reply::Array(Some(
            args[1..]
                .iter()
                .map(|member| match set.score(member) {
                    Some(score) => Reply::bulk(geohash::standard_geohash(score)),
                    None => Reply::null(),
                })
                .collect(),
        )))
    })
}

fn geopos(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    reading(context, db, |keys| {
        let set = sorted_of(load(keys, &args[0], now)?.as_ref())?;
        Ok(Reply::Array(Some(
            args[1..]
                .iter()
                .map(|member| match member_point(&set, member) {
                    Some(point) => coordinates_reply(point),
                    None => Reply::Array(None),
                })
                .collect(),
        )))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchForm {
    Radius { read_only: bool },
    RadiusByMember { read_only: bool },
    Search,
    SearchStore,
}

impl SearchForm {
    fn is_search(self) -> bool {
        matches!(self, Self::Search | Self::SearchStore)
    }

    fn takes_store_key(self) -> bool {
        matches!(
            self,
            Self::Radius { read_only: false } | Self::RadiusByMember { read_only: false }
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Center {
    Member(Vec<u8>),
    Point(Point),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Area {
    Radius {
        meters: f64,
    },
    Box {
        width_meters: f64,
        height_meters: f64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Order {
    AsFound,
    Nearest,
    Farthest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Destination {
    key: Vec<u8>,
    scores_are_distances: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct Search {
    source: Vec<u8>,
    center: Option<Center>,
    area: Option<Area>,
    unit_meters: f64,
    order: Order,
    count: Option<usize>,
    any: bool,
    with_coordinates: bool,
    with_distance: bool,
    with_hash: bool,
    destination: Option<Destination>,
}

#[derive(Debug, Clone, PartialEq)]
struct Found {
    member: Vec<u8>,
    score: f64,
    point: Point,
    distance_meters: f64,
}

fn radius_area(radius: &[u8], unit: &[u8]) -> Result<(Area, f64), String> {
    let radius = number_named(radius, "radius")?;
    if radius < 0.0 {
        return Err(NEGATIVE_RADIUS.to_owned());
    }
    let unit = unit_meters(unit)?;
    Ok((
        Area::Radius {
            meters: radius * unit,
        },
        unit,
    ))
}

fn box_area(width: &[u8], height: &[u8], unit: &[u8]) -> Result<(Area, f64), String> {
    let width = number_named(width, "width")?;
    let height = number_named(height, "height")?;
    if width < 0.0 || height < 0.0 {
        return Err(NEGATIVE_BOX.to_owned());
    }
    let unit = unit_meters(unit)?;
    Ok((
        Area::Box {
            width_meters: width * unit,
            height_meters: height * unit,
        },
        unit,
    ))
}

impl Search {
    fn new(source: Vec<u8>) -> Self {
        Self {
            source,
            center: None,
            area: None,
            unit_meters: 1.0,
            order: Order::AsFound,
            count: None,
            any: false,
            with_coordinates: false,
            with_distance: false,
            with_hash: false,
            destination: None,
        }
    }

    fn set_area(&mut self, (area, unit_meters): (Area, f64)) {
        self.area = Some(area);
        self.unit_meters = unit_meters;
    }

    fn parse(form: SearchForm, name: &str, args: &[Vec<u8>]) -> Result<Self, String> {
        let command = name.to_ascii_uppercase();
        let (mut search, options) = match form {
            SearchForm::Radius { .. } => {
                let mut search = Self::new(args[0].clone());
                search.center = Some(Center::Point(point_from(&args[1], &args[2])?));
                search.set_area(radius_area(&args[3], &args[4])?);
                (search, &args[5..])
            }
            SearchForm::RadiusByMember { .. } => {
                let mut search = Self::new(args[0].clone());
                search.center = Some(Center::Member(args[1].clone()));
                search.set_area(radius_area(&args[2], &args[3])?);
                (search, &args[4..])
            }
            SearchForm::Search => (Self::new(args[0].clone()), &args[1..]),
            SearchForm::SearchStore => {
                let mut search = Self::new(args[1].clone());
                search.destination = Some(Destination {
                    key: args[0].clone(),
                    scores_are_distances: false,
                });
                (search, &args[2..])
            }
        };
        search.parse_options(form, &command, options)?;
        search.check(form, &command)?;
        Ok(search)
    }

    fn parse_options(
        &mut self,
        form: SearchForm,
        command: &str,
        options: &[Vec<u8>],
    ) -> Result<(), String> {
        let mut at = 0;
        while let Some(word) = options.get(at) {
            let rest = &options[at + 1..];
            match String::from_utf8_lossy(word).to_ascii_lowercase().as_str() {
                "withdist" => self.with_distance = true,
                "withhash" => self.with_hash = true,
                "withcoord" => self.with_coordinates = true,
                "any" => self.any = true,
                "asc" => self.order = Order::Nearest,
                "desc" => self.order = Order::Farthest,
                "count" if !rest.is_empty() => {
                    let count = integer(&rest[0]).map_err(|_| NOT_INTEGER.to_owned())?;
                    if count <= 0 {
                        return Err(COUNT_NOT_POSITIVE.to_owned());
                    }
                    self.count = Some(usize::try_from(count).unwrap_or(usize::MAX));
                    at += 1;
                }
                store @ ("store" | "storedist") if form.takes_store_key() && !rest.is_empty() => {
                    self.destination = Some(Destination {
                        key: rest[0].clone(),
                        scores_are_distances: store == "storedist",
                    });
                    at += 1;
                }
                "storedist" if form == SearchForm::SearchStore => {
                    if let Some(destination) = self.destination.as_mut() {
                        destination.scores_are_distances = true;
                    }
                }
                "frommember" if form.is_search() && !rest.is_empty() => {
                    if matches!(self.center, Some(Center::Point(_))) {
                        return Err(format!(
                            "ERR FROMMEMBER and FROMLONLAT cannot both be specified for {command}"
                        ));
                    }
                    self.center = Some(Center::Member(rest[0].clone()));
                    at += 1;
                }
                "fromlonlat" if form.is_search() && rest.len() >= 2 => {
                    if matches!(self.center, Some(Center::Member(_))) {
                        return Err(format!(
                            "ERR FROMMEMBER and FROMLONLAT cannot both be specified for {command}"
                        ));
                    }
                    self.center = Some(Center::Point(point_from(&rest[0], &rest[1])?));
                    at += 2;
                }
                "byradius" if form.is_search() && rest.len() >= 2 => {
                    if matches!(self.area, Some(Area::Box { .. })) {
                        return Err(format!(
                            "ERR BYRADIUS and BYBOX cannot both be specified for {command}"
                        ));
                    }
                    self.set_area(radius_area(&rest[0], &rest[1])?);
                    at += 2;
                }
                "bybox" if form.is_search() && rest.len() >= 3 => {
                    if matches!(self.area, Some(Area::Radius { .. })) {
                        return Err(format!(
                            "ERR BYRADIUS and BYBOX cannot both be specified for {command}"
                        ));
                    }
                    self.set_area(box_area(&rest[0], &rest[1], &rest[2])?);
                    at += 3;
                }
                _ => return Err(SYNTAX.to_owned()),
            }
            at += 1;
        }
        Ok(())
    }

    fn check(&mut self, form: SearchForm, command: &str) -> Result<(), String> {
        if self.destination.is_some()
            && (self.with_distance || self.with_hash || self.with_coordinates)
        {
            let who = if form == SearchForm::SearchStore {
                "GEOSEARCHSTORE"
            } else {
                "STORE option in GEORADIUS"
            };
            return Err(format!(
                "ERR {who} is not compatible with WITHDIST, WITHHASH and WITHCOORD options"
            ));
        }
        if form.is_search() && self.center.is_none() {
            return Err(format!(
                "ERR exactly one of FROMMEMBER or FROMLONLAT can be specified for {command}"
            ));
        }
        if form.is_search() && self.area.is_none() {
            return Err(format!(
                "ERR exactly one of BYRADIUS and BYBOX can be specified for {command}"
            ));
        }
        if self.any && self.count.is_none() {
            return Err(ANY_WITHOUT_COUNT.to_owned());
        }
        if self.count.is_some() && self.order == Order::AsFound && !self.any {
            self.order = Order::Nearest;
        }
        Ok(())
    }

    fn distance_to(&self, center: Point, point: Point) -> Option<f64> {
        match self.area? {
            Area::Radius { meters } => geohash::distance_within_radius(center, point, meters),
            Area::Box {
                width_meters,
                height_meters,
            } => geohash::distance_within_box(center, point, width_meters, height_meters),
        }
    }

    fn candidates(&self, set: &SortedSet, center: Point) -> Vec<(f64, Vec<u8>)> {
        let ordered = set.ordered();
        let ranges = match self.area {
            Some(Area::Radius { meters }) => geohash::search_ranges(center, meters, meters),
            Some(Area::Box {
                width_meters,
                height_meters,
            }) => geohash::search_ranges(center, width_meters / 2.0, height_meters / 2.0),
            None => None,
        };
        let Some(ranges) = ranges else {
            return ordered;
        };
        let mut chosen = Vec::new();
        for (low, high) in ranges {
            let start = ordered.partition_point(|(score, _)| *score < low as f64);
            let end = ordered.partition_point(|(score, _)| *score < high as f64);
            chosen.extend(start..end);
        }
        chosen.sort_unstable();
        chosen.dedup();
        let mut ordered: Vec<Option<(f64, Vec<u8>)>> = ordered.into_iter().map(Some).collect();
        chosen.into_iter().filter_map(|index| ordered.get_mut(index).and_then(Option::take)).collect()
    }

    fn find(&self, set: &SortedSet) -> Result<Vec<Found>, String> {
        let center = match &self.center {
            Some(Center::Point(point)) => *point,
            Some(Center::Member(member)) => {
                member_point(set, member).ok_or_else(|| MEMBER_NOT_FOUND.to_owned())?
            }
            None => return Ok(Vec::new()),
        };
        let stop_after = if self.any { self.count } else { None };
        let mut found = Vec::new();
        for (score, member) in self.candidates(set, center) {
            let point = geohash::decode_score(score);
            if let Some(distance_meters) = self.distance_to(center, point) {
                found.push(Found {
                    member,
                    score,
                    point,
                    distance_meters,
                });
                if stop_after.is_some_and(|count| found.len() >= count) {
                    break;
                }
            }
        }
        match self.order {
            Order::AsFound => {}
            Order::Nearest => found.sort_by(|a, b| a.distance_meters.total_cmp(&b.distance_meters)),
            Order::Farthest => {
                found.sort_by(|a, b| b.distance_meters.total_cmp(&a.distance_meters))
            }
        }
        if let Some(count) = self.count {
            found.truncate(count);
        }
        Ok(found)
    }

    fn located(&self, keys: &dyn KeysRead, now: u64) -> Result<Option<Vec<Found>>, String> {
        match load(keys, &self.source, now)?.map(|entry| entry.data) {
            None => Ok(None),
            Some(Data::SortedSet(set)) => self.find(&set).map(Some),
            Some(_) => Err(WRONG_TYPE.to_owned()),
        }
    }

    fn item_reply(&self, found: Found) -> Reply {
        if !(self.with_distance || self.with_hash || self.with_coordinates) {
            return Reply::bulk(found.member);
        }
        let mut item = vec![Reply::bulk(found.member)];
        if self.with_distance {
            item.push(Reply::bulk(distance_text(
                found.distance_meters,
                self.unit_meters,
            )));
        }
        if self.with_hash {
            item.push(Reply::Integer(found.score as i64));
        }
        if self.with_coordinates {
            item.push(coordinates_reply(found.point));
        }
        Reply::Array(Some(item))
    }

    fn stored_set(&self, found: Vec<Found>, scores_are_distances: bool) -> SortedSet {
        let mut set = SortedSet::default();
        for found in found {
            let score = if scores_are_distances {
                found.distance_meters / self.unit_meters
            } else {
                found.score
            };
            set.insert(found.member, score);
        }
        set
    }
}

fn search(context: &Context<'_>, db: u32, form: SearchForm, name: &str, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    let search = match Search::parse(form, name, args) {
        Ok(search) => search,
        Err(error) => return Reply::Error(error),
    };
    let Some(destination) = search.destination.clone() else {
        return reading(context, db, |keys| {
            Ok(match search.located(keys, now)? {
                None => Reply::Array(Some(Vec::new())),
                Some(found) => Reply::Array(Some(
                    found
                        .into_iter()
                        .map(|found| search.item_reply(found))
                        .collect(),
                )),
            })
        });
    };
    writing(context, db, |keys| {
        let found = search.located(keys, now)?.unwrap_or_default();
        let stored = found.len();
        if stored == 0 {
            keys.delete(&destination.key)?;
            return Ok(Reply::Integer(0));
        }
        let set = search.stored_set(found, destination.scores_are_distances);
        save(
            keys,
            &destination.key,
            &Entry {
                data: Data::SortedSet(set),
                expires_at_ms: None,
            },
        )?;
        Ok(Reply::Integer(stored as i64))
    })
}
