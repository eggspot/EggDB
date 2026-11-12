use crate::decode::{F32, F64, FuncType, I32, I64, Module, Reader, WasmError};
use std::collections::BTreeMap;
use std::rc::Rc;

const PAGE: usize = 65_536;
const NATIVE_STACK_BUDGET: usize = 1 << 20;

fn native_stack_position() -> usize {
    let marker = 0u8;
    std::hint::black_box(&marker) as *const u8 as usize
}

/// A value passed to or returned by a function.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    /// A 32-bit integer.
    I32(i32),
    /// A 64-bit integer.
    I64(i64),
    /// A 32-bit IEEE 754 float.
    F32(f32),
    /// A 64-bit IEEE 754 float.
    F64(f64),
}

/// What one call may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Instructions the call may run.
    pub fuel: u64,
    /// The most 64 KiB pages the memory may grow to.
    pub max_memory_pages: u32,
    /// The deepest nesting of calls; a call also traps once nested calls have used 1 MiB of the
    /// host thread's stack, whichever comes first.
    pub max_call_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            fuel: 10_000_000,
            max_memory_pages: 256,
            max_call_depth: 512,
        }
    }
}

/// Why a call stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trap {
    /// The call ran out of fuel.
    OutOfFuel,
    /// `unreachable` ran.
    Unreachable,
    /// A load or store fell outside the memory.
    MemoryOutOfBounds,
    /// An integer division by zero.
    DivideByZero,
    /// An integer division overflowed, or a float converted to an integer was out of its range.
    IntegerOverflow,
    /// A NaN was converted to an integer.
    InvalidConversion,
    /// `call_indirect` named a table element past the table's end.
    UndefinedElement,
    /// `call_indirect` named a table element no segment filled.
    UninitializedElement,
    /// `call_indirect` found a function of another type than the call expects.
    IndirectCallTypeMismatch,
    /// Calls nested deeper than the limit, or deep enough to use up their native stack budget.
    CallDepth,
    /// No function is exported under that name.
    NoSuchExport(String),
    /// The arguments do not match the function's parameters.
    WrongArguments,
    /// The code is not valid WebAssembly, found while running it.
    Invalid(&'static str),
    /// The code uses an instruction this interpreter does not run yet.
    Unsupported(u8),
}

impl std::fmt::Display for Trap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Trap::OutOfFuel => f.write_str("the function ran out of fuel"),
            Trap::Unreachable => f.write_str("the function reached unreachable code"),
            Trap::MemoryOutOfBounds => f.write_str("the function read or wrote outside its memory"),
            Trap::DivideByZero => f.write_str("the function divided by zero"),
            Trap::IntegerOverflow => f.write_str("the function's integer arithmetic overflowed"),
            Trap::InvalidConversion => f.write_str("the function converted NaN to an integer"),
            Trap::UndefinedElement => f.write_str("the function called past the end of its table"),
            Trap::UninitializedElement => {
                f.write_str("the function called an empty element of its table")
            }
            Trap::IndirectCallTypeMismatch => {
                f.write_str("the function called a table element of another type")
            }
            Trap::CallDepth => f.write_str("the function's calls nested too deeply"),
            Trap::NoSuchExport(name) => write!(f, "the module exports no function named {name}"),
            Trap::WrongArguments => {
                f.write_str("the arguments do not match the function's parameters")
            }
            Trap::Invalid(what) => write!(f, "the function's code is invalid: {what}"),
            Trap::Unsupported(opcode) => write!(
                f,
                "the function uses instruction 0x{opcode:02x}, which is not supported yet"
            ),
        }
    }
}

impl std::error::Error for Trap {}

impl From<WasmError> for Trap {
    fn from(error: WasmError) -> Self {
        match error {
            WasmError::Malformed(what) | WasmError::Unsupported(what) => Trap::Invalid(what),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Targets {
    otherwise: Option<usize>,
    end: usize,
}

#[derive(Debug, Clone, Copy)]
struct Label {
    looping: bool,
    height: usize,
    arity: usize,
    target: usize,
}

/// A module ready to run: its memory and globals, kept between calls.
#[derive(Debug)]
pub struct Instance {
    module: Rc<Module>,
    memory: Vec<u8>,
    max_pages: u32,
    globals: Vec<u64>,
    table: Vec<Option<u32>>,
    targets: Rc<Vec<BTreeMap<usize, Targets>>>,
    entry_stack_position: usize,
}

const EMPTY_BLOCK_TYPE: i64 = -64;
const VALUE_BLOCK_TYPES: std::ops::RangeInclusive<i64> = -4..=-1;

fn block_type(reader: &mut Reader<'_>, types: &[FuncType]) -> Result<(usize, usize), Trap> {
    let code = reader.signed(33)?;
    match code {
        EMPTY_BLOCK_TYPE => Ok((0, 0)),
        code if VALUE_BLOCK_TYPES.contains(&code) => Ok((0, 1)),
        code if code >= 0 => {
            let signature = usize::try_from(code)
                .ok()
                .and_then(|index| types.get(index))
                .ok_or(Trap::Invalid("a block names a type that does not exist"))?;
            Ok((signature.params.len(), signature.results.len()))
        }
        _ => Err(Trap::Invalid("a block type that is not a value type or a type index")),
    }
}

fn skip_immediates(opcode: u8, reader: &mut Reader<'_>) -> Result<(), Trap> {
    match opcode {
        0x02..=0x04 => {
            reader.signed(33)?;
        }
        0x0c | 0x0d | 0x10 | 0x20..=0x24 => {
            reader.u32()?;
        }
        0x11 => {
            reader.u32()?;
            reader.byte()?;
        }
        0x0e => {
            let count = reader.u32()?;
            for _ in 0..=count {
                reader.u32()?;
            }
        }
        0x28..=0x3e => {
            reader.u32()?;
            reader.u32()?;
        }
        0x3f | 0x40 => {
            reader.byte()?;
        }
        0x41 => {
            reader.signed(32)?;
        }
        0x42 => {
            reader.signed(64)?;
        }
        0x43 => {
            reader.take(4)?;
        }
        0x44 => {
            reader.take(8)?;
        }
        0xfc => match reader.u32()? {
            8 => {
                reader.u32()?;
                reader.byte()?;
            }
            9 => {
                reader.u32()?;
            }
            10 => {
                reader.byte()?;
                reader.byte()?;
            }
            11 => {
                reader.byte()?;
            }
            _ => {}
        },
        _ => {}
    }
    Ok(())
}

fn find_targets(code: &[u8]) -> Result<BTreeMap<usize, Targets>, Trap> {
    let mut targets = BTreeMap::new();
    let mut open: Vec<(usize, Option<usize>)> = Vec::new();
    let mut reader = Reader { bytes: code, at: 0 };
    while reader.at < code.len() {
        let position = reader.at;
        let opcode = reader.byte()?;
        match opcode {
            0x02..=0x04 => open.push((position, None)),
            0x05 => {
                let top = open.last_mut().ok_or(Trap::Invalid("else outside if"))?;
                top.1 = Some(position);
            }
            0x0b => {
                if let Some((start, otherwise)) = open.pop() {
                    targets.insert(
                        start,
                        Targets {
                            otherwise,
                            end: position,
                        },
                    );
                }
            }
            _ => {}
        }
        skip_immediates(opcode, &mut reader)?;
    }
    if !open.is_empty() {
        return Err(Trap::Invalid("a block does not end"));
    }
    Ok(targets)
}

fn as_i32(value: u64) -> i32 {
    value as u32 as i32
}

fn from_i32(value: i32) -> u64 {
    u64::from(value as u32)
}

struct Frame<'c> {
    code: &'c [u8],
    targets: &'c BTreeMap<usize, Targets>,
    locals: Vec<u64>,
}

impl Instance {
    /// Prepares `module` to run, with memory no larger than `limits` allows.
    pub fn new(module: Module, limits: &Limits) -> Result<Self, Trap> {
        let pages = module.memory.map_or(0, |(minimum, _)| minimum);
        let max_pages = module
            .memory
            .and_then(|(_, maximum)| maximum)
            .unwrap_or(u32::MAX)
            .min(limits.max_memory_pages);
        if pages > max_pages {
            return Err(Trap::MemoryOutOfBounds);
        }
        let mut memory = vec![0u8; pages as usize * PAGE];
        for (offset, bytes) in &module.data {
            let start = *offset as usize;
            let end = start
                .checked_add(bytes.len())
                .filter(|end| *end <= memory.len())
                .ok_or(Trap::MemoryOutOfBounds)?;
            memory[start..end].copy_from_slice(bytes);
        }
        let targets = module
            .functions
            .iter()
            .map(|function| find_targets(&function.code))
            .collect::<Result<Vec<_>, _>>()?;
        let globals = module.globals.iter().map(|global| global.init).collect();
        let mut table = vec![None; module.table.map_or(0, |(minimum, _)| minimum as usize)];
        for (offset, functions) in &module.elements {
            let start = *offset as usize;
            let end = start
                .checked_add(functions.len())
                .filter(|end| *end <= table.len())
                .ok_or(Trap::UndefinedElement)?;
            for (slot, function) in table[start..end].iter_mut().zip(functions) {
                *slot = Some(*function);
            }
        }
        let start = module.start;
        let mut instance = Self {
            module: Rc::new(module),
            memory,
            max_pages,
            globals,
            table,
            targets: Rc::new(targets),
            entry_stack_position: 0,
        };
        if let Some(start) = start {
            let mut stack = Vec::new();
            let mut fuel = limits.fuel;
            instance.entry_stack_position = native_stack_position();
            instance.invoke(start, &mut stack, &mut fuel, 0, limits)?;
        }
        Ok(instance)
    }

    /// Calls the exported function `name` with `arguments`, within `limits`, and returns all
    /// of its results in order.
    pub fn call_values(
        &mut self,
        name: &str,
        arguments: &[Value],
        limits: &Limits,
    ) -> Result<Vec<Value>, Trap> {
        let index = self
            .module
            .exports
            .iter()
            .find(|(export, _)| export == name)
            .map(|(_, index)| *index)
            .ok_or_else(|| Trap::NoSuchExport(name.to_owned()))?;
        let function = &self.module.functions[index as usize];
        let signature = &self.module.types[function.type_index as usize];
        if signature.params.len() != arguments.len() {
            return Err(Trap::WrongArguments);
        }
        let mut stack = Vec::new();
        for (parameter, argument) in signature.params.iter().zip(arguments) {
            match (*parameter, argument) {
                (I32, Value::I32(value)) => stack.push(from_i32(*value)),
                (I64, Value::I64(value)) => stack.push(*value as u64),
                (F32, Value::F32(value)) => stack.push(u64::from(value.to_bits())),
                (F64, Value::F64(value)) => stack.push(value.to_bits()),
                _ => return Err(Trap::WrongArguments),
            }
        }
        let result_types = signature.results.clone();
        let mut fuel = limits.fuel;
        self.entry_stack_position = native_stack_position();
        self.invoke(index, &mut stack, &mut fuel, 0, limits)?;
        let first = stack
            .len()
            .checked_sub(result_types.len())
            .ok_or(Trap::Invalid("no result"))?;
        Ok(stack
            .split_off(first)
            .into_iter()
            .zip(result_types)
            .map(|(bits, result_type)| match result_type {
                I32 => Value::I32(as_i32(bits)),
                I64 => Value::I64(bits as i64),
                F32 => Value::F32(f32::from_bits(bits as u32)),
                _ => Value::F64(f64::from_bits(bits)),
            })
            .collect())
    }

    /// Calls the exported function `name` with `arguments`, within `limits`, and returns its
    /// first result (`None` for a function with none).
    pub fn call(
        &mut self,
        name: &str,
        arguments: &[Value],
        limits: &Limits,
    ) -> Result<Option<Value>, Trap> {
        Ok(self.call_values(name, arguments, limits)?.into_iter().next())
    }

    fn invoke(
        &mut self,
        index: u32,
        stack: &mut Vec<u64>,
        fuel: &mut u64,
        depth: usize,
        limits: &Limits,
    ) -> Result<(), Trap> {
        let native_stack_used = native_stack_position().abs_diff(self.entry_stack_position);
        if depth >= limits.max_call_depth || native_stack_used > NATIVE_STACK_BUDGET {
            return Err(Trap::CallDepth);
        }
        let module = Rc::clone(&self.module);
        let function = module
            .functions
            .get(index as usize)
            .ok_or(Trap::Invalid("a call to a function that does not exist"))?;
        let signature = &module.types[function.type_index as usize];
        let parameters = signature.params.len();
        if stack.len() < parameters {
            return Err(Trap::Invalid("too few values for a call"));
        }
        let mut locals = stack.split_off(stack.len() - parameters);
        locals.extend(std::iter::repeat_n(0, function.locals.len()));
        let all_targets = Rc::clone(&self.targets);
        let frame = Frame {
            code: &function.code,
            targets: &all_targets[index as usize],
            locals,
        };
        let base = stack.len();
        self.execute(frame, stack, fuel, depth, limits, signature.results.len())?;
        if stack.len() != base + signature.results.len() {
            return Err(Trap::Invalid("a function left the wrong number of values"));
        }
        Ok(())
    }

    fn address(
        &self,
        reader: &mut Reader<'_>,
        stack: &mut Vec<u64>,
        size: usize,
    ) -> Result<usize, Trap> {
        reader.u32()?;
        let offset = u64::from(reader.u32()?);
        let base = u64::from(as_i32(stack.pop().ok_or(Trap::Invalid("an empty stack"))?) as u32);
        let start = base + offset;
        if start + size as u64 > self.memory.len() as u64 {
            return Err(Trap::MemoryOutOfBounds);
        }
        Ok(start as usize)
    }

    fn load(&self, start: usize, size: usize) -> u64 {
        let mut bytes = [0u8; 8];
        bytes[..size].copy_from_slice(&self.memory[start..start + size]);
        u64::from_le_bytes(bytes)
    }

    fn execute(
        &mut self,
        mut frame: Frame<'_>,
        stack: &mut Vec<u64>,
        fuel: &mut u64,
        depth: usize,
        limits: &Limits,
        results: usize,
    ) -> Result<(), Trap> {
        let base = stack.len();
        let module = Rc::clone(&self.module);
        let mut labels: Vec<Label> = Vec::new();
        let mut reader = Reader {
            bytes: frame.code,
            at: 0,
        };
        let pop = |stack: &mut Vec<u64>| stack.pop().ok_or(Trap::Invalid("an empty stack"));
        loop {
            if *fuel == 0 {
                return Err(Trap::OutOfFuel);
            }
            *fuel -= 1;
            let position = reader.at;
            let opcode = reader.byte()?;
            match opcode {
                0x00 => return Err(Trap::Unreachable),
                0x01 => {}
                0x02 | 0x03 => {
                    let (params, results) = block_type(&mut reader, &module.types)?;
                    let end = frame
                        .targets
                        .get(&position)
                        .ok_or(Trap::Invalid("a block without an end"))?
                        .end;
                    let looping = opcode == 0x03;
                    labels.push(Label {
                        looping,
                        height: stack
                            .len()
                            .checked_sub(params)
                            .ok_or(Trap::Invalid("too few values for a block"))?,
                        arity: if looping { params } else { results },
                        target: if looping { position } else { end + 1 },
                    });
                }
                0x04 => {
                    let (params, arity) = block_type(&mut reader, &module.types)?;
                    let condition = pop(stack)?;
                    let height = stack
                        .len()
                        .checked_sub(params)
                        .ok_or(Trap::Invalid("too few values for an if"))?;
                    let targets = *frame
                        .targets
                        .get(&position)
                        .ok_or(Trap::Invalid("an if without an end"))?;
                    if condition as u32 != 0 {
                        labels.push(Label {
                            looping: false,
                            height,
                            arity,
                            target: targets.end + 1,
                        });
                    } else if let Some(otherwise) = targets.otherwise {
                        labels.push(Label {
                            looping: false,
                            height,
                            arity,
                            target: targets.end + 1,
                        });
                        reader.at = otherwise + 1;
                    } else {
                        reader.at = targets.end + 1;
                    }
                }
                0x05 => {
                    let label = labels.pop().ok_or(Trap::Invalid("else outside if"))?;
                    reader.at = label.target;
                }
                0x0b => {
                    if labels.pop().is_none() {
                        return Ok(());
                    }
                }
                0x0c..=0x0f => {
                    let depth_to = match opcode {
                        0x0c => reader.u32()? as usize,
                        0x0d => {
                            let target = reader.u32()? as usize;
                            if pop(stack)? as u32 == 0 {
                                continue;
                            }
                            target
                        }
                        0x0e => {
                            let count = reader.u32()? as usize;
                            let chosen = as_i32(pop(stack)?) as u32 as usize;
                            let mut picked = 0;
                            for index in 0..=count {
                                let target = reader.u32()? as usize;
                                if index == chosen.min(count) {
                                    picked = target;
                                }
                            }
                            picked
                        }
                        _ => labels.len(),
                    };
                    if depth_to >= labels.len() {
                        let kept = stack.split_off(
                            stack
                                .len()
                                .checked_sub(results)
                                .ok_or(Trap::Invalid("an empty stack"))?,
                        );
                        stack.truncate(base);
                        stack.extend(kept);
                        return Ok(());
                    }
                    let label = labels[labels.len() - 1 - depth_to];
                    let kept = stack.split_off(
                        stack
                            .len()
                            .checked_sub(label.arity)
                            .ok_or(Trap::Invalid("an empty stack"))?,
                    );
                    stack.truncate(label.height);
                    stack.extend(kept);
                    labels.truncate(labels.len() - depth_to - usize::from(!label.looping));
                    reader.at = label.target;
                    if label.looping {
                        reader.byte()?;
                        reader.signed(33)?;
                    }
                }
                0x10 => {
                    let callee = reader.u32()?;
                    self.invoke(callee, stack, fuel, depth + 1, limits)?;
                }
                0x11 => {
                    let expected = reader.u32()? as usize;
                    if reader.byte()? != 0 {
                        return Err(Trap::Invalid("call_indirect names a table other than 0"));
                    }
                    let element = as_i32(pop(stack)?) as u32 as usize;
                    let callee = self
                        .table
                        .get(element)
                        .ok_or(Trap::UndefinedElement)?
                        .ok_or(Trap::UninitializedElement)?;
                    let module = Rc::clone(&self.module);
                    let wanted = module
                        .types
                        .get(expected)
                        .ok_or(Trap::Invalid("call_indirect names a type that does not exist"))?;
                    let found = module
                        .functions
                        .get(callee as usize)
                        .and_then(|function| module.types.get(function.type_index as usize))
                        .ok_or(Trap::Invalid("a table element names no function"))?;
                    if wanted != found {
                        return Err(Trap::IndirectCallTypeMismatch);
                    }
                    self.invoke(callee, stack, fuel, depth + 1, limits)?;
                }
                0x1a => {
                    pop(stack)?;
                }
                0x1b => {
                    let condition = pop(stack)? as u32;
                    let second = pop(stack)?;
                    let first = pop(stack)?;
                    stack.push(if condition != 0 { first } else { second });
                }
                0x20 => {
                    let index = reader.u32()? as usize;
                    stack.push(
                        *frame
                            .locals
                            .get(index)
                            .ok_or(Trap::Invalid("a local that does not exist"))?,
                    );
                }
                0x21 | 0x22 => {
                    let index = reader.u32()? as usize;
                    let value = if opcode == 0x22 {
                        *stack.last().ok_or(Trap::Invalid("an empty stack"))?
                    } else {
                        pop(stack)?
                    };
                    *frame
                        .locals
                        .get_mut(index)
                        .ok_or(Trap::Invalid("a local that does not exist"))? = value;
                }
                0x23 => {
                    let index = reader.u32()? as usize;
                    stack.push(
                        *self
                            .globals
                            .get(index)
                            .ok_or(Trap::Invalid("a global that does not exist"))?,
                    );
                }
                0x24 => {
                    let index = reader.u32()? as usize;
                    if !self
                        .module
                        .globals
                        .get(index)
                        .is_some_and(|global| global.mutable)
                    {
                        return Err(Trap::Invalid("a write to an immutable global"));
                    }
                    self.globals[index] = pop(stack)?;
                }
                0x28..=0x35 => {
                    let (size, signed, wide) = match opcode {
                        0x28 | 0x2a => (4, false, false),
                        0x29 | 0x2b => (8, false, true),
                        0x2c => (1, true, false),
                        0x2d => (1, false, false),
                        0x2e => (2, true, false),
                        0x2f => (2, false, false),
                        0x30 => (1, true, true),
                        0x31 => (1, false, true),
                        0x32 => (2, true, true),
                        0x33 => (2, false, true),
                        0x34 => (4, true, true),
                        0x35 => (4, false, true),
                        _ => return Err(Trap::Unsupported(opcode)),
                    };
                    let start = self.address(&mut reader, stack, size)?;
                    let raw = self.load(start, size);
                    let bits = size as u32 * 8;
                    let value = if signed && bits < 64 {
                        (((raw << (64 - bits)) as i64) >> (64 - bits)) as u64
                    } else {
                        raw
                    };
                    stack.push(if wide { value } else { u64::from(value as u32) });
                }
                0x36..=0x3e => {
                    let size = match opcode {
                        0x36 | 0x38 | 0x3e => 4,
                        0x37 | 0x39 => 8,
                        0x3a | 0x3c => 1,
                        0x3b | 0x3d => 2,
                        _ => return Err(Trap::Unsupported(opcode)),
                    };
                    let value = pop(stack)?;
                    let start = self.address(&mut reader, stack, size)?;
                    self.memory[start..start + size].copy_from_slice(&value.to_le_bytes()[..size]);
                }
                0x3f => {
                    reader.byte()?;
                    stack.push((self.memory.len() / PAGE) as u64);
                }
                0x40 => {
                    reader.byte()?;
                    let more = as_i32(pop(stack)?) as u32;
                    let current = (self.memory.len() / PAGE) as u32;
                    match current
                        .checked_add(more)
                        .filter(|pages| *pages <= self.max_pages)
                    {
                        Some(pages) => {
                            self.memory.resize(pages as usize * PAGE, 0);
                            stack.push(u64::from(current));
                        }
                        None => stack.push(from_i32(-1)),
                    }
                }
                0x41 => stack.push(from_i32(reader.signed(32)? as i32)),
                0x42 => stack.push(reader.signed(64)? as u64),
                0x43 => {
                    let mut word = [0u8; 4];
                    word.copy_from_slice(reader.take(4)?);
                    stack.push(u64::from(u32::from_le_bytes(word)));
                }
                0x44 => {
                    let mut word = [0u8; 8];
                    word.copy_from_slice(reader.take(8)?);
                    stack.push(u64::from_le_bytes(word));
                }
                0x5b..=0x66 | 0x8b..=0xa6 | 0xa8..=0xab | 0xae..=0xbf => {
                    crate::float::apply(opcode, stack)?;
                }
                0xfc => {
                    let operation = reader.u32()?;
                    match operation {
                        0..=7 => crate::float::saturating(operation, stack)?,
                        10 => {
                            reader.byte()?;
                            reader.byte()?;
                            let length = as_i32(pop(stack)?) as u32 as u64;
                            let source = as_i32(pop(stack)?) as u32 as u64;
                            let destination = as_i32(pop(stack)?) as u32 as u64;
                            let size = self.memory.len() as u64;
                            if source + length > size || destination + length > size {
                                return Err(Trap::MemoryOutOfBounds);
                            }
                            if *fuel < length / 64 {
                                return Err(Trap::OutOfFuel);
                            }
                            *fuel -= length / 64;
                            let (source, destination, length) =
                                (source as usize, destination as usize, length as usize);
                            self.memory
                                .copy_within(source..source + length, destination);
                        }
                        11 => {
                            reader.byte()?;
                            let length = as_i32(pop(stack)?) as u32 as u64;
                            let value = pop(stack)? as u8;
                            let destination = as_i32(pop(stack)?) as u32 as u64;
                            if destination + length > self.memory.len() as u64 {
                                return Err(Trap::MemoryOutOfBounds);
                            }
                            if *fuel < length / 64 {
                                return Err(Trap::OutOfFuel);
                            }
                            *fuel -= length / 64;
                            let (destination, length) = (destination as usize, length as usize);
                            self.memory[destination..destination + length].fill(value);
                        }
                        _ => return Err(Trap::Unsupported(0xfc)),
                    }
                }
                0x45 => {
                    let value = pop(stack)? as u32;
                    stack.push(u64::from(value == 0));
                }
                0x46..=0x4f => {
                    let right = pop(stack)? as u32;
                    let left = pop(stack)? as u32;
                    let (signed_left, signed_right) = (left as i32, right as i32);
                    let truth = match opcode {
                        0x46 => left == right,
                        0x47 => left != right,
                        0x48 => signed_left < signed_right,
                        0x49 => left < right,
                        0x4a => signed_left > signed_right,
                        0x4b => left > right,
                        0x4c => signed_left <= signed_right,
                        0x4d => left <= right,
                        0x4e => signed_left >= signed_right,
                        _ => left >= right,
                    };
                    stack.push(u64::from(truth));
                }
                0x50 => {
                    let value = pop(stack)?;
                    stack.push(u64::from(value == 0));
                }
                0x51..=0x5a => {
                    let right = pop(stack)?;
                    let left = pop(stack)?;
                    let (signed_left, signed_right) = (left as i64, right as i64);
                    let truth = match opcode {
                        0x51 => left == right,
                        0x52 => left != right,
                        0x53 => signed_left < signed_right,
                        0x54 => left < right,
                        0x55 => signed_left > signed_right,
                        0x56 => left > right,
                        0x57 => signed_left <= signed_right,
                        0x58 => left <= right,
                        0x59 => signed_left >= signed_right,
                        _ => left >= right,
                    };
                    stack.push(u64::from(truth));
                }
                0x67..=0x69 => {
                    let value = pop(stack)? as u32;
                    stack.push(u64::from(match opcode {
                        0x67 => value.leading_zeros(),
                        0x68 => value.trailing_zeros(),
                        _ => value.count_ones(),
                    }));
                }
                0x6a..=0x78 => {
                    let right = pop(stack)? as u32;
                    let left = pop(stack)? as u32;
                    let value = match opcode {
                        0x6a => left.wrapping_add(right),
                        0x6b => left.wrapping_sub(right),
                        0x6c => left.wrapping_mul(right),
                        0x6d => {
                            if right == 0 {
                                return Err(Trap::DivideByZero);
                            }
                            (left as i32)
                                .checked_div(right as i32)
                                .ok_or(Trap::IntegerOverflow)? as u32
                        }
                        0x6e => left.checked_div(right).ok_or(Trap::DivideByZero)?,
                        0x6f => {
                            if right == 0 {
                                return Err(Trap::DivideByZero);
                            }
                            (left as i32).wrapping_rem(right as i32) as u32
                        }
                        0x70 => left.checked_rem(right).ok_or(Trap::DivideByZero)?,
                        0x71 => left & right,
                        0x72 => left | right,
                        0x73 => left ^ right,
                        0x74 => left.wrapping_shl(right),
                        0x75 => ((left as i32).wrapping_shr(right)) as u32,
                        0x76 => left.wrapping_shr(right),
                        0x77 => left.rotate_left(right % 32),
                        _ => left.rotate_right(right % 32),
                    };
                    stack.push(u64::from(value));
                }
                0x79..=0x7b => {
                    let value = pop(stack)?;
                    stack.push(u64::from(match opcode {
                        0x79 => value.leading_zeros(),
                        0x7a => value.trailing_zeros(),
                        _ => value.count_ones(),
                    }));
                }
                0x7c..=0x8a => {
                    let right = pop(stack)?;
                    let left = pop(stack)?;
                    let value = match opcode {
                        0x7c => left.wrapping_add(right),
                        0x7d => left.wrapping_sub(right),
                        0x7e => left.wrapping_mul(right),
                        0x7f => {
                            if right == 0 {
                                return Err(Trap::DivideByZero);
                            }
                            (left as i64)
                                .checked_div(right as i64)
                                .ok_or(Trap::IntegerOverflow)? as u64
                        }
                        0x80 => left.checked_div(right).ok_or(Trap::DivideByZero)?,
                        0x81 => {
                            if right == 0 {
                                return Err(Trap::DivideByZero);
                            }
                            (left as i64).wrapping_rem(right as i64) as u64
                        }
                        0x82 => left.checked_rem(right).ok_or(Trap::DivideByZero)?,
                        0x83 => left & right,
                        0x84 => left | right,
                        0x85 => left ^ right,
                        0x86 => left.wrapping_shl(right as u32),
                        0x87 => ((left as i64).wrapping_shr(right as u32)) as u64,
                        0x88 => left.wrapping_shr(right as u32),
                        0x89 => left.rotate_left((right % 64) as u32),
                        _ => left.rotate_right((right % 64) as u32),
                    };
                    stack.push(value);
                }
                0xa7 => {
                    let value = pop(stack)?;
                    stack.push(u64::from(value as u32));
                }
                0xac => {
                    let value = pop(stack)?;
                    stack.push(i64::from(as_i32(value)) as u64);
                }
                0xad => {
                    let value = pop(stack)?;
                    stack.push(u64::from(value as u32));
                }
                0xc0..=0xc4 => {
                    let value = pop(stack)?;
                    stack.push(match opcode {
                        0xc0 => from_i32(i32::from(value as u8 as i8)),
                        0xc1 => from_i32(i32::from(value as u16 as i16)),
                        0xc2 => i64::from(value as u8 as i8) as u64,
                        0xc3 => i64::from(value as u16 as i16) as u64,
                        _ => i64::from(value as u32 as i32) as u64,
                    });
                }
                other => return Err(Trap::Unsupported(other)),
            }
        }
    }
}
