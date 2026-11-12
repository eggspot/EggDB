use std::ffi::{CStr, c_char};

pub(crate) const SQL_NTS: isize = -3;

pub(crate) fn input_text(text: *const u8, length: isize) -> Option<String> {
    if text.is_null() {
        return None;
    }
    if length == SQL_NTS {
        // SAFETY: ODBC passes SQL_NTS only with a NUL-terminated string.
        let terminated = unsafe { CStr::from_ptr(text.cast::<c_char>()) };
        return Some(terminated.to_string_lossy().into_owned());
    }
    let length = usize::try_from(length).ok()?;
    // SAFETY: ODBC passes a length with `length` readable bytes at `text`.
    let bytes = unsafe { std::slice::from_raw_parts(text, length) };
    Some(String::from_utf8_lossy(bytes).into_owned())
}

pub(crate) fn input_bytes(value: *const u8, length: usize) -> Vec<u8> {
    if value.is_null() || length == 0 {
        return Vec::new();
    }
    // SAFETY: the application bound `length` readable bytes at `value` for the execution.
    unsafe { std::slice::from_raw_parts(value, length) }.to_vec()
}

pub(crate) fn terminated_length(value: *const u8) -> usize {
    // SAFETY: a character parameter with SQL_NTS is NUL-terminated (ODBC).
    unsafe { CStr::from_ptr(value.cast::<c_char>()) }.to_bytes().len()
}

pub(crate) fn store<T: Copy>(target: *mut T, value: T) {
    if target.is_null() {
        return;
    }
    // SAFETY: a non-NULL output pointer from ODBC points to writable memory for one `T`, which
    // may be unaligned in buffers applications allocate.
    unsafe { target.write_unaligned(value) };
}

pub(crate) fn read<T: Copy>(source: *const T) -> Option<T> {
    if source.is_null() {
        return None;
    }
    // SAFETY: a non-NULL input pointer from ODBC points to one readable `T`.
    Some(unsafe { source.read_unaligned() })
}

pub(crate) fn copy_into(target: *mut u8, bytes: &[u8]) {
    if target.is_null() || bytes.is_empty() {
        return;
    }
    // SAFETY: callers pass at most the buffer length the application gave for `target`.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), target, bytes.len()) };
}

pub(crate) fn zero(target: *mut u8, count: usize) {
    if target.is_null() || count == 0 {
        return;
    }
    // SAFETY: callers pass a count that fits the buffer the application gave for `target`.
    unsafe { std::ptr::write_bytes(target, 0, count) };
}

pub(crate) fn write_text(target: *mut u8, buffer_length: isize, text: &[u8], terminator: usize) -> bool {
    let capacity = usize::try_from(buffer_length).unwrap_or(0);
    if capacity < terminator {
        return !text.is_empty();
    }
    let room = capacity - terminator;
    let written = text.len().min(room - room % terminator.max(1));
    copy_into(target, &text[..written]);
    if !target.is_null() {
        zero(target.wrapping_add(written), terminator);
    }
    written < text.len()
}
