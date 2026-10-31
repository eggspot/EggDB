use eggdb_keyspace::{KeysRead, KeysWrite, Keyspace};

const BUCKET: u8 = b'b';
const OBJECT: u8 = b'o';
const DATA: u8 = b'd';
const UPLOAD: u8 = b'u';
const PART: u8 = b'p';
const PART_DATA: u8 = b'q';
const CHUNK: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StoreError {
    NoSuchBucket,
    NoSuchKey,
    NoSuchUpload,
    BucketNotEmpty,
    InvalidPart(u32),
    InvalidPartOrder,
    Storage(String),
}

type Result<T> = std::result::Result<T, StoreError>;

fn storage(message: String) -> StoreError {
    StoreError::Storage(message)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObjectInfo {
    pub(crate) size: u64,
    pub(crate) md5: [u8; 16],
    pub(crate) parts: u32,
    pub(crate) modified_ms: u64,
    pub(crate) content_type: String,
    pub(crate) metadata: Vec<(String, String)>,
}

fn put_text(out: &mut Vec<u8>, text: &str) {
    let length = u16::try_from(text.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(&text.as_bytes()[..usize::from(length)]);
}

struct Cursor<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Cursor<'b> {
    fn take(&mut self, count: usize) -> Result<&'b [u8]> {
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len()).ok_or_else(|| storage("a stored object description is damaged".to_owned()))?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().map_err(|_| storage("damaged".to_owned()))?))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().map_err(|_| storage("damaged".to_owned()))?))
    }

    fn text(&mut self) -> Result<String> {
        let length = u16::from_be_bytes(self.take(2)?.try_into().map_err(|_| storage("damaged".to_owned()))?);
        String::from_utf8(self.take(usize::from(length))?.to_vec()).map_err(|_| storage("a stored name is not UTF-8".to_owned()))
    }
}

impl ObjectInfo {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.size.to_be_bytes());
        out.extend_from_slice(&self.md5);
        out.extend_from_slice(&self.parts.to_be_bytes());
        out.extend_from_slice(&self.modified_ms.to_be_bytes());
        put_text(&mut out, &self.content_type);
        out.extend_from_slice(&u16::try_from(self.metadata.len()).unwrap_or(u16::MAX).to_be_bytes());
        for (name, value) in self.metadata.iter().take(usize::from(u16::MAX)) {
            put_text(&mut out, name);
            put_text(&mut out, value);
        }
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut cursor = Cursor { bytes, at: 0 };
        let size = cursor.u64()?;
        let md5 = cursor.take(16)?.try_into().map_err(|_| storage("damaged".to_owned()))?;
        let parts = cursor.u32()?;
        let modified_ms = cursor.u64()?;
        let content_type = cursor.text()?;
        let count = u16::from_be_bytes(cursor.take(2)?.try_into().map_err(|_| storage("damaged".to_owned()))?);
        let mut metadata = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            metadata.push((cursor.text()?, cursor.text()?));
        }
        Ok(Self { size, md5, parts, modified_ms, content_type, metadata })
    }

    fn chunks(&self) -> u64 {
        self.size.div_ceil(CHUNK as u64)
    }
}

fn joined(tag: u8, parts: &[&[u8]]) -> Vec<u8> {
    let mut key = vec![tag];
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            key.push(0);
        }
        key.extend_from_slice(part);
    }
    key
}

fn bucket_key(bucket: &str) -> Vec<u8> {
    joined(BUCKET, &[bucket.as_bytes()])
}

fn object_key(bucket: &str, key: &str) -> Vec<u8> {
    joined(OBJECT, &[bucket.as_bytes(), key.as_bytes()])
}

fn objects_prefix(bucket: &str) -> Vec<u8> {
    joined(OBJECT, &[bucket.as_bytes(), b""])
}

fn data_prefix(bucket: &str, key: &str) -> Vec<u8> {
    joined(DATA, &[bucket.as_bytes(), key.as_bytes(), b""])
}

fn chunk_key(prefix: &[u8], index: u64) -> Vec<u8> {
    let mut key = prefix.to_vec();
    key.extend_from_slice(&index.to_be_bytes());
    key
}

fn upload_key(upload: &str) -> Vec<u8> {
    joined(UPLOAD, &[upload.as_bytes()])
}

fn part_key(upload: &str, number: u32) -> Vec<u8> {
    let mut key = joined(PART, &[upload.as_bytes(), b""]);
    key.extend_from_slice(&number.to_be_bytes());
    key
}

fn part_data_prefix(upload: &str, number: u32) -> Vec<u8> {
    let mut key = joined(PART_DATA, &[upload.as_bytes(), b""]);
    key.extend_from_slice(&number.to_be_bytes());
    key
}

fn delete_prefix(keys: &mut dyn KeysWrite, prefix: &[u8]) -> Result<()> {
    for (key, _) in keys.scan(prefix).map_err(storage)? {
        keys.delete(&key).map_err(storage)?;
    }
    Ok(())
}

fn write_chunks(keys: &mut dyn KeysWrite, prefix: &[u8], body: &[u8]) -> Result<()> {
    for (index, chunk) in body.chunks(CHUNK).enumerate() {
        keys.put(&chunk_key(prefix, index as u64), chunk).map_err(storage)?;
    }
    Ok(())
}

fn read_chunks(keys: &dyn KeysRead, prefix: &[u8], size: u64, range: (u64, u64)) -> Result<Vec<u8>> {
    let (start, end) = range;
    let mut out = Vec::with_capacity(usize::try_from(end.saturating_sub(start)).unwrap_or(0));
    if start >= end || start >= size {
        return Ok(out);
    }
    let chunk = CHUNK as u64;
    for index in start / chunk..=(end - 1) / chunk {
        let bytes = keys.get(&chunk_key(prefix, index)).map_err(storage)?.ok_or_else(|| storage("an object's data is missing".to_owned()))?;
        let base = index * chunk;
        let from = usize::try_from(start.saturating_sub(base)).unwrap_or(0).min(bytes.len());
        let to = usize::try_from(end - base).unwrap_or(usize::MAX).min(bytes.len());
        out.extend_from_slice(&bytes[from..to]);
    }
    Ok(out)
}

/// The buckets and objects of one keyspace (its database 0).
pub(crate) struct Store<'k> {
    keyspace: &'k (dyn Keyspace + Sync),
}

impl<'k> Store<'k> {
    pub(crate) fn new(keyspace: &'k (dyn Keyspace + Sync)) -> Self {
        Self { keyspace }
    }

    fn read<T>(&self, body: impl FnOnce(&dyn KeysRead) -> Result<T>) -> Result<T> {
        let mut body = Some(body);
        let mut out = None;
        self.keyspace
            .read(0, &mut |keys| {
                if let Some(body) = body.take() {
                    out = Some(body(keys));
                }
                Ok(())
            })
            .map_err(storage)?;
        out.unwrap_or_else(|| Err(storage("the read did not run".to_owned())))
    }

    fn write<T>(&self, body: impl FnOnce(&mut dyn KeysWrite) -> Result<T>) -> Result<T> {
        let mut body = Some(body);
        let mut out = None;
        let result = self.keyspace.write(0, &mut |keys| {
            let Some(body) = body.take() else {
                return Err("the write ran twice".to_owned());
            };
            let outcome = body(keys);
            let failed = outcome.is_err();
            out = Some(outcome);
            if failed { Err("the object operation was rolled back".to_owned()) } else { Ok(()) }
        });
        match (result, out) {
            (_, Some(Err(error))) => Err(error),
            (Err(message), _) => Err(storage(message)),
            (Ok(()), Some(outcome)) => outcome,
            (Ok(()), None) => Err(storage("the write did not run".to_owned())),
        }
    }

    pub(crate) fn create_bucket(&self, bucket: &str, now_ms: u64) -> Result<bool> {
        self.write(|keys| {
            if keys.get(&bucket_key(bucket)).map_err(storage)?.is_some() {
                return Ok(false);
            }
            keys.put(&bucket_key(bucket), &now_ms.to_be_bytes()).map_err(storage)?;
            Ok(true)
        })
    }

    pub(crate) fn bucket_exists(&self, bucket: &str) -> Result<bool> {
        self.read(|keys| Ok(keys.get(&bucket_key(bucket)).map_err(storage)?.is_some()))
    }

    pub(crate) fn buckets(&self) -> Result<Vec<(String, u64)>> {
        self.read(|keys| {
            Ok(keys
                .scan(&[BUCKET])
                .map_err(storage)?
                .into_iter()
                .filter_map(|(key, value)| Some((String::from_utf8(key[1..].to_vec()).ok()?, u64::from_be_bytes(value.get(..8)?.try_into().ok()?))))
                .collect())
        })
    }

    pub(crate) fn delete_bucket(&self, bucket: &str) -> Result<()> {
        self.write(|keys| {
            if keys.get(&bucket_key(bucket)).map_err(storage)?.is_none() {
                return Err(StoreError::NoSuchBucket);
            }
            if !keys.scan(&objects_prefix(bucket)).map_err(storage)?.is_empty() {
                return Err(StoreError::BucketNotEmpty);
            }
            keys.delete(&bucket_key(bucket)).map_err(storage)?;
            Ok(())
        })
    }

    pub(crate) fn put_object(&self, bucket: &str, key: &str, body: &[u8], content_type: &str, metadata: Vec<(String, String)>, now_ms: u64) -> Result<ObjectInfo> {
        let info = ObjectInfo { size: body.len() as u64, md5: eggdb_crypto::md5_checksum(body), parts: 0, modified_ms: now_ms, content_type: content_type.to_owned(), metadata };
        self.write(|keys| {
            if keys.get(&bucket_key(bucket)).map_err(storage)?.is_none() {
                return Err(StoreError::NoSuchBucket);
            }
            let prefix = data_prefix(bucket, key);
            delete_prefix(keys, &prefix)?;
            write_chunks(keys, &prefix, body)?;
            keys.put(&object_key(bucket, key), &info.encode()).map_err(storage)?;
            Ok(())
        })?;
        Ok(info)
    }

    pub(crate) fn object(&self, bucket: &str, key: &str) -> Result<ObjectInfo> {
        self.read(|keys| {
            if keys.get(&bucket_key(bucket)).map_err(storage)?.is_none() {
                return Err(StoreError::NoSuchBucket);
            }
            let stored = keys.get(&object_key(bucket, key)).map_err(storage)?.ok_or(StoreError::NoSuchKey)?;
            ObjectInfo::decode(&stored)
        })
    }

    /// The object's description and its bytes from `range.0` up to (not including) `range.1`.
    pub(crate) fn read_object(&self, bucket: &str, key: &str, range: Option<(u64, u64)>) -> Result<(ObjectInfo, Vec<u8>)> {
        self.read(|keys| {
            if keys.get(&bucket_key(bucket)).map_err(storage)?.is_none() {
                return Err(StoreError::NoSuchBucket);
            }
            let stored = keys.get(&object_key(bucket, key)).map_err(storage)?.ok_or(StoreError::NoSuchKey)?;
            let info = ObjectInfo::decode(&stored)?;
            let range = range.unwrap_or((0, info.size));
            let body = read_chunks(keys, &data_prefix(bucket, key), info.size, (range.0, range.1.min(info.size)))?;
            Ok((info, body))
        })
    }

    pub(crate) fn delete_object(&self, bucket: &str, key: &str) -> Result<()> {
        self.write(|keys| {
            if keys.get(&bucket_key(bucket)).map_err(storage)?.is_none() {
                return Err(StoreError::NoSuchBucket);
            }
            delete_prefix(keys, &data_prefix(bucket, key))?;
            keys.delete(&object_key(bucket, key)).map_err(storage)?;
            Ok(())
        })
    }

    /// The objects of `bucket` whose keys start with `prefix` and sort after `after`, in key order.
    pub(crate) fn list(&self, bucket: &str, prefix: &str, after: &str) -> Result<Vec<(String, ObjectInfo)>> {
        self.read(|keys| {
            if keys.get(&bucket_key(bucket)).map_err(storage)?.is_none() {
                return Err(StoreError::NoSuchBucket);
            }
            let start = objects_prefix(bucket);
            let mut scanned = start.clone();
            scanned.extend_from_slice(prefix.as_bytes());
            let mut out = Vec::new();
            for (stored_key, value) in keys.scan(&scanned).map_err(storage)? {
                let Ok(name) = String::from_utf8(stored_key[start.len()..].to_vec()) else {
                    continue;
                };
                if name.as_str() > after {
                    out.push((name, ObjectInfo::decode(&value)?));
                }
            }
            Ok(out)
        })
    }

    pub(crate) fn create_upload(&self, upload: &str, bucket: &str, key: &str, content_type: &str, metadata: Vec<(String, String)>, now_ms: u64) -> Result<()> {
        let description = ObjectInfo { size: 0, md5: [0; 16], parts: 0, modified_ms: now_ms, content_type: content_type.to_owned(), metadata };
        let mut value = Vec::new();
        put_text(&mut value, bucket);
        let key_length = u32::try_from(key.len()).unwrap_or(u32::MAX);
        value.extend_from_slice(&key_length.to_be_bytes());
        value.extend_from_slice(key.as_bytes());
        value.extend(description.encode());
        self.write(|keys| {
            if keys.get(&bucket_key(bucket)).map_err(storage)?.is_none() {
                return Err(StoreError::NoSuchBucket);
            }
            keys.put(&upload_key(upload), &value).map_err(storage)?;
            Ok(())
        })
    }

    fn upload(keys: &dyn KeysRead, upload: &str) -> Result<(String, String, ObjectInfo)> {
        let value = keys.get(&upload_key(upload)).map_err(storage)?.ok_or(StoreError::NoSuchUpload)?;
        let mut cursor = Cursor { bytes: &value, at: 0 };
        let bucket = cursor.text()?;
        let length = usize::try_from(cursor.u32()?).unwrap_or(usize::MAX);
        let key = String::from_utf8(cursor.take(length)?.to_vec()).map_err(|_| storage("damaged".to_owned()))?;
        let description = ObjectInfo::decode(&value[cursor.at..])?;
        Ok((bucket, key, description))
    }

    pub(crate) fn put_part(&self, upload: &str, bucket: &str, key: &str, number: u32, body: &[u8]) -> Result<[u8; 16]> {
        let md5 = eggdb_crypto::md5_checksum(body);
        self.write(|keys| {
            let (owner_bucket, owner_key, _) = Self::upload(&*keys, upload)?;
            if owner_bucket != bucket || owner_key != key {
                return Err(StoreError::NoSuchUpload);
            }
            let prefix = part_data_prefix(upload, number);
            delete_prefix(keys, &prefix)?;
            write_chunks(keys, &prefix, body)?;
            let mut description = (body.len() as u64).to_be_bytes().to_vec();
            description.extend_from_slice(&md5);
            keys.put(&part_key(upload, number), &description).map_err(storage)?;
            Ok(())
        })?;
        Ok(md5)
    }

    pub(crate) fn complete(&self, upload: &str, bucket: &str, key: &str, parts: &[(u32, [u8; 16])], now_ms: u64) -> Result<ObjectInfo> {
        if parts.is_empty() || parts.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
            return Err(StoreError::InvalidPartOrder);
        }
        self.write(|keys| {
            let (owner_bucket, owner_key, mut info) = Self::upload(&*keys, upload)?;
            if owner_bucket != bucket || owner_key != key {
                return Err(StoreError::NoSuchUpload);
            }
            let mut body = Vec::new();
            let mut digests = Vec::new();
            for (number, md5) in parts {
                let stored = keys.get(&part_key(upload, *number)).map_err(storage)?.ok_or(StoreError::InvalidPart(*number))?;
                let size = u64::from_be_bytes(stored.get(..8).and_then(|raw| raw.try_into().ok()).ok_or(StoreError::InvalidPart(*number))?);
                if stored.get(8..24) != Some(md5.as_slice()) {
                    return Err(StoreError::InvalidPart(*number));
                }
                body.extend(read_chunks(&*keys, &part_data_prefix(upload, *number), size, (0, size))?);
                digests.extend_from_slice(md5);
            }
            for (stored_key, _) in keys.scan(&joined(PART, &[upload.as_bytes(), b""])).map_err(storage)? {
                keys.delete(&stored_key).map_err(storage)?;
            }
            delete_prefix(keys, &joined(PART_DATA, &[upload.as_bytes(), b""]))?;
            keys.delete(&upload_key(upload)).map_err(storage)?;
            let prefix = data_prefix(bucket, key);
            delete_prefix(keys, &prefix)?;
            write_chunks(keys, &prefix, &body)?;
            info.size = body.len() as u64;
            info.md5 = eggdb_crypto::md5_checksum(&digests);
            info.parts = u32::try_from(parts.len()).unwrap_or(u32::MAX);
            info.modified_ms = now_ms;
            keys.put(&object_key(bucket, key), &info.encode()).map_err(storage)?;
            Ok(info)
        })
    }

    pub(crate) fn abort(&self, upload: &str) -> Result<()> {
        self.write(|keys| {
            if keys.get(&upload_key(upload)).map_err(storage)?.is_none() {
                return Err(StoreError::NoSuchUpload);
            }
            delete_prefix(keys, &joined(PART, &[upload.as_bytes(), b""]))?;
            delete_prefix(keys, &joined(PART_DATA, &[upload.as_bytes(), b""]))?;
            keys.delete(&upload_key(upload)).map_err(storage)?;
            Ok(())
        })
    }
}
