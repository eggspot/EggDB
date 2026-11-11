use eggdb_keyspace::{KeysRead, KeysWrite};
use std::collections::BTreeMap;

const PRODUCER_IDS: u8 = 4;
const PRODUCER_STATE: u8 = 5;
const TRANSACTIONAL_ATTRIBUTE: i16 = 0x10;
const NO_PRODUCER: i64 = -1;
const STATE_BYTES: usize = 22;

pub(crate) const OUT_OF_ORDER_SEQUENCE_NUMBER: i16 = 45;
pub(crate) const INVALID_PRODUCER_EPOCH: i16 = 47;
pub(crate) const INVALID_TXN_STATE: i16 = 48;
pub(crate) const UNKNOWN_PRODUCER_ID: i16 = 59;

/// The idempotent producer fields of one record batch, as its header carries them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BatchProducer {
    pub(crate) id: i64,
    pub(crate) epoch: i16,
    pub(crate) base_sequence: i32,
    pub(crate) last_sequence: i32,
    pub(crate) record_count: i32,
    pub(crate) transactional: bool,
}

impl BatchProducer {
    pub(crate) fn of(batch: &[u8]) -> Option<Self> {
        let field = |range: std::ops::Range<usize>| batch.get(range);
        let attributes = i16::from_be_bytes(field(21..23)?.try_into().ok()?);
        let last_delta = i32::from_be_bytes(field(23..27)?.try_into().ok()?);
        let id = i64::from_be_bytes(field(43..51)?.try_into().ok()?);
        let epoch = i16::from_be_bytes(field(51..53)?.try_into().ok()?);
        let base_sequence = i32::from_be_bytes(field(53..57)?.try_into().ok()?);
        let record_count = i32::from_be_bytes(field(57..61)?.try_into().ok()?);
        if id == NO_PRODUCER {
            return None;
        }
        Some(Self {
            id,
            epoch,
            base_sequence,
            last_sequence: sequence_after(base_sequence, last_delta),
            record_count,
            transactional: attributes & TRANSACTIONAL_ATTRIBUTE != 0,
        })
    }
}

fn sequence_after(sequence: i32, steps: i32) -> i32 {
    let span = i64::from(i32::MAX) + 1;
    ((i64::from(sequence) + i64::from(steps)).rem_euclid(span)) as i32
}

/// What a partition last accepted from one producer: its epoch, the last sequence number, and the
/// last batch's first sequence, offset and record count, so a retried batch gets its first answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProducerState {
    epoch: i16,
    last_sequence: i32,
    last_base_sequence: i32,
    last_base_offset: i64,
    last_record_count: i32,
}

impl ProducerState {
    fn encode(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(STATE_BYTES);
        out.extend_from_slice(&self.epoch.to_be_bytes());
        out.extend_from_slice(&self.last_sequence.to_be_bytes());
        out.extend_from_slice(&self.last_base_sequence.to_be_bytes());
        out.extend_from_slice(&self.last_base_offset.to_be_bytes());
        out.extend_from_slice(&self.last_record_count.to_be_bytes());
        out
    }

    fn decode(raw: &[u8]) -> Option<Self> {
        if raw.len() != STATE_BYTES {
            return None;
        }
        Some(Self {
            epoch: i16::from_be_bytes(raw[0..2].try_into().ok()?),
            last_sequence: i32::from_be_bytes(raw[2..6].try_into().ok()?),
            last_base_sequence: i32::from_be_bytes(raw[6..10].try_into().ok()?),
            last_base_offset: i64::from_be_bytes(raw[10..18].try_into().ok()?),
            last_record_count: i32::from_be_bytes(raw[18..22].try_into().ok()?),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Accepted {
    Append,
    Duplicate(i64),
}

/// Checks a batch's producer fields against what the partition last accepted from that producer.
pub(crate) fn check(
    state: Option<ProducerState>,
    producer: &BatchProducer,
    allocated: i64,
) -> Result<Accepted, i16> {
    if producer.transactional {
        return Err(INVALID_TXN_STATE);
    }
    if producer.id < 0 || producer.id >= allocated || producer.epoch < 0 || producer.base_sequence < 0 {
        return Err(UNKNOWN_PRODUCER_ID);
    }
    match state {
        None if producer.base_sequence == 0 => Ok(Accepted::Append),
        None => Err(OUT_OF_ORDER_SEQUENCE_NUMBER),
        Some(state) if producer.epoch < state.epoch => Err(INVALID_PRODUCER_EPOCH),
        Some(state) if producer.epoch > state.epoch => {
            if producer.base_sequence == 0 {
                Ok(Accepted::Append)
            } else {
                Err(OUT_OF_ORDER_SEQUENCE_NUMBER)
            }
        }
        Some(state)
            if producer.base_sequence == state.last_base_sequence
                && producer.record_count == state.last_record_count =>
        {
            Ok(Accepted::Duplicate(state.last_base_offset))
        }
        Some(state) if producer.base_sequence == sequence_after(state.last_sequence, 1) => {
            Ok(Accepted::Append)
        }
        Some(_) => Err(OUT_OF_ORDER_SEQUENCE_NUMBER),
    }
}

pub(crate) fn accepted_state(producer: &BatchProducer, base_offset: i64) -> ProducerState {
    ProducerState {
        epoch: producer.epoch,
        last_sequence: producer.last_sequence,
        last_base_sequence: producer.base_sequence,
        last_base_offset: base_offset,
        last_record_count: producer.record_count,
    }
}

fn state_prefix(topic: &str) -> Vec<u8> {
    let mut key = vec![PRODUCER_STATE];
    key.extend_from_slice(topic.as_bytes());
    key.push(0);
    key
}

fn state_key(topic: &str, partition: i32, producer: i64) -> Vec<u8> {
    let mut key = state_prefix(topic);
    key.extend_from_slice(&partition.to_be_bytes());
    key.extend_from_slice(&producer.to_be_bytes());
    key
}

/// The producer states of one partition that a batch list refers to, read once per request.
pub(crate) struct PartitionProducers {
    topic: String,
    partition: i32,
    allocated: i64,
    states: BTreeMap<i64, Option<ProducerState>>,
}

impl PartitionProducers {
    pub(crate) fn read(keys: &dyn KeysRead, topic: &str, partition: i32) -> Result<Self, String> {
        Ok(Self {
            topic: topic.to_owned(),
            partition,
            allocated: allocated_ids(keys)?,
            states: BTreeMap::new(),
        })
    }

    pub(crate) fn check(
        &mut self,
        keys: &dyn KeysRead,
        producer: &BatchProducer,
    ) -> Result<Result<Accepted, i16>, String> {
        let state = match self.states.get(&producer.id) {
            Some(state) => *state,
            None => {
                let stored = keys
                    .get(&state_key(&self.topic, self.partition, producer.id))?
                    .and_then(|raw| ProducerState::decode(&raw));
                self.states.insert(producer.id, stored);
                stored
            }
        };
        Ok(check(state, producer, self.allocated))
    }

    pub(crate) fn accept(&mut self, producer: &BatchProducer, base_offset: i64) {
        self.states
            .insert(producer.id, Some(accepted_state(producer, base_offset)));
    }

    pub(crate) fn store(&self, keys: &mut dyn KeysWrite) -> Result<(), String> {
        for (producer, state) in &self.states {
            if let Some(state) = state {
                keys.put(&state_key(&self.topic, self.partition, *producer), &state.encode())?;
            }
        }
        Ok(())
    }
}

fn allocated_ids(keys: &dyn KeysRead) -> Result<i64, String> {
    Ok(keys
        .get(&[PRODUCER_IDS])?
        .and_then(|raw| raw.get(..8).and_then(|raw| raw.try_into().ok()))
        .map_or(0, i64::from_be_bytes))
}

/// Hands out the next producer id, kept in the file so ids are never reused after a restart.
pub(crate) fn next_producer_id(keys: &mut dyn KeysWrite) -> Result<i64, String> {
    let id = allocated_ids(&*keys)?;
    let next = id.checked_add(1).ok_or("every producer id is used")?;
    keys.put(&[PRODUCER_IDS], &next.to_be_bytes())?;
    Ok(id)
}

pub(crate) fn delete_topic_states(keys: &mut dyn KeysWrite, topic: &str) -> Result<(), String> {
    for (key, _) in keys.scan(&state_prefix(topic))? {
        keys.delete(&key)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn producer(epoch: i16, base_sequence: i32, records: i32) -> BatchProducer {
        BatchProducer {
            id: 0,
            epoch,
            base_sequence,
            last_sequence: sequence_after(base_sequence, records - 1),
            record_count: records,
            transactional: false,
        }
    }

    #[test]
    fn sequences_must_follow_on_and_a_retried_batch_gets_its_first_offset() {
        assert_eq!(check(None, &producer(0, 0, 3), 1), Ok(Accepted::Append));
        assert_eq!(check(None, &producer(0, 5, 3), 1), Err(OUT_OF_ORDER_SEQUENCE_NUMBER));
        let state = accepted_state(&producer(0, 0, 3), 40);
        assert_eq!(check(Some(state), &producer(0, 3, 2), 1), Ok(Accepted::Append));
        assert_eq!(check(Some(state), &producer(0, 0, 3), 1), Ok(Accepted::Duplicate(40)));
        assert_eq!(check(Some(state), &producer(0, 4, 1), 1), Err(OUT_OF_ORDER_SEQUENCE_NUMBER));
        assert_eq!(check(Some(state), &producer(1, 0, 1), 1), Ok(Accepted::Append));
        let bumped = accepted_state(&producer(2, 0, 1), 50);
        assert_eq!(check(Some(bumped), &producer(1, 1, 1), 1), Err(INVALID_PRODUCER_EPOCH));
        assert_eq!(check(None, &producer(0, 0, 1), 0), Err(UNKNOWN_PRODUCER_ID));
        let mut transactional = producer(0, 0, 1);
        transactional.transactional = true;
        assert_eq!(check(None, &transactional, 1), Err(INVALID_TXN_STATE));
    }

    #[test]
    fn sequence_numbers_wrap_after_the_largest_int32() {
        assert_eq!(sequence_after(i32::MAX, 1), 0);
        assert_eq!(sequence_after(i32::MAX - 1, 3), 1);
        let state = accepted_state(&producer(0, i32::MAX - 1, 2), 9);
        assert_eq!(check(Some(state), &producer(0, 0, 1), 1), Ok(Accepted::Append));
    }

    #[test]
    fn producer_state_round_trips_through_its_stored_form() {
        let state = accepted_state(&producer(3, 7, 4), 123);
        assert_eq!(ProducerState::decode(&state.encode()), Some(state));
        assert_eq!(ProducerState::decode(&[0; 5]), None);
    }
}
