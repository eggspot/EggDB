use eggdb_parquet::{Cell, Column, ColumnType};
use std::ffi::{CString, c_char, c_int, c_void};

/// The Arrow C data interface's `ArrowSchema`.
#[repr(C)]
#[derive(Debug)]
pub struct ArrowSchema {
    /// The type's format string.
    pub format: *const c_char,
    /// The field name.
    pub name: *const c_char,
    /// The metadata, NULL for none.
    pub metadata: *const c_char,
    /// `ARROW_FLAG_NULLABLE` and the others.
    pub flags: i64,
    /// How many children.
    pub n_children: i64,
    /// The children.
    pub children: *mut *mut ArrowSchema,
    /// The dictionary, NULL for none.
    pub dictionary: *mut ArrowSchema,
    /// Frees the schema; NULL once released.
    pub release: Option<unsafe extern "C" fn(*mut ArrowSchema)>,
    /// The producer's data.
    pub private_data: *mut c_void,
}

/// The Arrow C data interface's `ArrowArray`.
#[repr(C)]
#[derive(Debug)]
pub struct ArrowArray {
    /// How many values.
    pub length: i64,
    /// How many of them are null.
    pub null_count: i64,
    /// The first value's offset.
    pub offset: i64,
    /// How many buffers.
    pub n_buffers: i64,
    /// How many children.
    pub n_children: i64,
    /// The buffers.
    pub buffers: *mut *const c_void,
    /// The children.
    pub children: *mut *mut ArrowArray,
    /// The dictionary, NULL for none.
    pub dictionary: *mut ArrowArray,
    /// Frees the array; NULL once released.
    pub release: Option<unsafe extern "C" fn(*mut ArrowArray)>,
    /// The producer's data.
    pub private_data: *mut c_void,
}

/// The Arrow C stream interface's `ArrowArrayStream`.
#[repr(C)]
#[derive(Debug)]
pub struct ArrowArrayStream {
    /// Writes the stream's schema.
    pub get_schema: Option<unsafe extern "C" fn(*mut ArrowArrayStream, *mut ArrowSchema) -> c_int>,
    /// Writes the next batch, or a released array at the end.
    pub get_next: Option<unsafe extern "C" fn(*mut ArrowArrayStream, *mut ArrowArray) -> c_int>,
    /// The last error's text.
    pub get_last_error: Option<unsafe extern "C" fn(*mut ArrowArrayStream) -> *const c_char>,
    /// Frees the stream; NULL once released.
    pub release: Option<unsafe extern "C" fn(*mut ArrowArrayStream)>,
    /// The producer's data.
    pub private_data: *mut c_void,
}

const ARROW_FLAG_NULLABLE: i64 = 2;
const EINVAL: c_int = 22;

fn format_of(column_type: ColumnType) -> &'static str {
    match column_type {
        ColumnType::Boolean => "b",
        ColumnType::Int32 => "i",
        ColumnType::Int64 => "l",
        ColumnType::Float => "f",
        ColumnType::Double => "g",
        ColumnType::Text => "u",
        ColumnType::Binary => "z",
        ColumnType::Date => "tdD",
        ColumnType::Timestamp { utc: false } => "tsu:",
        ColumnType::Timestamp { utc: true } => "tsu:UTC",
        ColumnType::Decimal { .. } => "u",
    }
}

fn text(value: &str) -> CString {
    CString::new(value.replace('\0', "")).unwrap_or_default()
}

struct SchemaParts {
    format: CString,
    name: CString,
    children: Vec<*mut ArrowSchema>,
}

unsafe extern "C" fn release_schema(schema: *mut ArrowSchema) {
    if schema.is_null() {
        return;
    }
    // SAFETY: the consumer releases a schema this module wrote, which is live until now.
    let schema = unsafe { &mut *schema };
    if schema.private_data.is_null() {
        schema.release = None;
        return;
    }
    // SAFETY: `private_data` is the `SchemaParts` box `fill_schema` leaked, taken back once.
    let parts = unsafe { Box::from_raw(schema.private_data.cast::<SchemaParts>()) };
    for &child in &parts.children {
        // SAFETY: each child is a live schema `fill_schema` leaked, owned by this schema alone.
        let release = unsafe { (*child).release };
        if let Some(release) = release {
            // SAFETY: the child is a live schema of this module, released once here.
            unsafe { release(child) };
        }
        // SAFETY: the child came from `Box::into_raw` in `fill_schema` and is freed once here.
        drop(unsafe { Box::from_raw(child) });
    }
    schema.private_data = std::ptr::null_mut();
    schema.release = None;
}

fn empty_schema() -> ArrowSchema {
    ArrowSchema {
        format: std::ptr::null(),
        name: std::ptr::null(),
        metadata: std::ptr::null(),
        flags: 0,
        n_children: 0,
        children: std::ptr::null_mut(),
        dictionary: std::ptr::null_mut(),
        release: None,
        private_data: std::ptr::null_mut(),
    }
}

fn fill_schema(out: &mut ArrowSchema, format: &str, name: &str, flags: i64, children: Vec<ArrowSchema>) {
    let mut parts = Box::new(SchemaParts {
        format: text(format),
        name: text(name),
        children: children.into_iter().map(|child| Box::into_raw(Box::new(child))).collect(),
    });
    out.format = parts.format.as_ptr();
    out.name = parts.name.as_ptr();
    out.metadata = std::ptr::null();
    out.flags = flags;
    out.n_children = parts.children.len() as i64;
    out.children = parts.children.as_mut_ptr();
    out.dictionary = std::ptr::null_mut();
    out.release = Some(release_schema);
    out.private_data = Box::into_raw(parts).cast();
}

/// Writes the schema of a struct of `columns` (the record batch's schema).
pub fn export_schema(columns: &[Column], out: &mut ArrowSchema) {
    let children = columns
        .iter()
        .map(|column| {
            let mut child = empty_schema();
            fill_schema(&mut child, format_of(column.column_type), &column.name, ARROW_FLAG_NULLABLE, Vec::new());
            child
        })
        .collect();
    fill_schema(out, "+s", "", 0, children);
}

struct ArrayParts {
    words: Vec<Vec<u64>>,
    buffers: Vec<*const c_void>,
    children: Vec<*mut ArrowArray>,
}

unsafe extern "C" fn release_array(array: *mut ArrowArray) {
    if array.is_null() {
        return;
    }
    // SAFETY: the consumer releases an array this module wrote, which is live until now.
    let array = unsafe { &mut *array };
    if !array.private_data.is_null() {
        // SAFETY: `private_data` is the `ArrayParts` box `fill_array` leaked, taken back once.
        let parts = unsafe { Box::from_raw(array.private_data.cast::<ArrayParts>()) };
        for &child in &parts.children {
            // SAFETY: each child is a live array `fill_array` leaked, owned by this array alone.
            let release = unsafe { (*child).release };
            if let Some(release) = release {
                // SAFETY: the child is a live array of this module, released once here.
                unsafe { release(child) };
            }
            // SAFETY: the child came from `Box::into_raw` in `fill_array` and is freed once here.
            drop(unsafe { Box::from_raw(child) });
        }
    }
    array.private_data = std::ptr::null_mut();
    array.release = None;
}

fn aligned(bytes: &[u8]) -> Vec<u64> {
    let mut words: Vec<u64> = bytes
        .chunks(8)
        .map(|chunk| {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            u64::from_ne_bytes(word)
        })
        .collect();
    if words.is_empty() {
        words.push(0);
    }
    words
}

fn column_buffers(column: &Column) -> (Vec<Vec<u8>>, i64) {
    let length = column.cells.len();
    let mut validity = vec![0u8; length.div_ceil(8)];
    let mut nulls = 0i64;
    for (at, cell) in column.cells.iter().enumerate() {
        if matches!(cell, Cell::Null) {
            nulls += 1;
        } else {
            validity[at / 8] |= 1 << (at % 8);
        }
    }
    let mut buffers = vec![validity];
    match column.column_type {
        ColumnType::Boolean => {
            let mut values = vec![0u8; length.div_ceil(8)];
            for (at, cell) in column.cells.iter().enumerate() {
                if matches!(cell, Cell::Boolean(true)) {
                    values[at / 8] |= 1 << (at % 8);
                }
            }
            buffers.push(values);
        }
        ColumnType::Int32 | ColumnType::Date => buffers.push(
            column
                .cells
                .iter()
                .flat_map(|cell| match cell {
                    Cell::Int(value) => i32::try_from(*value).unwrap_or(0).to_le_bytes(),
                    _ => [0; 4],
                })
                .collect(),
        ),
        ColumnType::Int64 | ColumnType::Timestamp { .. } => buffers.push(
            column
                .cells
                .iter()
                .flat_map(|cell| match cell {
                    Cell::Int(value) => value.to_le_bytes(),
                    _ => [0; 8],
                })
                .collect(),
        ),
        ColumnType::Float => buffers.push(
            column
                .cells
                .iter()
                .flat_map(|cell| match cell {
                    Cell::Float(value) => (*value as f32).to_le_bytes(),
                    _ => [0; 4],
                })
                .collect(),
        ),
        ColumnType::Double => buffers.push(
            column
                .cells
                .iter()
                .flat_map(|cell| match cell {
                    Cell::Float(value) => value.to_le_bytes(),
                    _ => [0; 8],
                })
                .collect(),
        ),
        ColumnType::Text | ColumnType::Binary | ColumnType::Decimal { .. } => {
            let mut offsets = Vec::with_capacity((length + 1) * 4);
            let mut data = Vec::new();
            offsets.extend_from_slice(&0i32.to_le_bytes());
            for cell in &column.cells {
                if let Cell::Bytes(bytes) = cell {
                    data.extend_from_slice(bytes);
                }
                offsets.extend_from_slice(&i32::try_from(data.len()).unwrap_or(i32::MAX).to_le_bytes());
            }
            buffers.push(offsets);
            buffers.push(data);
        }
    }
    (buffers, nulls)
}

fn empty_array() -> ArrowArray {
    ArrowArray {
        length: 0,
        null_count: 0,
        offset: 0,
        n_buffers: 0,
        n_children: 0,
        buffers: std::ptr::null_mut(),
        children: std::ptr::null_mut(),
        dictionary: std::ptr::null_mut(),
        release: None,
        private_data: std::ptr::null_mut(),
    }
}

fn fill_array(out: &mut ArrowArray, length: usize, nulls: i64, buffers: Vec<Option<Vec<u8>>>, children: Vec<ArrowArray>) {
    let words: Vec<Vec<u64>> = buffers.iter().map(|buffer| buffer.as_deref().map(aligned).unwrap_or_default()).collect();
    let mut parts = Box::new(ArrayParts {
        buffers: Vec::new(),
        words,
        children: children.into_iter().map(|child| Box::into_raw(Box::new(child))).collect(),
    });
    parts.buffers = parts
        .words
        .iter()
        .zip(&buffers)
        .map(|(words, buffer)| match buffer {
            Some(_) => words.as_ptr().cast::<c_void>(),
            None => std::ptr::null(),
        })
        .collect();
    out.length = length as i64;
    out.null_count = nulls;
    out.offset = 0;
    out.n_buffers = parts.buffers.len() as i64;
    out.n_children = parts.children.len() as i64;
    out.buffers = parts.buffers.as_mut_ptr();
    out.children = parts.children.as_mut_ptr();
    out.dictionary = std::ptr::null_mut();
    out.release = Some(release_array);
    out.private_data = Box::into_raw(parts).cast();
}

/// Writes `columns` as one struct array (a record batch).
pub fn export_batch(columns: &[Column], out: &mut ArrowArray) {
    let length = columns.first().map_or(0, |column| column.cells.len());
    let children = columns
        .iter()
        .map(|column| {
            let (buffers, nulls) = column_buffers(column);
            let mut child = empty_array();
            fill_array(&mut child, column.cells.len(), nulls, buffers.into_iter().map(Some).collect(), Vec::new());
            child
        })
        .collect();
    fill_array(out, length, 0, vec![None], children);
}

/// A stream's state: the columns, given out in one batch, and the last error.
pub struct StreamState {
    columns: Vec<Column>,
    given: bool,
    error: CString,
}

unsafe extern "C" fn stream_schema(stream: *mut ArrowArrayStream, out: *mut ArrowSchema) -> c_int {
    if stream.is_null() || out.is_null() {
        return EINVAL;
    }
    // SAFETY: the consumer passes a live stream this module wrote.
    let stream = unsafe { &*stream };
    // SAFETY: a live stream's `private_data` is its `StreamState`.
    let state = unsafe { &*stream.private_data.cast::<StreamState>() };
    // SAFETY: the consumer passes writable memory for one schema.
    let out = unsafe { &mut *out };
    export_schema(&state.columns, out);
    0
}

unsafe extern "C" fn stream_next(stream: *mut ArrowArrayStream, out: *mut ArrowArray) -> c_int {
    if stream.is_null() || out.is_null() {
        return EINVAL;
    }
    // SAFETY: the consumer passes a live stream this module wrote.
    let stream = unsafe { &mut *stream };
    // SAFETY: a live stream's `private_data` is its `StreamState`.
    let state = unsafe { &mut *stream.private_data.cast::<StreamState>() };
    // SAFETY: the consumer passes writable memory for one array.
    let out = unsafe { &mut *out };
    if state.given {
        *out = empty_array();
        return 0;
    }
    state.given = true;
    export_batch(&state.columns, out);
    0
}

unsafe extern "C" fn stream_error(stream: *mut ArrowArrayStream) -> *const c_char {
    if stream.is_null() {
        return std::ptr::null();
    }
    // SAFETY: the consumer passes a live stream this module wrote.
    let stream = unsafe { &*stream };
    // SAFETY: a live stream's `private_data` is its `StreamState`.
    let state = unsafe { &*stream.private_data.cast::<StreamState>() };
    state.error.as_ptr()
}

unsafe extern "C" fn stream_release(stream: *mut ArrowArrayStream) {
    if stream.is_null() {
        return;
    }
    // SAFETY: the consumer releases a live stream this module wrote.
    let stream = unsafe { &mut *stream };
    if !stream.private_data.is_null() {
        // SAFETY: `private_data` is the `StreamState` box `export_stream` leaked, taken back once.
        drop(unsafe { Box::from_raw(stream.private_data.cast::<StreamState>()) });
    }
    stream.private_data = std::ptr::null_mut();
    stream.release = None;
}

/// Writes a stream giving `columns` as one batch.
pub fn export_stream(columns: Vec<Column>, out: &mut ArrowArrayStream) {
    let state = Box::new(StreamState {
        columns,
        given: false,
        error: CString::default(),
    });
    out.get_schema = Some(stream_schema);
    out.get_next = Some(stream_next);
    out.get_last_error = Some(stream_error);
    out.release = Some(stream_release);
    out.private_data = Box::into_raw(state).cast();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_carry_validity_and_offsets() {
        let column = Column {
            name: "name".to_owned(),
            column_type: ColumnType::Text,
            nullable: true,
            cells: vec![Cell::Bytes(b"ada".to_vec()), Cell::Null],
        };
        let (buffers, nulls) = column_buffers(&column);
        assert_eq!(nulls, 1);
        assert_eq!(buffers[0], [0b01]);
        assert_eq!(buffers[1], [0, 0, 0, 0, 3, 0, 0, 0, 3, 0, 0, 0]);
        let mut schema = empty_schema();
        export_schema(std::slice::from_ref(&column), &mut schema);
        assert_eq!(schema.n_children, 1);
        let release = schema.release.unwrap();
        // SAFETY: the schema was just written by `export_schema` and is released once.
        unsafe { release(&mut schema) };
        assert!(schema.release.is_none());
    }
}
