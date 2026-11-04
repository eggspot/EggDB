use crate::commands::{Context, WRONG_TYPE, load, reading, save, writing};
use crate::protocol::Reply;
use crate::value::{Data, Entry};

const NAMES: &[&str] = &["pfadd", "pfcount", "pfmerge"];

const MAGIC: &[u8; 4] = b"HYLL";
const HEADER_BYTES: usize = 16;
const DENSE: u8 = 0;
const SPARSE: u8 = 1;
const PRECISION_BITS: u32 = 14;
const REGISTERS: usize = 1 << PRECISION_BITS;
const REGISTER_BITS: usize = 6;
const REGISTER_MAX: u16 = 0x3F;
const DENSE_BODY_BYTES: usize = REGISTERS * REGISTER_BITS / 8;
const HASH_BITS_LEFT: u32 = 64 - PRECISION_BITS;
const HIGHEST_COUNT: usize = HASH_BITS_LEFT as usize + 1;
const SPARSE_MAX_BYTES: usize = 3000;
const SPARSE_MAX_VALUE: u8 = 32;
const SPARSE_MAX_VALUE_RUN: usize = 4;
const SPARSE_MAX_ZERO_RUN: usize = 64;
const SPARSE_MAX_LONG_ZERO_RUN: usize = 16384;
const SPARSE_VALUE: u8 = 0x80;
const SPARSE_LONG_ZERO: u8 = 0x40;
const STALE_CARDINALITY: u8 = 0x80;
const HASH_SEED: u64 = 0xadc8_3b19;

const NOT_A_SKETCH: &str = "WRONGTYPE Key is not a valid HyperLogLog string value.";
const CORRUPTED: &str = "INVALIDOBJ Corrupted HLL object detected";

pub(crate) fn is_known(name: &str) -> bool {
    NAMES.contains(&name)
}

pub(crate) fn execute(
    context: &Context<'_>,
    db: u32,
    name: &str,
    args: &[Vec<u8>],
) -> Option<Reply> {
    Some(match (name, args.len()) {
        ("pfadd", count) if count >= 1 => pfadd(context, db, args),
        ("pfcount", 1) => pfcount_one(context, db, &args[0]),
        ("pfcount", count) if count >= 2 => pfcount_many(context, db, args),
        ("pfmerge", count) if count >= 1 => pfmerge(context, db, args),
        _ => return None,
    })
}

fn murmur_hash_64a(data: &[u8], seed: u64) -> u64 {
    const MULTIPLIER: u64 = 0xc6a4_a793_5bd1_e995;
    const SHIFT: u32 = 47;
    let mut hash = seed ^ (data.len() as u64).wrapping_mul(MULTIPLIER);
    let mut blocks = data.chunks_exact(8);
    for block in &mut blocks {
        let mut word = [0u8; 8];
        word.copy_from_slice(block);
        let mut k = u64::from_le_bytes(word).wrapping_mul(MULTIPLIER);
        k ^= k >> SHIFT;
        k = k.wrapping_mul(MULTIPLIER);
        hash ^= k;
        hash = hash.wrapping_mul(MULTIPLIER);
    }
    let tail = blocks.remainder();
    if !tail.is_empty() {
        for (index, byte) in tail.iter().enumerate() {
            hash ^= u64::from(*byte) << (8 * index);
        }
        hash = hash.wrapping_mul(MULTIPLIER);
    }
    hash ^= hash >> SHIFT;
    hash = hash.wrapping_mul(MULTIPLIER);
    hash ^ (hash >> SHIFT)
}

fn register_and_count(element: &[u8]) -> (usize, u8) {
    let hash = murmur_hash_64a(element, HASH_SEED);
    let register = (hash & (REGISTERS as u64 - 1)) as usize;
    let pattern = (hash >> PRECISION_BITS) | (1 << HASH_BITS_LEFT);
    (register, pattern.trailing_zeros() as u8 + 1)
}

fn sigma(mut x: f64) -> f64 {
    if x == 1.0 {
        return f64::INFINITY;
    }
    let (mut y, mut z) = (1.0, x);
    for _ in 0..1100 {
        x *= x;
        let previous = z;
        z += x * y;
        y += y;
        if previous == z {
            break;
        }
    }
    z
}

fn tau(mut x: f64) -> f64 {
    if x == 0.0 || x == 1.0 {
        return 0.0;
    }
    let (mut y, mut z) = (1.0, 1.0 - x);
    for _ in 0..1100 {
        x = x.sqrt();
        let previous = z;
        y *= 0.5;
        z -= (1.0 - x) * (1.0 - x) * y;
        if previous == z {
            break;
        }
    }
    z / 3.0
}

fn estimate(registers: &[u8]) -> u64 {
    let mut histogram = [0u32; 64];
    for register in registers {
        if let Some(slot) = histogram.get_mut(usize::from(*register)) {
            *slot += 1;
        }
    }
    let count_of = |value: usize| f64::from(histogram.get(value).copied().unwrap_or(0));
    let m = REGISTERS as f64;
    let mut z = m * tau((m - count_of(HIGHEST_COUNT)) / m);
    for value in (1..HIGHEST_COUNT).rev() {
        z += count_of(value);
        z *= 0.5;
    }
    z += m * sigma(count_of(0) / m);
    let alpha_infinity = 1.0 / (2.0 * std::f64::consts::LN_2);
    (alpha_infinity * m * m / z).round() as u64
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Sketch {
    registers: Vec<u8>,
    dense: bool,
    cardinality: [u8; 8],
}

impl Sketch {
    fn empty() -> Self {
        Self {
            registers: vec![0; REGISTERS],
            dense: false,
            cardinality: [0; 8],
        }
    }

    fn decode(bytes: &[u8]) -> Result<Self, String> {
        let not_a_sketch = || NOT_A_SKETCH.to_owned();
        let header = bytes.get(..HEADER_BYTES).ok_or_else(not_a_sketch)?;
        if header.get(..4) != Some(MAGIC.as_slice()) {
            return Err(not_a_sketch());
        }
        let mut cardinality = [0u8; 8];
        cardinality.copy_from_slice(header.get(8..HEADER_BYTES).ok_or_else(not_a_sketch)?);
        let body = bytes.get(HEADER_BYTES..).unwrap_or(&[]);
        let (registers, dense) = match header.get(4).copied() {
            Some(DENSE) if body.len() == DENSE_BODY_BYTES => (dense_registers(body), true),
            Some(SPARSE) => (
                sparse_registers(body).ok_or_else(|| CORRUPTED.to_owned())?,
                false,
            ),
            _ => return Err(not_a_sketch()),
        };
        Ok(Self {
            registers,
            dense,
            cardinality,
        })
    }

    fn from_entry(entry: Option<&Entry>) -> Result<Option<Self>, String> {
        match entry.map(|entry| &entry.data) {
            None => Ok(None),
            Some(Data::String(bytes)) => Self::decode(bytes).map(Some),
            Some(_) => Err(WRONG_TYPE.to_owned()),
        }
    }

    fn encode(&mut self) -> Vec<u8> {
        let sparse = if self.dense {
            None
        } else {
            sparse_body(&self.registers)
        };
        let body = match sparse {
            Some(body) => body,
            None => {
                self.dense = true;
                dense_body(&self.registers)
            }
        };
        let mut out = Vec::with_capacity(HEADER_BYTES + body.len());
        out.extend_from_slice(MAGIC);
        out.push(if self.dense { DENSE } else { SPARSE });
        out.extend_from_slice(&[0, 0, 0]);
        out.extend_from_slice(&self.cardinality);
        out.extend_from_slice(&body);
        out
    }

    fn add(&mut self, element: &[u8]) -> bool {
        let (register, count) = register_and_count(element);
        match self.registers.get_mut(register) {
            Some(slot) if *slot < count => {
                *slot = count;
                true
            }
            _ => false,
        }
    }

    fn merge(&mut self, other: &Self) {
        for (mine, theirs) in self.registers.iter_mut().zip(&other.registers) {
            *mine = (*mine).max(*theirs);
        }
    }

    fn cached_cardinality(&self) -> Option<u64> {
        (self.cardinality[7] & STALE_CARDINALITY == 0).then(|| u64::from_le_bytes(self.cardinality))
    }

    fn mark_cardinality_stale(&mut self) {
        self.cardinality[7] |= STALE_CARDINALITY;
    }
}

fn dense_registers(body: &[u8]) -> Vec<u8> {
    (0..REGISTERS)
        .map(|register| {
            let bit = register * REGISTER_BITS;
            let (byte, shift) = (bit / 8, bit % 8);
            let low = u16::from(body.get(byte).copied().unwrap_or(0));
            let high = u16::from(body.get(byte + 1).copied().unwrap_or(0));
            (((low | (high << 8)) >> shift) & REGISTER_MAX) as u8
        })
        .collect()
}

fn dense_body(registers: &[u8]) -> Vec<u8> {
    let mut body = vec![0u8; DENSE_BODY_BYTES];
    for (register, value) in registers.iter().enumerate() {
        let bit = register * REGISTER_BITS;
        let (byte, shift) = (bit / 8, bit % 8);
        let wide = (u16::from(*value) & REGISTER_MAX) << shift;
        if let Some(low) = body.get_mut(byte) {
            *low |= (wide & 0xFF) as u8;
        }
        if let Some(high) = body.get_mut(byte + 1) {
            *high |= (wide >> 8) as u8;
        }
    }
    body
}

fn sparse_registers(body: &[u8]) -> Option<Vec<u8>> {
    let mut registers = Vec::with_capacity(REGISTERS);
    let mut at = 0;
    while let Some(&opcode) = body.get(at) {
        let (value, run) = if opcode & SPARSE_VALUE != 0 {
            at += 1;
            (((opcode >> 2) & 0x1F) + 1, usize::from(opcode & 0x03) + 1)
        } else if opcode & SPARSE_LONG_ZERO != 0 {
            let low = *body.get(at + 1)?;
            at += 2;
            (0, ((usize::from(opcode & 0x3F) << 8) | usize::from(low)) + 1)
        } else {
            at += 1;
            (0, usize::from(opcode & 0x3F) + 1)
        };
        let filled = registers.len() + run;
        if filled > REGISTERS {
            return None;
        }
        registers.resize(filled, value);
    }
    (registers.len() == REGISTERS).then_some(registers)
}

fn sparse_body(registers: &[u8]) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    let mut at = 0;
    while let Some(&value) = registers.get(at) {
        let run = registers
            .get(at..)
            .unwrap_or(&[])
            .iter()
            .take_while(|register| **register == value)
            .count();
        at += run;
        if value > SPARSE_MAX_VALUE {
            return None;
        }
        let mut left = run;
        while left > 0 {
            if value == 0 {
                let chunk = left.min(SPARSE_MAX_LONG_ZERO_RUN);
                if chunk <= SPARSE_MAX_ZERO_RUN {
                    body.push((chunk - 1) as u8);
                } else {
                    body.push(SPARSE_LONG_ZERO | ((chunk - 1) >> 8) as u8);
                    body.push(((chunk - 1) & 0xFF) as u8);
                }
                left -= chunk;
            } else {
                let chunk = left.min(SPARSE_MAX_VALUE_RUN);
                body.push(SPARSE_VALUE | ((value - 1) << 2) | (chunk - 1) as u8);
                left -= chunk;
            }
        }
        if HEADER_BYTES + body.len() > SPARSE_MAX_BYTES {
            return None;
        }
    }
    Some(body)
}

fn stored(sketch: &mut Sketch, expires_at_ms: Option<u64>) -> Entry {
    Entry {
        data: Data::String(sketch.encode()),
        expires_at_ms,
    }
}

fn pfadd(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    writing(context, db, |keys| {
        let entry = load(keys, &args[0], now)?;
        let expires_at_ms = entry.as_ref().and_then(|entry| entry.expires_at_ms);
        let (mut sketch, mut changed) = match Sketch::from_entry(entry.as_ref())? {
            Some(sketch) => (sketch, false),
            None => (Sketch::empty(), true),
        };
        for element in &args[1..] {
            changed |= sketch.add(element);
        }
        if changed {
            sketch.mark_cardinality_stale();
            save(keys, &args[0], &stored(&mut sketch, expires_at_ms))?;
        }
        Ok(Reply::Integer(i64::from(changed)))
    })
}

fn pfcount_one(context: &Context<'_>, db: u32, key: &[u8]) -> Reply {
    let now = context.now_ms;
    writing(context, db, |keys| {
        let entry = load(keys, key, now)?;
        let expires_at_ms = entry.as_ref().and_then(|entry| entry.expires_at_ms);
        let Some(mut sketch) = Sketch::from_entry(entry.as_ref())? else {
            return Ok(Reply::Integer(0));
        };
        if let Some(cardinality) = sketch.cached_cardinality() {
            return Ok(Reply::Integer(cardinality as i64));
        }
        let cardinality = estimate(&sketch.registers);
        sketch.cardinality = cardinality.to_le_bytes();
        save(keys, key, &stored(&mut sketch, expires_at_ms))?;
        Ok(Reply::Integer(cardinality as i64))
    })
}

fn pfcount_many(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    reading(context, db, |keys| {
        let mut union = Sketch::empty();
        for key in args {
            if let Some(sketch) = Sketch::from_entry(load(keys, key, now)?.as_ref())? {
                union.merge(&sketch);
            }
        }
        Ok(Reply::Integer(estimate(&union.registers) as i64))
    })
}

fn pfmerge(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    writing(context, db, |keys| {
        let mut union = Sketch::empty();
        let mut any_dense = false;
        let mut destination_expiry = None;
        let mut destination_cardinality = None;
        for (index, key) in args.iter().enumerate() {
            let entry = load(keys, key, now)?;
            if let Some(sketch) = Sketch::from_entry(entry.as_ref())? {
                any_dense |= sketch.dense;
                union.merge(&sketch);
                if index == 0 {
                    destination_expiry = entry.and_then(|entry| entry.expires_at_ms);
                    destination_cardinality = Some(sketch.cardinality);
                }
            }
        }
        union.dense = any_dense;
        if let Some(cardinality) = destination_cardinality {
            union.cardinality = cardinality;
        }
        union.mark_cardinality_stale();
        save(keys, &args[0], &stored(&mut union, destination_expiry))?;
        Ok(Reply::ok())
    })
}
