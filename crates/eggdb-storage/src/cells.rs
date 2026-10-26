use crate::error::StorageError;
use eggdb_platform::{Error, ErrorKind};
use std::cell::Cell;

pub(crate) fn filled<T: Copy>(len: usize, value: T) -> Result<Box<[Cell<T>]>, StorageError> {
    let mut cells = reserved(len)?;
    cells.extend((0..len).map(|_| Cell::new(value)));
    Ok(cells.into_boxed_slice())
}

pub(crate) fn reserved<T>(capacity: usize) -> Result<Vec<T>, StorageError> {
    let mut items = Vec::new();
    items
        .try_reserve_exact(capacity)
        .map_err(|_| StorageError::Io(Error::new(ErrorKind::OutOfMemory)))?;
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::filled;
    use crate::error::StorageError;
    use eggdb_platform::ErrorKind;

    #[test]
    fn memory_that_can_not_be_had_is_an_error_not_an_abort() {
        assert!(matches!(
            filled(usize::MAX / 4, 0u64),
            Err(StorageError::Io(error)) if error.kind() == ErrorKind::OutOfMemory
        ));
        assert_eq!(filled(3, 7u8).unwrap().len(), 3);
    }
}
