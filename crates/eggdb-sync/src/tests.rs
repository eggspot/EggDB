use crate::{Change, Cursors, Replica, Stamp, decode, encode, sync};
use eggdb_keyspace::MemoryKeyspace;
use std::collections::BTreeMap;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn changes_round_trip_as_bytes_and_damage_is_refused() {
    let changes = vec![
        Change { key: b"a".to_vec(), value: Some(b"1".to_vec()), stamp: Stamp { millis: 5, counter: 2, replica: 9 } },
        Change { key: Vec::new(), value: None, stamp: Stamp::default() },
    ];
    let mut bytes = Vec::new();
    encode(&changes, &mut bytes);
    assert_eq!(decode(&bytes).unwrap(), changes);
    assert!(decode(&bytes[..bytes.len() - 1]).is_err());
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(decode(&trailing).is_err());
    assert!(decode(&[0xff, 0xff, 0xff, 0xff]).is_err());
}

#[test]
fn disconnected_replicas_converge_with_the_newest_write_per_key() {
    for seed in 1..=30u64 {
        let server_space = MemoryKeyspace::default();
        let server = Replica::new(&server_space, 0, 1);
        let spaces: Vec<MemoryKeyspace> = (0..4).map(|_| MemoryKeyspace::default()).collect();
        let clients: Vec<Replica<'_>> = spaces.iter().enumerate().map(|(index, space)| Replica::new(space, 0, 10 + index as u64)).collect();
        let mut cursors = vec![Cursors::default(); clients.len()];
        let mut rng = Rng(seed);
        let mut newest: BTreeMap<Vec<u8>, (Stamp, Option<Vec<u8>>)> = BTreeMap::new();
        let mut clocks = vec![1_000u64; clients.len()];
        for _ in 0..300 {
            let index = (rng.next() % clients.len() as u64) as usize;
            clocks[index] += rng.next() % 5;
            if rng.next() % 4 == 0 {
                sync(&clients[index], &server, &mut cursors[index], 1 + (rng.next() % 7) as usize).unwrap();
            } else {
                let key = vec![b'k', (rng.next() % 8) as u8];
                let value = (rng.next() % 5 != 0).then(|| rng.next().to_le_bytes().to_vec());
                let stamp = clients[index].write(&key, value.as_deref(), clocks[index]).unwrap();
                let entry = newest.entry(key).or_insert((Stamp::default(), None));
                if stamp > entry.0 {
                    *entry = (stamp, value);
                }
            }
        }
        for _ in 0..3 {
            for (index, client) in clients.iter().enumerate() {
                loop {
                    let before = cursors[index];
                    sync(client, &server, &mut cursors[index], 64).unwrap();
                    if cursors[index] == before {
                        break;
                    }
                }
            }
        }
        let expected: Vec<(Vec<u8>, Vec<u8>)> = newest.iter().filter_map(|(key, (_, value))| value.clone().map(|value| (key.clone(), value))).collect();
        assert_eq!(server.rows().unwrap(), expected, "seed {seed}: the server");
        for client in &clients {
            assert_eq!(client.rows().unwrap(), expected, "seed {seed}: a client");
        }
    }
}

#[test]
fn a_replica_keeps_its_offline_writes_and_clock_across_reopening() {
    let space = MemoryKeyspace::default();
    let first = Replica::new(&space, 0, 7).write(b"note", Some(b"draft"), 100).unwrap();
    let reopened = Replica::new(&space, 0, 7);
    let second = reopened.write(b"note", Some(b"final"), 50).unwrap();
    assert!(second > first, "a clock that went back must not reorder writes");
    assert_eq!(reopened.get(b"note").unwrap(), Some(b"final".to_vec()));
    assert_eq!(reopened.changes_after(0, 10).unwrap().1, 2);
    reopened.write(b"note", None, 200).unwrap();
    assert_eq!(reopened.get(b"note").unwrap(), None);
    assert!(reopened.rows().unwrap().is_empty());
}
