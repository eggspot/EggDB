/// Why a module was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WasmError {
    /// The bytes are not a valid WebAssembly binary.
    Malformed(&'static str),
    /// The module uses a feature this interpreter does not run yet.
    Unsupported(&'static str),
}

impl std::fmt::Display for WasmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WasmError::Malformed(what) => write!(f, "the WebAssembly module is malformed: {what}"),
            WasmError::Unsupported(what) => write!(f, "the WebAssembly module uses {what}, which is not supported yet"),
        }
    }
}

impl std::error::Error for WasmError {}

const MAX_ITEMS: u32 = 100_000;
pub(crate) const I32: u8 = 0x7f;
pub(crate) const I64: u8 = 0x7e;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FuncType {
    pub(crate) params: Vec<u8>,
    pub(crate) results: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Function {
    pub(crate) type_index: u32,
    pub(crate) locals: Vec<u8>,
    pub(crate) code: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Global {
    pub(crate) value_type: u8,
    pub(crate) mutable: bool,
    pub(crate) init: u64,
}

/// A parsed WebAssembly module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Module {
    pub(crate) types: Vec<FuncType>,
    pub(crate) functions: Vec<Function>,
    pub(crate) memory: Option<(u32, Option<u32>)>,
    pub(crate) globals: Vec<Global>,
    pub(crate) exports: Vec<(String, u32)>,
    pub(crate) data: Vec<(u32, Vec<u8>)>,
}

pub(crate) struct Reader<'b> {
    pub(crate) bytes: &'b [u8],
    pub(crate) at: usize,
}

impl<'b> Reader<'b> {
    pub(crate) fn byte(&mut self) -> Result<u8, WasmError> {
        let byte = *self.bytes.get(self.at).ok_or(WasmError::Malformed("unexpected end"))?;
        self.at += 1;
        Ok(byte)
    }

    pub(crate) fn take(&mut self, count: usize) -> Result<&'b [u8], WasmError> {
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len()).ok_or(WasmError::Malformed("unexpected end"))?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    pub(crate) fn unsigned(&mut self, bits: u32) -> Result<u64, WasmError> {
        let mut value = 0u64;
        let mut shift = 0;
        loop {
            let byte = self.byte()?;
            if shift >= bits.div_ceil(7) * 7 {
                return Err(WasmError::Malformed("an integer is too long"));
            }
            value |= u64::from(byte & 0x7f) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                if bits < 64 && value >> bits != 0 {
                    return Err(WasmError::Malformed("an integer is out of range"));
                }
                return Ok(value);
            }
        }
    }

    pub(crate) fn signed(&mut self, bits: u32) -> Result<i64, WasmError> {
        let mut value = 0i64;
        let mut shift = 0u32;
        loop {
            let byte = self.byte()?;
            if shift >= bits.div_ceil(7) * 7 {
                return Err(WasmError::Malformed("an integer is too long"));
            }
            value |= i64::from(byte & 0x7f) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                if shift < 64 && byte & 0x40 != 0 {
                    value |= -1i64 << shift;
                }
                return Ok(value);
            }
        }
    }

    pub(crate) fn u32(&mut self) -> Result<u32, WasmError> {
        u32::try_from(self.unsigned(32)?).map_err(|_| WasmError::Malformed("an integer is out of range"))
    }

    fn count(&mut self) -> Result<u32, WasmError> {
        let count = self.u32()?;
        if count > MAX_ITEMS {
            return Err(WasmError::Unsupported("a section with more than 100,000 items"));
        }
        Ok(count)
    }

    fn name(&mut self) -> Result<String, WasmError> {
        let length = self.u32()? as usize;
        String::from_utf8(self.take(length)?.to_vec()).map_err(|_| WasmError::Malformed("a name is not UTF-8"))
    }

    fn value_type(&mut self) -> Result<u8, WasmError> {
        match self.byte()? {
            I32 => Ok(I32),
            I64 => Ok(I64),
            0x7d | 0x7c => Err(WasmError::Unsupported("floating point")),
            _ => Err(WasmError::Malformed("an unknown value type")),
        }
    }

    fn constant(&mut self, value_type: u8) -> Result<u64, WasmError> {
        let value = match (self.byte()?, value_type) {
            (0x41, I32) => u64::from(self.signed(32)? as i32 as u32),
            (0x42, I64) => self.signed(64)? as u64,
            _ => return Err(WasmError::Unsupported("an initializer other than a constant")),
        };
        if self.byte()? != 0x0b {
            return Err(WasmError::Malformed("an initializer does not end"));
        }
        Ok(value)
    }
}

impl Module {
    /// Reads a WebAssembly binary module.
    pub fn parse(bytes: &[u8]) -> Result<Self, WasmError> {
        let mut reader = Reader { bytes, at: 0 };
        if reader.take(4)? != b"\0asm" || reader.take(4)? != [1, 0, 0, 0] {
            return Err(WasmError::Malformed("not a WebAssembly 1.0 binary"));
        }
        let mut module = Module { types: Vec::new(), functions: Vec::new(), memory: None, globals: Vec::new(), exports: Vec::new(), data: Vec::new() };
        let mut type_indices = Vec::new();
        while reader.at < bytes.len() {
            let id = reader.byte()?;
            let size = reader.u32()? as usize;
            let mut section = Reader { bytes: reader.take(size)?, at: 0 };
            match id {
                0 => {}
                1 => {
                    for _ in 0..section.count()? {
                        if section.byte()? != 0x60 {
                            return Err(WasmError::Malformed("a function type does not start with 0x60"));
                        }
                        let params = (0..section.count()?).map(|_| section.value_type()).collect::<Result<_, _>>()?;
                        let results: Vec<u8> = (0..section.count()?).map(|_| section.value_type()).collect::<Result<_, _>>()?;
                        if results.len() > 1 {
                            return Err(WasmError::Unsupported("several results"));
                        }
                        module.types.push(FuncType { params, results });
                    }
                }
                2 => {
                    if section.count()? > 0 {
                        return Err(WasmError::Unsupported("imports"));
                    }
                }
                3 => {
                    for _ in 0..section.count()? {
                        type_indices.push(section.u32()?);
                    }
                }
                4 | 9 => return Err(WasmError::Unsupported("tables")),
                5 => {
                    let count = section.count()?;
                    if count > 1 {
                        return Err(WasmError::Malformed("more than one memory"));
                    }
                    if count == 1 {
                        let flags = section.byte()?;
                        let minimum = section.u32()?;
                        let maximum = match flags {
                            0 => None,
                            1 => Some(section.u32()?),
                            _ => return Err(WasmError::Unsupported("shared or 64-bit memory")),
                        };
                        module.memory = Some((minimum, maximum));
                    }
                }
                6 => {
                    for _ in 0..section.count()? {
                        let value_type = section.value_type()?;
                        let mutable = match section.byte()? {
                            0 => false,
                            1 => true,
                            _ => return Err(WasmError::Malformed("a global's mutability is not 0 or 1")),
                        };
                        let init = section.constant(value_type)?;
                        module.globals.push(Global { value_type, mutable, init });
                    }
                }
                7 => {
                    for _ in 0..section.count()? {
                        let name = section.name()?;
                        let kind = section.byte()?;
                        let index = section.u32()?;
                        if kind == 0 {
                            module.exports.push((name, index));
                        }
                    }
                }
                8 => return Err(WasmError::Unsupported("a start function")),
                10 => {
                    let count = section.count()?;
                    if count as usize != type_indices.len() {
                        return Err(WasmError::Malformed("the code and function sections differ in length"));
                    }
                    for type_index in &type_indices {
                        let size = section.u32()? as usize;
                        let mut body = Reader { bytes: section.take(size)?, at: 0 };
                        let mut locals = Vec::new();
                        for _ in 0..body.count()? {
                            let repeat = body.u32()?;
                            let value_type = body.value_type()?;
                            if locals.len() + repeat as usize > 50_000 {
                                return Err(WasmError::Unsupported("more than 50,000 locals"));
                            }
                            locals.extend(std::iter::repeat_n(value_type, repeat as usize));
                        }
                        let code = body.bytes[body.at..].to_vec();
                        if code.last() != Some(&0x0b) {
                            return Err(WasmError::Malformed("a function body does not end"));
                        }
                        module.functions.push(Function { type_index: *type_index, locals, code });
                    }
                }
                11 => {
                    for _ in 0..section.count()? {
                        if section.u32()? != 0 {
                            return Err(WasmError::Unsupported("passive data segments"));
                        }
                        let offset = section.constant(I32)? as u32;
                        let length = section.u32()? as usize;
                        module.data.push((offset, section.take(length)?.to_vec()));
                    }
                }
                12 => {}
                _ => return Err(WasmError::Malformed("an unknown section")),
            }
        }
        if module.functions.len() != type_indices.len() {
            return Err(WasmError::Malformed("functions have no code"));
        }
        for function in &module.functions {
            if function.type_index as usize >= module.types.len() {
                return Err(WasmError::Malformed("a function names a type that does not exist"));
            }
        }
        for (_, index) in &module.exports {
            if *index as usize >= module.functions.len() {
                return Err(WasmError::Malformed("an export names a function that does not exist"));
            }
        }
        Ok(module)
    }

    /// The names of the exported functions.
    pub fn exports(&self) -> impl Iterator<Item = &str> {
        self.exports.iter().map(|(name, _)| name.as_str())
    }
}
