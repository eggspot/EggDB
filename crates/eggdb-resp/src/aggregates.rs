use crate::commands::{
    Context, NOT_INTEGER, SYNTAX, WRONG_TYPE, float, integer, load, range_of, reading, save,
    writing,
};
use crate::protocol::{Reply, format_double};
use crate::value::{Data, Entry, SortedSet};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

fn hash_of(entry: Option<&Entry>) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, String> {
    match entry.map(|entry| &entry.data) {
        None => Ok(BTreeMap::new()),
        Some(Data::Hash(map)) => Ok(map.clone()),
        Some(_) => Err(WRONG_TYPE.to_owned()),
    }
}

fn list_of(entry: Option<&Entry>) -> Result<VecDeque<Vec<u8>>, String> {
    match entry.map(|entry| &entry.data) {
        None => Ok(VecDeque::new()),
        Some(Data::List(list)) => Ok(list.clone()),
        Some(_) => Err(WRONG_TYPE.to_owned()),
    }
}

fn set_of(entry: Option<&Entry>) -> Result<BTreeSet<Vec<u8>>, String> {
    match entry.map(|entry| &entry.data) {
        None => Ok(BTreeSet::new()),
        Some(Data::Set(set)) => Ok(set.clone()),
        Some(_) => Err(WRONG_TYPE.to_owned()),
    }
}

fn sorted_of(entry: Option<&Entry>) -> Result<SortedSet, String> {
    match entry.map(|entry| &entry.data) {
        None => Ok(SortedSet::default()),
        Some(Data::SortedSet(set)) => Ok(set.clone()),
        Some(_) => Err(WRONG_TYPE.to_owned()),
    }
}

fn expiry(entry: Option<&Entry>) -> Option<u64> {
    entry.and_then(|entry| entry.expires_at_ms)
}

fn bulks(items: impl IntoIterator<Item = Vec<u8>>) -> Reply {
    Reply::Array(Some(items.into_iter().map(Reply::bulk).collect()))
}

const NAMES: &[&str] = &[
    "hset", "hmset", "hsetnx", "hget", "hmget", "hdel", "hgetall", "hkeys", "hvals", "hlen",
    "hexists", "hincrby", "lpush", "rpush", "lpop", "rpop", "llen", "lrange", "lindex", "lset",
    "lrem", "ltrim", "sadd", "srem", "smembers", "sismember", "scard", "sinter", "sunion",
    "sdiff", "zadd", "zrem", "zscore", "zcard", "zrange", "zrank", "zincrby", "zrangebyscore",
    "zcount",
];

/// Whether `name` is one of these commands.
pub(crate) fn is_known(name: &str) -> bool {
    NAMES.contains(&name)
}

/// Runs a hash, list, set or sorted-set command, or `None` when `name` with this many
/// arguments is not one.
pub(crate) fn execute(context: &Context<'_>, db: u32, name: &str, args: &[Vec<u8>]) -> Option<Reply> {
    let now = context.now_ms;
    Some(match (name, args.len()) {
        ("hset" | "hmset", count) if count >= 3 && count % 2 == 1 => writing(context, db, |keys| {
            let entry = load(keys, &args[0], now)?;
            let mut map = hash_of(entry.as_ref())?;
            let mut added = 0;
            for pair in args[1..].chunks(2) {
                if map.insert(pair[0].clone(), pair[1].clone()).is_none() {
                    added += 1;
                }
            }
            save(keys, &args[0], &Entry { data: Data::Hash(map), expires_at_ms: expiry(entry.as_ref()) })?;
            Ok(if name == "hset" { Reply::Integer(added) } else { Reply::ok() })
        }),
        ("hsetnx", 3) => writing(context, db, |keys| {
            let entry = load(keys, &args[0], now)?;
            let mut map = hash_of(entry.as_ref())?;
            if map.contains_key(&args[1]) {
                return Ok(Reply::Integer(0));
            }
            map.insert(args[1].clone(), args[2].clone());
            save(keys, &args[0], &Entry { data: Data::Hash(map), expires_at_ms: expiry(entry.as_ref()) })?;
            Ok(Reply::Integer(1))
        }),
        ("hget", 2) => reading(context, db, |keys| {
            Ok(Reply::Bulk(hash_of(load(keys, &args[0], now)?.as_ref())?.remove(&args[1])))
        }),
        ("hmget", count) if count >= 2 => reading(context, db, |keys| {
            let map = hash_of(load(keys, &args[0], now)?.as_ref())?;
            Ok(Reply::Array(Some(args[1..].iter().map(|field| Reply::Bulk(map.get(field).cloned())).collect())))
        }),
        ("hdel", count) if count >= 2 => writing(context, db, |keys| {
            let entry = load(keys, &args[0], now)?;
            let mut map = hash_of(entry.as_ref())?;
            let removed = args[1..].iter().filter(|field| map.remove(*field).is_some()).count();
            if entry.is_some() {
                save(keys, &args[0], &Entry { data: Data::Hash(map), expires_at_ms: expiry(entry.as_ref()) })?;
            }
            Ok(Reply::Integer(removed as i64))
        }),
        ("hgetall", 1) => reading(context, db, |keys| {
            let map = hash_of(load(keys, &args[0], now)?.as_ref())?;
            Ok(Reply::Map(map.into_iter().map(|(field, value)| (Reply::bulk(field), Reply::bulk(value))).collect()))
        }),
        ("hkeys" | "hvals", 1) => reading(context, db, |keys| {
            let map = hash_of(load(keys, &args[0], now)?.as_ref())?;
            Ok(if name == "hkeys" { bulks(map.into_keys()) } else { bulks(map.into_values()) })
        }),
        ("hlen", 1) => reading(context, db, |keys| {
            Ok(Reply::Integer(hash_of(load(keys, &args[0], now)?.as_ref())?.len() as i64))
        }),
        ("hexists", 2) => reading(context, db, |keys| {
            Ok(Reply::Integer(i64::from(hash_of(load(keys, &args[0], now)?.as_ref())?.contains_key(&args[1]))))
        }),
        ("hincrby", 3) => {
            let by = match integer(&args[2]) {
                Ok(by) => by,
                Err(error) => return Some(Reply::Error(error)),
            };
            writing(context, db, |keys| {
                let entry = load(keys, &args[0], now)?;
                let mut map = hash_of(entry.as_ref())?;
                let current = match map.get(&args[1]) {
                    None => 0,
                    Some(value) => integer(value).map_err(|_| "ERR hash value is not an integer".to_owned())?,
                };
                let next = current
                    .checked_add(by)
                    .ok_or_else(|| "ERR increment or decrement would overflow".to_owned())?;
                map.insert(args[1].clone(), next.to_string().into_bytes());
                save(keys, &args[0], &Entry { data: Data::Hash(map), expires_at_ms: expiry(entry.as_ref()) })?;
                Ok(Reply::Integer(next))
            })
        }
        ("lpush" | "rpush", count) if count >= 2 => writing(context, db, |keys| {
            let entry = load(keys, &args[0], now)?;
            let mut list = list_of(entry.as_ref())?;
            for item in &args[1..] {
                if name == "lpush" {
                    list.push_front(item.clone());
                } else {
                    list.push_back(item.clone());
                }
            }
            let length = list.len();
            save(keys, &args[0], &Entry { data: Data::List(list), expires_at_ms: expiry(entry.as_ref()) })?;
            Ok(Reply::Integer(length as i64))
        }),
        ("lpop" | "rpop", count @ (1 | 2)) => {
            let wanted = if count == 2 {
                match integer(&args[1]) {
                    Ok(wanted) if wanted >= 0 => Some(wanted as usize),
                    _ => return Some(Reply::Error("ERR value is out of range, must be positive".to_owned())),
                }
            } else {
                None
            };
            writing(context, db, |keys| {
                let entry = load(keys, &args[0], now)?;
                if entry.is_none() {
                    return Ok(if wanted.is_some() { Reply::Array(None) } else { Reply::null() });
                }
                let mut list = list_of(entry.as_ref())?;
                let mut popped = Vec::new();
                for _ in 0..wanted.unwrap_or(1) {
                    let item = if name == "lpop" { list.pop_front() } else { list.pop_back() };
                    match item {
                        Some(item) => popped.push(item),
                        None => break,
                    }
                }
                save(keys, &args[0], &Entry { data: Data::List(list), expires_at_ms: expiry(entry.as_ref()) })?;
                Ok(match wanted {
                    Some(_) => bulks(popped),
                    None => Reply::Bulk(popped.into_iter().next()),
                })
            })
        }
        ("llen", 1) => reading(context, db, |keys| {
            Ok(Reply::Integer(list_of(load(keys, &args[0], now)?.as_ref())?.len() as i64))
        }),
        ("lrange", 3) => {
            let (Ok(start), Ok(stop)) = (integer(&args[1]), integer(&args[2])) else {
                return Some(Reply::Error(NOT_INTEGER.to_owned()));
            };
            reading(context, db, |keys| {
                let list = list_of(load(keys, &args[0], now)?.as_ref())?;
                Ok(match range_of(list.len(), start, stop) {
                    Some((from, to)) => bulks(list.into_iter().skip(from).take(to - from + 1)),
                    None => Reply::Array(Some(Vec::new())),
                })
            })
        }
        ("lindex", 2) => {
            let Ok(index) = integer(&args[1]) else {
                return Some(Reply::Error(NOT_INTEGER.to_owned()));
            };
            reading(context, db, |keys| {
                let list = list_of(load(keys, &args[0], now)?.as_ref())?;
                let at = if index < 0 { list.len() as i64 + index } else { index };
                Ok(Reply::Bulk(usize::try_from(at).ok().and_then(|at| list.get(at).cloned())))
            })
        }
        ("lset", 3) => {
            let Ok(index) = integer(&args[1]) else {
                return Some(Reply::Error(NOT_INTEGER.to_owned()));
            };
            writing(context, db, |keys| {
                let entry = load(keys, &args[0], now)?;
                if entry.is_none() {
                    return Err("ERR no such key".to_owned());
                }
                let mut list = list_of(entry.as_ref())?;
                let at = if index < 0 { list.len() as i64 + index } else { index };
                let slot = usize::try_from(at)
                    .ok()
                    .and_then(|at| list.get_mut(at))
                    .ok_or_else(|| "ERR index out of range".to_owned())?;
                *slot = args[2].clone();
                save(keys, &args[0], &Entry { data: Data::List(list), expires_at_ms: expiry(entry.as_ref()) })?;
                Ok(Reply::ok())
            })
        }
        ("lrem", 3) => {
            let Ok(count) = integer(&args[1]) else {
                return Some(Reply::Error(NOT_INTEGER.to_owned()));
            };
            writing(context, db, |keys| {
                let entry = load(keys, &args[0], now)?;
                let list = list_of(entry.as_ref())?;
                let limit = if count == 0 { usize::MAX } else { count.unsigned_abs() as usize };
                let mut removed = 0;
                let mut kept: Vec<Vec<u8>> = Vec::with_capacity(list.len());
                let ordered: Vec<Vec<u8>> = if count < 0 { list.into_iter().rev().collect() } else { list.into_iter().collect() };
                for item in ordered {
                    if removed < limit && item == args[2] {
                        removed += 1;
                    } else {
                        kept.push(item);
                    }
                }
                if count < 0 {
                    kept.reverse();
                }
                if entry.is_some() {
                    save(keys, &args[0], &Entry { data: Data::List(kept.into()), expires_at_ms: expiry(entry.as_ref()) })?;
                }
                Ok(Reply::Integer(removed as i64))
            })
        }
        ("ltrim", 3) => {
            let (Ok(start), Ok(stop)) = (integer(&args[1]), integer(&args[2])) else {
                return Some(Reply::Error(NOT_INTEGER.to_owned()));
            };
            writing(context, db, |keys| {
                let entry = load(keys, &args[0], now)?;
                let list = list_of(entry.as_ref())?;
                let kept: VecDeque<Vec<u8>> = match range_of(list.len(), start, stop) {
                    Some((from, to)) => list.into_iter().skip(from).take(to - from + 1).collect(),
                    None => VecDeque::new(),
                };
                if entry.is_some() {
                    save(keys, &args[0], &Entry { data: Data::List(kept), expires_at_ms: expiry(entry.as_ref()) })?;
                }
                Ok(Reply::ok())
            })
        }
        ("sadd" | "srem", count) if count >= 2 => writing(context, db, |keys| {
            let entry = load(keys, &args[0], now)?;
            let mut set = set_of(entry.as_ref())?;
            let changed = args[1..]
                .iter()
                .filter(|member| if name == "sadd" { set.insert((*member).clone()) } else { set.remove(*member) })
                .count();
            if name == "sadd" || entry.is_some() {
                save(keys, &args[0], &Entry { data: Data::Set(set), expires_at_ms: expiry(entry.as_ref()) })?;
            }
            Ok(Reply::Integer(changed as i64))
        }),
        ("smembers", 1) => reading(context, db, |keys| {
            let set = set_of(load(keys, &args[0], now)?.as_ref())?;
            Ok(Reply::Set(set.into_iter().map(Reply::bulk).collect()))
        }),
        ("sismember", 2) => reading(context, db, |keys| {
            Ok(Reply::Integer(i64::from(set_of(load(keys, &args[0], now)?.as_ref())?.contains(&args[1]))))
        }),
        ("scard", 1) => reading(context, db, |keys| {
            Ok(Reply::Integer(set_of(load(keys, &args[0], now)?.as_ref())?.len() as i64))
        }),
        ("sinter" | "sunion" | "sdiff", count) if count >= 1 => reading(context, db, |keys| {
            let mut sets = Vec::with_capacity(args.len());
            for key in args {
                sets.push(set_of(load(keys, key, now)?.as_ref())?);
            }
            let mut result = sets.remove(0);
            for other in sets {
                result = match name {
                    "sinter" => result.intersection(&other).cloned().collect(),
                    "sunion" => result.union(&other).cloned().collect(),
                    _ => result.difference(&other).cloned().collect(),
                };
            }
            Ok(Reply::Set(result.into_iter().map(Reply::bulk).collect()))
        }),
        ("zadd", count) if count >= 3 => zadd(context, db, args),
        ("zrem", count) if count >= 2 => writing(context, db, |keys| {
            let entry = load(keys, &args[0], now)?;
            let mut set = sorted_of(entry.as_ref())?;
            let removed = args[1..].iter().filter(|member| set.remove(member)).count();
            if entry.is_some() {
                save(keys, &args[0], &Entry { data: Data::SortedSet(set), expires_at_ms: expiry(entry.as_ref()) })?;
            }
            Ok(Reply::Integer(removed as i64))
        }),
        ("zscore", 2) => reading(context, db, |keys| {
            Ok(match sorted_of(load(keys, &args[0], now)?.as_ref())?.score(&args[1]) {
                Some(score) => Reply::Double(score),
                None => Reply::null(),
            })
        }),
        ("zcard", 1) => reading(context, db, |keys| {
            Ok(Reply::Integer(sorted_of(load(keys, &args[0], now)?.as_ref())?.len() as i64))
        }),
        ("zincrby", 3) => {
            let by = match float(&args[1]) {
                Ok(by) => by,
                Err(error) => return Some(Reply::Error(error)),
            };
            writing(context, db, |keys| {
                let entry = load(keys, &args[0], now)?;
                let mut set = sorted_of(entry.as_ref())?;
                let next = set.score(&args[2]).unwrap_or(0.0) + by;
                if next.is_nan() {
                    return Err("ERR resulting score is not a number (NaN)".to_owned());
                }
                set.insert(args[2].clone(), next);
                save(keys, &args[0], &Entry { data: Data::SortedSet(set), expires_at_ms: expiry(entry.as_ref()) })?;
                Ok(Reply::Double(next))
            })
        }
        ("zrank", 2) => reading(context, db, |keys| {
            let ordered = sorted_of(load(keys, &args[0], now)?.as_ref())?.ordered();
            Ok(match ordered.iter().position(|(_, member)| *member == args[1]) {
                Some(rank) => Reply::Integer(rank as i64),
                None => Reply::null(),
            })
        }),
        ("zrange", count) if count >= 3 => zrange(context, db, args),
        ("zrangebyscore" | "zcount", count) if (name == "zcount" && count == 3) || (name == "zrangebyscore" && count >= 3) => {
            let (Ok(low), Ok(high)) = (score_bound(&args[1]), score_bound(&args[2])) else {
                return Some(Reply::Error("ERR min or max is not a float".to_owned()));
            };
            let with_scores = args.len() > 3 && args[3].eq_ignore_ascii_case(b"withscores");
            if name == "zrangebyscore" && args.len() > 3 && !with_scores {
                return Some(Reply::Error(SYNTAX.to_owned()));
            }
            reading(context, db, |keys| {
                let ordered = sorted_of(load(keys, &args[0], now)?.as_ref())?.ordered();
                let selected: Vec<(f64, Vec<u8>)> = ordered
                    .into_iter()
                    .filter(|(score, _)| low.admits_low(*score) && high.admits_high(*score))
                    .collect();
                Ok(if name == "zcount" {
                    Reply::Integer(selected.len() as i64)
                } else {
                    scored(selected, with_scores)
                })
            })
        }
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy)]
struct Bound {
    value: f64,
    exclusive: bool,
}

impl Bound {
    fn admits_low(self, score: f64) -> bool {
        if self.exclusive { score > self.value } else { score >= self.value }
    }

    fn admits_high(self, score: f64) -> bool {
        if self.exclusive { score < self.value } else { score <= self.value }
    }
}

fn score_bound(bytes: &[u8]) -> Result<Bound, String> {
    match bytes.strip_prefix(b"(") {
        Some(rest) => Ok(Bound { value: float(rest)?, exclusive: true }),
        None => Ok(Bound { value: float(bytes)?, exclusive: false }),
    }
}

fn scored(items: Vec<(f64, Vec<u8>)>, with_scores: bool) -> Reply {
    let mut out = Vec::with_capacity(items.len() * if with_scores { 2 } else { 1 });
    for (score, member) in items {
        out.push(Reply::bulk(member));
        if with_scores {
            out.push(Reply::bulk(format_double(score)));
        }
    }
    Reply::Array(Some(out))
}

fn zadd(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    let mut at = 1;
    let (mut only_new, mut only_existing, mut changed_count, mut increment) = (false, false, false, false);
    while at < args.len() {
        match String::from_utf8_lossy(&args[at]).to_ascii_lowercase().as_str() {
            "nx" => only_new = true,
            "xx" => only_existing = true,
            "ch" => changed_count = true,
            "incr" => increment = true,
            _ => break,
        }
        at += 1;
    }
    let pairs = &args[at..];
    if pairs.is_empty() || pairs.len() % 2 != 0 || (only_new && only_existing) || (increment && pairs.len() != 2) {
        return Reply::Error(SYNTAX.to_owned());
    }
    let mut parsed = Vec::with_capacity(pairs.len() / 2);
    for pair in pairs.chunks(2) {
        match float(&pair[0]) {
            Ok(score) => parsed.push((score, pair[1].clone())),
            Err(error) => return Reply::Error(error),
        }
    }
    writing(context, db, |keys| {
        let entry = load(keys, &args[0], now)?;
        let mut set = sorted_of(entry.as_ref())?;
        let (mut added, mut changed) = (0, 0);
        let mut last = None;
        for (score, member) in parsed {
            let existing = set.score(&member);
            if (only_new && existing.is_some()) || (only_existing && existing.is_none()) {
                continue;
            }
            let next = if increment { existing.unwrap_or(0.0) + score } else { score };
            if next.is_nan() {
                return Err("ERR resulting score is not a number (NaN)".to_owned());
            }
            if existing.is_none() {
                added += 1;
            } else if existing != Some(next) {
                changed += 1;
            }
            set.insert(member, next);
            last = Some(next);
        }
        save(keys, &args[0], &Entry { data: Data::SortedSet(set), expires_at_ms: expiry(entry.as_ref()) })?;
        Ok(if increment {
            last.map_or(Reply::null(), Reply::Double)
        } else {
            Reply::Integer(if changed_count { added + changed } else { added })
        })
    })
}

fn zrange(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    let (Ok(start), Ok(stop)) = (integer(&args[1]), integer(&args[2])) else {
        return Reply::Error(NOT_INTEGER.to_owned());
    };
    let (mut with_scores, mut reverse) = (false, false);
    for option in &args[3..] {
        match String::from_utf8_lossy(option).to_ascii_lowercase().as_str() {
            "withscores" => with_scores = true,
            "rev" => reverse = true,
            _ => return Reply::Error(SYNTAX.to_owned()),
        }
    }
    reading(context, db, |keys| {
        let mut ordered = sorted_of(load(keys, &args[0], now)?.as_ref())?.ordered();
        if reverse {
            ordered.reverse();
        }
        Ok(match range_of(ordered.len(), start, stop) {
            Some((from, to)) => scored(ordered.into_iter().skip(from).take(to - from + 1).collect(), with_scores),
            None => Reply::Array(Some(Vec::new())),
        })
    })
}
