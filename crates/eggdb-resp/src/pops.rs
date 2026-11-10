use crate::commands::{
    Context, NOT_INTEGER, SYNTAX, WRONG_TYPE, bulk_reply_bytes, integer, load, save, writing,
};
use crate::keyspace::{KeysRead, KeysWrite};
use crate::protocol::Reply;
use crate::value::{Data, Entry, SortedSet};
use std::collections::VecDeque;

const NAMES: &[&str] = &["lmove", "rpoplpush", "zpopmin", "zpopmax", "lmpop", "zmpop"];
const SCORE_REPLY_BYTES: usize = 64;
const COUNT_NOT_POSITIVE: &str = "ERR value is out of range, must be positive";
const NUMKEYS_NOT_POSITIVE: &str = "ERR numkeys should be greater than 0";
const MANY_COUNT_NOT_POSITIVE: &str = "ERR count should be greater than 0";

pub(crate) type ListItems = Vec<Vec<u8>>;
pub(crate) type ScoredMembers = Vec<(Vec<u8>, f64)>;
pub(crate) type KeyedItems = (Vec<u8>, ListItems);
pub(crate) type KeyedMembers = (Vec<u8>, ScoredMembers);
type StoredList = (VecDeque<Vec<u8>>, Option<u64>);
type StoredSorted = (SortedSet, Option<u64>);

pub(crate) fn is_known(name: &str) -> bool {
    NAMES.contains(&name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListEnd {
    Left,
    Right,
}

impl ListEnd {
    pub(crate) fn named(word: &[u8]) -> Result<Self, String> {
        if word.eq_ignore_ascii_case(b"left") {
            Ok(Self::Left)
        } else if word.eq_ignore_ascii_case(b"right") {
            Ok(Self::Right)
        } else {
            Err(SYNTAX.to_owned())
        }
    }

    fn take(self, list: &mut VecDeque<Vec<u8>>) -> Option<Vec<u8>> {
        match self {
            Self::Left => list.pop_front(),
            Self::Right => list.pop_back(),
        }
    }

    fn put(self, list: &mut VecDeque<Vec<u8>>, item: Vec<u8>) {
        match self {
            Self::Left => list.push_front(item),
            Self::Right => list.push_back(item),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScoreEnd {
    Lowest,
    Highest,
}

impl ScoreEnd {
    pub(crate) fn named(word: &[u8]) -> Result<Self, String> {
        if word.eq_ignore_ascii_case(b"min") {
            Ok(Self::Lowest)
        } else if word.eq_ignore_ascii_case(b"max") {
            Ok(Self::Highest)
        } else {
            Err(SYNTAX.to_owned())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnWrongType {
    Refuse,
    Wait,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManyPop<End> {
    pub(crate) keys: Vec<Vec<u8>>,
    pub(crate) end: End,
    pub(crate) count: usize,
}

pub(crate) fn parse_many_pop<End>(
    args: &[Vec<u8>],
    end_named: fn(&[u8]) -> Result<End, String>,
) -> Result<ManyPop<End>, String> {
    let numkeys = match args.first().map(|numkeys| integer(numkeys)) {
        Some(Ok(numkeys)) if numkeys >= 1 => usize::try_from(numkeys).unwrap_or(usize::MAX),
        _ => return Err(NUMKEYS_NOT_POSITIVE.to_owned()),
    };
    let Some(end_at) = numkeys.checked_add(1).filter(|end_at| *end_at < args.len()) else {
        return Err(SYNTAX.to_owned());
    };
    let end = end_named(&args[end_at])?;
    let mut count = None;
    let mut at = end_at + 1;
    while let Some(option) = args.get(at) {
        match args.get(at + 1) {
            Some(value) if count.is_none() && option.eq_ignore_ascii_case(b"count") => {
                count = Some(match integer(value) {
                    Ok(count) if count >= 1 => usize::try_from(count).unwrap_or(usize::MAX),
                    _ => return Err(MANY_COUNT_NOT_POSITIVE.to_owned()),
                });
                at += 2;
            }
            _ => return Err(SYNTAX.to_owned()),
        }
    }
    Ok(ManyPop {
        keys: args[1..end_at].to_vec(),
        end,
        count: count.unwrap_or(1),
    })
}

fn stored_list(
    keys: &dyn KeysRead,
    key: &[u8],
    now: u64,
    on_wrong_type: OnWrongType,
) -> Result<Option<StoredList>, String> {
    match load(keys, key, now)? {
        None => Ok(None),
        Some(Entry {
            data: Data::List(list),
            expires_at_ms,
        }) => Ok((!list.is_empty()).then_some((list, expires_at_ms))),
        Some(_) => match on_wrong_type {
            OnWrongType::Refuse => Err(WRONG_TYPE.to_owned()),
            OnWrongType::Wait => Ok(None),
        },
    }
}

fn stored_sorted(
    keys: &dyn KeysRead,
    key: &[u8],
    now: u64,
    on_wrong_type: OnWrongType,
) -> Result<Option<StoredSorted>, String> {
    match load(keys, key, now)? {
        None => Ok(None),
        Some(Entry {
            data: Data::SortedSet(set),
            expires_at_ms,
        }) => Ok((!set.is_empty()).then_some((set, expires_at_ms))),
        Some(_) => match on_wrong_type {
            OnWrongType::Refuse => Err(WRONG_TYPE.to_owned()),
            OnWrongType::Wait => Ok(None),
        },
    }
}

pub(crate) fn any_list_to_pop(
    keys: &dyn KeysRead,
    list_keys: &[Vec<u8>],
    now: u64,
    on_wrong_type: OnWrongType,
) -> Result<bool, String> {
    for key in list_keys {
        if stored_list(keys, key, now, on_wrong_type)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn any_sorted_to_pop(
    keys: &dyn KeysRead,
    sorted_keys: &[Vec<u8>],
    now: u64,
    on_wrong_type: OnWrongType,
) -> Result<bool, String> {
    for key in sorted_keys {
        if stored_sorted(keys, key, now, on_wrong_type)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn save_list(
    keys: &mut dyn KeysWrite,
    key: &[u8],
    list: VecDeque<Vec<u8>>,
    expires_at_ms: Option<u64>,
) -> Result<(), String> {
    save(
        keys,
        key,
        &Entry {
            data: Data::List(list),
            expires_at_ms,
        },
    )
}

pub(crate) fn pop_list(
    keys: &mut dyn KeysWrite,
    key: &[u8],
    end: ListEnd,
    count: usize,
    context: &Context<'_>,
    on_wrong_type: OnWrongType,
) -> Result<Option<ListItems>, String> {
    let Some((mut list, expires_at_ms)) = stored_list(keys, key, context.now_ms, on_wrong_type)?
    else {
        return Ok(None);
    };
    let mut popped = Vec::with_capacity(count.min(list.len()));
    let mut reply_bytes = 0usize;
    while popped.len() < count {
        let Some(item) = end.take(&mut list) else {
            break;
        };
        reply_bytes = reply_bytes.saturating_add(bulk_reply_bytes(&item));
        context.check_reply_room(reply_bytes)?;
        popped.push(item);
    }
    if !popped.is_empty() {
        save_list(keys, key, list, expires_at_ms)?;
    }
    Ok(Some(popped))
}

pub(crate) fn pop_first_list(
    keys: &mut dyn KeysWrite,
    list_keys: &[Vec<u8>],
    end: ListEnd,
    count: usize,
    context: &Context<'_>,
    on_wrong_type: OnWrongType,
) -> Result<Option<KeyedItems>, String> {
    for key in list_keys {
        if let Some(popped) = pop_list(keys, key, end, count, context, on_wrong_type)? {
            return Ok(Some((key.clone(), popped)));
        }
    }
    Ok(None)
}

pub(crate) fn move_list_item(
    keys: &mut dyn KeysWrite,
    source: &[u8],
    destination: &[u8],
    from: ListEnd,
    to: ListEnd,
    now: u64,
    on_wrong_type: OnWrongType,
) -> Result<Option<Vec<u8>>, String> {
    let Some((mut list, source_expiry)) = stored_list(keys, source, now, on_wrong_type)? else {
        return Ok(None);
    };
    if source == destination {
        let Some(item) = from.take(&mut list) else {
            return Ok(None);
        };
        to.put(&mut list, item.clone());
        save_list(keys, source, list, source_expiry)?;
        return Ok(Some(item));
    }
    let (mut target, target_expiry) =
        stored_list(keys, destination, now, OnWrongType::Refuse)?.unwrap_or_default();
    let Some(item) = from.take(&mut list) else {
        return Ok(None);
    };
    to.put(&mut target, item.clone());
    save_list(keys, source, list, source_expiry)?;
    save_list(keys, destination, target, target_expiry)?;
    Ok(Some(item))
}

pub(crate) fn pop_sorted(
    keys: &mut dyn KeysWrite,
    key: &[u8],
    end: ScoreEnd,
    count: usize,
    context: &Context<'_>,
    on_wrong_type: OnWrongType,
) -> Result<Option<ScoredMembers>, String> {
    let Some((mut set, expires_at_ms)) = stored_sorted(keys, key, context.now_ms, on_wrong_type)?
    else {
        return Ok(None);
    };
    let chosen = set.first(count, end == ScoreEnd::Highest);
    let mut popped = Vec::with_capacity(chosen.len());
    let mut reply_bytes = 0usize;
    for (score, member) in chosen {
        reply_bytes = reply_bytes
            .saturating_add(bulk_reply_bytes(&member))
            .saturating_add(SCORE_REPLY_BYTES);
        context.check_reply_room(reply_bytes)?;
        set.remove(&member);
        popped.push((member, score));
    }
    if !popped.is_empty() {
        save(
            keys,
            key,
            &Entry {
                data: Data::SortedSet(set),
                expires_at_ms,
            },
        )?;
    }
    Ok(Some(popped))
}

pub(crate) fn pop_first_sorted(
    keys: &mut dyn KeysWrite,
    sorted_keys: &[Vec<u8>],
    end: ScoreEnd,
    count: usize,
    context: &Context<'_>,
    on_wrong_type: OnWrongType,
) -> Result<Option<KeyedMembers>, String> {
    for key in sorted_keys {
        if let Some(popped) = pop_sorted(keys, key, end, count, context, on_wrong_type)? {
            return Ok(Some((key.clone(), popped)));
        }
    }
    Ok(None)
}

pub(crate) fn list_many_reply(popped: Option<KeyedItems>) -> Reply {
    match popped {
        None => Reply::Array(None),
        Some((key, items)) => Reply::Array(Some(vec![
            Reply::bulk(key),
            Reply::Array(Some(items.into_iter().map(Reply::bulk).collect())),
        ])),
    }
}

pub(crate) fn sorted_many_reply(popped: Option<KeyedMembers>) -> Reply {
    match popped {
        None => Reply::Array(None),
        Some((key, members)) => Reply::Array(Some(vec![
            Reply::bulk(key),
            Reply::Array(Some(
                members
                    .into_iter()
                    .map(|(member, score)| {
                        Reply::Array(Some(vec![Reply::bulk(member), Reply::Double(score)]))
                    })
                    .collect(),
            )),
        ])),
    }
}

fn move_reply(
    context: &Context<'_>,
    db: u32,
    source: &[u8],
    destination: &[u8],
    from: ListEnd,
    to: ListEnd,
) -> Reply {
    writing(context, db, |keys| {
        Ok(Reply::Bulk(move_list_item(
            keys,
            source,
            destination,
            from,
            to,
            context.now_ms,
            OnWrongType::Refuse,
        )?))
    })
}

fn sorted_pop_reply(context: &Context<'_>, db: u32, end: ScoreEnd, args: &[Vec<u8>]) -> Reply {
    let wanted = match args.get(1).map(|count| integer(count)) {
        None => None,
        Some(Ok(count)) if count >= 0 => Some(usize::try_from(count).unwrap_or(usize::MAX)),
        Some(Ok(_)) => return Reply::Error(COUNT_NOT_POSITIVE.to_owned()),
        Some(Err(_)) => return Reply::Error(NOT_INTEGER.to_owned()),
    };
    writing(context, db, |keys| {
        let popped = pop_sorted(
            keys,
            &args[0],
            end,
            wanted.unwrap_or(1),
            context,
            OnWrongType::Refuse,
        )?
        .unwrap_or_default();
        Ok(match wanted {
            None => Reply::Array(Some(
                popped
                    .into_iter()
                    .flat_map(|(member, score)| [Reply::bulk(member), Reply::Double(score)])
                    .collect(),
            )),
            Some(_) => Reply::Pairs(
                popped
                    .into_iter()
                    .map(|(member, score)| (Reply::bulk(member), Reply::Double(score)))
                    .collect(),
            ),
        })
    })
}

pub(crate) fn execute(
    context: &Context<'_>,
    db: u32,
    name: &str,
    args: &[Vec<u8>],
) -> Option<Reply> {
    Some(match (name, args.len()) {
        ("lmove", 4) => match (ListEnd::named(&args[2]), ListEnd::named(&args[3])) {
            (Ok(from), Ok(to)) => move_reply(context, db, &args[0], &args[1], from, to),
            _ => Reply::Error(SYNTAX.to_owned()),
        },
        ("rpoplpush", 2) => move_reply(
            context,
            db,
            &args[0],
            &args[1],
            ListEnd::Right,
            ListEnd::Left,
        ),
        ("zpopmin", 1 | 2) => sorted_pop_reply(context, db, ScoreEnd::Lowest, args),
        ("zpopmax", 1 | 2) => sorted_pop_reply(context, db, ScoreEnd::Highest, args),
        ("zpopmin" | "zpopmax", count) if count > 2 => Reply::Error(SYNTAX.to_owned()),
        ("lmpop", count) if count >= 3 => match parse_many_pop(args, ListEnd::named) {
            Ok(pop) => writing(context, db, |keys| {
                Ok(list_many_reply(pop_first_list(
                    keys,
                    &pop.keys,
                    pop.end,
                    pop.count,
                    context,
                    OnWrongType::Refuse,
                )?))
            }),
            Err(error) => Reply::Error(error),
        },
        ("zmpop", count) if count >= 3 => match parse_many_pop(args, ScoreEnd::named) {
            Ok(pop) => writing(context, db, |keys| {
                Ok(sorted_many_reply(pop_first_sorted(
                    keys,
                    &pop.keys,
                    pop.end,
                    pop.count,
                    context,
                    OnWrongType::Refuse,
                )?))
            }),
            Err(error) => Reply::Error(error),
        },
        _ => return None,
    })
}
