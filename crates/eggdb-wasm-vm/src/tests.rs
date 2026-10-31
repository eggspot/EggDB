use crate::{Instance, Limits, Module, Trap, Value, WasmError};

fn leb(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn section(id: u8, body: &[u8], out: &mut Vec<u8>) {
    out.push(id);
    leb(body.len() as u64, out);
    out.extend_from_slice(body);
}

struct Function {
    params: Vec<u8>,
    results: Vec<u8>,
    locals: Vec<u8>,
    code: Vec<u8>,
    export: &'static str,
}

fn module(functions: &[Function], memory: Option<u32>, data: &[(u32, &[u8])]) -> Vec<u8> {
    let mut out = b"\0asm\x01\0\0\0".to_vec();
    let mut types = Vec::new();
    leb(functions.len() as u64, &mut types);
    for function in functions {
        types.push(0x60);
        leb(function.params.len() as u64, &mut types);
        types.extend_from_slice(&function.params);
        leb(function.results.len() as u64, &mut types);
        types.extend_from_slice(&function.results);
    }
    section(1, &types, &mut out);
    let mut declared = Vec::new();
    leb(functions.len() as u64, &mut declared);
    for index in 0..functions.len() {
        leb(index as u64, &mut declared);
    }
    section(3, &declared, &mut out);
    if let Some(pages) = memory {
        section(5, &[1, 0, pages as u8], &mut out);
    }
    let mut exports = Vec::new();
    leb(functions.len() as u64, &mut exports);
    for (index, function) in functions.iter().enumerate() {
        leb(function.export.len() as u64, &mut exports);
        exports.extend_from_slice(function.export.as_bytes());
        exports.push(0);
        leb(index as u64, &mut exports);
    }
    section(7, &exports, &mut out);
    let mut code = Vec::new();
    leb(functions.len() as u64, &mut code);
    for function in functions {
        let mut body = Vec::new();
        leb(function.locals.len() as u64, &mut body);
        for local in &function.locals {
            body.push(1);
            body.push(*local);
        }
        body.extend_from_slice(&function.code);
        leb(body.len() as u64, &mut code);
        code.extend_from_slice(&body);
    }
    section(10, &code, &mut out);
    if !data.is_empty() {
        let mut segments = Vec::new();
        leb(data.len() as u64, &mut segments);
        for (offset, bytes) in data {
            segments.push(0);
            segments.push(0x41);
            leb(u64::from(*offset), &mut segments);
            segments.push(0x0b);
            leb(bytes.len() as u64, &mut segments);
            segments.extend_from_slice(bytes);
        }
        section(11, &segments, &mut out);
    }
    out
}

const I32: u8 = 0x7f;
const I64: u8 = 0x7e;

fn instance(functions: &[Function], memory: Option<u32>, data: &[(u32, &[u8])]) -> Instance {
    Instance::new(Module::parse(&module(functions, memory, data)).unwrap(), &Limits::default()).unwrap()
}

#[test]
fn arithmetic_recursion_and_loops_run() {
    let mut vm = instance(
        &[
            Function { params: vec![I32, I32], results: vec![I32], locals: vec![], code: vec![0x20, 0, 0x20, 1, 0x6a, 0x0b], export: "add" },
            Function {
                params: vec![I64],
                results: vec![I64],
                locals: vec![],
                code: vec![0x20, 0, 0x50, 0x04, I64, 0x42, 1, 0x05, 0x20, 0, 0x20, 0, 0x42, 1, 0x7d, 0x10, 1, 0x7e, 0x0b, 0x0b],
                export: "factorial",
            },
            Function {
                params: vec![I32],
                results: vec![I32],
                locals: vec![I32],
                code: vec![0x02, 0x40, 0x03, 0x40, 0x20, 0, 0x45, 0x0d, 1, 0x20, 1, 0x20, 0, 0x6a, 0x21, 1, 0x20, 0, 0x41, 1, 0x6b, 0x21, 0, 0x0c, 0, 0x0b, 0x0b, 0x20, 1, 0x0b],
                export: "sum",
            },
        ],
        None,
        &[],
    );
    let limits = Limits::default();
    assert_eq!(vm.call("add", &[Value::I32(40), Value::I32(2)], &limits), Ok(Some(Value::I32(42))));
    assert_eq!(vm.call("add", &[Value::I32(i32::MAX), Value::I32(1)], &limits), Ok(Some(Value::I32(i32::MIN))));
    assert_eq!(vm.call("factorial", &[Value::I64(20)], &limits), Ok(Some(Value::I64(2_432_902_008_176_640_000))));
    assert_eq!(vm.call("sum", &[Value::I32(100)], &limits), Ok(Some(Value::I32(5050))));
    assert_eq!(vm.call("add", &[Value::I32(1)], &limits), Err(Trap::WrongArguments));
    assert_eq!(vm.call("nope", &[], &limits), Err(Trap::NoSuchExport("nope".to_owned())));
}

#[test]
fn memory_reads_data_segments_and_grows_within_its_limit() {
    let mut vm = instance(
        &[
            Function { params: vec![I32], results: vec![I32], locals: vec![], code: vec![0x20, 0, 0x2d, 0, 0, 0x0b], export: "byte" },
            Function { params: vec![I32, I64], results: vec![], locals: vec![], code: vec![0x20, 0, 0x20, 1, 0x37, 3, 0, 0x0b], export: "store" },
            Function { params: vec![I32], results: vec![I64], locals: vec![], code: vec![0x20, 0, 0x29, 3, 0, 0x0b], export: "load" },
            Function { params: vec![I32], results: vec![I32], locals: vec![], code: vec![0x20, 0, 0x40, 0, 0x0b], export: "grow" },
        ],
        Some(1),
        &[(10, b"egg")],
    );
    let limits = Limits { max_memory_pages: 3, ..Limits::default() };
    assert_eq!(vm.call("byte", &[Value::I32(11)], &limits), Ok(Some(Value::I32(i32::from(b'g')))));
    vm.call("store", &[Value::I32(100), Value::I64(-7)], &limits).unwrap();
    assert_eq!(vm.call("load", &[Value::I32(100)], &limits), Ok(Some(Value::I64(-7))));
    assert_eq!(vm.call("byte", &[Value::I32(65_536)], &limits), Err(Trap::MemoryOutOfBounds));
    let mut capped = Instance::new(Module::parse(&module(&[Function { params: vec![I32], results: vec![I32], locals: vec![], code: vec![0x20, 0, 0x40, 0, 0x0b], export: "grow" }], Some(1), &[])).unwrap(), &limits).unwrap();
    assert_eq!(capped.call("grow", &[Value::I32(2)], &limits), Ok(Some(Value::I32(1))));
    assert_eq!(capped.call("grow", &[Value::I32(1)], &limits), Ok(Some(Value::I32(-1))));
}

#[test]
fn hostile_functions_trap_instead_of_hanging_or_crashing() {
    let mut vm = instance(
        &[
            Function { params: vec![], results: vec![], locals: vec![], code: vec![0x03, 0x40, 0x0c, 0, 0x0b, 0x0b], export: "spin" },
            Function { params: vec![], results: vec![], locals: vec![], code: vec![0x10, 1, 0x0b], export: "recurse" },
            Function { params: vec![I32], results: vec![I32], locals: vec![], code: vec![0x41, 1, 0x20, 0, 0x6d, 0x0b], export: "divide" },
            Function { params: vec![], results: vec![], locals: vec![], code: vec![0x00, 0x0b], export: "trap" },
        ],
        None,
        &[],
    );
    let limits = Limits { fuel: 10_000, ..Limits::default() };
    assert_eq!(vm.call("spin", &[], &limits), Err(Trap::OutOfFuel));
    assert_eq!(vm.call("recurse", &[], &Limits::default()), Err(Trap::CallDepth));
    assert_eq!(vm.call("divide", &[Value::I32(0)], &limits), Err(Trap::DivideByZero));
    assert_eq!(vm.call("trap", &[], &limits), Err(Trap::Unreachable));
}

#[test]
fn malformed_and_unsupported_modules_are_refused() {
    assert!(matches!(Module::parse(b"\0asx\x01\0\0\0"), Err(WasmError::Malformed(_))));
    assert!(matches!(Module::parse(b"\0asm\x01\0\0\0\x01\x05\x01\x60\x01\x7d\x00"), Err(WasmError::Unsupported("floating point"))));
    assert!(matches!(Module::parse(b"\0asm\x01\0\0\0\x02\x01\x01"), Err(WasmError::Unsupported("imports")) | Err(WasmError::Malformed(_))));
    assert!(Module::parse(b"\0asm\x01\0\0\0\x0a\xff\xff\xff\xff\x0f").is_err());
    for cut in 0..60 {
        let bytes = module(&[Function { params: vec![I32], results: vec![I32], locals: vec![], code: vec![0x20, 0, 0x0b], export: "id" }], None, &[]);
        let _ = Module::parse(&bytes[..cut.min(bytes.len())]);
    }
}
