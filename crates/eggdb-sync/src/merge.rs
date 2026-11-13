//! Merges finer than a whole value (D1108), built from the replica's own last-writer-wins keys
//! so the server and the wire stay as they are: a record's fields are each a key of their own, so
//! two devices changing different fields both keep their change; a counter keeps one key per
//! replica holding that replica's running total, which only it writes, so concurrent increments
//! add up (a positive-negative counter); a set keeps one key per element, present or removed
//! with the newest change winning (a last-writer-wins element set).

use crate::{Replica, Stamp, SyncError};

const FIELD: u8 = 0x1f;
const COUNTER: u8 = 0x1e;
const ELEMENT: u8 = 0x1d;
const PRESENT: &[u8] = b"\x01";

fn sub_key(key: &[u8], marker: u8, part: &[u8]) -> Vec<u8> {
    let mut sub = key.to_vec();
    sub.push(marker);
    sub.extend_from_slice(part);
    sub
}

fn prefix_of(key: &[u8], marker: u8) -> Vec<u8> {
    sub_key(key, marker, &[])
}

impl Replica<'_> {
    /// Writes the given fields of record `key` (`None` removes a field), each as its own key, so
    /// a concurrent change to another field of the same record on another device is kept.
    pub fn write_fields(&self, key: &[u8], fields: &[(&str, Option<&[u8]>)], now_ms: u64) -> Result<Stamp, SyncError> {
        let mut last = Stamp::default();
        for (name, value) in fields {
            last = self.write(&sub_key(key, FIELD, name.as_bytes()), *value, now_ms)?;
        }
        Ok(last)
    }

    /// The fields of record `key`, by name.
    pub fn fields(&self, key: &[u8]) -> Result<Vec<(String, Vec<u8>)>, SyncError> {
        let prefix = prefix_of(key, FIELD);
        Ok(self
            .rows_with_prefix(&prefix)?
            .into_iter()
            .map(|(stored, value)| (String::from_utf8_lossy(&stored[prefix.len()..]).into_owned(), value))
            .collect())
    }

    /// Adds `delta` (negative to take away) to counter `key`: this replica's own running total
    /// moves, so increments made on several devices at once all count.
    pub fn add_to_counter(&self, key: &[u8], delta: i64, now_ms: u64) -> Result<i64, SyncError> {
        let own = sub_key(key, COUNTER, &self.id().to_be_bytes());
        let held = self
            .get(&own)?
            .and_then(|bytes| bytes.try_into().ok())
            .map(i64::from_be_bytes)
            .unwrap_or(0);
        let total = held.saturating_add(delta);
        self.write(&own, Some(&total.to_be_bytes()), now_ms)?;
        self.counter(key)
    }

    /// The value of counter `key`: the sum of every replica's running total.
    pub fn counter(&self, key: &[u8]) -> Result<i64, SyncError> {
        Ok(self
            .rows_with_prefix(&prefix_of(key, COUNTER))?
            .into_iter()
            .filter_map(|(_, value)| value.try_into().ok().map(i64::from_be_bytes))
            .fold(0i64, i64::saturating_add))
    }

    /// Adds `element` to set `key`.
    pub fn add_to_set(&self, key: &[u8], element: &[u8], now_ms: u64) -> Result<Stamp, SyncError> {
        self.write(&sub_key(key, ELEMENT, element), Some(PRESENT), now_ms)
    }

    /// Removes `element` from set `key`; an add made after the removal, anywhere, wins.
    pub fn remove_from_set(&self, key: &[u8], element: &[u8], now_ms: u64) -> Result<Stamp, SyncError> {
        self.write(&sub_key(key, ELEMENT, element), None, now_ms)
    }

    /// The elements of set `key`, in order.
    pub fn set_members(&self, key: &[u8]) -> Result<Vec<Vec<u8>>, SyncError> {
        let prefix = prefix_of(key, ELEMENT);
        Ok(self
            .rows_with_prefix(&prefix)?
            .into_iter()
            .map(|(stored, _)| stored[prefix.len()..].to_vec())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use crate::{Cursors, Replica, sync};
    use eggdb_keyspace::MemoryKeyspace;

    #[test]
    fn different_fields_counters_and_sets_merge_across_devices() {
        let (server_keys, phone_keys, laptop_keys) = (MemoryKeyspace::default(), MemoryKeyspace::default(), MemoryKeyspace::default());
        let server = Replica::new(&server_keys, 0, 1);
        let phone = Replica::new(&phone_keys, 0, 2);
        let laptop = Replica::new(&laptop_keys, 0, 3);
        let (mut phone_cursors, mut laptop_cursors) = (Cursors::default(), Cursors::default());
        phone.write_fields(b"contact:7", &[("name", Some(b"Ada")), ("phone", Some(b"111"))], 10).unwrap();
        sync(&phone, &server, &mut phone_cursors, 100).unwrap();
        sync(&laptop, &server, &mut laptop_cursors, 100).unwrap();
        phone.write_fields(b"contact:7", &[("phone", Some(b"222"))], 20).unwrap();
        laptop.write_fields(b"contact:7", &[("name", Some(b"Ada L."))], 21).unwrap();
        phone.add_to_counter(b"likes", 2, 22).unwrap();
        laptop.add_to_counter(b"likes", 3, 23).unwrap();
        phone.add_to_set(b"tags", b"friend", 24).unwrap();
        laptop.add_to_set(b"tags", b"work", 25).unwrap();
        laptop.remove_from_set(b"tags", b"work", 26).unwrap();
        for _ in 0..2 {
            sync(&phone, &server, &mut phone_cursors, 100).unwrap();
            sync(&laptop, &server, &mut laptop_cursors, 100).unwrap();
        }
        for device in [&phone, &laptop] {
            let fields = device.fields(b"contact:7").unwrap();
            assert_eq!(fields, vec![("name".to_owned(), b"Ada L.".to_vec()), ("phone".to_owned(), b"222".to_vec())]);
            assert_eq!(device.counter(b"likes").unwrap(), 5);
            assert_eq!(device.set_members(b"tags").unwrap(), vec![b"friend".to_vec()]);
        }
    }
}
