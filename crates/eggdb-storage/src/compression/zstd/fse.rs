use super::bits::{BackwardBits, ForwardBits, Problem, highest_bit};

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Entry {
    pub(super) symbol: u8,
    pub(super) bits: u8,
    pub(super) base: u16,
}

#[derive(Debug, Clone, Default)]
pub(super) struct DecodeTable {
    pub(super) accuracy: u32,
    pub(super) entries: Vec<Entry>,
}

pub(super) fn read_distribution(
    bytes: &[u8],
    most_accuracy: u32,
    largest_symbol: usize,
) -> Result<(Vec<i16>, u32, usize), Problem> {
    let mut bits = ForwardBits::new(bytes);
    let accuracy = bits.read(4)? + 5;
    if accuracy > most_accuracy {
        return Err("a table's accuracy is above its limit");
    }
    let mut remaining: i32 = (1 << accuracy) + 1;
    let mut threshold: i32 = 1 << accuracy;
    let mut width = accuracy + 1;
    let mut counts: Vec<i16> = Vec::new();
    let mut previous_zero = false;
    while remaining > 1 {
        if counts.len() > largest_symbol {
            return Err("a table describes more symbols than its kind has");
        }
        if previous_zero {
            loop {
                let run = bits.read(2)?;
                for _ in 0..run {
                    counts.push(0);
                }
                if counts.len() > largest_symbol + 1 {
                    return Err("a table's run of zeros passes its last symbol");
                }
                if run != 3 {
                    break;
                }
            }
            if counts.len() > largest_symbol {
                return Err("a table describes more symbols than its kind has");
            }
        }
        let most = (2 * threshold - 1) - remaining;
        let low = bits.peek(width - 1) as i32;
        let mut count = if low < most {
            bits.skip(width - 1)?;
            low
        } else {
            let value = bits.read(width)? as i32;
            if value >= threshold {
                value - most
            } else {
                value
            }
        };
        count -= 1;
        remaining -= count.abs();
        counts.push(count as i16);
        previous_zero = count == 0;
        while remaining < threshold && threshold > 1 {
            width -= 1;
            threshold >>= 1;
        }
    }
    if remaining != 1 {
        return Err("a table's probabilities do not add up");
    }
    Ok((counts, accuracy, bits.bytes_used()))
}

pub(super) fn spread(counts: &[i16], accuracy: u32) -> Result<Vec<u8>, Problem> {
    let size = 1_usize << accuracy;
    let mut symbols = vec![0_u8; size];
    let mut high = size as i64 - 1;
    for (symbol, &count) in counts.iter().enumerate() {
        if count == -1 {
            if high < 0 {
                return Err("a table has more rare symbols than states");
            }
            symbols[high as usize] = symbol as u8;
            high -= 1;
        }
    }
    let mask = size - 1;
    let step = (size >> 1) + (size >> 3) + 3;
    let mut position = 0_usize;
    for (symbol, &count) in counts.iter().enumerate() {
        for _ in 0..count.max(0) {
            symbols[position] = symbol as u8;
            loop {
                position = (position + step) & mask;
                if (position as i64) <= high {
                    break;
                }
            }
        }
    }
    if position != 0 {
        return Err("a table's probabilities do not fill its states");
    }
    Ok(symbols)
}

pub(super) fn decode_table(counts: &[i16], accuracy: u32) -> Result<DecodeTable, Problem> {
    let size = 1_u32 << accuracy;
    let symbols = spread(counts, accuracy)?;
    let mut next: Vec<u32> = counts
        .iter()
        .map(|&count| if count == -1 { 1 } else { count.max(0) as u32 })
        .collect();
    let mut entries = Vec::with_capacity(size as usize);
    for &symbol in &symbols {
        let slot = next
            .get_mut(symbol as usize)
            .ok_or("a table names a symbol it has no count for")?;
        let state = *slot;
        *slot += 1;
        if state == 0 {
            return Err("a table's state has no count");
        }
        let bits = accuracy - highest_bit(state);
        let base = (state << bits).wrapping_sub(size);
        entries.push(Entry {
            symbol,
            bits: bits as u8,
            base: base as u16,
        });
    }
    Ok(DecodeTable { accuracy, entries })
}

pub(super) fn rle_table(symbol: u8) -> DecodeTable {
    DecodeTable {
        accuracy: 0,
        entries: vec![Entry {
            symbol,
            bits: 0,
            base: 0,
        }],
    }
}

pub(super) struct DecodeState<'t> {
    table: &'t DecodeTable,
    state: usize,
}

impl<'t> DecodeState<'t> {
    pub(super) fn new(table: &'t DecodeTable, bits: &mut BackwardBits<'_>) -> Self {
        let state = bits.read(table.accuracy) as usize;
        Self { table, state }
    }

    pub(super) fn symbol(&self) -> u8 {
        self.table.entries[self.state].symbol
    }

    pub(super) fn update(&mut self, bits: &mut BackwardBits<'_>) {
        let entry = self.table.entries[self.state];
        let low = bits.read(u32::from(entry.bits)) as usize;
        self.state = (usize::from(entry.base) + low) & (self.table.entries.len() - 1);
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct SymbolTransform {
    find_state: i32,
    nb_bits: u32,
}

pub(super) struct EncodeTable {
    accuracy: u32,
    next_states: Vec<u16>,
    transforms: Vec<SymbolTransform>,
}

impl EncodeTable {
    pub(super) fn new(counts: &[i16], accuracy: u32) -> Self {
        let size = 1_u32 << accuracy;
        let symbols = spread(counts, accuracy).unwrap_or_default();
        let mut cumulative = vec![0_u32; counts.len() + 1];
        for (symbol, &count) in counts.iter().enumerate() {
            let width = if count == -1 { 1 } else { count.max(0) as u32 };
            cumulative[symbol + 1] = cumulative[symbol] + width;
        }
        let mut next_states = vec![0_u16; size as usize];
        let mut fill = cumulative.clone();
        for (position, &symbol) in symbols.iter().enumerate() {
            let slot = &mut fill[symbol as usize];
            next_states[*slot as usize] = (size + position as u32) as u16;
            *slot += 1;
        }
        let mut transforms = vec![SymbolTransform::default(); counts.len()];
        let mut total = 0_i32;
        for (symbol, &count) in counts.iter().enumerate() {
            transforms[symbol] = match count {
                0 => SymbolTransform {
                    find_state: 0,
                    nb_bits: ((accuracy + 1) << 16).wrapping_sub(size),
                },
                -1 | 1 => {
                    let transform = SymbolTransform {
                        find_state: total - 1,
                        nb_bits: (accuracy << 16).wrapping_sub(size),
                    };
                    total += 1;
                    transform
                }
                _ => {
                    let count = count as u32;
                    let most_bits_out = accuracy - highest_bit(count - 1);
                    let least_state_plus = count << most_bits_out;
                    let transform = SymbolTransform {
                        find_state: total - count as i32,
                        nb_bits: (most_bits_out << 16).wrapping_sub(least_state_plus),
                    };
                    total += count as i32;
                    transform
                }
            };
        }
        Self {
            accuracy,
            next_states,
            transforms,
        }
    }

    pub(super) fn start(&self, symbol: u8) -> u32 {
        let transform = self.transforms[symbol as usize];
        let bits_out = (transform.nb_bits.wrapping_add(1 << 15)) >> 16;
        let value = (bits_out << 16).wrapping_sub(transform.nb_bits);
        let index = (value >> bits_out) as i32 + transform.find_state;
        u32::from(self.next_states[index as usize])
    }

    pub(super) fn encode(&self, state: &mut u32, symbol: u8, out: &mut super::bits::BitWriter) {
        let transform = self.transforms[symbol as usize];
        let bits_out = (state.wrapping_add(transform.nb_bits)) >> 16;
        out.add(u64::from(*state), bits_out);
        let index = (*state >> bits_out) as i32 + transform.find_state;
        *state = u32::from(self.next_states[index as usize]);
    }

    pub(super) fn flush(&self, state: u32, out: &mut super::bits::BitWriter) {
        out.add(u64::from(state), self.accuracy);
    }
}
