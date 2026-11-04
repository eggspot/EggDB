use super::{BoxedWrite, EntryVisitor, KeyValueRead, KeyValueWrite, ScanDirection};
use crate::error::SqlResult;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

struct KeyValueToolWrites {
    calls: Rc<RefCell<Vec<String>>>,
}

impl KeyValueRead for KeyValueToolWrites {
    fn get(&self, _key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        Ok(None)
    }

    fn scan(
        &self,
        _start: &[u8],
        _end: &[u8],
        _direction: ScanDirection,
        _visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        Ok(())
    }
}

impl KeyValueWrite for KeyValueToolWrites {
    fn put(&mut self, _key: &[u8], _value: &[u8]) -> SqlResult<()> {
        Ok(())
    }

    fn delete(&mut self, _key: &[u8]) -> SqlResult<bool> {
        Ok(false)
    }

    fn put_with_expiry(&mut self, key: &[u8], _value: &[u8], ttl: Duration) -> SqlResult<()> {
        self.calls.borrow_mut().push(format!(
            "put_with_expiry {} {}",
            String::from_utf8_lossy(key),
            ttl.as_secs()
        ));
        Ok(())
    }

    fn collection_put(
        &mut self,
        name: &str,
        key: &[u8],
        _value: &[u8],
        _ttl: Option<Duration>,
    ) -> SqlResult<bool> {
        self.calls.borrow_mut().push(format!(
            "collection_put {name} {}",
            String::from_utf8_lossy(key)
        ));
        Ok(true)
    }

    fn collection_delete(&mut self, name: &str, key: &[u8]) -> SqlResult<Option<bool>> {
        self.calls.borrow_mut().push(format!(
            "collection_delete {name} {}",
            String::from_utf8_lossy(key)
        ));
        Ok(Some(true))
    }

    fn commit(self) -> SqlResult<()> {
        Ok(())
    }

    fn rollback(self) -> SqlResult<()> {
        Ok(())
    }
}

#[test]
fn a_boxed_writer_passes_on_the_key_value_tools_writes() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    {
        let mut boxed: Box<dyn BoxedWrite> = Box::new(KeyValueToolWrites {
            calls: Rc::clone(&calls),
        });
        boxed
            .put_with_expiry(b"k", b"v", Duration::from_secs(9))
            .unwrap();
        assert!(boxed.collection_put("c", b"a", b"v", None).unwrap());
        assert_eq!(boxed.collection_delete("c", b"a").unwrap(), Some(true));
    }
    assert_eq!(
        *calls.borrow(),
        [
            "put_with_expiry k 9",
            "collection_put c a",
            "collection_delete c a"
        ]
    );
}
