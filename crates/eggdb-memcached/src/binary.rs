use crate::commands::{
    MAX_ITEM_BYTES, Mode, Shared, Stored, arithmetic, check_key, delete, flush_all, get, storage,
    touch,
};
use eggdb_platform::connection_limits::MemoryBudget;
use std::io::{self, Read, Write};

pub(crate) const REQUEST_MAGIC: u8 = 0x80;
const RESPONSE_MAGIC: u8 = 0x81;
const HEADER_BYTES: usize = 24;
const MOST_KEY_BYTES: usize = 250;
const MOST_EXTRAS_BYTES: usize = 20;

const NO_ERROR: u16 = 0x0000;
const KEY_NOT_FOUND: u16 = 0x0001;
const KEY_EXISTS: u16 = 0x0002;
const VALUE_TOO_LARGE: u16 = 0x0003;
const INVALID_ARGUMENTS: u16 = 0x0004;
const NOT_STORED: u16 = 0x0005;
const NON_NUMERIC: u16 = 0x0006;
const UNKNOWN_COMMAND: u16 = 0x0081;
const OUT_OF_MEMORY: u16 = 0x0082;

const GET: u8 = 0x00;
const SET: u8 = 0x01;
const ADD: u8 = 0x02;
const REPLACE: u8 = 0x03;
const DELETE: u8 = 0x04;
const INCREMENT: u8 = 0x05;
const DECREMENT: u8 = 0x06;
const QUIT: u8 = 0x07;
const FLUSH: u8 = 0x08;
const GET_QUIET: u8 = 0x09;
const NO_OP: u8 = 0x0a;
const VERSION: u8 = 0x0b;
const GET_KEY: u8 = 0x0c;
const GET_KEY_QUIET: u8 = 0x0d;
const APPEND: u8 = 0x0e;
const PREPEND: u8 = 0x0f;
const STAT: u8 = 0x10;
const SET_QUIET: u8 = 0x11;
const ADD_QUIET: u8 = 0x12;
const REPLACE_QUIET: u8 = 0x13;
const DELETE_QUIET: u8 = 0x14;
const INCREMENT_QUIET: u8 = 0x15;
const DECREMENT_QUIET: u8 = 0x16;
const QUIT_QUIET: u8 = 0x17;
const FLUSH_QUIET: u8 = 0x18;
const APPEND_QUIET: u8 = 0x19;
const PREPEND_QUIET: u8 = 0x1a;
const TOUCH: u8 = 0x1c;
const GET_AND_TOUCH: u8 = 0x1d;
const GET_AND_TOUCH_QUIET: u8 = 0x1e;
const SASL_LIST_MECHANISMS: u8 = 0x20;
const SASL_AUTH: u8 = 0x21;
const SASL_STEP: u8 = 0x22;
const NEVER_CREATE: u32 = 0xffff_ffff;

struct Request {
    opcode: u8,
    opaque: u32,
    cas: u64,
    extras: Vec<u8>,
    key: Vec<u8>,
    value: Vec<u8>,
}

#[derive(Default)]
struct Response {
    status: u16,
    cas: u64,
    extras: Vec<u8>,
    key: Vec<u8>,
    value: Vec<u8>,
}

impl Response {
    fn status(status: u16, message: &str) -> Self {
        Self {
            status,
            value: message.as_bytes().to_vec(),
            ..Self::default()
        }
    }

    fn encode(&self, opcode: u8, opaque: u32) -> Vec<u8> {
        let body = self.extras.len() + self.key.len() + self.value.len();
        let mut out = Vec::with_capacity(HEADER_BYTES + body);
        out.push(RESPONSE_MAGIC);
        out.push(opcode);
        out.extend_from_slice(&u16::try_from(self.key.len()).unwrap_or(u16::MAX).to_be_bytes());
        out.push(u8::try_from(self.extras.len()).unwrap_or(u8::MAX));
        out.push(0);
        out.extend_from_slice(&self.status.to_be_bytes());
        out.extend_from_slice(&u32::try_from(body).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(&opaque.to_be_bytes());
        out.extend_from_slice(&self.cas.to_be_bytes());
        out.extend_from_slice(&self.extras);
        out.extend_from_slice(&self.key);
        out.extend_from_slice(&self.value);
        out
    }
}

fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn be_u64(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

enum Incoming {
    Request(Request),
    Skipped(u8, u32, u16),
    End,
}

fn read_request(reader: &mut impl Read, budget_ok: &mut dyn FnMut(usize) -> bool) -> io::Result<Incoming> {
    let mut header = [0u8; HEADER_BYTES];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(Incoming::End),
        Err(error) => return Err(error),
    }
    if header[0] != REQUEST_MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad request magic"));
    }
    let opcode = header[1];
    let key_length = usize::from(u16::from_be_bytes([header[2], header[3]]));
    let extras_length = usize::from(header[4]);
    let body_length = be_u32(&header, 8).unwrap_or(0) as usize;
    let opaque = be_u32(&header, 12).unwrap_or(0);
    let cas = be_u64(&header, 16).unwrap_or(0);
    let well_formed = key_length <= MOST_KEY_BYTES
        && extras_length <= MOST_EXTRAS_BYTES
        && body_length >= key_length + extras_length;
    let value_length = body_length.saturating_sub(key_length + extras_length);
    if !well_formed || value_length > MAX_ITEM_BYTES || !budget_ok(body_length) {
        let skipped = io::copy(&mut reader.take(body_length as u64), &mut io::sink())?;
        if skipped < body_length as u64 {
            return Ok(Incoming::End);
        }
        let status = if !well_formed {
            INVALID_ARGUMENTS
        } else if value_length > MAX_ITEM_BYTES {
            VALUE_TOO_LARGE
        } else {
            OUT_OF_MEMORY
        };
        return Ok(Incoming::Skipped(opcode, opaque, status));
    }
    let mut body = vec![0u8; body_length];
    reader.read_exact(&mut body)?;
    let value = body.split_off(extras_length + key_length);
    let key = body.split_off(extras_length);
    Ok(Incoming::Request(Request {
        opcode,
        opaque,
        cas,
        extras: body,
        key,
        value,
    }))
}

fn is_quiet(opcode: u8) -> bool {
    matches!(
        opcode,
        GET_QUIET
            | GET_KEY_QUIET
            | SET_QUIET
            | ADD_QUIET
            | REPLACE_QUIET
            | DELETE_QUIET
            | INCREMENT_QUIET
            | DECREMENT_QUIET
            | QUIT_QUIET
            | FLUSH_QUIET
            | APPEND_QUIET
            | PREPEND_QUIET
            | GET_AND_TOUCH_QUIET
    )
}

fn failure(error: String) -> Response {
    if error.contains("out of memory") {
        Response::status(OUT_OF_MEMORY, "Out of memory")
    } else if error.contains("too large") {
        Response::status(VALUE_TOO_LARGE, "Too large.")
    } else if error.contains("non-numeric") {
        Response::status(NON_NUMERIC, "Non-numeric server-side value for incr or decr")
    } else {
        Response::status(INVALID_ARGUMENTS, &error)
    }
}

fn current_cas(shared: &Shared<'_>, key: &[u8], now: u64) -> u64 {
    get(shared, &[key.to_vec()], None, now)
        .ok()
        .and_then(|found| found.first().map(|(_, item)| item.cas))
        .unwrap_or(0)
}

fn reading(shared: &Shared<'_>, request: &Request, now: u64) -> Response {
    let touch_exptime = match request.opcode {
        GET_AND_TOUCH | GET_AND_TOUCH_QUIET => match be_u32(&request.extras, 0) {
            Some(exptime) => Some(i64::from(exptime)),
            None => return Response::status(INVALID_ARGUMENTS, "Invalid arguments"),
        },
        _ => None,
    };
    match get(shared, &[request.key.clone()], touch_exptime, now) {
        Ok(found) => match found.into_iter().next() {
            Some((key, item)) => Response {
                status: NO_ERROR,
                cas: item.cas,
                extras: item.flags.to_be_bytes().to_vec(),
                key: if matches!(request.opcode, GET_KEY | GET_KEY_QUIET) {
                    key
                } else {
                    Vec::new()
                },
                value: item.data,
            },
            None => Response {
                key: if matches!(request.opcode, GET_KEY | GET_KEY_QUIET) {
                    request.key.clone()
                } else {
                    Vec::new()
                },
                ..Response::status(KEY_NOT_FOUND, "Not found")
            },
        },
        Err(error) => failure(error),
    }
}

fn storing(shared: &Shared<'_>, request: &Request, now: u64) -> Response {
    let (flags, exptime) = match request.opcode {
        APPEND | PREPEND | APPEND_QUIET | PREPEND_QUIET => (0, 0),
        _ => match (be_u32(&request.extras, 0), be_u32(&request.extras, 4)) {
            (Some(flags), Some(exptime)) => (flags, i64::from(exptime)),
            _ => return Response::status(INVALID_ARGUMENTS, "Invalid arguments"),
        },
    };
    let mode = match request.opcode {
        SET | SET_QUIET if request.cas != 0 => Mode::Cas(request.cas),
        SET | SET_QUIET => Mode::Set,
        ADD | ADD_QUIET => Mode::Add,
        REPLACE | REPLACE_QUIET => Mode::Replace,
        APPEND | APPEND_QUIET => Mode::Append,
        _ => Mode::Prepend,
    };
    match storage(
        shared,
        mode,
        &request.key,
        flags,
        exptime,
        request.value.clone(),
        now,
    ) {
        Ok(Stored::Stored) => Response {
            cas: current_cas(shared, &request.key, now),
            ..Response::default()
        },
        Ok(Stored::NotStored) if matches!(mode, Mode::Add) => {
            Response::status(KEY_EXISTS, "Data exists for key.")
        }
        Ok(Stored::NotStored) if matches!(mode, Mode::Replace) => {
            Response::status(KEY_NOT_FOUND, "Not found")
        }
        Ok(Stored::NotStored) => Response::status(NOT_STORED, "Not stored."),
        Ok(Stored::Exists) => Response::status(KEY_EXISTS, "Data exists for key."),
        Ok(Stored::NotFound) => Response::status(KEY_NOT_FOUND, "Not found"),
        Err(error) => failure(error),
    }
}

fn counting(shared: &Shared<'_>, request: &Request, now: u64) -> Response {
    let (Some(delta), Some(initial), Some(exptime)) = (
        be_u64(&request.extras, 0),
        be_u64(&request.extras, 8),
        be_u32(&request.extras, 16),
    ) else {
        return Response::status(INVALID_ARGUMENTS, "Invalid arguments");
    };
    let increment = matches!(request.opcode, INCREMENT | INCREMENT_QUIET);
    let counted = match arithmetic(shared, &request.key, delta, increment, now) {
        Ok(Some(value)) => value,
        Ok(None) if exptime == NEVER_CREATE => return Response::status(KEY_NOT_FOUND, "Not found"),
        Ok(None) => {
            let stored = storage(
                shared,
                Mode::Add,
                &request.key,
                0,
                i64::from(exptime),
                initial.to_string().into_bytes(),
                now,
            );
            match stored {
                Ok(Stored::Stored) => initial,
                Ok(_) => return Response::status(KEY_EXISTS, "Data exists for key."),
                Err(error) => return failure(error),
            }
        }
        Err(error) => return failure(error),
    };
    Response {
        cas: current_cas(shared, &request.key, now),
        value: counted.to_be_bytes().to_vec(),
        ..Response::default()
    }
}

fn answer(shared: &Shared<'_>, request: &Request, now: u64) -> Response {
    if !request.key.is_empty()
        && !matches!(request.opcode, SASL_AUTH | SASL_STEP)
        && let Err(error) = check_key(&request.key)
    {
        return Response::status(INVALID_ARGUMENTS, &error);
    }
    match request.opcode {
        GET | GET_QUIET | GET_KEY | GET_KEY_QUIET | GET_AND_TOUCH | GET_AND_TOUCH_QUIET => {
            reading(shared, request, now)
        }
        SET | ADD | REPLACE | APPEND | PREPEND | SET_QUIET | ADD_QUIET | REPLACE_QUIET
        | APPEND_QUIET | PREPEND_QUIET => storing(shared, request, now),
        DELETE | DELETE_QUIET => match delete(shared, &request.key, now) {
            Ok(true) => Response::default(),
            Ok(false) => Response::status(KEY_NOT_FOUND, "Not found"),
            Err(error) => failure(error),
        },
        INCREMENT | DECREMENT | INCREMENT_QUIET | DECREMENT_QUIET => {
            counting(shared, request, now)
        }
        FLUSH | FLUSH_QUIET => match flush_all(shared) {
            Ok(()) => Response::default(),
            Err(error) => failure(error),
        },
        TOUCH => match be_u32(&request.extras, 0) {
            Some(exptime) => match touch(shared, &request.key, i64::from(exptime), now) {
                Ok(true) => Response::default(),
                Ok(false) => Response::status(KEY_NOT_FOUND, "Not found"),
                Err(error) => failure(error),
            },
            None => Response::status(INVALID_ARGUMENTS, "Invalid arguments"),
        },
        NO_OP | QUIT | QUIT_QUIET => Response::default(),
        VERSION => Response {
            value: crate::server::VERSION.as_bytes().to_vec(),
            ..Response::default()
        },
        SASL_LIST_MECHANISMS => Response {
            value: b"PLAIN".to_vec(),
            ..Response::default()
        },
        SASL_AUTH | SASL_STEP => Response {
            value: b"Authenticated".to_vec(),
            ..Response::default()
        },
        _ => Response::status(UNKNOWN_COMMAND, "Unknown command"),
    }
}

/// Serves memcached's binary protocol on one connection until the client quits or the input
/// ends.
pub(crate) fn serve_binary(
    shared: &Shared<'_>,
    budget: &MemoryBudget,
    mut reader: impl Read,
    writer: &mut impl Write,
) -> io::Result<()> {
    let mut held = budget.empty_reservation();
    loop {
        let read = read_request(&mut reader, &mut |bytes| held.hold_if_large(bytes))?;
        let request = match read {
            Incoming::End => return Ok(()),
            Incoming::Skipped(opcode, opaque, status) => {
                let message = match status {
                    VALUE_TOO_LARGE => "Too large.",
                    OUT_OF_MEMORY => "Out of memory",
                    _ => "Invalid arguments",
                };
                writer.write_all(&Response::status(status, message).encode(opcode, opaque))?;
                continue;
            }
            Incoming::Request(request) => request,
        };
        let now = crate::server::now_ms();
        if request.opcode == STAT {
            writer.write_all(&Response::default().encode(STAT, request.opaque))?;
            continue;
        }
        let response = answer(shared, &request, now);
        let silent = is_quiet(request.opcode)
            && (response.status == NO_ERROR
                || (matches!(request.opcode, GET_QUIET | GET_KEY_QUIET | GET_AND_TOUCH_QUIET)
                    && response.status == KEY_NOT_FOUND));
        if !silent {
            writer.write_all(&response.encode(request.opcode, request.opaque))?;
        }
        held.hold_if_large(0);
        if matches!(request.opcode, QUIT | QUIT_QUIET) {
            writer.flush()?;
            return Ok(());
        }
    }
}
