//! The changes of in-progress transactions that `pgoutput` streams before they commit
//! (protocol version 2, `streaming 'on'`): kept until the transaction's Stream Commit, dropped
//! at its Stream Abort, and cut back at the abort of one of its subtransactions.

use super::ReplicationError;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

const SPILL_SUFFIX: &str = ".spool";
const LENGTH_BYTES: usize = 4;

#[derive(Debug)]
struct Subtransaction {
    xid: u32,
    first_message: usize,
    file_offset: Option<u64>,
}

#[derive(Debug)]
struct SpillFile {
    path: PathBuf,
    writer: BufWriter<File>,
    length: u64,
    messages: usize,
}

#[derive(Debug, Default)]
struct Spool {
    messages: Vec<Vec<u8>>,
    memory: usize,
    file: Option<SpillFile>,
    subtransactions: Vec<Subtransaction>,
}

impl Spool {
    fn count(&self) -> usize {
        self.file.as_ref().map_or(0, |file| file.messages) + self.messages.len()
    }
}

fn spill_failed(path: &std::path::Path, error: std::io::Error) -> ReplicationError {
    ReplicationError(format!(
        "cannot keep a streamed transaction's changes in {}: {error}",
        path.display()
    ))
}

/// The in-progress transactions streamed so far, each one's changes in arrival order. Up to
/// `memory_limit` bytes across all of them stay in memory; past it the transaction that grew
/// goes to a file of its own in the spill directory, so a transaction of any size is kept
/// without holding it in memory.
#[derive(Debug)]
pub struct StreamedTransactions {
    spools: HashMap<u32, Spool>,
    memory_limit: usize,
    in_memory: usize,
    spill_directory: PathBuf,
    spill_prefix: String,
}

/// One committed streamed transaction's changes, in order, each a `pgoutput` message without
/// its transaction ID.
#[derive(Debug)]
pub struct SpooledChanges {
    memory: std::vec::IntoIter<Vec<u8>>,
    file: Option<(PathBuf, BufReader<File>, usize)>,
}

impl Iterator for SpooledChanges {
    type Item = Result<Vec<u8>, ReplicationError>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some((path, reader, remaining)) = &mut self.file {
            if *remaining > 0 {
                *remaining -= 1;
                let mut length = [0u8; LENGTH_BYTES];
                let read = reader.read_exact(&mut length).and_then(|()| {
                    let mut message = vec![0u8; u32::from_be_bytes(length) as usize];
                    reader.read_exact(&mut message).map(|()| message)
                });
                return Some(read.map_err(|error| spill_failed(path, error)));
            }
            let _ = std::fs::remove_file(&*path);
            self.file = None;
        }
        self.memory.next().map(Ok)
    }
}

impl Drop for SpooledChanges {
    fn drop(&mut self) {
        if let Some((path, _, _)) = &self.file {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl StreamedTransactions {
    /// Keeps up to `memory_limit` bytes in memory and spills the rest to files named
    /// `<spill_prefix><xid>.spool` in `spill_directory`. Files of that name left by an earlier
    /// run that stopped are removed.
    pub fn new(spill_directory: PathBuf, spill_prefix: String, memory_limit: usize) -> Self {
        if let Ok(entries) = std::fs::read_dir(&spill_directory) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(&spill_prefix) && name.ends_with(SPILL_SUFFIX) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        Self {
            spools: HashMap::new(),
            memory_limit,
            in_memory: 0,
            spill_directory,
            spill_prefix,
        }
    }

    /// Whether any streamed transaction is waiting for its commit or abort.
    pub fn is_empty(&self) -> bool {
        self.spools.is_empty()
    }

    /// The bytes held in memory across all streamed transactions.
    pub fn bytes_in_memory(&self) -> usize {
        self.in_memory
    }

    /// Whether the transaction's changes went to a file.
    pub fn is_spilled(&self, xid: u32) -> bool {
        self.spools
            .get(&xid)
            .is_some_and(|spool| spool.file.is_some())
    }

    /// A Stream Start: a first segment begins the transaction afresh.
    pub fn start(&mut self, xid: u32, first_segment: bool) {
        if first_segment {
            self.discard(xid);
        }
        self.spools.entry(xid).or_default();
    }

    /// Keeps one change of the streamed transaction `xid`, made by its subtransaction
    /// `subtransaction` (`xid` itself for the top level).
    pub fn push(
        &mut self,
        xid: u32,
        subtransaction: u32,
        message: Vec<u8>,
    ) -> Result<(), ReplicationError> {
        let spool = self.spools.entry(xid).or_default();
        if subtransaction != xid
            && !spool
                .subtransactions
                .iter()
                .any(|known| known.xid == subtransaction)
        {
            let first_message = spool.count();
            let file_offset = spool.file.as_ref().map(|file| file.length);
            spool.subtransactions.push(Subtransaction {
                xid: subtransaction,
                first_message,
                file_offset,
            });
        }
        if let Some(file) = &mut spool.file {
            return write_record(file, &message);
        }
        spool.memory += message.len();
        self.in_memory += message.len();
        spool.messages.push(message);
        if self.in_memory > self.memory_limit {
            self.spill(xid)?;
        }
        Ok(())
    }

    fn spill(&mut self, xid: u32) -> Result<(), ReplicationError> {
        let path = self
            .spill_directory
            .join(format!("{}{xid}{SPILL_SUFFIX}", self.spill_prefix));
        let Some(spool) = self.spools.get_mut(&xid) else {
            return Ok(());
        };
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| spill_failed(&path, error))?;
        let mut spill = SpillFile {
            path,
            writer: BufWriter::new(file),
            length: 0,
            messages: 0,
        };
        let messages = std::mem::take(&mut spool.messages);
        for (index, message) in messages.iter().enumerate() {
            for subtransaction in &mut spool.subtransactions {
                if subtransaction.file_offset.is_none() && subtransaction.first_message == index {
                    subtransaction.file_offset = Some(spill.length);
                }
            }
            write_record(&mut spill, message)?;
        }
        for subtransaction in &mut spool.subtransactions {
            subtransaction.file_offset.get_or_insert(spill.length);
        }
        self.in_memory -= spool.memory;
        spool.memory = 0;
        spool.file = Some(spill);
        Ok(())
    }

    fn discard(&mut self, xid: u32) {
        if let Some(spool) = self.spools.remove(&xid) {
            self.in_memory -= spool.memory;
            if let Some(file) = spool.file {
                drop(file.writer);
                let _ = std::fs::remove_file(&file.path);
            }
        }
    }

    /// A Stream Abort: the whole transaction when `subtransaction` is `xid`, otherwise that
    /// subtransaction's changes and those made after it began.
    pub fn abort(&mut self, xid: u32, subtransaction: u32) -> Result<(), ReplicationError> {
        if subtransaction == xid {
            self.discard(xid);
            return Ok(());
        }
        let Some(spool) = self.spools.get_mut(&xid) else {
            return Ok(());
        };
        let Some(position) = spool
            .subtransactions
            .iter()
            .position(|known| known.xid == subtransaction)
        else {
            return Ok(());
        };
        let Some(cut) = spool.subtransactions.split_off(position).into_iter().next() else {
            return Ok(());
        };
        match &mut spool.file {
            Some(file) => {
                let offset = cut.file_offset.unwrap_or(file.length);
                file.writer
                    .flush()
                    .and_then(|()| file.writer.get_mut().set_len(offset))
                    .and_then(|()| file.writer.get_mut().seek(SeekFrom::Start(offset)))
                    .map_err(|error| spill_failed(&file.path, error))?;
                file.length = offset;
                file.messages = cut.first_message;
            }
            None => {
                let freed: usize = spool
                    .messages
                    .iter()
                    .skip(cut.first_message)
                    .map(Vec::len)
                    .sum();
                spool.messages.truncate(cut.first_message);
                spool.memory -= freed;
                self.in_memory -= freed;
            }
        }
        Ok(())
    }

    /// A Stream Commit: the transaction's changes, in order, and it is forgotten. A
    /// transaction that streamed nothing has no changes.
    pub fn commit(&mut self, xid: u32) -> Result<SpooledChanges, ReplicationError> {
        let Some(mut spool) = self.spools.remove(&xid) else {
            return Ok(SpooledChanges {
                memory: Vec::new().into_iter(),
                file: None,
            });
        };
        self.in_memory -= spool.memory;
        let file = match spool.file.take() {
            Some(spill) => {
                let SpillFile {
                    path,
                    writer,
                    messages,
                    ..
                } = spill;
                let mut file = writer
                    .into_inner()
                    .map_err(|error| spill_failed(&path, error.into_error()))?;
                file.seek(SeekFrom::Start(0))
                    .map_err(|error| spill_failed(&path, error))?;
                Some((path, BufReader::new(file), messages))
            }
            None => None,
        };
        Ok(SpooledChanges {
            memory: std::mem::take(&mut spool.messages).into_iter(),
            file,
        })
    }

    /// Forgets every streamed transaction, as after a lost connection, when the server streams
    /// them again from their start.
    pub fn clear(&mut self) {
        let xids: Vec<u32> = self.spools.keys().copied().collect();
        for xid in xids {
            self.discard(xid);
        }
    }
}

impl Drop for StreamedTransactions {
    fn drop(&mut self) {
        self.clear();
    }
}

fn write_record(file: &mut SpillFile, message: &[u8]) -> Result<(), ReplicationError> {
    let length = u32::try_from(message.len())
        .map_err(|_| ReplicationError("a streamed change is larger than 4 GiB".to_owned()))?;
    file.writer
        .write_all(&length.to_be_bytes())
        .and_then(|()| file.writer.write_all(message))
        .map_err(|error| spill_failed(&file.path, error))?;
    file.length += (LENGTH_BYTES + message.len()) as u64;
    file.messages += 1;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("eggdb-spool-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn committed(transactions: &mut StreamedTransactions, xid: u32) -> Vec<Vec<u8>> {
        transactions
            .commit(xid)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn exercise(memory_limit: usize, name: &str) {
        let mut transactions =
            StreamedTransactions::new(directory(name), "follow-".to_owned(), memory_limit);
        transactions.start(700, true);
        transactions.push(700, 700, b"top 1".to_vec()).unwrap();
        transactions.push(700, 701, b"sub 701".to_vec()).unwrap();
        transactions.push(700, 700, b"top 2".to_vec()).unwrap();
        transactions.push(700, 702, b"sub 702".to_vec()).unwrap();
        transactions
            .push(700, 702, b"sub 702 again".to_vec())
            .unwrap();
        transactions.start(800, true);
        transactions.push(800, 800, b"other".to_vec()).unwrap();
        transactions.abort(700, 702).unwrap();
        transactions.push(700, 700, b"top 3".to_vec()).unwrap();
        transactions.abort(800, 800).unwrap();
        assert_eq!(
            committed(&mut transactions, 700),
            vec![
                b"top 1".to_vec(),
                b"sub 701".to_vec(),
                b"top 2".to_vec(),
                b"top 3".to_vec()
            ]
        );
        assert!(committed(&mut transactions, 800).is_empty());
        assert!(transactions.is_empty());
        assert_eq!(transactions.bytes_in_memory(), 0);
    }

    #[test]
    fn a_streamed_transaction_applies_what_its_aborted_subtransactions_did_not_do() {
        exercise(1 << 20, "memory");
    }

    #[test]
    fn a_spilled_streamed_transaction_reads_back_the_same_from_its_file() {
        exercise(8, "spilled");
    }

    #[test]
    fn the_memory_limit_sends_the_growing_transaction_to_a_file() {
        let spill_directory = directory("limit");
        let mut transactions =
            StreamedTransactions::new(spill_directory.clone(), "follow-".to_owned(), 16);
        transactions.start(5, true);
        transactions.push(5, 5, vec![1; 10]).unwrap();
        assert!(!transactions.is_spilled(5));
        transactions.push(5, 5, vec![2; 10]).unwrap();
        assert!(transactions.is_spilled(5));
        assert_eq!(transactions.bytes_in_memory(), 0);
        assert!(spill_directory.join("follow-5.spool").exists());
        assert_eq!(
            committed(&mut transactions, 5),
            vec![vec![1; 10], vec![2; 10]]
        );
        assert!(!spill_directory.join("follow-5.spool").exists());
    }

    #[test]
    fn a_first_segment_and_a_lost_connection_start_over() {
        let mut transactions =
            StreamedTransactions::new(directory("restart"), "follow-".to_owned(), 1 << 20);
        transactions.start(9, true);
        transactions.push(9, 9, b"before".to_vec()).unwrap();
        transactions.start(9, false);
        transactions.push(9, 9, b"kept".to_vec()).unwrap();
        transactions.start(9, true);
        transactions.push(9, 9, b"again".to_vec()).unwrap();
        assert_eq!(committed(&mut transactions, 9), vec![b"again".to_vec()]);
        transactions.start(10, true);
        transactions.push(10, 10, b"lost".to_vec()).unwrap();
        transactions.clear();
        assert!(transactions.is_empty());
        assert!(transactions.abort(10, 11).is_ok());
    }
}
