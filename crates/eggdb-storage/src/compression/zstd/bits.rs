pub(super) type Problem = &'static str;

pub(super) struct BackwardBits<'a> {
    bytes: &'a [u8],
    position: i64,
}

impl<'a> BackwardBits<'a> {
    pub(super) fn new(bytes: &'a [u8]) -> Result<Self, Problem> {
        let last = *bytes.last().ok_or("a bitstream is empty")?;
        if last == 0 {
            return Err("a bitstream's last byte has no end marker");
        }
        let padding = i64::from(last.leading_zeros()) + 1;
        Ok(Self {
            bytes,
            position: bytes.len() as i64 * 8 - padding,
        })
    }

    pub(super) fn peek(&self, count: u32) -> u64 {
        if count == 0 {
            return 0;
        }
        let start = self.position - i64::from(count);
        let mut value = 0_u64;
        for index in 0..i64::from(count) {
            let bit = start + index;
            if bit < 0 {
                continue;
            }
            let byte = self.bytes[(bit / 8) as usize];
            if byte >> (bit % 8) & 1 == 1 {
                value |= 1 << index;
            }
        }
        value
    }

    pub(super) fn read(&mut self, count: u32) -> u64 {
        let value = self.peek(count);
        self.position -= i64::from(count);
        value
    }

    pub(super) fn skip(&mut self, count: u32) {
        self.position -= i64::from(count);
    }

    pub(super) fn overflowed(&self) -> bool {
        self.position < 0
    }

    pub(super) fn finished_exactly(&self) -> bool {
        self.position == 0
    }
}

pub(super) struct ForwardBits<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> ForwardBits<'a> {
    pub(super) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    pub(super) fn read(&mut self, count: u32) -> Result<u32, Problem> {
        let mut value = 0_u32;
        for index in 0..count {
            let bit = self.position + index as usize;
            let byte = *self
                .bytes
                .get(bit / 8)
                .ok_or("a table description runs past its block")?;
            if byte >> (bit % 8) & 1 == 1 {
                value |= 1 << index;
            }
        }
        self.position += count as usize;
        Ok(value)
    }

    pub(super) fn peek(&self, count: u32) -> u32 {
        let mut value = 0_u32;
        for index in 0..count {
            let bit = self.position + index as usize;
            if let Some(byte) = self.bytes.get(bit / 8)
                && byte >> (bit % 8) & 1 == 1
            {
                value |= 1 << index;
            }
        }
        value
    }

    pub(super) fn skip(&mut self, count: u32) -> Result<(), Problem> {
        self.position += count as usize;
        if self.position > self.bytes.len() * 8 {
            return Err("a table description runs past its block");
        }
        Ok(())
    }

    pub(super) fn bytes_used(&self) -> usize {
        self.position.div_ceil(8)
    }
}

#[derive(Default)]
pub(super) struct BitWriter {
    bytes: Vec<u8>,
    pending: u64,
    pending_bits: u32,
}

impl BitWriter {
    pub(super) fn add(&mut self, value: u64, count: u32) {
        if count == 0 {
            return;
        }
        let value = value & ((1_u64 << count) - 1);
        self.pending |= value << self.pending_bits;
        self.pending_bits += count;
        while self.pending_bits >= 8 {
            self.bytes.push(self.pending as u8);
            self.pending >>= 8;
            self.pending_bits -= 8;
        }
    }

    pub(super) fn close(mut self) -> Vec<u8> {
        self.add(1, 1);
        if self.pending_bits > 0 {
            self.bytes.push(self.pending as u8);
        }
        self.bytes
    }
}

pub(super) fn highest_bit(value: u32) -> u32 {
    31 - value.leading_zeros()
}
