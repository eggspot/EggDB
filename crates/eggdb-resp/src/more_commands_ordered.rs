use crate::aggregates::expiry;
use crate::commands::{Context, NOT_INTEGER, SYNTAX, WRONG_TYPE, float, integer, load, range_of, reading, save, writing};
use crate::more_commands::{random_members, sets_in, sorted_in};
use crate::protocol::{Reply, format_double};
use crate::value::{Data, Entry, SortedSet};
use std::collections::VecDeque;

const NAMES: &[&str] = &[
    "zrevrange",
    "zrevrangebyscore",
    "zrangebylex",
    "zrevrangebylex",
    "zlexcount",
    "zremrangebyrank",
    "zremrangebyscore",
    "zremrangebylex",
    "zrevrank",
    "spop",
    "linsert",
    "lpushx",
    "rpushx",
    "time",
    "lastsave",
    "config",
];

pub(crate) fn is_known(name: &str) -> bool {
    NAMES.contains(&name)
}

fn lower(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_ascii_lowercase()
}

#[derive(Clone, Copy)]
struct ScoreBound {
    value: f64,
    exclusive: bool,
}

fn score_bound(bytes: &[u8]) -> Result<ScoreBound, String> {
    let (rest, exclusive) = match bytes.strip_prefix(b"(") {
        Some(rest) => (rest, true),
        None => (bytes, false),
    };
    Ok(ScoreBound {
        value: float(rest).map_err(|_| "ERR min or max is not a float".to_owned())?,
        exclusive,
    })
}

fn above(score: f64, bound: ScoreBound) -> bool {
    if bound.exclusive { score > bound.value } else { score >= bound.value }
}

fn below(score: f64, bound: ScoreBound) -> bool {
    if bound.exclusive { score < bound.value } else { score <= bound.value }
}

#[derive(Clone)]
enum LexBound {
    Lowest,
    Highest,
    Inclusive(Vec<u8>),
    Exclusive(Vec<u8>),
}

fn lex_bound(bytes: &[u8]) -> Result<LexBound, String> {
    match bytes {
        b"-" => Ok(LexBound::Lowest),
        b"+" => Ok(LexBound::Highest),
        [b'[', rest @ ..] => Ok(LexBound::Inclusive(rest.to_vec())),
        [b'(', rest @ ..] => Ok(LexBound::Exclusive(rest.to_vec())),
        _ => Err("ERR min or max not valid string range item".to_owned()),
    }
}

fn lex_above(member: &[u8], bound: &LexBound) -> bool {
    match bound {
        LexBound::Lowest => true,
        LexBound::Highest => false,
        LexBound::Inclusive(value) => member >= value.as_slice(),
        LexBound::Exclusive(value) => member > value.as_slice(),
    }
}

fn lex_below(member: &[u8], bound: &LexBound) -> bool {
    match bound {
        LexBound::Lowest => false,
        LexBound::Highest => true,
        LexBound::Inclusive(value) => member <= value.as_slice(),
        LexBound::Exclusive(value) => member < value.as_slice(),
    }
}

fn scored(items: Vec<(f64, Vec<u8>)>, with_scores: bool) -> Reply {
    let mut out = Vec::new();
    for (score, member) in items {
        out.push(Reply::bulk(member));
        if with_scores {
            out.push(Reply::bulk(format_double(score)));
        }
    }
    Reply::Array(Some(out))
}

fn paging(options: &[Vec<u8>], allow_scores: bool) -> Result<(bool, Option<(usize, Option<usize>)>), String> {
    let (mut with_scores, mut limit) = (false, None);
    let mut at = 0;
    while at < options.len() {
        match lower(&options[at]).as_str() {
            "withscores" if allow_scores => {
                with_scores = true;
                at += 1;
            }
            "limit" => {
                let (Some(offset), Some(count)) = (options.get(at + 1), options.get(at + 2)) else {
                    return Err(SYNTAX.to_owned());
                };
                let offset = integer(offset)?;
                let count = integer(count)?;
                if offset < 0 {
                    limit = Some((usize::MAX, Some(0)));
                } else {
                    limit = Some((offset as usize, (count >= 0).then_some(count as usize)));
                }
                at += 3;
            }
            _ => return Err(SYNTAX.to_owned()),
        }
    }
    Ok((with_scores, limit))
}

fn paged(items: Vec<(f64, Vec<u8>)>, limit: Option<(usize, Option<usize>)>) -> Vec<(f64, Vec<u8>)> {
    match limit {
        None => items,
        Some((offset, count)) => items
            .into_iter()
            .skip(offset)
            .take(count.unwrap_or(usize::MAX))
            .collect(),
    }
}

fn list_in(entry: Option<&Entry>) -> Result<VecDeque<Vec<u8>>, String> {
    match entry.map(|entry| &entry.data) {
        None => Ok(VecDeque::new()),
        Some(Data::List(list)) => Ok(list.clone()),
        Some(_) => Err(WRONG_TYPE.to_owned()),
    }
}

fn store_sorted(
    keys: &mut dyn eggdb_keyspace::KeysWrite,
    key: &[u8],
    set: SortedSet,
    expires_at_ms: Option<u64>,
) -> Result<(), String> {
    save(
        keys,
        key,
        &Entry {
            data: Data::SortedSet(set),
            expires_at_ms,
        },
    )
}

pub(crate) fn execute(
    context: &Context<'_>,
    db: u32,
    name: &str,
    args: &[Vec<u8>],
) -> Option<Reply> {
    let now = context.now_ms;
    Some(match (name, args.len()) {
        ("zrevrange", 3 | 4) => {
            let (Ok(start), Ok(stop)) = (integer(&args[1]), integer(&args[2])) else {
                return Some(Reply::Error(NOT_INTEGER.to_owned()));
            };
            let with_scores = match args.get(3) {
                None => false,
                Some(option) if lower(option) == "withscores" => true,
                Some(_) => return Some(Reply::Error(SYNTAX.to_owned())),
            };
            reading(context, db, |keys| {
                let mut ordered = sorted_in(load(keys, &args[0], now)?.as_ref())?.ordered();
                ordered.reverse();
                Ok(match range_of(ordered.len(), start, stop) {
                    Some((from, to)) => scored(
                        ordered.into_iter().skip(from).take(to - from + 1).collect(),
                        with_scores,
                    ),
                    None => Reply::Array(Some(Vec::new())),
                })
            })
        }
        ("zrevrangebyscore", count) if count >= 3 => {
            let (high, low) = match (score_bound(&args[1]), score_bound(&args[2])) {
                (Ok(high), Ok(low)) => (high, low),
                (Err(error), _) | (_, Err(error)) => return Some(Reply::Error(error)),
            };
            let (with_scores, limit) = match paging(&args[3..], true) {
                Ok(found) => found,
                Err(error) => return Some(Reply::Error(error)),
            };
            reading(context, db, |keys| {
                let mut ordered = sorted_in(load(keys, &args[0], now)?.as_ref())?.ordered();
                ordered.reverse();
                let selected = ordered
                    .into_iter()
                    .filter(|(score, _)| above(*score, low) && below(*score, high))
                    .collect();
                Ok(scored(paged(selected, limit), with_scores))
            })
        }
        ("zrangebylex" | "zrevrangebylex" | "zlexcount", count)
            if count >= 3 && (name != "zlexcount" || count == 3) =>
        {
            let reverse = name == "zrevrangebylex";
            let (first, second) = match (lex_bound(&args[1]), lex_bound(&args[2])) {
                (Ok(first), Ok(second)) => (first, second),
                (Err(error), _) | (_, Err(error)) => return Some(Reply::Error(error)),
            };
            let (low, high) = if reverse { (second, first) } else { (first, second) };
            let limit = match paging(&args[3..], false) {
                Ok((_, limit)) => limit,
                Err(error) => return Some(Reply::Error(error)),
            };
            reading(context, db, |keys| {
                let mut ordered = sorted_in(load(keys, &args[0], now)?.as_ref())?.ordered();
                ordered.sort_by(|left, right| left.1.cmp(&right.1));
                if reverse {
                    ordered.reverse();
                }
                let selected: Vec<(f64, Vec<u8>)> = ordered
                    .into_iter()
                    .filter(|(_, member)| lex_above(member, &low) && lex_below(member, &high))
                    .collect();
                Ok(if name == "zlexcount" {
                    Reply::Integer(selected.len() as i64)
                } else {
                    scored(paged(selected, limit), false)
                })
            })
        }
        ("zremrangebyrank" | "zremrangebyscore" | "zremrangebylex", 3) => {
            enum Chosen {
                Rank(i64, i64),
                Score(ScoreBound, ScoreBound),
                Lex(LexBound, LexBound),
            }
            let chosen = match name {
                "zremrangebyrank" => match (integer(&args[1]), integer(&args[2])) {
                    (Ok(start), Ok(stop)) => Chosen::Rank(start, stop),
                    _ => return Some(Reply::Error(NOT_INTEGER.to_owned())),
                },
                "zremrangebyscore" => match (score_bound(&args[1]), score_bound(&args[2])) {
                    (Ok(low), Ok(high)) => Chosen::Score(low, high),
                    (Err(error), _) | (_, Err(error)) => return Some(Reply::Error(error)),
                },
                _ => match (lex_bound(&args[1]), lex_bound(&args[2])) {
                    (Ok(low), Ok(high)) => Chosen::Lex(low, high),
                    (Err(error), _) | (_, Err(error)) => return Some(Reply::Error(error)),
                },
            };
            writing(context, db, |keys| {
                let entry = load(keys, &args[0], now)?;
                let mut set = sorted_in(entry.as_ref())?;
                let ordered = set.ordered();
                let removed: Vec<Vec<u8>> = match &chosen {
                    Chosen::Rank(start, stop) => match range_of(ordered.len(), *start, *stop) {
                        Some((from, to)) => ordered[from..=to].iter().map(|(_, member)| member.clone()).collect(),
                        None => Vec::new(),
                    },
                    Chosen::Score(low, high) => ordered
                        .iter()
                        .filter(|(score, _)| above(*score, *low) && below(*score, *high))
                        .map(|(_, member)| member.clone())
                        .collect(),
                    Chosen::Lex(low, high) => ordered
                        .iter()
                        .filter(|(_, member)| lex_above(member, low) && lex_below(member, high))
                        .map(|(_, member)| member.clone())
                        .collect(),
                };
                for member in &removed {
                    set.remove(member);
                }
                if !removed.is_empty() {
                    store_sorted(keys, &args[0], set, expiry(entry.as_ref()))?;
                }
                Ok(Reply::Integer(removed.len() as i64))
            })
        }
        ("zrevrank", 2 | 3) => {
            let with_score = match args.get(2) {
                None => false,
                Some(option) if lower(option) == "withscore" => true,
                Some(_) => return Some(Reply::Error(SYNTAX.to_owned())),
            };
            reading(context, db, |keys| {
                let mut ordered = sorted_in(load(keys, &args[0], now)?.as_ref())?.ordered();
                ordered.reverse();
                Ok(match ordered.iter().position(|(_, member)| *member == args[1]) {
                    Some(rank) if with_score => Reply::Array(Some(vec![
                        Reply::Integer(rank as i64),
                        Reply::bulk(format_double(ordered[rank].0)),
                    ])),
                    Some(rank) => Reply::Integer(rank as i64),
                    None => Reply::null(),
                })
            })
        }
        ("spop", 1 | 2) => {
            let wanted = match args.get(1).map(|value| integer(value)) {
                None => None,
                Some(Ok(value)) if value >= 0 => Some(value),
                Some(_) => {
                    return Some(Reply::Error(
                        "ERR value is out of range, must be positive".to_owned(),
                    ));
                }
            };
            writing(context, db, |keys| {
                let entry = load(keys, &args[0], now)?;
                let mut set = sets_in(entry.as_ref())?;
                let members: Vec<Vec<u8>> = set.iter().cloned().collect();
                let chosen: Vec<Vec<u8>> = random_members(members.len(), Some(wanted.unwrap_or(1)), now)
                    .into_iter()
                    .map(|at| members[at].clone())
                    .collect();
                for member in &chosen {
                    set.remove(member);
                }
                if !chosen.is_empty() {
                    save(
                        keys,
                        &args[0],
                        &Entry {
                            data: Data::Set(set),
                            expires_at_ms: expiry(entry.as_ref()),
                        },
                    )?;
                }
                Ok(match wanted {
                    None => chosen.into_iter().next().map_or(Reply::Bulk(None), Reply::bulk),
                    Some(_) => Reply::Array(Some(chosen.into_iter().map(Reply::bulk).collect())),
                })
            })
        }
        ("linsert", 4) => {
            let before = match lower(&args[1]).as_str() {
                "before" => true,
                "after" => false,
                _ => return Some(Reply::Error(SYNTAX.to_owned())),
            };
            writing(context, db, |keys| {
                let entry = load(keys, &args[0], now)?;
                if entry.is_none() {
                    return Ok(Reply::Integer(0));
                }
                let mut list = list_in(entry.as_ref())?;
                let Some(at) = list.iter().position(|item| *item == args[2]) else {
                    return Ok(Reply::Integer(-1));
                };
                list.insert(if before { at } else { at + 1 }, args[3].clone());
                let length = list.len();
                save(
                    keys,
                    &args[0],
                    &Entry {
                        data: Data::List(list),
                        expires_at_ms: expiry(entry.as_ref()),
                    },
                )?;
                Ok(Reply::Integer(length as i64))
            })
        }
        ("lpushx" | "rpushx", count) if count >= 2 => writing(context, db, |keys| {
            let entry = load(keys, &args[0], now)?;
            if entry.is_none() {
                return Ok(Reply::Integer(0));
            }
            let mut list = list_in(entry.as_ref())?;
            for item in &args[1..] {
                if name == "lpushx" {
                    list.push_front(item.clone());
                } else {
                    list.push_back(item.clone());
                }
            }
            let length = list.len();
            save(
                keys,
                &args[0],
                &Entry {
                    data: Data::List(list),
                    expires_at_ms: expiry(entry.as_ref()),
                },
            )?;
            Ok(Reply::Integer(length as i64))
        }),
        ("time", 0) => {
            let since = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            Reply::Array(Some(vec![
                Reply::bulk(since.as_secs().to_string()),
                Reply::bulk(since.subsec_micros().to_string()),
            ]))
        }
        ("lastsave", 0) => Reply::Integer((now / 1000) as i64),
        ("config", count) if count >= 1 => config(args),
        _ => return None,
    })
}

fn config(args: &[Vec<u8>]) -> Reply {
    const KNOWN: [(&str, &str); 6] = [
        ("databases", "16"),
        ("maxmemory", "0"),
        ("maxmemory-policy", "noeviction"),
        ("appendonly", "yes"),
        ("save", ""),
        ("timeout", "0"),
    ];
    match (lower(&args[0]).as_str(), &args[1..]) {
        ("get", patterns) if !patterns.is_empty() => {
            let mut out = Vec::new();
            for (name, value) in KNOWN {
                if patterns
                    .iter()
                    .any(|pattern| crate::commands::glob(&pattern.to_ascii_lowercase(), name.as_bytes()))
                {
                    out.push(Reply::bulk(name));
                    out.push(Reply::bulk(value));
                }
            }
            Reply::Array(Some(out))
        }
        ("set", pairs) if !pairs.is_empty() && pairs.len() % 2 == 0 => Reply::Error(format!(
            "ERR CONFIG SET is not supported by EggDB for '{}'; set eggdb serve's options instead",
            String::from_utf8_lossy(&pairs[0])
        )),
        ("resetstat", []) => Reply::ok(),
        _ => Reply::Error(SYNTAX.to_owned()),
    }
}
