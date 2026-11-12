#![allow(non_snake_case, clippy::too_many_arguments)]

use crate::api::{
    SQLColAttribute, SQLColumns, SQLConnect, SQLDescribeCol, SQLDriverConnect, SQLExecDirect,
    SQLGetDiagRec, SQLGetInfo, SQLPrepare, SQLTables,
};
use crate::memory::{SQL_NTS, store};
use std::ffi::{CString, c_void};
use std::ptr;

type SqlReturn = i16;

const SQL_SUCCESS_WITH_INFO: SqlReturn = 1;
const SQL_ERROR: SqlReturn = -1;
const SCRATCH_BYTES: usize = 4096;

fn units(text: *const u16, length: isize) -> Option<Vec<u16>> {
    if text.is_null() {
        return None;
    }
    let count = if length == SQL_NTS {
        let mut count = 0usize;
        loop {
            // SAFETY: ODBC passes SQL_NTS only with a NUL-terminated UTF-16 string, so every unit up
            // to and including the terminator is readable.
            let unit = unsafe { text.add(count).read_unaligned() };
            if unit == 0 {
                break count;
            }
            count += 1;
        }
    } else {
        usize::try_from(length).ok()?
    };
    // SAFETY: the string holds `count` readable units, counted above or given by ODBC.
    let slice = unsafe { std::slice::from_raw_parts(text, count) };
    Some(slice.to_vec())
}

fn narrowed(text: *const u16, length: isize) -> Result<Option<CString>, SqlReturn> {
    let Some(units) = units(text, length) else {
        return Ok(None);
    };
    let decoded = String::from_utf16(&units).map_err(|_| SQL_ERROR)?;
    CString::new(decoded).map(Some).map_err(|_| SQL_ERROR)
}

fn pointer(text: &Option<CString>) -> *const u8 {
    text.as_ref().map_or(ptr::null(), |text| text.as_ptr().cast())
}

fn length_of(text: &Option<CString>) -> i16 {
    if text.is_some() { SQL_NTS as i16 } else { 0 }
}

fn widen_into(target: *mut u16, capacity_units: usize, text: &[u8]) -> (bool, usize) {
    let wide: Vec<u16> = String::from_utf8_lossy(text).encode_utf16().collect();
    if target.is_null() || capacity_units == 0 {
        return (!wide.is_empty(), wide.len());
    }
    let written = wide.len().min(capacity_units - 1);
    for (index, unit) in wide.iter().take(written).chain(std::iter::once(&0)).enumerate() {
        // SAFETY: the application's buffer holds `capacity_units` units and `index` is below it.
        unsafe { target.add(index).write_unaligned(*unit) };
    }
    (written < wide.len(), wide.len())
}

fn used(bytes: &[u8], length: i16) -> &[u8] {
    &bytes[..usize::try_from(length).unwrap_or(0).min(bytes.len())]
}

/// [`SQLConnect`] with UTF-16 text.
#[unsafe(no_mangle)]
pub extern "system" fn SQLConnectW(dbc: *mut c_void, server: *const u16, server_length: i16, user: *const u16, user_length: i16, password: *const u16, password_length: i16) -> SqlReturn {
    let (Ok(server), Ok(user), Ok(password)) = (
        narrowed(server, isize::from(server_length)),
        narrowed(user, isize::from(user_length)),
        narrowed(password, isize::from(password_length)),
    ) else {
        return SQL_ERROR;
    };
    SQLConnect(dbc, pointer(&server), length_of(&server), pointer(&user), length_of(&user), pointer(&password), length_of(&password))
}

/// [`SQLDriverConnect`] with UTF-16 text; lengths in characters.
#[unsafe(no_mangle)]
pub extern "system" fn SQLDriverConnectW(dbc: *mut c_void, window: *mut c_void, input: *const u16, input_length: i16, output: *mut u16, output_capacity: i16, output_length: *mut i16, completion: u16) -> SqlReturn {
    let Ok(input) = narrowed(input, isize::from(input_length)) else {
        return SQL_ERROR;
    };
    let mut scratch = vec![0u8; SCRATCH_BYTES];
    let mut written = 0i16;
    let outcome = SQLDriverConnect(dbc, window, pointer(&input), length_of(&input), scratch.as_mut_ptr(), SCRATCH_BYTES as i16, &raw mut written, completion);
    let (truncated, total) = widen_into(output, usize::try_from(output_capacity).unwrap_or(0), used(&scratch, written));
    store(output_length, i16::try_from(total).unwrap_or(i16::MAX));
    if truncated && outcome >= 0 { SQL_SUCCESS_WITH_INFO } else { outcome }
}

/// [`SQLExecDirect`] with UTF-16 text.
#[unsafe(no_mangle)]
pub extern "system" fn SQLExecDirectW(stmt: *mut c_void, text: *const u16, length: i32) -> SqlReturn {
    match narrowed(text, length as isize) {
        Ok(text) => SQLExecDirect(stmt, pointer(&text), SQL_NTS as i32),
        Err(outcome) => outcome,
    }
}

/// [`SQLPrepare`] with UTF-16 text.
#[unsafe(no_mangle)]
pub extern "system" fn SQLPrepareW(stmt: *mut c_void, text: *const u16, length: i32) -> SqlReturn {
    match narrowed(text, length as isize) {
        Ok(text) => SQLPrepare(stmt, pointer(&text), SQL_NTS as i32),
        Err(outcome) => outcome,
    }
}

/// [`SQLDescribeCol`] with the name in UTF-16; lengths in characters.
#[unsafe(no_mangle)]
pub extern "system" fn SQLDescribeColW(stmt: *mut c_void, number: u16, name: *mut u16, name_capacity: i16, name_length: *mut i16, data_type: *mut i16, column_size: *mut usize, digits: *mut i16, nullable: *mut i16) -> SqlReturn {
    let mut scratch = vec![0u8; SCRATCH_BYTES];
    let mut written = 0i16;
    let outcome = SQLDescribeCol(stmt, number, scratch.as_mut_ptr(), SCRATCH_BYTES as i16, &raw mut written, data_type, column_size, digits, nullable);
    if outcome < 0 {
        return outcome;
    }
    let (truncated, total) = widen_into(name, usize::try_from(name_capacity).unwrap_or(0), used(&scratch, written));
    store(name_length, i16::try_from(total).unwrap_or(i16::MAX));
    if truncated { SQL_SUCCESS_WITH_INFO } else { outcome }
}

/// [`SQLColAttribute`] with text attributes in UTF-16; the buffer length in bytes.
#[unsafe(no_mangle)]
pub extern "system" fn SQLColAttributeW(stmt: *mut c_void, number: u16, field: u16, text: *mut c_void, text_capacity: i16, text_length: *mut i16, numeric: *mut isize) -> SqlReturn {
    let mut scratch = vec![0u8; SCRATCH_BYTES];
    let mut written = 0i16;
    let outcome = SQLColAttribute(stmt, number, field, scratch.as_mut_ptr().cast(), SCRATCH_BYTES as i16, &raw mut written, numeric);
    if outcome < 0 || written == 0 {
        return outcome;
    }
    let capacity_units = usize::try_from(text_capacity).unwrap_or(0) / 2;
    let (truncated, total) = widen_into(text.cast(), capacity_units, used(&scratch, written));
    store(text_length, i16::try_from(total * 2).unwrap_or(i16::MAX));
    if truncated { SQL_SUCCESS_WITH_INFO } else { outcome }
}

/// [`SQLGetDiagRec`] with the state and message in UTF-16; lengths in characters.
#[unsafe(no_mangle)]
pub extern "system" fn SQLGetDiagRecW(handle_type: i16, handle: *mut c_void, record: i16, sqlstate: *mut u16, native: *mut i32, message: *mut u16, capacity: i16, written: *mut i16) -> SqlReturn {
    let mut state = [0u8; 6];
    let mut scratch = vec![0u8; SCRATCH_BYTES];
    let mut length = 0i16;
    let outcome = SQLGetDiagRec(handle_type, handle, record, state.as_mut_ptr(), native, scratch.as_mut_ptr(), SCRATCH_BYTES as i16, &raw mut length);
    if outcome < 0 || outcome == 100 {
        return outcome;
    }
    widen_into(sqlstate, 6, &state[..5]);
    let (truncated, total) = widen_into(message, usize::try_from(capacity).unwrap_or(0), used(&scratch, length));
    store(written, i16::try_from(total).unwrap_or(i16::MAX));
    if truncated { SQL_SUCCESS_WITH_INFO } else { outcome }
}

/// [`SQLGetInfo`] with text answers in UTF-16; the buffer length in bytes.
#[unsafe(no_mangle)]
pub extern "system" fn SQLGetInfoW(dbc: *mut c_void, info: u16, value: *mut c_void, capacity: i16, written: *mut i16) -> SqlReturn {
    let mut scratch = vec![0u8; SCRATCH_BYTES];
    let mut length = 0i16;
    let outcome = SQLGetInfo(dbc, info, scratch.as_mut_ptr().cast(), SCRATCH_BYTES as i16, &raw mut length);
    if outcome < 0 {
        return outcome;
    }
    let textual = matches!(info, 2 | 6 | 7 | 13 | 17 | 18 | 29 | 77);
    if !textual {
        let size = usize::try_from(length).unwrap_or(0).min(usize::try_from(capacity).unwrap_or(usize::MAX));
        crate::memory::copy_into(value.cast(), &scratch[..size]);
        store(written, length);
        return outcome;
    }
    let (truncated, total) = widen_into(value.cast(), usize::try_from(capacity).unwrap_or(0) / 2, used(&scratch, length));
    store(written, i16::try_from(total * 2).unwrap_or(i16::MAX));
    if truncated { SQL_SUCCESS_WITH_INFO } else { outcome }
}

/// [`SQLTables`] with UTF-16 patterns.
#[unsafe(no_mangle)]
pub extern "system" fn SQLTablesW(stmt: *mut c_void, catalog: *const u16, catalog_length: i16, schema: *const u16, schema_length: i16, table: *const u16, table_length: i16, types: *const u16, types_length: i16) -> SqlReturn {
    let (Ok(catalog), Ok(schema), Ok(table), Ok(types)) = (
        narrowed(catalog, isize::from(catalog_length)),
        narrowed(schema, isize::from(schema_length)),
        narrowed(table, isize::from(table_length)),
        narrowed(types, isize::from(types_length)),
    ) else {
        return SQL_ERROR;
    };
    SQLTables(stmt, pointer(&catalog), length_of(&catalog), pointer(&schema), length_of(&schema), pointer(&table), length_of(&table), pointer(&types), length_of(&types))
}

/// [`SQLColumns`] with UTF-16 patterns.
#[unsafe(no_mangle)]
pub extern "system" fn SQLColumnsW(stmt: *mut c_void, catalog: *const u16, catalog_length: i16, schema: *const u16, schema_length: i16, table: *const u16, table_length: i16, column: *const u16, column_length: i16) -> SqlReturn {
    let (Ok(catalog), Ok(schema), Ok(table), Ok(column)) = (
        narrowed(catalog, isize::from(catalog_length)),
        narrowed(schema, isize::from(schema_length)),
        narrowed(table, isize::from(table_length)),
        narrowed(column, isize::from(column_length)),
    ) else {
        return SQL_ERROR;
    };
    SQLColumns(stmt, pointer(&catalog), length_of(&catalog), pointer(&schema), length_of(&schema), pointer(&table), length_of(&table), pointer(&column), length_of(&column))
}

#[cfg(test)]
mod tests {
    use super::{narrowed, widen_into};

    #[test]
    fn text_crosses_between_utf16_and_utf8() {
        let wide: Vec<u16> = "café\0".encode_utf16().collect();
        let narrow = narrowed(wide.as_ptr(), -3).ok().flatten().unwrap();
        assert_eq!(narrow.to_str().unwrap(), "café");
        let mut out = [0u16; 3];
        let (truncated, total) = widen_into(out.as_mut_ptr(), 3, "café".as_bytes());
        assert!(truncated);
        assert_eq!(total, 4);
        assert_eq!(out, [u16::from(b'c'), u16::from(b'a'), 0]);
    }
}
