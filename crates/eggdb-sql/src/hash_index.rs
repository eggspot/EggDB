use crate::error::SqlResult;
use crate::memory::WorkMemory;
use crate::types::{Value, equality_key};
use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

const END: usize = usize::MAX;

#[derive(Clone)]
pub(crate) struct SeededIntegerHash {
    seed: u64,
}

impl Default for SeededIntegerHash {
    fn default() -> Self {
        Self {
            seed: RandomState::new().hash_one(0x5EED_u64) | 1,
        }
    }
}

pub(crate) struct IntegerHasher {
    seed: u64,
    state: u64,
}

impl Hasher for IntegerHasher {
    fn finish(&self) -> u64 {
        let mut mixed = self.state ^ self.seed;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        mixed ^ (mixed >> 31)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.state =
                (self.state.rotate_left(8) ^ u64::from(byte)).wrapping_mul(0x100_0000_01B3);
        }
    }

    fn write_i64(&mut self, value: i64) {
        self.state = value as u64;
    }
}

impl BuildHasher for SeededIntegerHash {
    type Hasher = IntegerHasher;

    fn build_hasher(&self) -> IntegerHasher {
        IntegerHasher {
            seed: self.seed,
            state: 0,
        }
    }
}

enum Heads {
    Integer(HashMap<i64, usize, SeededIntegerHash>),
    Bytes(HashMap<Vec<u8>, usize>),
}

pub(crate) struct HashIndex {
    heads: Heads,
    next: Vec<usize>,
    values: Vec<Value>,
    width: usize,
    key: Vec<u8>,
}

fn single_integer(row: &[Value], offsets: &[usize]) -> Option<Option<i64>> {
    match offsets {
        [offset] => match &row[*offset] {
            Value::Null => Some(None),
            value => value.as_integer().map(Some),
        },
        _ => None,
    }
}

impl HashIndex {
    pub(crate) fn new(integer_keys: bool, width: usize, expected_rows: usize) -> Self {
        Self {
            heads: if integer_keys {
                Heads::Integer(HashMap::with_capacity_and_hasher(
                    expected_rows,
                    SeededIntegerHash::default(),
                ))
            } else {
                Heads::Bytes(HashMap::with_capacity(expected_rows))
            },
            next: Vec::with_capacity(expected_rows),
            values: Vec::with_capacity(expected_rows * width),
            width,
            key: Vec::new(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.next.len()
    }

    pub(crate) fn insert(
        &mut self,
        row: &mut Vec<Value>,
        offsets: &[usize],
        memory: &mut WorkMemory,
    ) -> SqlResult<()> {
        let at = self.next.len();
        let previous = match &mut self.heads {
            Heads::Integer(heads) => match single_integer(row, offsets) {
                Some(Some(key)) => {
                    memory.charge(24)?;
                    heads.insert(key, at)
                }
                _ => return Ok(()),
            },
            Heads::Bytes(heads) => {
                if !bytes_key(row, offsets, &mut self.key) {
                    return Ok(());
                }
                memory.charge(self.key.len() + 32)?;
                heads.insert(self.key.clone(), at)
            }
        };
        memory.charge_row(row)?;
        self.next.push(previous.unwrap_or(END));
        self.values.append(row);
        Ok(())
    }

    pub(crate) fn first(&self, row: &[Value], offsets: &[usize], key: &mut Vec<u8>) -> usize {
        let found = match &self.heads {
            Heads::Integer(heads) => match single_integer(row, offsets) {
                Some(Some(probe)) => heads.get(&probe),
                _ => None,
            },
            Heads::Bytes(heads) => {
                if bytes_key(row, offsets, key) {
                    heads.get(key.as_slice())
                } else {
                    None
                }
            }
        };
        found.copied().unwrap_or(END)
    }

    pub(crate) fn row(&self, at: usize) -> &[Value] {
        &self.values[at * self.width..(at + 1) * self.width]
    }

    pub(crate) fn following(&self, at: usize) -> usize {
        self.next.get(at).copied().unwrap_or(END)
    }

    pub(crate) fn is_end(at: usize) -> bool {
        at == END
    }
}

fn bytes_key(row: &[Value], offsets: &[usize], key: &mut Vec<u8>) -> bool {
    key.clear();
    for &offset in offsets {
        let value = &row[offset];
        if value.is_null() {
            return false;
        }
        equality_key(key, value);
    }
    true
}
