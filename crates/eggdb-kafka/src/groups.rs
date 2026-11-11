use crate::log;
use crate::wire::{Parsed, Reader, Writer};
use eggdb_keyspace::Keyspace;
use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

const OFFSET: u8 = 3;
const NOT_COORDINATOR: i16 = 16;
const ILLEGAL_GENERATION: i16 = 22;
const INCONSISTENT_GROUP_PROTOCOL: i16 = 23;
const INVALID_GROUP_ID: i16 = 24;
const UNKNOWN_MEMBER_ID: i16 = 25;
const INVALID_SESSION_TIMEOUT: i16 = 26;
const REBALANCE_IN_PROGRESS: i16 = 27;
const MIN_SESSION_TIMEOUT: Duration = Duration::from_secs(6);
const MAX_SESSION_TIMEOUT: Duration = Duration::from_secs(1800);
const MAX_REBALANCE_WAIT: Duration = Duration::from_secs(60);
const MAX_GROUPS: usize = 10_000;
const MAX_MEMBERS: usize = 1000;
const MAX_METADATA_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Stable,
    Joining,
    Syncing,
}

#[derive(Debug, Clone)]
struct Member {
    protocols: Vec<(String, Vec<u8>)>,
    session_timeout: Duration,
    last_seen: Instant,
    joined_round: u64,
}

#[derive(Debug)]
struct Group {
    phase: Phase,
    generation: i32,
    round: u64,
    protocol_type: String,
    protocol: Option<String>,
    leader: Option<String>,
    members: BTreeMap<String, Member>,
    assignments: BTreeMap<String, Vec<u8>>,
    next_member: u64,
}

impl Group {
    fn new(protocol_type: &str) -> Self {
        Self {
            phase: Phase::Stable,
            generation: 0,
            round: 0,
            protocol_type: protocol_type.to_owned(),
            protocol: None,
            leader: None,
            members: BTreeMap::new(),
            assignments: BTreeMap::new(),
            next_member: 1,
        }
    }

    fn expire(&mut self) -> bool {
        let before = self.members.len();
        self.members
            .retain(|_, member| member.last_seen.elapsed() <= member.session_timeout);
        before != self.members.len()
    }

    fn start_rebalance(&mut self) {
        if self.phase != Phase::Joining {
            self.phase = Phase::Joining;
            self.round += 1;
            self.assignments.clear();
        }
    }

    fn everyone_joined(&self) -> bool {
        self.members.values().all(|member| member.joined_round == self.round)
    }

    fn complete_join(&mut self) {
        let round = self.round;
        self.members.retain(|_, member| member.joined_round == round);
        self.generation = self.generation.wrapping_add(1).max(1);
        self.leader = self.members.keys().next().cloned();
        self.protocol = self.leader.as_ref().and_then(|leader| {
            self.members[leader].protocols.iter().map(|(name, _)| name.clone()).find(|name| {
                self.members
                    .values()
                    .all(|member| member.protocols.iter().any(|(known, _)| known == name))
            })
        });
        self.phase = Phase::Syncing;
    }
}

/// The consumer group coordinator every connection of one Kafka listener shares: groups,
/// members and generations in memory, committed offsets in the keyspace.
#[derive(Default)]
pub(crate) struct Coordinator {
    groups: Mutex<BTreeMap<String, Group>>,
    changed: Condvar,
}

fn offset_key(group: &str, topic: &str, partition: i32) -> Vec<u8> {
    let mut key = vec![OFFSET];
    key.extend_from_slice(group.as_bytes());
    key.push(0);
    key.extend_from_slice(topic.as_bytes());
    key.push(0);
    key.extend_from_slice(&partition.to_be_bytes());
    key
}

fn throttle(version: i16, minimum: i16, body: &mut Writer) {
    if version >= minimum {
        body.i32(0);
    }
}

impl Coordinator {
    fn locked(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Group>> {
        self.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn wait_until<'g>(
        &'g self,
        mut groups: std::sync::MutexGuard<'g, BTreeMap<String, Group>>,
        deadline: Instant,
        done: impl Fn(&BTreeMap<String, Group>) -> bool,
    ) -> std::sync::MutexGuard<'g, BTreeMap<String, Group>> {
        while !done(&groups) {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            groups = self
                .changed
                .wait_timeout(groups, deadline - now)
                .map_or_else(|poisoned| poisoned.into_inner().0, |(guard, _)| guard);
        }
        groups
    }

    pub(crate) fn find_coordinator(
        version: i16,
        request: &mut Reader<'_>,
        host: &str,
        port: i32,
        body: &mut Writer,
    ) -> Parsed<()> {
        let _key = request.string()?;
        if version >= 1 {
            let _key_type = request.i8()?;
        }
        throttle(version, 1, body);
        body.i16(log::error::NONE);
        if version >= 1 {
            body.string(None);
        }
        body.i32(0).string(Some(host)).i32(port);
        Ok(())
    }

    pub(crate) fn join_group(&self, version: i16, request: &mut Reader<'_>, body: &mut Writer) -> Parsed<()> {
        let group_id = request.string()?.unwrap_or_default();
        let session_timeout = Duration::from_millis(u64::try_from(request.i32()?).unwrap_or(0));
        let rebalance_timeout = if version >= 1 {
            Duration::from_millis(u64::try_from(request.i32()?).unwrap_or(0))
        } else {
            session_timeout
        };
        let member_id = request.string()?.unwrap_or_default();
        let protocol_type = request.string()?.unwrap_or_default();
        let protocols = request.array(|reader| {
            let name = reader.string()?.unwrap_or_default();
            let metadata = reader.bytes()?.unwrap_or_default().to_vec();
            Ok((name, metadata))
        })?;
        throttle(version, 2, body);
        let refuse = |body: &mut Writer, code: i16, member: &str| {
            body.i16(code).i32(-1).string(Some("")).string(Some("")).string(Some(member)).count(0);
        };
        if group_id.is_empty() {
            refuse(body, INVALID_GROUP_ID, &member_id);
            return Ok(());
        }
        if !(MIN_SESSION_TIMEOUT..=MAX_SESSION_TIMEOUT).contains(&session_timeout) {
            refuse(body, INVALID_SESSION_TIMEOUT, &member_id);
            return Ok(());
        }
        if protocols.is_empty() || protocols.iter().map(|(_, metadata)| metadata.len()).sum::<usize>() > MAX_METADATA_BYTES {
            refuse(body, INCONSISTENT_GROUP_PROTOCOL, &member_id);
            return Ok(());
        }
        let mut groups = self.locked();
        if !groups.contains_key(&group_id) && groups.len() >= MAX_GROUPS {
            refuse(body, NOT_COORDINATOR, &member_id);
            return Ok(());
        }
        let group = groups.entry(group_id.clone()).or_insert_with(|| Group::new(&protocol_type));
        group.expire();
        if group.members.is_empty() {
            group.protocol_type = protocol_type.clone();
        }
        if group.protocol_type != protocol_type {
            refuse(body, INCONSISTENT_GROUP_PROTOCOL, &member_id);
            return Ok(());
        }
        let member_id = if member_id.is_empty() {
            let id = format!("member-{}-{}", group.generation.max(0), group.next_member);
            group.next_member += 1;
            id
        } else if group.members.contains_key(&member_id) {
            member_id
        } else {
            refuse(body, UNKNOWN_MEMBER_ID, &member_id);
            return Ok(());
        };
        if !group.members.contains_key(&member_id) && group.members.len() >= MAX_MEMBERS {
            refuse(body, NOT_COORDINATOR, &member_id);
            return Ok(());
        }
        group.start_rebalance();
        let round = group.round;
        group.members.insert(
            member_id.clone(),
            Member {
                protocols,
                session_timeout,
                last_seen: Instant::now(),
                joined_round: round,
            },
        );
        self.changed.notify_all();
        let deadline = Instant::now() + rebalance_timeout.min(MAX_REBALANCE_WAIT);
        let mut groups = self.wait_until(groups, deadline, |groups| {
            groups
                .get(&group_id)
                .is_none_or(|group| group.round != round || group.phase != Phase::Joining || group.everyone_joined())
        });
        let Some(group) = groups.get_mut(&group_id) else {
            refuse(body, UNKNOWN_MEMBER_ID, &member_id);
            return Ok(());
        };
        if group.round == round && group.phase == Phase::Joining {
            group.complete_join();
            self.changed.notify_all();
        }
        let Some(protocol) = group.protocol.clone().filter(|_| group.members.contains_key(&member_id)) else {
            refuse(body, if group.members.contains_key(&member_id) { INCONSISTENT_GROUP_PROTOCOL } else { UNKNOWN_MEMBER_ID }, &member_id);
            return Ok(());
        };
        let leader = group.leader.clone().unwrap_or_default();
        body.i16(log::error::NONE)
            .i32(group.generation)
            .string(Some(&protocol))
            .string(Some(&leader))
            .string(Some(&member_id));
        if member_id == leader {
            body.count(group.members.len());
            for (id, member) in &group.members {
                let metadata = member
                    .protocols
                    .iter()
                    .find(|(name, _)| *name == protocol)
                    .map(|(_, metadata)| metadata.as_slice())
                    .unwrap_or_default();
                body.string(Some(id)).bytes(metadata);
            }
        } else {
            body.count(0);
        }
        Ok(())
    }

    pub(crate) fn sync_group(&self, version: i16, request: &mut Reader<'_>, body: &mut Writer) -> Parsed<()> {
        let group_id = request.string()?.unwrap_or_default();
        let generation = request.i32()?;
        let member_id = request.string()?.unwrap_or_default();
        let assignments = request.array(|reader| {
            let member = reader.string()?.unwrap_or_default();
            let assignment = reader.bytes()?.unwrap_or_default().to_vec();
            Ok((member, assignment))
        })?;
        throttle(version, 1, body);
        let mut groups = self.locked();
        let code = match groups.get_mut(&group_id) {
            None => UNKNOWN_MEMBER_ID,
            Some(group) if !group.members.contains_key(&member_id) => UNKNOWN_MEMBER_ID,
            Some(group) if group.generation != generation => ILLEGAL_GENERATION,
            Some(group) if group.phase == Phase::Joining => REBALANCE_IN_PROGRESS,
            Some(group) => {
                if let Some(member) = group.members.get_mut(&member_id) {
                    member.last_seen = Instant::now();
                }
                if group.leader.as_deref() == Some(member_id.as_str()) && group.phase == Phase::Syncing {
                    group.assignments = assignments.into_iter().collect();
                    group.phase = Phase::Stable;
                    self.changed.notify_all();
                }
                log::error::NONE
            }
        };
        if code != log::error::NONE {
            body.i16(code).bytes(&[]);
            return Ok(());
        }
        let deadline = Instant::now() + MAX_REBALANCE_WAIT;
        groups = self.wait_until(groups, deadline, |groups| {
            groups
                .get(&group_id)
                .is_none_or(|group| group.generation != generation || group.phase != Phase::Syncing)
        });
        match groups.get(&group_id) {
            Some(group) if group.generation == generation && group.phase == Phase::Stable => {
                let assignment = group.assignments.get(&member_id).cloned().unwrap_or_default();
                body.i16(log::error::NONE).bytes(&assignment);
            }
            _ => {
                body.i16(REBALANCE_IN_PROGRESS).bytes(&[]);
            }
        }
        Ok(())
    }

    pub(crate) fn heartbeat(&self, version: i16, request: &mut Reader<'_>, body: &mut Writer) -> Parsed<()> {
        let group_id = request.string()?.unwrap_or_default();
        let generation = request.i32()?;
        let member_id = request.string()?.unwrap_or_default();
        throttle(version, 1, body);
        let mut groups = self.locked();
        let code = match groups.get_mut(&group_id) {
            None => UNKNOWN_MEMBER_ID,
            Some(group) => {
                if group.expire() {
                    group.start_rebalance();
                    self.changed.notify_all();
                }
                match group.members.get_mut(&member_id) {
                    None => UNKNOWN_MEMBER_ID,
                    Some(member) => {
                        member.last_seen = Instant::now();
                        if group.phase == Phase::Joining {
                            REBALANCE_IN_PROGRESS
                        } else if group.generation != generation {
                            ILLEGAL_GENERATION
                        } else {
                            log::error::NONE
                        }
                    }
                }
            }
        };
        body.i16(code);
        Ok(())
    }

    pub(crate) fn leave_group(&self, version: i16, request: &mut Reader<'_>, body: &mut Writer) -> Parsed<()> {
        let group_id = request.string()?.unwrap_or_default();
        let member_id = request.string()?.unwrap_or_default();
        throttle(version, 1, body);
        let mut groups = self.locked();
        let removed = groups
            .get_mut(&group_id)
            .is_some_and(|group| group.members.remove(&member_id).is_some());
        let code = if removed {
            if groups.get(&group_id).is_some_and(|group| group.members.is_empty()) {
                groups.remove(&group_id);
            } else if let Some(group) = groups.get_mut(&group_id) {
                group.start_rebalance();
            }
            self.changed.notify_all();
            log::error::NONE
        } else {
            UNKNOWN_MEMBER_ID
        };
        body.i16(code);
        Ok(())
    }

    pub(crate) fn offset_commit(
        &self,
        keyspace: &dyn Keyspace,
        version: i16,
        request: &mut Reader<'_>,
        body: &mut Writer,
    ) -> Parsed<()> {
        let group_id = request.string()?.unwrap_or_default();
        let generation = request.i32()?;
        let member_id = request.string()?.unwrap_or_default();
        let _retention = request.i64()?;
        let topics = request.array(|reader| {
            let name = reader.string()?.unwrap_or_default();
            let partitions = reader.array(|partition| {
                let index = partition.i32()?;
                let offset = partition.i64()?;
                let metadata = partition.string()?;
                Ok((index, offset, metadata))
            })?;
            Ok((name, partitions))
        })?;
        throttle(version, 3, body);
        let check = if generation < 0 {
            log::error::NONE
        } else {
            match self.locked().get(&group_id) {
                None => UNKNOWN_MEMBER_ID,
                Some(group) if !group.members.contains_key(&member_id) => UNKNOWN_MEMBER_ID,
                Some(group) if group.generation != generation => ILLEGAL_GENERATION,
                Some(group) if group.phase == Phase::Joining => REBALANCE_IN_PROGRESS,
                Some(_) => log::error::NONE,
            }
        };
        let stored = if check == log::error::NONE && !group_id.is_empty() {
            log::write(keyspace, |keys| {
                for (topic, partitions) in &topics {
                    for (index, offset, metadata) in partitions {
                        let mut value = offset.to_be_bytes().to_vec();
                        value.extend_from_slice(metadata.as_deref().unwrap_or_default().as_bytes());
                        keys.put(&offset_key(&group_id, topic, *index), &value)?;
                    }
                }
                Ok(())
            })
            .is_ok()
        } else {
            false
        };
        let code = match (check, stored) {
            (log::error::NONE, false) if group_id.is_empty() => INVALID_GROUP_ID,
            (log::error::NONE, false) => log::error::CORRUPT_MESSAGE,
            (code, _) => code,
        };
        body.count(topics.len());
        for (topic, partitions) in &topics {
            body.string(Some(topic)).count(partitions.len());
            for (index, _, _) in partitions {
                body.i32(*index).i16(code);
            }
        }
        Ok(())
    }

    pub(crate) fn offset_fetch(
        keyspace: &dyn Keyspace,
        version: i16,
        request: &mut Reader<'_>,
        body: &mut Writer,
    ) -> Parsed<()> {
        let group_id = request.string()?.unwrap_or_default();
        let topics = request.array(|reader| {
            let name = reader.string()?.unwrap_or_default();
            let partitions = reader.array(|partition| partition.i32())?;
            Ok((name, partitions))
        })?;
        throttle(version, 3, body);
        let committed = log::read(keyspace, |keys| {
            let mut prefix = vec![OFFSET];
            prefix.extend_from_slice(group_id.as_bytes());
            prefix.push(0);
            keys.scan(&prefix)
        })
        .unwrap_or_default();
        let mut found: BTreeMap<(String, i32), (i64, String)> = BTreeMap::new();
        let prefix_length = 1 + group_id.len() + 1;
        for (key, value) in committed {
            let Some(rest) = key.get(prefix_length..) else { continue };
            let Some(split) = rest.iter().position(|byte| *byte == 0) else { continue };
            let topic = String::from_utf8_lossy(&rest[..split]).into_owned();
            let Some(partition) = rest.get(split + 1..split + 5).and_then(|raw| raw.try_into().ok()).map(i32::from_be_bytes) else {
                continue;
            };
            let Some(offset) = value.get(..8).and_then(|raw| raw.try_into().ok()).map(i64::from_be_bytes) else {
                continue;
            };
            let metadata = String::from_utf8_lossy(&value[8..]).into_owned();
            found.insert((topic, partition), (offset, metadata));
        }
        let wanted: Vec<(String, Vec<i32>)> = if topics.is_empty() && version >= 2 {
            let mut all: BTreeMap<String, Vec<i32>> = BTreeMap::new();
            for (topic, partition) in found.keys() {
                all.entry(topic.clone()).or_default().push(*partition);
            }
            all.into_iter().collect()
        } else {
            topics
        };
        body.count(wanted.len());
        for (topic, partitions) in &wanted {
            body.string(Some(topic)).count(partitions.len());
            for partition in partitions {
                let (offset, metadata) = found
                    .get(&(topic.clone(), *partition))
                    .cloned()
                    .unwrap_or((-1, String::new()));
                body.i32(*partition).i64(offset).string(Some(&metadata)).i16(log::error::NONE);
            }
        }
        if version >= 2 {
            body.i16(log::error::NONE);
        }
        Ok(())
    }
}
