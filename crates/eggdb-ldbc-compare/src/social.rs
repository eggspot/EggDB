use crate::bolt::Value;

const FIRST_NAMES: [&str; 24] = [
    "Ada", "Alan", "Grace", "Edsger", "Barbara", "Donald", "Frances", "John", "Margaret", "Dennis", "Radia", "Ken", "Leslie",
    "Tim", "Hedy", "Niklaus", "Katherine", "Tony", "Shafi", "Robin", "Jean", "Bjarne", "Anita", "Linus",
];
const LAST_NAMES: [&str; 16] = [
    "Lovelace", "Turing", "Hopper", "Dijkstra", "Liskov", "Knuth", "Allen", "Backus", "Hamilton", "Ritchie", "Perlman",
    "Thompson", "Lamport", "Berners", "Lamarr", "Wirth",
];
const FIRST_DAY: i64 = 1_262_304_000_000;
const DAY_MILLIS: i64 = 86_400_000;
const DAYS: i64 = 3 * 365;

/// How large the generated network is: the persons, and per person the average friends and
/// posts, in the shape of the LDBC Social Network Benchmark's core (Person, KNOWS, Post,
/// HAS_CREATOR, LIKES).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Scale {
    pub(crate) persons: u64,
    pub(crate) friends: u64,
    pub(crate) posts: u64,
}

pub(crate) fn mixed(mut state: u64) -> u64 {
    state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    state = (state ^ (state >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    state = (state ^ (state >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    state ^ (state >> 31)
}

fn int(value: u64) -> Value {
    Value::Int(i64::try_from(value).unwrap_or(i64::MAX))
}

fn day(seed: u64) -> i64 {
    FIRST_DAY + (mixed(seed) % DAYS as u64) as i64 * DAY_MILLIS
}

pub(crate) fn first_name(person: u64) -> &'static str {
    FIRST_NAMES[(mixed(person ^ 0xF1) % FIRST_NAMES.len() as u64) as usize]
}

/// The persons from `first` up to `end`, as maps for `UNWIND`.
pub(crate) fn persons(first: u64, end: u64) -> Vec<Value> {
    (first..end)
        .map(|id| {
            Value::map(vec![
                ("id", int(id)),
                ("firstName", Value::Text(first_name(id).to_owned())),
                ("lastName", Value::Text(LAST_NAMES[(mixed(id ^ 0x1A) % LAST_NAMES.len() as u64) as usize].to_owned())),
                ("birthday", Value::Int(day(id ^ 0xB1) - 30 * 365 * DAY_MILLIS)),
                ("creationDate", Value::Int(day(id ^ 0xC2))),
            ])
        })
        .collect()
}

/// A person's friends: a skewed choice, so a few persons have many friends as in a real network,
/// each pair listed once (from the smaller id).
pub(crate) fn friendships(person: u64, scale: Scale) -> Vec<Value> {
    let count = 1 + mixed(person ^ 0xD3) % (scale.friends * 2);
    (0..count)
        .filter_map(|edge| {
            let draw = mixed(person.wrapping_mul(1_000_003).wrapping_add(edge)) % scale.persons;
            let skewed = draw * draw / scale.persons.max(1);
            (skewed > person).then(|| {
                Value::map(vec![("a", int(person)), ("b", int(skewed)), ("creationDate", Value::Int(day(person ^ edge ^ 0xE4)))])
            })
        })
        .collect()
}

/// A person's posts, with their ids, creation dates and lengths.
pub(crate) fn posts(person: u64, scale: Scale) -> Vec<Value> {
    (0..scale.posts)
        .map(|index| {
            let id = person * scale.posts + index;
            Value::map(vec![
                ("id", int(id)),
                ("creator", int(person)),
                ("content", Value::Text(format!("post {id} by person {person}"))),
                ("length", int(16 + mixed(id) % 200)),
                ("creationDate", Value::Int(day(id ^ 0xF5))),
            ])
        })
        .collect()
}

/// The posts a person likes.
pub(crate) fn likes(person: u64, scale: Scale) -> Vec<Value> {
    let total_posts = scale.persons * scale.posts;
    (0..1 + mixed(person ^ 0x99) % 6)
        .map(|index| {
            Value::map(vec![
                ("person", int(person)),
                ("post", int(mixed(person.wrapping_mul(31).wrapping_add(index)) % total_posts.max(1))),
                ("creationDate", Value::Int(day(person ^ index ^ 0x77))),
            ])
        })
        .collect()
}

pub(crate) const DATE_MIDDLE: i64 = FIRST_DAY + DAYS / 2 * DAY_MILLIS;
