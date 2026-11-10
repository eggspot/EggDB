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

    fn increment(
        &mut self,
        collection: Option<&str>,
        key: &[u8],
        by: i64,
    ) -> SqlResult<Option<i64>> {
        self.calls.borrow_mut().push(format!(
            "increment {} {} {by}",
            collection.unwrap_or("-"),
            String::from_utf8_lossy(key)
        ));
        Ok(Some(by))
    }

    fn compare_and_set(
        &mut self,
        collection: Option<&str>,
        key: &[u8],
        expected: Option<&[u8]>,
        new: Option<&[u8]>,
    ) -> SqlResult<Option<(bool, Option<Vec<u8>>)>> {
        self.calls.borrow_mut().push(format!(
            "compare_and_set {} {} {} {}",
            collection.unwrap_or("-"),
            String::from_utf8_lossy(key),
            expected.map_or_else(|| "absent".into(), String::from_utf8_lossy),
            new.map_or_else(|| "delete".into(), String::from_utf8_lossy)
        ));
        Ok(Some((true, expected.map(<[u8]>::to_vec))))
    }

    fn set_expiry(
        &mut self,
        collection: Option<&str>,
        key: &[u8],
        ttl: Duration,
    ) -> SqlResult<Option<bool>> {
        self.calls.borrow_mut().push(format!(
            "set_expiry {} {} {}",
            collection.unwrap_or("-"),
            String::from_utf8_lossy(key),
            ttl.as_secs()
        ));
        Ok(Some(true))
    }

    fn clear_expiry(&mut self, collection: Option<&str>, key: &[u8]) -> SqlResult<Option<bool>> {
        self.calls.borrow_mut().push(format!(
            "clear_expiry {} {}",
            collection.unwrap_or("-"),
            String::from_utf8_lossy(key)
        ));
        Ok(None)
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

#[test]
fn a_boxed_writer_passes_on_counters_compare_and_set_and_expiry() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    {
        let mut boxed: Box<dyn BoxedWrite> = Box::new(KeyValueToolWrites {
            calls: Rc::clone(&calls),
        });
        assert_eq!(boxed.increment(None, b"hits", -3).unwrap(), Some(-3));
        assert_eq!(
            boxed
                .compare_and_set(Some("c"), b"lock", Some(b"me".as_slice()), None)
                .unwrap(),
            Some((true, Some(b"me".to_vec())))
        );
        assert_eq!(
            boxed
                .set_expiry(None, b"k", Duration::from_secs(30))
                .unwrap(),
            Some(true)
        );
        assert_eq!(boxed.clear_expiry(Some("c"), b"k").unwrap(), None);
    }
    assert_eq!(
        *calls.borrow(),
        [
            "increment - hits -3",
            "compare_and_set c lock me delete",
            "set_expiry - k 30",
            "clear_expiry c k"
        ]
    );
}

struct OnlyPuts;

impl KeyValueRead for OnlyPuts {
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

impl KeyValueWrite for OnlyPuts {
    fn put(&mut self, _key: &[u8], _value: &[u8]) -> SqlResult<()> {
        Ok(())
    }

    fn delete(&mut self, _key: &[u8]) -> SqlResult<bool> {
        Ok(false)
    }

    fn commit(self) -> SqlResult<()> {
        Ok(())
    }

    fn rollback(self) -> SqlResult<()> {
        Ok(())
    }
}

#[test]
fn a_store_without_the_key_value_tools_refuses_them_with_0a000() {
    let mut writer = OnlyPuts;
    let refusals = [
        writer.increment(None, b"k", 1).map(|_| ()),
        writer
            .compare_and_set(None, b"k", None, Some(b"v".as_slice()))
            .map(|_| ()),
        writer
            .set_expiry(None, b"k", Duration::from_secs(0))
            .map(|_| ()),
        writer.clear_expiry(Some("c"), b"k").map(|_| ()),
    ];
    for refusal in refusals {
        let Err(error) = refusal else {
            panic!("a store without the tools ran one");
        };
        assert_eq!(error.state().code(), "0A000");
    }
}
