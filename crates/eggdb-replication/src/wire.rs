use crate::{Batch, Change};

const MAX_CHANGES: u64 = 1 << 20;
const MAX_BYTES: u64 = 64 << 20;

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// A batch as bytes for the network: term, sequence, then each change.
pub fn encode_batch(batch: &Batch) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&batch.term.to_le_bytes());
    out.extend_from_slice(&batch.sequence.to_le_bytes());
    out.extend_from_slice(&(batch.changes.len() as u64).to_le_bytes());
    for change in &batch.changes {
        match change {
            Change::Put { collection, key, value } => {
                out.push(1);
                put_bytes(&mut out, collection.as_bytes());
                put_bytes(&mut out, key);
                put_bytes(&mut out, value);
            }
            Change::Delete { collection, key } => {
                out.push(2);
                put_bytes(&mut out, collection.as_bytes());
                put_bytes(&mut out, key);
            }
        }
    }
    out
}

struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Reader<'b> {
    fn u64(&mut self) -> Result<u64, String> {
        let raw = self.bytes.get(self.at..self.at + 8).ok_or("a batch ends early")?;
        self.at += 8;
        Ok(u64::from_le_bytes(raw.try_into().map_err(|_| "a batch ends early")?))
    }

    fn bytes(&mut self) -> Result<&'b [u8], String> {
        let length = self.u64()?;
        if length > MAX_BYTES || length > (self.bytes.len() - self.at) as u64 {
            return Err("a batch's field is longer than its bytes".to_owned());
        }
        let start = self.at;
        self.at += length as usize;
        Ok(&self.bytes[start..self.at])
    }
}

/// Reads what [`encode_batch`] wrote, checking every length.
pub fn decode_batch(bytes: &[u8]) -> Result<Batch, String> {
    let mut reader = Reader { bytes, at: 0 };
    let term = reader.u64()?;
    let sequence = reader.u64()?;
    let count = reader.u64()?;
    if count > MAX_CHANGES || count > bytes.len() as u64 {
        return Err("a batch claims more changes than it holds".to_owned());
    }
    let mut changes = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let kind = *bytes.get(reader.at).ok_or("a batch ends early")?;
        reader.at += 1;
        let collection = String::from_utf8(reader.bytes()?.to_vec()).map_err(|_| "a collection name is not UTF-8")?;
        let key = reader.bytes()?.to_vec();
        changes.push(match kind {
            1 => Change::Put { collection, key, value: reader.bytes()?.to_vec() },
            2 => Change::Delete { collection, key },
            _ => return Err("a change has an unknown kind".to_owned()),
        });
    }
    if reader.at != bytes.len() {
        return Err("bytes follow the batch".to_owned());
    }
    Ok(Batch { term, sequence, changes })
}
