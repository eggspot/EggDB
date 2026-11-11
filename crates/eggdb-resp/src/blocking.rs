use crate::commands::{Context, integer, reading, writing};
use crate::pops::{
    ListEnd, ManyPop, OnWrongType, ScoreEnd, any_list_to_pop, any_sorted_to_pop, list_many_reply,
    move_list_item, parse_many_pop, pop_first_list, pop_first_sorted, sorted_many_reply,
};
use crate::protocol::Reply;
use crate::stream_groups::{GroupRead, parse_xreadgroup, read_group};
use crate::streams::{StreamRead, parse_xread, read_new_entries};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

const TIMEOUT_NOT_FLOAT: &str = "ERR timeout is not a float or out of range";
const TIMEOUT_NOT_INTEGER: &str = "ERR timeout is not an integer or out of range";
const TIMEOUT_NEGATIVE: &str = "ERR timeout is negative";
const TIMEOUT_OUT_OF_RANGE: &str = "ERR timeout is out of range";

pub(crate) fn seconds_timeout(bytes: &[u8]) -> Result<u64, String> {
    let seconds: f64 = std::str::from_utf8(bytes)
        .ok()
        .filter(|text| !text.is_empty() && !text.starts_with(char::is_whitespace))
        .and_then(|text| text.parse().ok())
        .filter(|seconds: &f64| !seconds.is_nan())
        .ok_or_else(|| TIMEOUT_NOT_FLOAT.to_owned())?;
    let millis = (seconds * 1000.0).ceil();
    if millis > i64::MAX as f64 {
        return Err(TIMEOUT_OUT_OF_RANGE.to_owned());
    }
    if millis < 0.0 {
        return Err(TIMEOUT_NEGATIVE.to_owned());
    }
    Ok(millis as u64)
}

pub(crate) fn milliseconds_timeout(bytes: &[u8]) -> Result<u64, String> {
    let millis = integer(bytes).map_err(|_| TIMEOUT_NOT_INTEGER.to_owned())?;
    u64::try_from(millis).map_err(|_| TIMEOUT_NEGATIVE.to_owned())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Wanted {
    FirstOfLists {
        keys: Vec<Vec<u8>>,
        end: ListEnd,
    },
    Move {
        source: Vec<u8>,
        destination: Vec<u8>,
        from: ListEnd,
        to: ListEnd,
    },
    ManyOfLists(ManyPop<ListEnd>),
    FirstOfSorted {
        keys: Vec<Vec<u8>>,
        end: ScoreEnd,
    },
    ManyOfSorted(ManyPop<ScoreEnd>),
    Stream(StreamRead),
    Group(GroupRead),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlockingCommand {
    wanted: Wanted,
    timeout_ms: u64,
}

fn list_end_of(name: &str) -> ListEnd {
    if name.starts_with("bl") {
        ListEnd::Left
    } else {
        ListEnd::Right
    }
}

fn score_end_of(name: &str) -> ScoreEnd {
    if name.ends_with("min") {
        ScoreEnd::Lowest
    } else {
        ScoreEnd::Highest
    }
}

impl BlockingCommand {
    pub(crate) fn parse(name: &str, args: &[Vec<u8>]) -> Option<Result<Self, String>> {
        match name {
            "blpop" | "brpop" | "brpoplpush" | "blmove" | "blmpop" | "bzpopmin" | "bzpopmax"
            | "bzmpop" => Some(Self::parse_pop(name, args)),
            "xread" => {
                let read = parse_xread(args).ok()?;
                let timeout_ms = read.block_ms?;
                Some(Ok(Self {
                    wanted: Wanted::Stream(read),
                    timeout_ms,
                }))
            }
            "xreadgroup" => {
                let read = parse_xreadgroup(args).ok()?;
                let timeout_ms = read.block_ms?;
                Some(Ok(Self {
                    wanted: Wanted::Group(read),
                    timeout_ms,
                }))
            }
            _ => None,
        }
    }

    fn parse_pop(name: &str, args: &[Vec<u8>]) -> Result<Self, String> {
        let (wanted, timeout_ms) = match (name, args) {
            ("blpop" | "brpop", [keys @ .., timeout]) if !keys.is_empty() => (
                Wanted::FirstOfLists {
                    keys: keys.to_vec(),
                    end: list_end_of(name),
                },
                seconds_timeout(timeout)?,
            ),
            ("bzpopmin" | "bzpopmax", [keys @ .., timeout]) if !keys.is_empty() => (
                Wanted::FirstOfSorted {
                    keys: keys.to_vec(),
                    end: score_end_of(name),
                },
                seconds_timeout(timeout)?,
            ),
            ("brpoplpush", [source, destination, timeout]) => (
                Wanted::Move {
                    source: source.clone(),
                    destination: destination.clone(),
                    from: ListEnd::Right,
                    to: ListEnd::Left,
                },
                seconds_timeout(timeout)?,
            ),
            ("blmove", [source, destination, from, to, timeout]) => {
                let (from, to) = (ListEnd::named(from)?, ListEnd::named(to)?);
                (
                    Wanted::Move {
                        source: source.clone(),
                        destination: destination.clone(),
                        from,
                        to,
                    },
                    seconds_timeout(timeout)?,
                )
            }
            ("blmpop", [timeout, rest @ ..]) if rest.len() >= 3 => {
                let timeout_ms = seconds_timeout(timeout)?;
                (
                    Wanted::ManyOfLists(parse_many_pop(rest, ListEnd::named)?),
                    timeout_ms,
                )
            }
            ("bzmpop", [timeout, rest @ ..]) if rest.len() >= 3 => {
                let timeout_ms = seconds_timeout(timeout)?;
                (
                    Wanted::ManyOfSorted(parse_many_pop(rest, ScoreEnd::named)?),
                    timeout_ms,
                )
            }
            _ => {
                return Err(format!(
                    "ERR wrong number of arguments for '{name}' command"
                ));
            }
        };
        Ok(Self { wanted, timeout_ms })
    }

    pub(crate) fn keys(&self) -> &[Vec<u8>] {
        match &self.wanted {
            Wanted::FirstOfLists { keys, .. } | Wanted::FirstOfSorted { keys, .. } => keys,
            Wanted::Move { source, .. } => std::slice::from_ref(source),
            Wanted::ManyOfLists(pop) => &pop.keys,
            Wanted::ManyOfSorted(pop) => &pop.keys,
            Wanted::Stream(read) => &read.keys,
            Wanted::Group(read) => &read.keys,
        }
    }

    pub(crate) fn timeout(&self) -> Option<Duration> {
        (self.timeout_ms > 0).then_some(Duration::from_millis(self.timeout_ms))
    }

    fn consumes(&self) -> bool {
        !matches!(self.wanted, Wanted::Stream(_))
    }

    pub(crate) fn nothing_to_take(&self) -> Reply {
        match self.wanted {
            Wanted::Move { .. } => Reply::null(),
            _ => Reply::Array(None),
        }
    }

    fn may_take(&self, context: &Context<'_>, db: u32, on_wrong_type: OnWrongType) -> bool {
        let now = context.now_ms;
        let mut found = true;
        reading(context, db, |store| {
            found = match &self.wanted {
                Wanted::FirstOfLists { keys, .. } => {
                    any_list_to_pop(store, keys, now, on_wrong_type)?
                }
                Wanted::Move { source, .. } => {
                    any_list_to_pop(store, std::slice::from_ref(source), now, on_wrong_type)?
                }
                Wanted::ManyOfLists(pop) => any_list_to_pop(store, &pop.keys, now, on_wrong_type)?,
                Wanted::FirstOfSorted { keys, .. } => {
                    any_sorted_to_pop(store, keys, now, on_wrong_type)?
                }
                Wanted::ManyOfSorted(pop) => {
                    any_sorted_to_pop(store, &pop.keys, now, on_wrong_type)?
                }
                Wanted::Stream(_) | Wanted::Group(_) => true,
            };
            Ok(Reply::ok())
        });
        found
    }

    pub(crate) fn attempt(
        &mut self,
        context: &Context<'_>,
        db: u32,
        on_wrong_type: OnWrongType,
    ) -> Option<Reply> {
        if !self.may_take(context, db, on_wrong_type) {
            return None;
        }
        let mut taken = None;
        let outcome = match &mut self.wanted {
            Wanted::FirstOfLists { keys, end } => writing(context, db, |store| {
                taken = pop_first_list(store, keys, *end, 1, context, on_wrong_type)?.and_then(
                    |(key, items)| {
                        items.into_iter().next().map(|item| {
                            Reply::Array(Some(vec![Reply::bulk(key), Reply::bulk(item)]))
                        })
                    },
                );
                Ok(Reply::ok())
            }),
            Wanted::Move {
                source,
                destination,
                from,
                to,
            } => writing(context, db, |store| {
                taken = move_list_item(
                    store,
                    source,
                    destination,
                    *from,
                    *to,
                    context.now_ms,
                    on_wrong_type,
                )?
                .map(Reply::bulk);
                Ok(Reply::ok())
            }),
            Wanted::ManyOfLists(pop) => writing(context, db, |store| {
                taken =
                    pop_first_list(store, &pop.keys, pop.end, pop.count, context, on_wrong_type)?
                        .map(|popped| list_many_reply(Some(popped)));
                Ok(Reply::ok())
            }),
            Wanted::FirstOfSorted { keys, end } => writing(context, db, |store| {
                taken = pop_first_sorted(store, keys, *end, 1, context, on_wrong_type)?.and_then(
                    |(key, members)| {
                        members.into_iter().next().map(|(member, score)| {
                            Reply::Array(Some(vec![
                                Reply::bulk(key),
                                Reply::bulk(member),
                                Reply::Double(score),
                            ]))
                        })
                    },
                );
                Ok(Reply::ok())
            }),
            Wanted::ManyOfSorted(pop) => writing(context, db, |store| {
                taken =
                    pop_first_sorted(store, &pop.keys, pop.end, pop.count, context, on_wrong_type)?
                        .map(|popped| sorted_many_reply(Some(popped)));
                Ok(Reply::ok())
            }),
            Wanted::Stream(read) => reading(context, db, |store| {
                taken = read_new_entries(store, read, context, on_wrong_type)?;
                Ok(Reply::ok())
            }),
            Wanted::Group(read) => writing(context, db, |store| {
                taken = read_group(store, read, context, on_wrong_type)?;
                Ok(Reply::ok())
            }),
        };
        match outcome {
            Reply::Error(_) => Some(outcome),
            _ => taken,
        }
    }
}

pub(crate) fn execute(
    context: &Context<'_>,
    db: u32,
    name: &str,
    args: &[Vec<u8>],
) -> Option<Reply> {
    if name == "xread" {
        return None;
    }
    let mut command = match BlockingCommand::parse(name, args)? {
        Ok(command) => command,
        Err(error) => return Some(Reply::Error(error)),
    };
    Some(
        command
            .attempt(context, db, OnWrongType::Refuse)
            .unwrap_or_else(|| command.nothing_to_take()),
    )
}

type Slot = (u32, Vec<u8>);

#[derive(Debug)]
struct Waiter {
    generation: u64,
    consumes: bool,
}

#[derive(Debug, Default)]
struct Waiters {
    next_id: u64,
    queues: HashMap<Slot, VecDeque<u64>>,
    waiters: HashMap<u64, Waiter>,
}

impl Waiters {
    fn generation(&self, waiter: u64) -> Option<u64> {
        self.waiters.get(&waiter).map(|state| state.generation)
    }

    fn wake(&mut self, waiter: u64) -> bool {
        match self.waiters.get_mut(&waiter) {
            Some(state) => {
                state.generation = state.generation.wrapping_add(1);
                true
            }
            None => false,
        }
    }

    fn wake_from(&mut self, slot: &Slot, start: usize) -> bool {
        let Some(queue) = self.queues.get(slot) else {
            return false;
        };
        let mut chosen = Vec::new();
        for waiter in queue.iter().skip(start) {
            chosen.push(*waiter);
            if self.waiters.get(waiter).is_some_and(|state| state.consumes) {
                break;
            }
        }
        let mut woke = false;
        for waiter in chosen {
            woke |= self.wake(waiter);
        }
        woke
    }

    fn wake_next_after(&mut self, waiter: u64, slot: &Slot) -> bool {
        let start = self
            .queues
            .get(slot)
            .and_then(|queue| queue.iter().position(|queued| *queued == waiter))
            .map(|at| at + 1);
        start.is_some_and(|start| self.wake_from(slot, start))
    }
}

#[derive(Debug, Default)]
pub(crate) struct WaiterRegistry {
    state: Mutex<Waiters>,
    woken: Condvar,
    registered: AtomicUsize,
}

#[derive(Debug)]
struct Registration<'r> {
    registry: &'r WaiterRegistry,
    id: u64,
    slots: Vec<Slot>,
    consumes: bool,
    seen: u64,
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        let mut guard = self.registry.locked();
        let state = &mut *guard;
        let woken_but_unserved = state
            .generation(self.id)
            .is_some_and(|generation| generation != self.seen);
        let mut passed_on = false;
        for slot in &self.slots {
            if woken_but_unserved {
                passed_on |= state.wake_next_after(self.id, slot);
            }
            if let Some(queue) = state.queues.get_mut(slot) {
                queue.retain(|queued| *queued != self.id);
                if queue.is_empty() {
                    state.queues.remove(slot);
                }
            }
        }
        state.waiters.remove(&self.id);
        self.registry.registered.fetch_sub(1, Ordering::SeqCst);
        drop(guard);
        if passed_on {
            self.registry.woken.notify_all();
        }
    }
}

impl WaiterRegistry {
    fn locked(&self) -> MutexGuard<'_, Waiters> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn register(&self, db: u32, keys: &[Vec<u8>], consumes: bool) -> Registration<'_> {
        let mut unique = HashSet::with_capacity(keys.len());
        let slots: Vec<Slot> = keys
            .iter()
            .filter(|key| unique.insert(key.as_slice()))
            .map(|key| (db, key.clone()))
            .collect();
        let mut guard = self.locked();
        let state = &mut *guard;
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1);
        state.waiters.insert(
            id,
            Waiter {
                generation: 0,
                consumes,
            },
        );
        for slot in &slots {
            state.queues.entry(slot.clone()).or_default().push_back(id);
        }
        self.registered.fetch_add(1, Ordering::SeqCst);
        drop(guard);
        Registration {
            registry: self,
            id,
            slots,
            consumes,
            seen: 0,
        }
    }

    pub(crate) fn blocked_clients(&self) -> usize {
        self.registered.load(Ordering::SeqCst)
    }

    pub(crate) fn notify_written(&self, written: &[Slot]) {
        if written.is_empty() || self.registered.load(Ordering::SeqCst) == 0 {
            return;
        }
        let mut state = self.locked();
        let mut woke = false;
        for slot in written {
            woke |= state.wake_from(slot, 0);
        }
        drop(state);
        if woke {
            self.woken.notify_all();
        }
    }

    fn pass_on(&self, registration: &Registration<'_>) {
        let mut state = self.locked();
        let mut woke = false;
        for slot in &registration.slots {
            woke |= state.wake_next_after(registration.id, slot);
        }
        drop(state);
        if woke {
            self.woken.notify_all();
        }
    }

    fn is_head(&self, registration: &Registration<'_>) -> bool {
        let state = self.locked();
        registration
            .slots
            .iter()
            .any(|slot| state.queues.get(slot).and_then(VecDeque::front) == Some(&registration.id))
    }

    fn wait_slice(&self, registration: &Registration<'_>, slice: Duration) -> u64 {
        let guard = self.locked();
        let (guard, _) = self
            .woken
            .wait_timeout_while(guard, slice, |state| {
                state.generation(registration.id) == Some(registration.seen)
            })
            .unwrap_or_else(PoisonError::into_inner);
        guard
            .generation(registration.id)
            .unwrap_or(registration.seen)
    }

    pub(crate) fn wait(
        &self,
        db: u32,
        command: &mut BlockingCommand,
        slice: Duration,
        retry: &mut dyn FnMut(&mut BlockingCommand) -> Option<Reply>,
        abandoned: &mut dyn FnMut() -> bool,
    ) -> Option<Reply> {
        let deadline = command
            .timeout()
            .and_then(|timeout| Instant::now().checked_add(timeout));
        let mut registration = self.register(db, command.keys(), command.consumes());
        let mut woken = false;
        loop {
            if let Some(reply) = retry(&mut *command) {
                return Some(reply);
            }
            if woken {
                self.pass_on(&registration);
            }
            woken = loop {
                let wait = match deadline {
                    Some(deadline) => match deadline.checked_duration_since(Instant::now()) {
                        Some(left) if !left.is_zero() => left.min(slice),
                        _ => return Some(Reply::Array(None)),
                    },
                    None => slice,
                };
                let generation = self.wait_slice(&registration, wait);
                if abandoned() {
                    return None;
                }
                if generation != registration.seen {
                    registration.seen = generation;
                    break true;
                }
                if !registration.consumes || self.is_head(&registration) {
                    break false;
                }
            };
        }
    }
}
