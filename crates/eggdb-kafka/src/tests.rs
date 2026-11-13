use crate::server::{ServerConfig, answer, serve_stream};
use crate::wire::{Reader, Writer, crc32c, topic_id};
use eggdb_keyspace::MemoryKeyspace;
use eggdb_platform::connection_limits::MemoryBudget;
use std::sync::Arc;

fn config() -> ServerConfig {
    ServerConfig {
        advertised: "127.0.0.1:9092".parse().unwrap(),
        max_connections: 4,
        memory_budget: Arc::default(),
        credentials: None,
    }
}

fn varint(out: &mut Vec<u8>, value: i64) {
    let mut zigzag = ((value << 1) ^ (value >> 63)) as u64;
    loop {
        let byte = (zigzag & 0x7f) as u8;
        zigzag >>= 7;
        if zigzag == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn batch(values: &[&str]) -> Vec<u8> {
    let mut records = Vec::new();
    for (delta, value) in values.iter().enumerate() {
        let mut record = vec![0u8];
        varint(&mut record, 0);
        varint(&mut record, delta as i64);
        varint(&mut record, -1);
        varint(&mut record, value.len() as i64);
        record.extend_from_slice(value.as_bytes());
        varint(&mut record, 0);
        varint(&mut records, record.len() as i64);
        records.extend(record);
    }
    let mut tail = Vec::new();
    tail.extend_from_slice(&0i16.to_be_bytes());
    tail.extend_from_slice(&(values.len() as i32 - 1).to_be_bytes());
    tail.extend_from_slice(&0i64.to_be_bytes());
    tail.extend_from_slice(&0i64.to_be_bytes());
    tail.extend_from_slice(&(-1i64).to_be_bytes());
    tail.extend_from_slice(&(-1i16).to_be_bytes());
    tail.extend_from_slice(&(-1i32).to_be_bytes());
    tail.extend_from_slice(&(values.len() as i32).to_be_bytes());
    tail.extend(records);
    let mut out = Vec::new();
    out.extend_from_slice(&0i64.to_be_bytes());
    out.extend_from_slice(&((4 + 1 + 4 + tail.len()) as i32).to_be_bytes());
    out.extend_from_slice(&0i32.to_be_bytes());
    out.push(2);
    out.extend_from_slice(&crc32c(&tail).to_be_bytes());
    out.extend(tail);
    out
}

fn request(key: i16, version: i16, body: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut writer = Writer::default();
    writer.i16(key).i16(version).i32(7).string(Some("test"));
    body(&mut writer);
    writer.out
}

fn produce(topic: &str, records: &[u8]) -> Vec<u8> {
    request(0, 3, |writer| {
        writer
            .string(None)
            .i16(1)
            .i32(1000)
            .count(1)
            .string(Some(topic))
            .count(1)
            .i32(0)
            .bytes(records);
    })
}

fn response(keyspace: &MemoryKeyspace, frame: &[u8]) -> Vec<u8> {
    let framed = answer(keyspace, &config(), frame).expect("an answer");
    assert_eq!(
        i32::from_be_bytes(framed[..4].try_into().unwrap()) as usize,
        framed.len() - 4
    );
    assert_eq!(i32::from_be_bytes(framed[4..8].try_into().unwrap()), 7);
    framed[8..].to_vec()
}

#[test]
fn api_versions_lists_what_is_supported_and_refuses_newer_versions() {
    let keyspace = MemoryKeyspace::default();
    let body = response(&keyspace, &request(18, 0, |_| {}));
    assert_eq!(i16::from_be_bytes([body[0], body[1]]), 0);
    assert_eq!(i32::from_be_bytes(body[2..6].try_into().unwrap()), 25);
    let newer = response(&keyspace, &request(18, 3, |_| {}));
    assert_eq!(i16::from_be_bytes([newer[0], newer[1]]), 35);
    assert!(answer(&keyspace, &config(), &request(1, 99, |_| {})).is_none());
}

#[test]
fn produced_records_are_fetched_from_their_offsets() {
    let keyspace = MemoryKeyspace::default();
    for (values, base) in [(&["a", "b"][..], 0i64), (&["c"][..], 2)] {
        let body = response(&keyspace, &produce("events", &batch(values)));
        let mut reader = Reader::new(&body);
        assert_eq!(reader.i32().unwrap(), 1);
        assert_eq!(reader.string().unwrap().as_deref(), Some("events"));
        assert_eq!(reader.i32().unwrap(), 1);
        assert_eq!(
            (
                reader.i32().unwrap(),
                reader.i16().unwrap(),
                reader.i64().unwrap()
            ),
            (0, 0, base)
        );
    }
    let fetched = response(
        &keyspace,
        &request(1, 4, |writer| {
            writer
                .i32(-1)
                .i32(100)
                .i32(1)
                .i32(1 << 20)
                .i8(0)
                .count(1)
                .string(Some("events"))
                .count(1)
                .i32(0)
                .i64(2)
                .i32(1 << 20);
        }),
    );
    let mut reader = Reader::new(&fetched);
    let _throttle = reader.i32().unwrap();
    reader.i32().unwrap();
    reader.string().unwrap();
    reader.i32().unwrap();
    assert_eq!(
        (
            reader.i32().unwrap(),
            reader.i16().unwrap(),
            reader.i64().unwrap()
        ),
        (0, 0, 3)
    );
    reader.i64().unwrap();
    reader.i32().unwrap();
    let records = reader.bytes().unwrap().unwrap();
    assert_eq!(i64::from_be_bytes(records[..8].try_into().unwrap()), 2);
    assert!(records.windows(1).any(|window| window == b"c"));
    let offsets = response(
        &keyspace,
        &request(2, 1, |writer| {
            writer
                .i32(-1)
                .count(1)
                .string(Some("events"))
                .count(2)
                .i32(0)
                .i64(-2)
                .i32(0)
                .i64(-1);
        }),
    );
    let mut reader = Reader::new(&offsets);
    reader.i32().unwrap();
    reader.string().unwrap();
    reader.i32().unwrap();
    assert_eq!(
        (
            reader.i32().unwrap(),
            reader.i16().unwrap(),
            reader.i64().unwrap(),
            reader.i64().unwrap()
        ),
        (0, 0, -1, 0)
    );
    assert_eq!(
        (
            reader.i32().unwrap(),
            reader.i16().unwrap(),
            reader.i64().unwrap(),
            reader.i64().unwrap()
        ),
        (0, 0, -1, 3)
    );
}

#[test]
fn metadata_lists_and_creates_topics() {
    let keyspace = MemoryKeyspace::default();
    let body = response(
        &keyspace,
        &request(3, 1, |writer| {
            writer
                .count(2)
                .string(Some("orders"))
                .string(Some("bad topic!"));
        }),
    );
    let mut reader = Reader::new(&body);
    assert_eq!(reader.i32().unwrap(), 1);
    assert_eq!(reader.i32().unwrap(), 0);
    assert_eq!(reader.string().unwrap().as_deref(), Some("127.0.0.1"));
    assert_eq!(reader.i32().unwrap(), 9092);
    reader.string().unwrap();
    reader.i32().unwrap();
    assert_eq!(reader.i32().unwrap(), 2);
    assert_eq!(
        (
            reader.i16().unwrap(),
            reader.string().unwrap().as_deref().map(str::to_owned)
        ),
        (0, Some("orders".to_owned()))
    );
}

#[test]
fn corrupt_batches_and_hostile_frames_are_refused() {
    let keyspace = MemoryKeyspace::default();
    let mut damaged = batch(&["x"]);
    let last = damaged.len() - 1;
    damaged[last] ^= 1;
    let body = response(&keyspace, &produce("t", &damaged));
    let mut reader = Reader::new(&body);
    reader.i32().unwrap();
    reader.string().unwrap();
    reader.i32().unwrap();
    assert_eq!((reader.i32().unwrap(), reader.i16().unwrap()), (0, 2));
    let valid = produce("t", &batch(&["y"]));
    for cut in 0..valid.len() {
        let _ = answer(&keyspace, &config(), &valid[..cut]);
    }
    let mut huge = request(3, 1, |writer| {
        writer.i32(i32::MAX);
    });
    huge.extend_from_slice(&[0; 8]);
    assert!(answer(&keyspace, &config(), &huge).is_none());
}

fn framed(frame: &[u8]) -> Vec<u8> {
    let mut bytes = u32::try_from(frame.len()).unwrap().to_be_bytes().to_vec();
    bytes.extend_from_slice(frame);
    bytes
}

#[test]
fn a_request_past_the_memory_budget_ends_its_connection_before_it_is_read() {
    let keyspace = MemoryKeyspace::default();
    let budget = Arc::new(MemoryBudget::new(100_000));
    let config = ServerConfig {
        memory_budget: Arc::clone(&budget),
        ..config()
    };
    let versions = framed(&request(18, 0, |_| {}));
    let mut oversized = 200_000u32.to_be_bytes().to_vec();
    oversized.extend_from_slice(&versions);
    let mut answered = Vec::new();
    serve_stream(&keyspace, &config, &crate::groups::Coordinator::default(), &oversized[..], &mut answered).unwrap();
    assert!(answered.is_empty());
    assert_eq!(budget.used_bytes(), 0);
    let mut answered = Vec::new();
    serve_stream(&keyspace, &config, &crate::groups::Coordinator::default(), &versions[..], &mut answered).unwrap();
    assert!(!answered.is_empty());
    let held = budget.reserve(90_000).unwrap();
    let mut large = request(18, 0, |_| {});
    large.resize(70_000, 0);
    let mut answered = Vec::new();
    serve_stream(&keyspace, &config, &crate::groups::Coordinator::default(), &framed(&large)[..], &mut answered).unwrap();
    assert!(answered.is_empty());
    drop(held);
    assert_eq!(budget.used_bytes(), 0);
}

#[test]
fn a_lone_consumer_joins_syncs_and_commits_offsets() {
    use crate::groups::Coordinator;
    use crate::server::{Login, answer_for};
    let keyspace = MemoryKeyspace::default();
    let coordinator = Coordinator::default();
    let call = |frame: Vec<u8>| {
        let mut login = Login::Done;
        let framed = answer_for(&keyspace, &config(), &coordinator, &frame, &mut login).expect("an answer");
        framed[8..].to_vec()
    };
    let joined = call(request(11, 0, |writer| {
        writer
            .string(Some("readers"))
            .i32(10_000)
            .string(Some(""))
            .string(Some("consumer"))
            .count(1)
            .string(Some("range"))
            .bytes(b"meta");
    }));
    let mut reader = Reader::new(&joined);
    assert_eq!(reader.i16().unwrap(), 0);
    let generation = reader.i32().unwrap();
    assert_eq!(generation, 1);
    assert_eq!(reader.string().unwrap().as_deref(), Some("range"));
    let leader = reader.string().unwrap().unwrap();
    let member = reader.string().unwrap().unwrap();
    assert_eq!(leader, member);
    assert_eq!(reader.i32().unwrap(), 1);
    let synced = call(request(14, 0, |writer| {
        writer
            .string(Some("readers"))
            .i32(generation)
            .string(Some(&member))
            .count(1)
            .string(Some(&member))
            .bytes(b"assigned");
    }));
    let mut reader = Reader::new(&synced);
    assert_eq!(reader.i16().unwrap(), 0);
    assert_eq!(reader.bytes().unwrap(), Some(&b"assigned"[..]));
    let committed = call(request(8, 2, |writer| {
        writer
            .string(Some("readers"))
            .i32(generation)
            .string(Some(&member))
            .i64(-1)
            .count(1)
            .string(Some("events"))
            .count(1)
            .i32(0)
            .i64(42)
            .string(None);
    }));
    let mut reader = Reader::new(&committed);
    assert_eq!(reader.i32().unwrap(), 1);
    assert_eq!(reader.string().unwrap().as_deref(), Some("events"));
    assert_eq!(reader.i32().unwrap(), 1);
    assert_eq!(reader.i32().unwrap(), 0);
    assert_eq!(reader.i16().unwrap(), 0);
    let fetched = call(request(9, 1, |writer| {
        writer.string(Some("readers")).count(1).string(Some("events")).count(1).i32(0);
    }));
    let mut reader = Reader::new(&fetched);
    reader.i32().unwrap();
    reader.string().unwrap();
    reader.i32().unwrap();
    assert_eq!(reader.i32().unwrap(), 0);
    assert_eq!(reader.i64().unwrap(), 42);
    let beat = call(request(12, 0, |writer| {
        writer.string(Some("readers")).i32(generation).string(Some(&member));
    }));
    assert_eq!(Reader::new(&beat).i16().unwrap(), 0);
}

#[test]
fn a_static_member_restarts_without_a_rebalance_and_fences_its_old_id() {
    use crate::groups::Coordinator;
    use crate::server::{Login, answer_for};
    let keyspace = MemoryKeyspace::default();
    let coordinator = Coordinator::default();
    let call = |frame: Vec<u8>| {
        let mut login = Login::Done;
        let framed = answer_for(&keyspace, &config(), &coordinator, &frame, &mut login).expect("an answer");
        framed[8..].to_vec()
    };
    let join = || {
        request(11, 5, |writer| {
            writer
                .string(Some("static"))
                .i32(10_000)
                .i32(10_000)
                .string(Some(""))
                .string(Some("reader-1"))
                .string(Some("consumer"))
                .count(1)
                .string(Some("range"))
                .bytes(b"meta");
        })
    };
    let read_join = |answer: &[u8]| {
        let mut reader = Reader::new(answer);
        assert_eq!(reader.i32().unwrap(), 0);
        assert_eq!(reader.i16().unwrap(), 0);
        let generation = reader.i32().unwrap();
        assert_eq!(reader.string().unwrap().as_deref(), Some("range"));
        let leader = reader.string().unwrap().unwrap();
        let member = reader.string().unwrap().unwrap();
        assert_eq!(leader, member);
        assert_eq!(reader.i32().unwrap(), 1);
        assert_eq!(reader.string().unwrap().as_deref(), Some(member.as_str()));
        assert_eq!(reader.string().unwrap().as_deref(), Some("reader-1"));
        (generation, member)
    };
    let (generation, first) = read_join(&call(join()));
    assert_eq!(generation, 1);
    let synced = call(request(14, 3, |writer| {
        writer
            .string(Some("static"))
            .i32(generation)
            .string(Some(&first))
            .string(Some("reader-1"))
            .count(1)
            .string(Some(&first))
            .bytes(b"assigned");
    }));
    let mut reader = Reader::new(&synced);
    reader.i32().unwrap();
    assert_eq!(reader.i16().unwrap(), 0);
    let (again, second) = read_join(&call(join()));
    assert_eq!(again, generation);
    assert_ne!(second, first);
    let fenced = call(request(12, 3, |writer| {
        writer.string(Some("static")).i32(generation).string(Some(&first)).string(Some("reader-1"));
    }));
    let mut reader = Reader::new(&fenced);
    reader.i32().unwrap();
    assert_eq!(reader.i16().unwrap(), 82);
    let resynced = call(request(14, 3, |writer| {
        writer
            .string(Some("static"))
            .i32(generation)
            .string(Some(&second))
            .string(Some("reader-1"))
            .count(0);
    }));
    let mut reader = Reader::new(&resynced);
    reader.i32().unwrap();
    assert_eq!(reader.i16().unwrap(), 0);
    assert_eq!(reader.bytes().unwrap(), Some(&b"assigned"[..]));
    let left = call(request(13, 3, |writer| {
        writer.string(Some("static")).count(1).string(Some("")).string(Some("reader-1"));
    }));
    let mut reader = Reader::new(&left);
    reader.i32().unwrap();
    assert_eq!(reader.i16().unwrap(), 0);
    assert_eq!(reader.i32().unwrap(), 1);
    reader.string().unwrap();
    assert_eq!(reader.string().unwrap().as_deref(), Some("reader-1"));
    assert_eq!(reader.i16().unwrap(), 0);
}

#[test]
fn group_listing_describes_unknown_groups_as_dead_and_deletes_committed_offsets() {
    let keyspace = MemoryKeyspace::default();
    let listed = response(&keyspace, &request(16, 1, |_| {}));
    let mut reader = Reader::new(&listed);
    assert_eq!(reader.i32().unwrap(), 0);
    assert_eq!(reader.i16().unwrap(), 0);
    assert_eq!(reader.i32().unwrap(), 0);
    let described = response(
        &keyspace,
        &request(15, 0, |writer| {
            writer.i32(1).string(Some("billing"));
        }),
    );
    let mut reader = Reader::new(&described);
    assert_eq!(reader.i32().unwrap(), 1);
    assert_eq!(reader.i16().unwrap(), 0);
    assert_eq!(reader.string().unwrap().as_deref(), Some("billing"));
    assert_eq!(reader.string().unwrap().as_deref(), Some("Dead"));
    let committed = response(
        &keyspace,
        &request(8, 2, |writer| {
            writer
                .string(Some("billing"))
                .i32(-1)
                .string(Some(""))
                .i64(-1)
                .i32(1)
                .string(Some("events"))
                .i32(1)
                .i32(0)
                .i64(5)
                .string(Some(""));
        }),
    );
    assert!(!committed.is_empty());
    let deleted = response(
        &keyspace,
        &request(42, 0, |writer| {
            writer.i32(2).string(Some("billing")).string(Some("nobody"));
        }),
    );
    let mut reader = Reader::new(&deleted);
    assert_eq!(reader.i32().unwrap(), 0);
    assert_eq!(reader.i32().unwrap(), 2);
    assert_eq!(reader.string().unwrap().as_deref(), Some("billing"));
    assert_eq!(reader.i16().unwrap(), 0);
    assert_eq!(reader.string().unwrap().as_deref(), Some("nobody"));
    assert_eq!(reader.i16().unwrap(), 69);
}

#[test]
fn init_producer_id_hands_out_new_ids_and_refuses_transactional_ids() {
    let keyspace = MemoryKeyspace::default();
    for expected in [0i64, 1] {
        let body = response(
            &keyspace,
            &request(22, 0, |writer| {
                writer.string(None).i32(60_000);
            }),
        );
        let mut reader = Reader::new(&body);
        assert_eq!(reader.i32().unwrap(), 0);
        assert_eq!(reader.i16().unwrap(), 0);
        assert_eq!(reader.i64().unwrap(), expected);
        assert_eq!(reader.i16().unwrap(), 0);
    }
    let refused = response(
        &keyspace,
        &request(22, 0, |writer| {
            writer.string(Some("payments")).i32(60_000);
        }),
    );
    let mut reader = Reader::new(&refused);
    assert_eq!(reader.i32().unwrap(), 0);
    assert_eq!(reader.i16().unwrap(), 42);
}

fn flexible_request(key: i16, version: i16, body: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut writer = Writer::default();
    writer.i16(key).i16(version).i32(7).string(Some("test")).no_tags();
    body(&mut writer);
    writer.out
}

#[test]
fn metadata_version_12_carries_topic_ids_in_the_flexible_encoding() {
    let keyspace = MemoryKeyspace::default();
    let wanted = flexible_request(3, 12, |writer| {
        writer.compact_count(Some(1)).uuid(&[0; 16]).compact_string(Some("orders")).no_tags();
        writer.i8(1).i8(0).no_tags();
    });
    let body = response(&keyspace, &wanted);
    let mut reader = Reader::new(&body);
    reader.skip_tags().unwrap();
    assert_eq!(reader.i32().unwrap(), 0);
    let brokers = reader
        .compact_array(|reader| {
            let id = reader.i32()?;
            let _host = reader.compact_string()?;
            let _port = reader.i32()?;
            let _rack = reader.compact_string()?;
            reader.skip_tags()?;
            Ok(id)
        })
        .unwrap()
        .unwrap();
    assert_eq!(brokers.len(), 1);
    assert_eq!(reader.compact_string().unwrap().as_deref(), Some("eggdb"));
    reader.i32().unwrap();
    assert_eq!(reader.uvarint().unwrap(), 2);
    assert_eq!(reader.i16().unwrap(), 0);
    assert_eq!(reader.compact_string().unwrap().as_deref(), Some("orders"));
    assert_eq!(reader.uuid().unwrap(), topic_id("orders"));
    assert_ne!(topic_id("orders"), topic_id("order"));
}

#[test]
fn a_consumer_joining_with_the_new_protocol_gets_its_partitions_from_the_coordinator() {
    let keyspace = MemoryKeyspace::default();
    response(
        &keyspace,
        &flexible_request(3, 12, |writer| {
            writer.compact_count(Some(1)).uuid(&[0; 16]).compact_string(Some("orders")).no_tags();
            writer.i8(1).i8(0).no_tags();
        }),
    );
    let join = flexible_request(68, 0, |writer| {
        writer
            .compact_string(Some("billing"))
            .compact_string(Some(""))
            .i32(0)
            .compact_string(None)
            .compact_string(None)
            .i32(60_000)
            .compact_count(Some(1))
            .compact_string(Some("orders"))
            .compact_string(Some("range"))
            .compact_count(Some(0))
            .no_tags();
    });
    let body = response(&keyspace, &join);
    let mut reader = Reader::new(&body);
    reader.skip_tags().unwrap();
    assert_eq!(reader.i32().unwrap(), 0);
    assert_eq!(reader.i16().unwrap(), 0);
    assert_eq!(reader.compact_string().unwrap(), None);
    assert!(reader.compact_string().unwrap().unwrap().starts_with("eggdb-member-"));
    assert_eq!(reader.i32().unwrap(), 1);
    assert_eq!(reader.i32().unwrap(), 5_000);
    assert_eq!(reader.i8().unwrap(), 1);
    let assigned = reader
        .compact_array(|reader| {
            let id = reader.uuid()?;
            let partitions = reader.compact_array(|reader| reader.i32())?.unwrap_or_default();
            reader.skip_tags()?;
            Ok((id, partitions))
        })
        .unwrap()
        .unwrap();
    assert_eq!(assigned.len(), 1);
    assert_eq!(assigned[0].0, topic_id("orders"));
    assert!(!assigned[0].1.is_empty());
    let unknown = flexible_request(68, 0, |writer| {
        writer
            .compact_string(Some("billing"))
            .compact_string(Some("nobody"))
            .i32(3)
            .compact_string(None)
            .compact_string(None)
            .i32(60_000)
            .compact_count(None)
            .compact_string(None)
            .compact_count(None)
            .no_tags();
    });
    let refused = response(&keyspace, &unknown);
    let mut reader = Reader::new(&refused);
    reader.skip_tags().unwrap();
    reader.i32().unwrap();
    assert_eq!(reader.i16().unwrap(), 25);
}

