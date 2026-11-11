use crate::frame::{Body, Out};

const ROWS_KIND: i32 = 2;
const GLOBAL_TABLE_SPEC: i32 = 0x0001;
const HAS_MORE_PAGES: i32 = 0x0002;
const LIST_TYPE: u16 = 0x0020;
const MAP_TYPE: u16 = 0x0021;
const SET_TYPE: u16 = 0x0022;
const STATE_BYTES: usize = 16;

/// The paging a `QUERY` or `EXECUTE` asks for: its page size, when positive, and the state a
/// previous page ended with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PageRequest {
    pub(crate) page_size: Option<usize>,
    pub(crate) state: Option<Vec<u8>>,
}

/// The first 8 bytes of the SHA-256 of a statement's text and bound values, which a paging
/// state carries so it is only accepted for the same query.
pub(crate) fn fingerprint(text: &str, values: &[Option<Vec<u8>>]) -> [u8; 8] {
    let mut seed = text.as_bytes().to_vec();
    for value in values {
        match value {
            Some(bytes) => {
                seed.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
                seed.extend_from_slice(bytes);
            }
            None => seed.extend_from_slice(&u32::MAX.to_be_bytes()),
        }
    }
    let digest = eggdb_crypto::sha256(&seed);
    let mut out = [0u8; 8];
    out.copy_from_slice(&digest[..8]);
    out
}

fn start_of(state: Option<&[u8]>, fingerprint: [u8; 8]) -> Result<usize, String> {
    let Some(state) = state else {
        return Ok(0);
    };
    if state.len() != STATE_BYTES || state[8..] != fingerprint {
        return Err("Invalid value for the paging state".to_owned());
    }
    let mut offset = [0u8; 8];
    offset.copy_from_slice(&state[..8]);
    usize::try_from(u64::from_be_bytes(offset)).map_err(|_| "Invalid value for the paging state".to_owned())
}

fn skip_type(body: &mut Body<'_>) -> Result<(), String> {
    match body.short()? {
        LIST_TYPE | SET_TYPE => {
            body.short()?;
        }
        MAP_TYPE => {
            body.short()?;
            body.short()?;
        }
        _ => {}
    }
    Ok(())
}

/// A rows result cut to one page of `request`: the rows from the offset its state names, at
/// most its page size of them, with `HAS_MORE_PAGES` and the next state when rows remain. Any
/// other result, and any request without a page size, comes back unchanged.
pub(crate) fn page(result: Vec<u8>, request: &PageRequest, fingerprint: [u8; 8]) -> Result<Vec<u8>, String> {
    let Some(page_size) = request.page_size else {
        return Ok(result);
    };
    let mut body = Body::new(&result);
    if body.int()? != ROWS_KIND {
        return Ok(result);
    }
    let flags = body.int()?;
    let column_count = body.int()?;
    let metadata_start = body.position();
    if flags & GLOBAL_TABLE_SPEC != 0 {
        body.string()?;
        body.string()?;
    }
    for _ in 0..column_count {
        if flags & GLOBAL_TABLE_SPEC == 0 {
            body.string()?;
            body.string()?;
        }
        body.string()?;
        skip_type(&mut body)?;
    }
    let metadata_end = body.position();
    let row_count = usize::try_from(body.int()?).map_err(|_| "a rows result has a negative row count")?;
    let columns = usize::try_from(column_count).map_err(|_| "a rows result has a negative column count")?;
    let start = start_of(request.state.as_deref(), fingerprint)?;
    let mut rows = Vec::new();
    for index in 0..row_count {
        let row_start = body.position();
        for _ in 0..columns {
            body.bytes()?;
        }
        if index >= start && index < start.saturating_add(page_size) {
            rows.push(&result[row_start..body.position()]);
        }
    }
    let next = start.saturating_add(page_size);
    let more = next < row_count;
    let mut out = Out::default();
    out.int(ROWS_KIND)
        .int(if more { flags | HAS_MORE_PAGES } else { flags & !HAS_MORE_PAGES })
        .int(column_count);
    if more {
        let mut state = (next as u64).to_be_bytes().to_vec();
        state.extend_from_slice(&fingerprint);
        out.bytes(Some(&state));
    }
    out.bytes.extend_from_slice(&result[metadata_start..metadata_end]);
    out.int(i32::try_from(rows.len()).unwrap_or(i32::MAX));
    for row in rows {
        out.bytes.extend_from_slice(row);
    }
    Ok(out.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(rows: i32) -> Vec<u8> {
        let mut out = Out::default();
        out.int(ROWS_KIND).int(GLOBAL_TABLE_SPEC).int(1).string("ks").string("t").string("id").short(0x0009);
        out.int(rows);
        for row in 0..rows {
            out.bytes(Some(&row.to_be_bytes()));
        }
        out.bytes
    }

    fn rows_of(page: &[u8]) -> (bool, Option<Vec<u8>>, i32) {
        let mut body = Body::new(page);
        body.int().unwrap();
        let flags = body.int().unwrap();
        body.int().unwrap();
        let state = if flags & HAS_MORE_PAGES != 0 { body.bytes().unwrap() } else { None };
        for _ in 0..3 {
            body.string().unwrap();
        }
        body.short().unwrap();
        (flags & HAS_MORE_PAGES != 0, state, body.int().unwrap())
    }

    #[test]
    fn pages_follow_their_state() {
        let print = fingerprint("SELECT id FROM t", &[]);
        let first = page(result(5), &PageRequest { page_size: Some(2), state: None }, print).unwrap();
        let (more, state, count) = rows_of(&first);
        assert!(more);
        assert_eq!(count, 2);
        let last = page(result(5), &PageRequest { page_size: Some(4), state: state.clone() }, print).unwrap();
        assert_eq!(rows_of(&last), (false, None, 3));
        let other = fingerprint("SELECT id FROM u", &[]);
        assert!(page(result(5), &PageRequest { page_size: Some(2), state }, other).is_err());
        assert_eq!(page(result(1), &PageRequest::default(), print).unwrap(), result(1));
    }
}
