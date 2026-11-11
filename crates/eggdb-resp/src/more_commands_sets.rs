use crate::aggregates::expiry;
use crate::commands::{Context, SYNTAX, float, integer, load, reading, save, writing};
use crate::more_commands::{bulk_list, input_keys, random_members, sets_in, sorted_in};
use crate::protocol::{Reply, format_double};
use crate::value::{Data, Entry, SortedSet};
use eggdb_keyspace::KeysRead;
use std::collections::{BTreeMap, BTreeSet};

const MAX_RANDOM_COUNT: i64 = 1 << 20;

fn lower(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_ascii_lowercase()
}

#[derive(Clone, Copy)]
enum SetOperation {
    Inter,
    Union,
    Diff,
}

fn combined_sets(
    keys: &dyn KeysRead,
    names: &[Vec<u8>],
    operation: SetOperation,
    now: u64,
) -> Result<BTreeSet<Vec<u8>>, String> {
    let mut sets = Vec::with_capacity(names.len());
    for name in names {
        sets.push(sets_in(load(keys, name, now)?.as_ref())?);
    }
    let mut iterator = sets.into_iter();
    let first = iterator.next().unwrap_or_default();
    Ok(iterator.fold(first, |held, next| match operation {
        SetOperation::Inter => held.intersection(&next).cloned().collect(),
        SetOperation::Union => held.union(&next).cloned().collect(),
        SetOperation::Diff => held.difference(&next).cloned().collect(),
    }))
}

#[derive(Clone, Copy)]
enum Aggregate {
    Sum,
    Min,
    Max,
}

fn combined_sorted(
    keys: &dyn KeysRead,
    names: &[Vec<u8>],
    weights: &[f64],
    aggregate: Aggregate,
    operation: SetOperation,
    now: u64,
) -> Result<SortedSet, String> {
    let mut sets: Vec<BTreeMap<Vec<u8>, f64>> = Vec::with_capacity(names.len());
    for (at, name) in names.iter().enumerate() {
        let entry = load(keys, name, now)?;
        let weight = weights.get(at).copied().unwrap_or(1.0);
        let scored: BTreeMap<Vec<u8>, f64> = match entry.as_ref().map(|entry| &entry.data) {
            Some(Data::Set(set)) => set.iter().map(|member| (member.clone(), weight)).collect(),
            _ => sorted_in(entry.as_ref())?
                .ordered()
                .into_iter()
                .map(|(score, member)| {
                    let weighted = score * weight;
                    (member, if weighted.is_nan() { 0.0 } else { weighted })
                })
                .collect(),
        };
        sets.push(scored);
    }
    let mut iterator = sets.into_iter();
    let mut held = iterator.next().unwrap_or_default();
    for next in iterator {
        held = match operation {
            SetOperation::Diff => held
                .into_iter()
                .filter(|(member, _)| !next.contains_key(member))
                .collect(),
            SetOperation::Inter => held
                .into_iter()
                .filter_map(|(member, score)| {
                    next.get(&member)
                        .map(|other| (member, merged(score, *other, aggregate)))
                })
                .collect(),
            SetOperation::Union => {
                let mut joined = held;
                for (member, score) in next {
                    let value = match joined.get(&member) {
                        Some(existing) => merged(*existing, score, aggregate),
                        None => score,
                    };
                    joined.insert(member, value);
                }
                joined
            }
        };
    }
    let mut result = SortedSet::default();
    for (member, score) in held {
        result.insert(member, score);
    }
    Ok(result)
}

fn merged(left: f64, right: f64, aggregate: Aggregate) -> f64 {
    let value = match aggregate {
        Aggregate::Sum => left + right,
        Aggregate::Min => left.min(right),
        Aggregate::Max => left.max(right),
    };
    if value.is_nan() { 0.0 } else { value }
}

fn scored_reply(set: &SortedSet, with_scores: bool) -> Reply {
    let mut out = Vec::new();
    for (score, member) in set.ordered() {
        out.push(Reply::bulk(member));
        if with_scores {
            out.push(Reply::bulk(format_double(score)));
        }
    }
    Reply::Array(Some(out))
}

struct SortedOptions {
    weights: Vec<f64>,
    aggregate: Aggregate,
    with_scores: bool,
}

fn sorted_options(rest: &[Vec<u8>], inputs: usize, allow_scores: bool) -> Result<SortedOptions, Reply> {
    let mut options = SortedOptions {
        weights: Vec::new(),
        aggregate: Aggregate::Sum,
        with_scores: false,
    };
    let mut at = 0;
    while at < rest.len() {
        match lower(&rest[at]).as_str() {
            "weights" => {
                let given = rest.get(at + 1..at + 1 + inputs).ok_or_else(|| Reply::Error(SYNTAX.to_owned()))?;
                options.weights = given
                    .iter()
                    .map(|weight| {
                        float(weight).map_err(|_| {
                            Reply::Error("ERR weight value is not a float".to_owned())
                        })
                    })
                    .collect::<Result<_, _>>()?;
                at += 1 + inputs;
            }
            "aggregate" => {
                options.aggregate = match rest.get(at + 1).map(|value| lower(value)).as_deref() {
                    Some("sum") => Aggregate::Sum,
                    Some("min") => Aggregate::Min,
                    Some("max") => Aggregate::Max,
                    _ => return Err(Reply::Error(SYNTAX.to_owned())),
                };
                at += 2;
            }
            "withscores" if allow_scores => {
                options.with_scores = true;
                at += 1;
            }
            _ => return Err(Reply::Error(SYNTAX.to_owned())),
        }
    }
    Ok(options)
}

fn limit_option(rest: &[Vec<u8>]) -> Result<usize, Reply> {
    match rest {
        [] => Ok(0),
        [word, value] if lower(word) == "limit" => match integer(value) {
            Ok(limit) if limit >= 0 => Ok(limit as usize),
            _ => Err(Reply::Error("ERR LIMIT can't be negative".to_owned())),
        },
        _ => Err(Reply::Error(SYNTAX.to_owned())),
    }
}

pub(crate) fn execute(
    context: &Context<'_>,
    db: u32,
    name: &str,
    args: &[Vec<u8>],
) -> Option<Reply> {
    let now = context.now_ms;
    Some(match (name, args.len()) {
        ("smismember", count) if count >= 2 => reading(context, db, |keys| {
            let set = sets_in(load(keys, &args[0], now)?.as_ref())?;
            Ok(Reply::Array(Some(
                args[1..]
                    .iter()
                    .map(|member| Reply::Integer(i64::from(set.contains(member))))
                    .collect(),
            )))
        }),
        ("srandmember", 1 | 2) => {
            let wanted = match args.get(1).map(|value| integer(value)) {
                None => None,
                Some(Ok(value)) if value.abs() <= MAX_RANDOM_COUNT => Some(value),
                Some(Ok(_)) => return Some(Reply::Error("ERR value is out of range".to_owned())),
                Some(Err(error)) => return Some(Reply::Error(error)),
            };
            reading(context, db, |keys| {
                let members: Vec<Vec<u8>> =
                    sets_in(load(keys, &args[0], now)?.as_ref())?.into_iter().collect();
                let chosen = random_members(members.len(), wanted, now);
                Ok(match wanted {
                    None => chosen
                        .first()
                        .map_or(Reply::Bulk(None), |&at| Reply::bulk(members[at].clone())),
                    Some(_) => bulk_list(chosen.into_iter().map(|at| members[at].clone())),
                })
            })
        }
        ("smove", 3) => writing(context, db, |keys| {
            let source = load(keys, &args[0], now)?;
            let mut from = sets_in(source.as_ref())?;
            let target = load(keys, &args[1], now)?;
            let mut to = sets_in(target.as_ref())?;
            if !from.remove(&args[2]) {
                return Ok(Reply::Integer(0));
            }
            if args[0] == args[1] {
                return Ok(Reply::Integer(1));
            }
            to.insert(args[2].clone());
            save(
                keys,
                &args[0],
                &Entry {
                    data: Data::Set(from),
                    expires_at_ms: expiry(source.as_ref()),
                },
            )?;
            save(
                keys,
                &args[1],
                &Entry {
                    data: Data::Set(to),
                    expires_at_ms: expiry(target.as_ref()),
                },
            )?;
            Ok(Reply::Integer(1))
        }),
        ("sintercard", count) if count >= 2 => {
            let (inputs, used) = match input_keys(args, name) {
                Ok(found) => found,
                Err(reply) => return Some(reply),
            };
            let limit = match limit_option(&args[used..]) {
                Ok(limit) => limit,
                Err(reply) => return Some(reply),
            };
            reading(context, db, |keys| {
                let found = combined_sets(keys, &inputs, SetOperation::Inter, now)?.len();
                Ok(Reply::Integer(if limit == 0 { found } else { found.min(limit) } as i64))
            })
        }
        ("sinterstore" | "sunionstore" | "sdiffstore", count) if count >= 2 => {
            let operation = match name {
                "sinterstore" => SetOperation::Inter,
                "sunionstore" => SetOperation::Union,
                _ => SetOperation::Diff,
            };
            writing(context, db, |keys| {
                let set = combined_sets(&*keys, &args[1..], operation, now)?;
                let length = set.len();
                if set.is_empty() {
                    keys.delete(&args[0])?;
                } else {
                    save(
                        keys,
                        &args[0],
                        &Entry {
                            data: Data::Set(set),
                            expires_at_ms: None,
                        },
                    )?;
                }
                Ok(Reply::Integer(length as i64))
            })
        }
        ("zmscore", count) if count >= 2 => reading(context, db, |keys| {
            let set = sorted_in(load(keys, &args[0], now)?.as_ref())?;
            Ok(Reply::Array(Some(
                args[1..]
                    .iter()
                    .map(|member| match set.score(member) {
                        Some(score) => Reply::bulk(format_double(score)),
                        None => Reply::Bulk(None),
                    })
                    .collect(),
            )))
        }),
        ("zrandmember", count) if (1..=3).contains(&count) => {
            let wanted = match args.get(1).map(|value| integer(value)) {
                None => None,
                Some(Ok(value)) if value.abs() <= MAX_RANDOM_COUNT => Some(value),
                Some(Ok(_)) => return Some(Reply::Error("ERR value is out of range".to_owned())),
                Some(Err(error)) => return Some(Reply::Error(error)),
            };
            let with_scores = match args.get(2) {
                None => false,
                Some(option) if lower(option) == "withscores" => true,
                Some(_) => return Some(Reply::Error(SYNTAX.to_owned())),
            };
            reading(context, db, |keys| {
                let members = sorted_in(load(keys, &args[0], now)?.as_ref())?.ordered();
                let chosen = random_members(members.len(), wanted, now);
                Ok(match wanted {
                    None => chosen
                        .first()
                        .map_or(Reply::Bulk(None), |&at| Reply::bulk(members[at].1.clone())),
                    Some(_) => {
                        let mut out = Vec::new();
                        for at in chosen {
                            out.push(Reply::bulk(members[at].1.clone()));
                            if with_scores {
                                out.push(Reply::bulk(format_double(members[at].0)));
                            }
                        }
                        Reply::Array(Some(out))
                    }
                })
            })
        }
        ("zunion" | "zinter" | "zdiff", count) if count >= 2 => {
            let (inputs, used) = match input_keys(args, name) {
                Ok(found) => found,
                Err(reply) => return Some(reply),
            };
            let options = match sorted_options(&args[used..], inputs.len(), true) {
                Ok(options) => options,
                Err(reply) => return Some(reply),
            };
            let operation = match name {
                "zunion" => SetOperation::Union,
                "zinter" => SetOperation::Inter,
                _ => SetOperation::Diff,
            };
            reading(context, db, |keys| {
                let set = combined_sorted(keys, &inputs, &options.weights, options.aggregate, operation, now)?;
                Ok(scored_reply(&set, options.with_scores))
            })
        }
        ("zunionstore" | "zinterstore" | "zdiffstore", count) if count >= 3 => {
            let (inputs, used) = match input_keys(&args[1..], name) {
                Ok(found) => found,
                Err(reply) => return Some(reply),
            };
            let options = match sorted_options(&args[1 + used..], inputs.len(), false) {
                Ok(options) => options,
                Err(reply) => return Some(reply),
            };
            let operation = match name {
                "zunionstore" => SetOperation::Union,
                "zinterstore" => SetOperation::Inter,
                _ => SetOperation::Diff,
            };
            writing(context, db, |keys| {
                let set = combined_sorted(&*keys, &inputs, &options.weights, options.aggregate, operation, now)?;
                let length = set.len();
                if set.is_empty() {
                    keys.delete(&args[0])?;
                } else {
                    save(
                        keys,
                        &args[0],
                        &Entry {
                            data: Data::SortedSet(set),
                            expires_at_ms: None,
                        },
                    )?;
                }
                Ok(Reply::Integer(length as i64))
            })
        }
        ("zintercard", count) if count >= 2 => {
            let (inputs, used) = match input_keys(args, name) {
                Ok(found) => found,
                Err(reply) => return Some(reply),
            };
            let limit = match limit_option(&args[used..]) {
                Ok(limit) => limit,
                Err(reply) => return Some(reply),
            };
            reading(context, db, |keys| {
                let found = combined_sorted(keys, &inputs, &[], Aggregate::Sum, SetOperation::Inter, now)?.len();
                Ok(Reply::Integer(if limit == 0 { found } else { found.min(limit) } as i64))
            })
        }
        _ => return None,
    })
}
