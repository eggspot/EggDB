//! Kafka's consumer group protocol of KIP-848 (D1054): members send `ConsumerGroupHeartbeat`
//! with their subscriptions and the partitions they own, the coordinator computes every
//! member's target assignment itself (the range assignor, per topic), raises the group's epoch
//! when membership or subscriptions change, and gives a member a partition only once no other
//! member reports owning it, so no partition is consumed twice during a rebalance and no member
//! waits for a group-wide join.

use crate::groups::Coordinator;
use crate::log;
use crate::wire::{Parsed, Reader, Writer, topic_id};
use eggdb_keyspace::Keyspace;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const INVALID_GROUP_ID: i16 = 24;
const UNKNOWN_MEMBER_ID: i16 = 25;
const GROUP_MAX_SIZE_REACHED: i16 = 81;
const FENCED_MEMBER_EPOCH: i16 = 110;
const UNSUPPORTED_ASSIGNOR: i16 = 112;
const LEAVE_EPOCH: i32 = -1;
const STATIC_LEAVE_EPOCH: i32 = -2;
const SESSION_TIMEOUT: Duration = Duration::from_secs(45);
const HEARTBEAT_INTERVAL_MS: i32 = 5_000;
const MAX_MEMBERS: usize = 1000;
const ASSIGNORS: [&str; 2] = ["range", "uniform"];

type Partitions = BTreeMap<String, Vec<i32>>;

#[derive(Debug, Clone)]
struct ConsumerMember {
    epoch: i32,
    instance: Option<String>,
    subscribed: Vec<String>,
    owned: Partitions,
    sent: Option<Partitions>,
    last_seen: Instant,
}

/// One group of the new protocol: its epoch, members and target assignment.
#[derive(Debug, Default)]
pub(crate) struct ConsumerGroup {
    epoch: i32,
    members: BTreeMap<String, ConsumerMember>,
    target: BTreeMap<String, Partitions>,
    target_epoch: i32,
    next_member: u64,
}

/// The target assignment of the range assignor: for each topic, the members subscribed to it in
/// id order take contiguous runs of its partitions, the first ones one more when they do not
/// divide evenly.
pub(crate) fn range_assignment(
    members: &BTreeMap<String, Vec<String>>,
    partitions_of: &dyn Fn(&str) -> i32,
) -> BTreeMap<String, Partitions> {
    let mut target: BTreeMap<String, Partitions> = members.keys().map(|member| (member.clone(), Partitions::new())).collect();
    let mut topics: Vec<&String> = members.values().flatten().collect();
    topics.sort();
    topics.dedup();
    for topic in topics {
        let subscribers: Vec<&String> = members.iter().filter(|(_, topics)| topics.contains(topic)).map(|(member, _)| member).collect();
        let count = partitions_of(topic).max(0) as usize;
        if subscribers.is_empty() || count == 0 {
            continue;
        }
        let each = count / subscribers.len();
        let extra = count % subscribers.len();
        let mut next = 0usize;
        for (index, member) in subscribers.iter().enumerate() {
            let take = each + usize::from(index < extra);
            if take > 0 {
                let run: Vec<i32> = (next..next + take).map(|partition| partition as i32).collect();
                target.entry((*member).clone()).or_default().insert(topic.clone(), run);
            }
            next += take;
        }
    }
    target
}

/// What a member may be given now: its target, less any partition another member still
/// reports owning.
fn reconciled(member: &str, group: &ConsumerGroup) -> Partitions {
    let mut assignment = Partitions::new();
    for (topic, partitions) in group.target.get(member).into_iter().flatten() {
        let free: Vec<i32> = partitions
            .iter()
            .copied()
            .filter(|partition| {
                !group.members.iter().any(|(other, state)| {
                    other != member && state.owned.get(topic).is_some_and(|owned| owned.contains(partition))
                })
            })
            .collect();
        if !free.is_empty() {
            assignment.insert(topic.clone(), free);
        }
    }
    assignment
}

struct Heartbeat {
    group: String,
    member: String,
    epoch: i32,
    instance: Option<String>,
    subscribed: Option<Vec<String>>,
    assignor: Option<String>,
    owned: Option<Vec<([u8; 16], Vec<i32>)>>,
}

fn read_heartbeat(request: &mut Reader<'_>) -> Parsed<Heartbeat> {
    let group = request.compact_string()?.unwrap_or_default();
    let member = request.compact_string()?.unwrap_or_default();
    let epoch = request.i32()?;
    let instance = request.compact_string()?;
    let _rack = request.compact_string()?;
    let _rebalance_timeout_ms = request.i32()?;
    let subscribed = request.compact_array(|reader| Ok(reader.compact_string()?.unwrap_or_default()))?;
    let assignor = request.compact_string()?;
    let owned = request.compact_array(|reader| {
        let id = reader.uuid()?;
        let partitions = reader.compact_array(|reader| reader.i32())?.unwrap_or_default();
        reader.skip_tags()?;
        Ok((id, partitions))
    })?;
    request.skip_tags()?;
    Ok(Heartbeat { group, member, epoch, instance, subscribed, assignor, owned })
}

fn write_reply(body: &mut Writer, error: i16, message: Option<&str>, member: Option<&str>, epoch: i32, assignment: Option<&Partitions>) {
    body.i32(0).i16(error).compact_string(message).compact_string(member).i32(epoch);
    body.i32(if error == 0 { HEARTBEAT_INTERVAL_MS } else { 0 });
    match assignment {
        Some(assignment) => {
            body.i8(1);
            body.compact_count(Some(assignment.len()));
            for (topic, partitions) in assignment {
                body.uuid(&topic_id(topic));
                body.compact_count(Some(partitions.len()));
                for partition in partitions {
                    body.i32(*partition);
                }
                body.no_tags();
            }
            body.no_tags();
        }
        None => {
            body.i8(-1);
        }
    }
    body.no_tags();
}

fn names_of(owned: Vec<([u8; 16], Vec<i32>)>, topics: &[(String, i32)]) -> Partitions {
    owned
        .into_iter()
        .filter_map(|(id, partitions)| {
            topics.iter().find(|(name, _)| topic_id(name) == id).map(|(name, _)| (name.clone(), partitions))
        })
        .collect()
}

impl ConsumerGroup {
    fn expire(&mut self, now: Instant) {
        let before = self.members.len();
        self.members.retain(|_, member| now.duration_since(member.last_seen) < SESSION_TIMEOUT);
        if self.members.len() != before {
            self.epoch += 1;
        }
    }

    fn retarget(&mut self, topics: &[(String, i32)]) {
        if self.target_epoch == self.epoch {
            return;
        }
        let subscriptions: BTreeMap<String, Vec<String>> =
            self.members.iter().map(|(id, member)| (id.clone(), member.subscribed.clone())).collect();
        let partitions_of = |topic: &str| topics.iter().find(|(name, _)| name == topic).map_or(0, |(_, count)| *count);
        self.target = range_assignment(&subscriptions, &partitions_of);
        self.target_epoch = self.epoch;
    }
}

impl Coordinator {
    /// Answers `ConsumerGroupHeartbeat` (version 0).
    pub(crate) fn consumer_heartbeat(&self, keyspace: &dyn Keyspace, request: &mut Reader<'_>, body: &mut Writer) -> Parsed<()> {
        let heartbeat = read_heartbeat(request)?;
        if heartbeat.group.is_empty() {
            write_reply(body, INVALID_GROUP_ID, Some("the group ID is empty"), None, heartbeat.epoch, None);
            return Ok(());
        }
        if let Some(assignor) = heartbeat.assignor.as_deref().filter(|assignor| !ASSIGNORS.contains(assignor)) {
            write_reply(body, UNSUPPORTED_ASSIGNOR, Some(&format!("the assignor {assignor} is not offered; use range or uniform")), None, heartbeat.epoch, None);
            return Ok(());
        }
        let topics = log::topics(keyspace)?;
        let now = Instant::now();
        let mut groups = self.consumer_groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let group = groups.entry(heartbeat.group.clone()).or_default();
        group.expire(now);
        let member_id = match heartbeat.epoch {
            0 => {
                if group.members.len() >= MAX_MEMBERS {
                    write_reply(body, GROUP_MAX_SIZE_REACHED, Some("the group is full"), None, 0, None);
                    return Ok(());
                }
                let replaced = heartbeat.instance.as_ref().and_then(|instance| {
                    group.members.iter().find(|(_, member)| member.instance.as_ref() == Some(instance)).map(|(id, _)| id.clone())
                });
                if let Some(old) = replaced {
                    group.members.remove(&old);
                }
                let id = if heartbeat.member.is_empty() {
                    group.next_member += 1;
                    format!("eggdb-member-{}-{}", heartbeat.group.len(), group.next_member)
                } else {
                    heartbeat.member.clone()
                };
                group.epoch += 1;
                group.members.insert(
                    id.clone(),
                    ConsumerMember {
                        epoch: 0,
                        instance: heartbeat.instance.clone(),
                        subscribed: heartbeat.subscribed.clone().unwrap_or_default(),
                        owned: Partitions::new(),
                        sent: None,
                        last_seen: now,
                    },
                );
                id
            }
            LEAVE_EPOCH | STATIC_LEAVE_EPOCH => {
                if group.members.remove(&heartbeat.member).is_some() {
                    group.epoch += 1;
                }
                write_reply(body, 0, None, Some(&heartbeat.member), heartbeat.epoch, None);
                return Ok(());
            }
            epoch => {
                let Some(member) = group.members.get(&heartbeat.member) else {
                    write_reply(body, UNKNOWN_MEMBER_ID, Some("the member is not in the group; join with epoch 0"), None, epoch, None);
                    return Ok(());
                };
                if epoch != member.epoch {
                    write_reply(body, FENCED_MEMBER_EPOCH, Some("the member's epoch is stale; join again"), None, epoch, None);
                    return Ok(());
                }
                heartbeat.member.clone()
            }
        };
        let mut subscription_changed = false;
        if let Some(member) = group.members.get_mut(&member_id) {
            member.last_seen = now;
            if let Some(subscribed) = &heartbeat.subscribed
                && *subscribed != member.subscribed
            {
                member.subscribed = subscribed.clone();
                subscription_changed = true;
            }
            if let Some(owned) = heartbeat.owned {
                member.owned = names_of(owned, &topics);
            }
        }
        if subscription_changed {
            group.epoch += 1;
        }
        group.retarget(&topics);
        let assignment = reconciled(&member_id, group);
        let epoch = group.target_epoch;
        let Some(member) = group.members.get_mut(&member_id) else {
            return Err("the member left while it was answered".to_owned());
        };
        member.epoch = epoch;
        let changed = member.sent.as_ref() != Some(&assignment);
        if changed {
            member.sent = Some(assignment.clone());
        }
        write_reply(body, 0, None, Some(&member_id), epoch, changed.then_some(&assignment));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_range_assignor_splits_each_topic_among_its_subscribers() {
        let members: BTreeMap<String, Vec<String>> = [
            ("a".to_owned(), vec!["orders".to_owned(), "audit".to_owned()]),
            ("b".to_owned(), vec!["orders".to_owned()]),
        ]
        .into_iter()
        .collect();
        let partitions = |topic: &str| if topic == "orders" { 5 } else { 2 };
        let target = range_assignment(&members, &partitions);
        assert_eq!(target["a"]["orders"], vec![0, 1, 2]);
        assert_eq!(target["b"]["orders"], vec![3, 4]);
        assert_eq!(target["a"]["audit"], vec![0, 1]);
        assert!(!target["b"].contains_key("audit"));
    }

    #[test]
    fn a_partition_moves_only_once_its_old_owner_lets_it_go() {
        let mut group = ConsumerGroup::default();
        let member = |owned: Partitions| ConsumerMember {
            epoch: 1,
            instance: None,
            subscribed: vec!["orders".to_owned()],
            owned,
            sent: None,
            last_seen: Instant::now(),
        };
        group.members.insert("a".to_owned(), member([("orders".to_owned(), vec![0, 1, 2, 3])].into_iter().collect()));
        group.members.insert("b".to_owned(), member(Partitions::new()));
        group.epoch = 2;
        group.retarget(&[("orders".to_owned(), 4)]);
        assert_eq!(reconciled("a", &group)["orders"], vec![0, 1]);
        assert!(reconciled("b", &group).is_empty());
        group.members.get_mut("a").unwrap().owned = [("orders".to_owned(), vec![0, 1])].into_iter().collect();
        assert_eq!(reconciled("b", &group)["orders"], vec![2, 3]);
    }
}
