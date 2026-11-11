use crate::aggregates::{expiry, sorted_of};
use crate::commands::{
    Context, NOT_INTEGER, SYNTAX, WRONG_TYPE, float, glob, integer, load, reading, save, writing,
};
use crate::protocol::{Reply, format_double};
use crate::value::{Data, Entry, SortedSet};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_RANDOM_COUNT: i64 = 1 << 20;
const MAX_SETRANGE_BYTES: i64 = 512 << 20;

static RANDOM_STATE: AtomicU64 = AtomicU64::new(0x9e37_79b9_7f4a_7c15);

fn random_below(bound: usize, now: u64) -> usize {
    if bound == 0 {
        return 0;
    }
    let mut value = RANDOM_STATE.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed) ^ now;
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    (value % bound as u64) as usize
}

const NAMES: &[&str] = &[
    "getex",
    "setrange",
    "msetnx",
    "lcs",
    "touch",
    "expiretime",
    "pexpiretime",
    "randomkey",
    "hstrlen",
    "hincrbyfloat",
    "hrandfield",
    "hscan",
    "lpos",
    "smismember",
    "srandmember",
    "smove",
    "sintercard",
    "sinterstore",
    "sunionstore",
    "sdiffstore",
    "sscan",
    "zmscore",
    "zrandmember",
    "zscan",
    "zunion",
    "zinter",
    "zdiff",
    "zunionstore",
    "zinterstore",
    "zdiffstore",
    "zintercard",
];

pub(crate) fn is_known(name: &str) -> bool {
    NAMES.contains(&name)
}

fn lower(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_ascii_lowercase()
}

fn set_in(entry: Option<&Entry>) -> Result<BTreeSet<Vec<u8>>, String> {
    match entry.map(|entry| &entry.data) {
        None => Ok(BTreeSet::new()),
        Some(Data::Set(set)) => Ok(set.clone()),
        Some(_) => Err(WRONG_TYPE.to_owned()),
    }
}

fn hash_in(entry: Option<&Entry>) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, String> {
    match entry.map(|entry| &entry.data) {
        None => Ok(BTreeMap::new()),
        Some(Data::Hash(map)) => Ok(map.clone()),
        Some(_) => Err(WRONG_TYPE.to_owned()),
    }
}

fn string_in(entry: Option<&Entry>) -> Result<Option<Vec<u8>>, String> {
    match entry.map(|entry| &entry.data) {
        None => Ok(None),
        Some(Data::String(bytes)) => Ok(Some(bytes.clone())),
        Some(_) => Err(WRONG_TYPE.to_owned()),
    }
}

fn bulks(items: impl IntoIterator<Item = Vec<u8>>) -> Reply {
    Reply::Array(Some(items.into_iter().map(Reply::bulk).collect()))
}

fn numkeys(args: &[Vec<u8>], name: &str) -> Result<(Vec<Vec<u8>>, usize), Reply> {
    let count = integer(&args[0]).map_err(Reply::Error)?;
    if count <= 0 {
        return Err(Reply::Error(format!(
            "ERR at least 1 input key is needed for '{name}' command"
        )));
    }
    let count = count as usize;
    if args.len() < 1 + count {
        return Err(Reply::Error(SYNTAX.to_owned()));
    }
    Ok((args[1..=count].to_vec(), 1 + count))
}

pub(crate) fn execute(
    context: &Context<'_>,
    db: u32,
    name: &str,
    args: &[Vec<u8>],
) -> Option<Reply> {
    let now = context.now_ms;
    Some(match (name, args.len()) {
        ("getex", count) if count >= 1 => {
            let mut change: Option<Option<u64>> = None;
            let mut options = args[1..].iter();
            while let Some(option) = options.next() {
                let at = match lower(option).as_str() {
                    "persist" => {
                        change = Some(None);
                        continue;
                    }
                    word @ ("ex" | "px" | "exat" | "pxat") => {
                        let Some(Ok(amount)) = options.next().map(|value| integer(value)) else {
                            return Some(Reply::Error(SYNTAX.to_owned()));
                        };
                        if amount <= 0 {
                            return Some(Reply::Error(
                                "ERR invalid expire time in 'getex' command".to_owned(),
                            ));
                        }
                        match word {
                            "ex" => (now as i64).saturating_add(amount.saturating_mul(1000)),
                            "px" => (now as i64).saturating_add(amount),
                            "exat" => amount.saturating_mul(1000),
                            _ => amount,
                        }
                    }
                    _ => return Some(Reply::Error(SYNTAX.to_owned())),
                };
                change = Some(Some(at.max(0) as u64));
            }
            writing(context, db, |keys| {
                let entry = load(keys, &args[0], now)?;
                let value = string_in(entry.as_ref())?;
                if let (Some(change), Some(entry)) = (change, entry) {
                    save(
                        keys,
                        &args[0],
                        &Entry {
                            data: entry.data,
                            expires_at_ms: change,
                        },
                    )?;
                }
                Ok(Reply::Bulk(value))
            })
        }
        ("setrange", 3) => {
            let Ok(offset) = integer(&args[1]) else {
                return Some(Reply::Error(NOT_INTEGER.to_owned()));
            };
            if offset < 0 || offset.saturating_add(args[2].len() as i64) > MAX_SETRANGE_BYTES {
                return Some(Reply::Error("ERR offset is out of range".to_owned()));
            }
            writing(context, db, |keys| {
                let entry = load(keys, &args[0], now)?;
                let mut value = string_in(entry.as_ref())?.unwrap_or_default();
                if args[2].is_empty() {
                    return Ok(Reply::Integer(value.len() as i64));
                }
                let offset = offset as usize;
                let end = offset + args[2].len();
                if value.len() < end {
                    value.resize(end, 0);
                }
                value[offset..end].copy_from_slice(&args[2]);
                let length = value.len();
                save(
                    keys,
                    &args[0],
                    &Entry {
                        data: Data::String(value),
                        expires_at_ms: expiry(entry.as_ref()),
                    },
                )?;
                Ok(Reply::Integer(length as i64))
            })
        }
        ("msetnx", count) if count >= 2 && count % 2 == 0 => writing(context, db, |keys| {
            for pair in args.chunks(2) {
                if load(keys, &pair[0], now)?.is_some() {
                    return Ok(Reply::Integer(0));
                }
            }
            for pair in args.chunks(2) {
                save(
                    keys,
                    &pair[0],
                    &Entry {
                        data: Data::String(pair[1].clone()),
                        expires_at_ms: None,
                    },
                )?;
            }
            Ok(Reply::Integer(1))
        }),
        ("lcs", count) if count >= 2 => {
            let mut length_only = false;
            for option in &args[2..] {
                match lower(option).as_str() {
                    "len" => length_only = true,
                    _ => {
                        return Some(Reply::Error(
                            "ERR LCS takes LEN only in EggDB; IDX is not supported yet".to_owned(),
                        ));
                    }
                }
            }
            reading(context, db, |keys| {
                let left = string_in(load(keys, &args[0], now)?.as_ref())?.unwrap_or_default();
                let right = string_in(load(keys, &args[1], now)?.as_ref())?.unwrap_or_default();
                if left.len().saturating_mul(right.len()) > 64 << 20 {
                    return Err("ERR LCS inputs are too large".to_owned());
                }
                let common = longest_common(&left, &right);
                Ok(if length_only {
                    Reply::Integer(common.len() as i64)
                } else {
                    Reply::bulk(common)
                })
            })
        }
        ("touch", count) if count >= 1 => reading(context, db, |keys| {
            let mut found = 0;
            for key in args {
                if load(keys, key, now)?.is_some() {
                    found += 1;
                }
            }
            Ok(Reply::Integer(found))
        }),
        ("expiretime" | "pexpiretime", 1) => reading(context, db, |keys| {
            Ok(Reply::Integer(match load(keys, &args[0], now)? {
                None => -2,
                Some(Entry {
                    expires_at_ms: None,
                    ..
                }) => -1,
                Some(Entry {
                    expires_at_ms: Some(at),
                    ..
                }) => {
                    if name == "expiretime" {
                        (at / 1000) as i64
                    } else {
                        at as i64
                    }
                }
            }))
        }),
        ("randomkey", 0) => reading(context, db, |keys| {
            let all = keys.keys()?;
            let mut live = Vec::new();
            for key in all {
                if load(keys, &key, now)?.is_some() {
                    live.push(key);
                }
            }
            Ok(if live.is_empty() {
                Reply::Bulk(None)
            } else {
                Reply::bulk(live.swap_remove(random_below(live.len(), now)))
            })
        }),
        ("hstrlen", 2) => reading(context, db, |keys| {
            let map = hash_in(load(keys, &args[0], now)?.as_ref())?;
            Ok(Reply::Integer(map.get(&args[1]).map_or(0, Vec::len) as i64))
        }),
        ("hincrbyfloat", 3) => {
            let by = match float(&args[2]) {
                Ok(by) => by,
                Err(error) => return Some(Reply::Error(error)),
            };
            writing(context, db, |keys| {
                let entry = load(keys, &args[0], now)?;
                let mut map = hash_in(entry.as_ref())?;
                let current = match map.get(&args[1]) {
                    None => 0.0,
                    Some(bytes) => float(bytes)
                        .map_err(|_| "ERR hash value is not a float".to_owned())?,
                };
                let next = current + by;
                if !next.is_finite() {
                    return Err("ERR increment would produce NaN or Infinity".to_owned());
                }
                let text = format_double(next);
                map.insert(args[1].clone(), text.clone().into_bytes());
                save(
                    keys,
                    &args[0],
                    &Entry {
                        data: Data::Hash(map),
                        expires_at_ms: expiry(entry.as_ref()),
                    },
                )?;
                Ok(Reply::bulk(text))
            })
        }
        ("hrandfield", count) if (1..=3).contains(&count) => {
            let wanted = match args.get(1).map(|value| integer(value)) {
                None => None,
                Some(Ok(value)) if value.abs() <= MAX_RANDOM_COUNT => Some(value),
                Some(Ok(_)) => return Some(Reply::Error("ERR value is out of range".to_owned())),
                Some(Err(error)) => return Some(Reply::Error(error)),
            };
            let with_values = match args.get(2) {
                None => false,
                Some(option) if lower(option) == "withvalues" => true,
                Some(_) => return Some(Reply::Error(SYNTAX.to_owned())),
            };
            reading(context, db, |keys| {
                let pairs: Vec<(Vec<u8>, Vec<u8>)> =
                    hash_in(load(keys, &args[0], now)?.as_ref())?.into_iter().collect();
                let chosen = random_picks(pairs.len(), wanted, now);
                Ok(match wanted {
                    None => match chosen.first() {
                        Some(&at) => Reply::bulk(pairs[at].0.clone()),
                        None => Reply::Bulk(None),
                    },
                    Some(_) => {
                        let mut out = Vec::new();
                        for at in chosen {
                            out.push(Reply::bulk(pairs[at].0.clone()));
                            if with_values {
                                out.push(Reply::bulk(pairs[at].1.clone()));
                            }
                        }
                        Reply::Array(Some(out))
                    }
                })
            })
        }
        ("hscan" | "sscan" | "zscan", count) if count >= 2 => collection_scan(context, db, name, args),
        ("lpos", count) if count >= 2 => lpos(context, db, args),
        _ => return crate::more_commands_sets::execute(context, db, name, args),
    })
}

fn random_picks(len: usize, wanted: Option<i64>, now: u64) -> Vec<usize> {
    match wanted {
        None => {
            if len == 0 {
                Vec::new()
            } else {
                vec![random_below(len, now)]
            }
        }
        Some(count) if count >= 0 => {
            let mut indices: Vec<usize> = (0..len).collect();
            let take = (count as usize).min(len);
            for at in 0..take {
                let swap = at + random_below(len - at, now);
                indices.swap(at, swap);
            }
            indices.truncate(take);
            indices
        }
        Some(count) => {
            if len == 0 {
                return Vec::new();
            }
            (0..count.unsigned_abs() as usize)
                .map(|_| random_below(len, now))
                .collect()
        }
    }
}

pub(crate) fn random_members(len: usize, wanted: Option<i64>, now: u64) -> Vec<usize> {
    random_picks(len, wanted, now)
}

fn longest_common(left: &[u8], right: &[u8]) -> Vec<u8> {
    let mut table = vec![vec![0u32; right.len() + 1]; left.len() + 1];
    for (row, a) in left.iter().enumerate() {
        for (column, b) in right.iter().enumerate() {
            table[row + 1][column + 1] = if a == b {
                table[row][column] + 1
            } else {
                table[row][column + 1].max(table[row + 1][column])
            };
        }
    }
    let (mut row, mut column) = (left.len(), right.len());
    let mut out = Vec::new();
    while row > 0 && column > 0 {
        if left[row - 1] == right[column - 1] {
            out.push(left[row - 1]);
            row -= 1;
            column -= 1;
        } else if table[row - 1][column] >= table[row][column - 1] {
            row -= 1;
        } else {
            column -= 1;
        }
    }
    out.reverse();
    out
}

fn lpos(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    let (mut rank, mut count, mut max_length) = (1i64, None, 0usize);
    let mut options = args[2..].iter();
    while let Some(option) = options.next() {
        let Some(Ok(value)) = options.next().map(|value| integer(value)) else {
            return Reply::Error(SYNTAX.to_owned());
        };
        match lower(option).as_str() {
            "rank" if value != 0 && value != i64::MIN => rank = value,
            "rank" => {
                return Reply::Error(
                    "ERR RANK can't be zero: use 1 to start from the first match, 2 from the second ... or use negative to start from the end of the list".to_owned(),
                );
            }
            "count" if value >= 0 => count = Some(value as usize),
            "maxlen" if value >= 0 => max_length = value as usize,
            "count" | "maxlen" => return Reply::Error("ERR value is out of range".to_owned()),
            _ => return Reply::Error(SYNTAX.to_owned()),
        }
    }
    reading(context, db, |keys| {
        let list = match load(keys, &args[0], now)?.map(|entry| entry.data) {
            None => Default::default(),
            Some(Data::List(list)) => list,
            Some(_) => return Err(WRONG_TYPE.to_owned()),
        };
        let indices: Box<dyn Iterator<Item = usize>> = if rank > 0 {
            Box::new(0..list.len())
        } else {
            Box::new((0..list.len()).rev())
        };
        let limit = if max_length == 0 { list.len() } else { max_length };
        let mut skip = rank.unsigned_abs() as usize - 1;
        let wanted = match count {
            Some(0) => usize::MAX,
            Some(count) => count,
            None => 1,
        };
        let mut found = Vec::new();
        for at in indices.take(limit) {
            if list[at] == args[1] {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                found.push(at as i64);
                if found.len() >= wanted {
                    break;
                }
            }
        }
        Ok(match count {
            None => found.first().map_or(Reply::Bulk(None), |&at| Reply::Integer(at)),
            Some(_) => Reply::Array(Some(found.into_iter().map(Reply::Integer).collect())),
        })
    })
}

fn collection_scan(context: &Context<'_>, db: u32, name: &str, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    let Ok(cursor) = integer(&args[1]) else {
        return Reply::Error("ERR invalid cursor".to_owned());
    };
    let (mut pattern, mut count) = (None, 10usize);
    let mut options = args[2..].iter();
    while let Some(option) = options.next() {
        match lower(option).as_str() {
            "match" => pattern = options.next().cloned(),
            "count" => match options.next().map(|value| integer(value)) {
                Some(Ok(value)) if value > 0 => count = value as usize,
                _ => return Reply::Error(SYNTAX.to_owned()),
            },
            "novalues" if name == "hscan" => {}
            _ => return Reply::Error(SYNTAX.to_owned()),
        }
    }
    let no_values = args[2..].iter().any(|option| lower(option) == "novalues");
    reading(context, db, |keys| {
        let entry = load(keys, &args[0], now)?;
        let items: Vec<(Vec<u8>, Option<Vec<u8>>)> = match (name, entry.map(|entry| entry.data)) {
            (_, None) => Vec::new(),
            ("hscan", Some(Data::Hash(map))) => map
                .into_iter()
                .map(|(field, value)| (field, (!no_values).then_some(value)))
                .collect(),
            ("sscan", Some(Data::Set(set))) => set.into_iter().map(|member| (member, None)).collect(),
            ("zscan", Some(Data::SortedSet(set))) => set
                .ordered()
                .into_iter()
                .map(|(score, member)| (member, Some(format_double(score).into_bytes())))
                .collect(),
            _ => return Err(WRONG_TYPE.to_owned()),
        };
        let start = cursor.max(0) as usize;
        let end = start.saturating_add(count).min(items.len());
        let next = if end >= items.len() { 0 } else { end };
        let mut out = Vec::new();
        for (member, value) in items.get(start..end).unwrap_or_default() {
            if pattern.as_ref().is_some_and(|pattern| !glob(pattern, member)) {
                continue;
            }
            out.push(Reply::bulk(member.clone()));
            if let Some(value) = value {
                out.push(Reply::bulk(value.clone()));
            }
        }
        Ok(Reply::Array(Some(vec![
            Reply::bulk(next.to_string()),
            Reply::Array(Some(out)),
        ])))
    })
}

pub(crate) fn sorted_in(entry: Option<&Entry>) -> Result<SortedSet, String> {
    sorted_of(entry)
}

pub(crate) fn sets_in(entry: Option<&Entry>) -> Result<BTreeSet<Vec<u8>>, String> {
    set_in(entry)
}

pub(crate) fn bulk_list(items: impl IntoIterator<Item = Vec<u8>>) -> Reply {
    bulks(items)
}

pub(crate) fn input_keys(args: &[Vec<u8>], name: &str) -> Result<(Vec<Vec<u8>>, usize), Reply> {
    numkeys(args, name)
}
