pub(crate) struct Builder {
    reversed: Vec<u8>,
}

#[derive(Clone, Copy)]
pub(crate) struct Offset(usize);

pub(crate) enum Field {
    Bool(bool),
    U8(u8),
    I16(i16),
    I32(i32),
    I64(i64),
    Offset(Offset),
}

impl Builder {
    pub(crate) fn new() -> Self {
        Self { reversed: Vec::new() }
    }

    fn len(&self) -> usize {
        self.reversed.len()
    }

    fn align(&mut self, size: usize, additional: usize) {
        while !(self.len() + additional).is_multiple_of(size) {
            self.reversed.push(0);
        }
    }

    fn prepend(&mut self, bytes: &[u8]) {
        self.reversed.extend(bytes.iter().rev());
    }

    fn prepend_offset(&mut self, target: Offset) {
        self.align(4, 4);
        let position = self.len() + 4;
        let value = u32::try_from(position - target.0).unwrap_or(0);
        self.prepend(&value.to_le_bytes());
    }

    pub(crate) fn string(&mut self, text: &str) -> Offset {
        self.align(4, text.len() + 1);
        self.prepend(&[0]);
        self.prepend(text.as_bytes());
        self.prepend(&u32::try_from(text.len()).unwrap_or(u32::MAX).to_le_bytes());
        Offset(self.len())
    }

    pub(crate) fn offsets(&mut self, items: &[Offset]) -> Offset {
        self.align(4, 4 * items.len());
        for item in items.iter().rev() {
            self.prepend_offset(*item);
        }
        self.prepend(&u32::try_from(items.len()).unwrap_or(u32::MAX).to_le_bytes());
        Offset(self.len())
    }

    pub(crate) fn structs(&mut self, items: &[[i64; 2]]) -> Offset {
        self.align(8, 16 * items.len());
        for item in items.iter().rev() {
            self.prepend(&item[1].to_le_bytes());
            self.prepend(&item[0].to_le_bytes());
        }
        self.align(4, 4);
        self.prepend(&u32::try_from(items.len()).unwrap_or(u32::MAX).to_le_bytes());
        Offset(self.len())
    }

    pub(crate) fn table(&mut self, fields: &[Option<Field>]) -> Offset {
        let start = self.len();
        let mut positions = vec![0usize; fields.len()];
        for (index, field) in fields.iter().enumerate().rev() {
            let Some(field) = field else {
                continue;
            };
            match field {
                Field::Bool(value) => {
                    self.prepend(&[u8::from(*value)]);
                }
                Field::U8(value) => {
                    self.prepend(&[*value]);
                }
                Field::I16(value) => {
                    self.align(2, 2);
                    self.prepend(&value.to_le_bytes());
                }
                Field::I32(value) => {
                    self.align(4, 4);
                    self.prepend(&value.to_le_bytes());
                }
                Field::I64(value) => {
                    self.align(8, 8);
                    self.prepend(&value.to_le_bytes());
                }
                Field::Offset(target) => self.prepend_offset(*target),
            }
            positions[index] = self.len();
        }
        self.align(4, 4);
        self.prepend(&[0, 0, 0, 0]);
        let table = self.len();
        let inline_size = u16::try_from(table - start).unwrap_or(u16::MAX);
        let mut vtable = Vec::with_capacity(4 + 2 * fields.len());
        vtable.extend_from_slice(&u16::try_from(4 + 2 * fields.len()).unwrap_or(u16::MAX).to_le_bytes());
        vtable.extend_from_slice(&inline_size.to_le_bytes());
        for position in &positions {
            let offset = if *position == 0 { 0 } else { u16::try_from(table - position).unwrap_or(0) };
            vtable.extend_from_slice(&offset.to_le_bytes());
        }
        self.align(2, vtable.len());
        self.prepend(&vtable);
        let vtable_position = self.len();
        let soffset = i32::try_from(vtable_position - table).unwrap_or(0).to_le_bytes();
        for (index, byte) in soffset.iter().enumerate() {
            self.reversed[table - 1 - index] = *byte;
        }
        Offset(table)
    }

    pub(crate) fn finish(mut self, root: Offset) -> Vec<u8> {
        self.align(8, 4);
        self.prepend_offset(root);
        self.reversed.reverse();
        self.reversed
    }
}
