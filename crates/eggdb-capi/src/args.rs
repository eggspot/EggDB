use crate::status::Failure;
use std::ffi::{CStr, c_char};

pub(crate) unsafe fn bytes<'a>(data: *const u8, len: usize, what: &str) -> Result<&'a [u8], Failure> {
    if len == 0 {
        return Ok(&[]);
    }
    if data.is_null() {
        return Err(Failure::invalid_argument(format!(
            "{what} is NULL but its length is {len}; pass a pointer to {len} bytes, or a length of 0"
        )));
    }
    if isize::try_from(len).is_err() {
        return Err(Failure::invalid_argument(format!(
            "{what} is {len} bytes long, more than any buffer can be"
        )));
    }
    // SAFETY: eggdb.h asks for `len` readable bytes at a non-NULL `data` that stay unchanged
    // during the call; `data` is not NULL and `len` fits an isize.
    Ok(unsafe { std::slice::from_raw_parts(data, len) })
}

pub(crate) unsafe fn optional_bytes<'a>(
    data: *const u8,
    len: usize,
    what: &str,
) -> Result<Option<&'a [u8]>, Failure> {
    if data.is_null() {
        if len != 0 {
            return Err(Failure::invalid_argument(format!(
                "{what} is NULL, which means none, but its length is {len}; pass a length of 0 with NULL"
            )));
        }
        return Ok(None);
    }
    // SAFETY: the caller's promise for `data` and `len` is the one `bytes` needs.
    unsafe { bytes(data, len, what) }.map(Some)
}

pub(crate) unsafe fn text<'a>(text: *const c_char, what: &str) -> Result<&'a str, Failure> {
    if text.is_null() {
        return Err(Failure::invalid_argument(format!(
            "{what} is NULL; pass a NUL-terminated UTF-8 string"
        )));
    }
    // SAFETY: eggdb.h asks for a NUL-terminated string that stays unchanged during the call,
    // and `text` is not NULL.
    let text = unsafe { CStr::from_ptr(text) };
    text.to_str().map_err(|_| {
        Failure::invalid_argument(format!("{what} is not UTF-8; EggDB takes text as UTF-8"))
    })
}

pub(crate) unsafe fn optional_text<'a>(
    text: *const c_char,
    what: &str,
) -> Result<Option<&'a str>, Failure> {
    if text.is_null() {
        return Ok(None);
    }
    // SAFETY: the caller's promise for `text` is the one `self::text` needs.
    unsafe { self::text(text, what) }.map(Some)
}

pub(crate) unsafe fn handle<'a, T>(handle: *const T, what: &str) -> Result<&'a T, Failure> {
    if handle.is_null() {
        return Err(Failure::invalid_argument(format!("{what} is NULL")));
    }
    // SAFETY: eggdb.h asks for a handle this library returned and that was not ended or freed,
    // so it points to a live, aligned `T` that this library made with `Box::into_raw`.
    Ok(unsafe { &*handle })
}

pub(crate) unsafe fn handle_mut<'a, T>(handle: *mut T, what: &str) -> Result<&'a mut T, Failure> {
    if handle.is_null() {
        return Err(Failure::invalid_argument(format!("{what} is NULL")));
    }
    // SAFETY: as in `handle`; eggdb.h also asks that a transaction, scan or SQL handle is used
    // by one call at a time, so no other reference to it is live.
    Ok(unsafe { &mut *handle })
}

pub(crate) unsafe fn optional_struct<'a, T>(value: *const T) -> Option<&'a T> {
    if value.is_null() {
        return None;
    }
    // SAFETY: eggdb.h asks for NULL or a pointer to an initialized, aligned struct of this
    // type that stays unchanged during the call.
    Some(unsafe { &*value })
}

pub(crate) unsafe fn take<T>(handle: *mut T) -> Option<Box<T>> {
    if handle.is_null() {
        return None;
    }
    // SAFETY: eggdb.h asks for a handle this library returned and that was not ended or freed;
    // this library made it with `Box::into_raw`, and the caller gives it up with this call.
    Some(unsafe { Box::from_raw(handle) })
}

pub(crate) fn give<T>(value: T) -> *mut T {
    Box::into_raw(Box::new(value))
}

pub(crate) unsafe fn write<T>(out: *mut T, value: T, what: &str) -> Result<(), Failure> {
    if out.is_null() {
        return Err(Failure::invalid_argument(format!(
            "{what} is NULL; pass where to put the result"
        )));
    }
    // SAFETY: eggdb.h asks for a pointer to writable, aligned memory for one value of this
    // type, and `out` is not NULL.
    unsafe { out.write(value) };
    Ok(())
}

pub(crate) unsafe fn write_if_wanted<T>(out: *mut T, value: T) {
    if out.is_null() {
        return;
    }
    // SAFETY: eggdb.h asks for NULL or a pointer to writable, aligned memory for one value of
    // this type, and `out` is not NULL.
    unsafe { out.write(value) };
}

pub(crate) fn library_bytes(bytes: Box<[u8]>) -> (*mut u8, usize) {
    let len = bytes.len();
    (Box::into_raw(bytes).cast::<u8>(), len)
}

pub(crate) fn flag(value: bool) -> std::ffi::c_int {
    std::ffi::c_int::from(value)
}
