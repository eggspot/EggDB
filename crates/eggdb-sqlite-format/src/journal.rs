//! Writing a database back into an existing SQLite file the way SQLite's rollback journal
//! makes it crash-safe (D1309), and rolling back the hot journal a crash leaves behind.

use crate::{SqliteFormatError, is_sqlite};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const JOURNAL_MAGIC: [u8; 8] = [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];
const SECTOR_BYTES: usize = 512;
const HEADER_FIELDS_BYTES: usize = 28;
const CHANGE_COUNTER_OFFSET: usize = 24;
const SCHEMA_COOKIE_OFFSET: usize = 40;
const VERSION_VALID_FOR_OFFSET: usize = 92;
const FILE_HEADER_BYTES: usize = 100;
const UNKNOWN_RECORD_COUNT: u32 = u32::MAX;

type Result<T> = std::result::Result<T, SqliteFormatError>;

fn io(error: &std::io::Error) -> SqliteFormatError {
    SqliteFormatError::Io(error.to_string())
}

fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let field = bytes.get(at..at + 4)?;
    Some(u32::from_be_bytes([field[0], field[1], field[2], field[3]]))
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    if let Some(field) = bytes.get_mut(at..at + 4) {
        field.copy_from_slice(&value.to_be_bytes());
    }
}

fn page_size_of(header: &[u8]) -> Option<usize> {
    let field = header.get(16..18)?;
    match u16::from_be_bytes([field[0], field[1]]) {
        1 => Some(65_536),
        size if size >= 512 && size.is_power_of_two() => Some(usize::from(size)),
        _ => None,
    }
}

fn page_checksum(nonce: u32, page: &[u8]) -> u32 {
    let mut sum = nonce;
    let mut at = page.len().saturating_sub(200);
    while at > 0 {
        sum = sum.wrapping_add(u32::from(page[at]));
        at = at.saturating_sub(200);
    }
    sum
}

fn nonce() -> u32 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    nanos ^ std::process::id().rotate_left(16)
}

fn sync_directory_of(path: &Path) {
    if let Some(directory) = path.parent().filter(|parent| !parent.as_os_str().is_empty())
        && let Ok(handle) = File::open(directory)
    {
        let _ = handle.sync_all();
    }
}

/// The file change counter in the header of the SQLite file at `path`, which SQLite raises
/// at every commit.
///
/// # Errors
/// The file cannot be read, or is not an SQLite file.
pub fn change_counter(path: &Path) -> Result<u32> {
    let mut header = [0u8; FILE_HEADER_BYTES];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut header))
        .map_err(|error| io(&error))?;
    if !is_sqlite(&header) {
        return Err(SqliteFormatError::NotSqlite);
    }
    u32_at(&header, CHANGE_COUNTER_OFFSET).ok_or(SqliteFormatError::NotSqlite)
}

fn journal_bytes(original: &[u8], page_size: usize) -> Vec<u8> {
    let pages = original.len() / page_size;
    let nonce = nonce();
    let mut journal = vec![0u8; SECTOR_BYTES];
    journal[..8].copy_from_slice(&JOURNAL_MAGIC);
    put_u32(&mut journal, 8, u32::try_from(pages).unwrap_or(u32::MAX));
    put_u32(&mut journal, 12, nonce);
    put_u32(&mut journal, 16, u32::try_from(pages).unwrap_or(u32::MAX));
    put_u32(&mut journal, 20, SECTOR_BYTES as u32);
    put_u32(&mut journal, 24, u32::try_from(page_size).unwrap_or(u32::MAX));
    journal.reserve(pages.saturating_mul(page_size + 8));
    for (index, page) in original.chunks_exact(page_size).enumerate() {
        journal.extend_from_slice(&u32::try_from(index + 1).unwrap_or(u32::MAX).to_be_bytes());
        journal.extend_from_slice(page);
        journal.extend_from_slice(&page_checksum(nonce, page).to_be_bytes());
    }
    journal
}

/// Replaces the contents of the SQLite file at `path` with `database`, a whole file as
/// [`crate::write_database`] makes it, crash-safely as SQLite does in rollback journal mode:
/// the old pages go to `<path>-journal` and are synced first, then the new pages are written
/// and synced, and deleting the journal commits. The new file's change counter and schema
/// cookie follow the old ones, so SQLite connections see the change.
///
/// # Errors
/// The file's change counter is no longer `expected_counter` (another program wrote to it),
/// a write-ahead log holds frames beside it, a journal is already there, or reading, writing
/// or syncing fails.
pub fn replace_database(path: &Path, database: &[u8], expected_counter: u32) -> Result<()> {
    let wal = beside(path, "-wal");
    if std::fs::metadata(&wal).is_ok_and(|metadata| metadata.len() > 0) {
        return Err(SqliteFormatError::Unsupported(
            "writing back to a file whose write-ahead log holds frames".to_owned(),
        ));
    }
    let journal_path = beside(path, "-journal");
    if journal_path.exists() {
        return Err(SqliteFormatError::Io(format!(
            "{} exists: another program is writing to the file, or a crash left it to roll back",
            journal_path.display()
        )));
    }
    let mut file = OpenOptions::new().read(true).write(true).open(path).map_err(|error| io(&error))?;
    let mut original = Vec::new();
    file.read_to_end(&mut original).map_err(|error| io(&error))?;
    if !is_sqlite(&original) || database.len() < FILE_HEADER_BYTES || !is_sqlite(database) {
        return Err(SqliteFormatError::NotSqlite);
    }
    let counter = u32_at(&original, CHANGE_COUNTER_OFFSET).ok_or(SqliteFormatError::NotSqlite)?;
    if counter != expected_counter {
        return Err(SqliteFormatError::Io(
            "another program changed the file since EggDB read it; the changes are not written back".to_owned(),
        ));
    }
    let page_size = page_size_of(&original)
        .ok_or_else(|| SqliteFormatError::Damaged("the page size is not a power of two from 512 to 65536".to_owned()))?;
    let cookie = u32_at(&original, SCHEMA_COOKIE_OFFSET).unwrap_or(0);
    let mut replacement = database.to_vec();
    let next = counter.wrapping_add(1);
    put_u32(&mut replacement, CHANGE_COUNTER_OFFSET, next);
    put_u32(&mut replacement, VERSION_VALID_FOR_OFFSET, next);
    put_u32(&mut replacement, SCHEMA_COOKIE_OFFSET, cookie.wrapping_add(1));
    let whole_pages = original.len() - original.len() % page_size;
    let journal = journal_bytes(&original[..whole_pages], page_size);
    let mut journal_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&journal_path)
        .map_err(|error| io(&error))?;
    journal_file
        .write_all(&journal)
        .and_then(|()| journal_file.sync_all())
        .map_err(|error| io(&error))?;
    drop(journal_file);
    sync_directory_of(&journal_path);
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.write_all(&replacement))
        .and_then(|()| file.set_len(replacement.len() as u64))
        .and_then(|()| file.sync_all())
        .map_err(|error| io(&error))?;
    drop(file);
    std::fs::remove_file(&journal_path).map_err(|error| io(&error))?;
    sync_directory_of(&journal_path);
    Ok(())
}

struct JournalHeader {
    records: u32,
    nonce: u32,
    original_pages: u32,
    sector_bytes: usize,
    page_size: usize,
}

fn journal_header(journal: &[u8]) -> Option<JournalHeader> {
    if journal.len() < HEADER_FIELDS_BYTES || journal[..8] != JOURNAL_MAGIC {
        return None;
    }
    let sector_bytes = usize::try_from(u32_at(journal, 20)?).ok()?;
    let page_size = usize::try_from(u32_at(journal, 24)?).ok()?;
    if !(32..=65_536).contains(&sector_bytes)
        || !sector_bytes.is_power_of_two()
        || !(512..=65_536).contains(&page_size)
        || !page_size.is_power_of_two()
    {
        return None;
    }
    Some(JournalHeader {
        records: u32_at(journal, 8)?,
        nonce: u32_at(journal, 12)?,
        original_pages: u32_at(journal, 16)?,
        sector_bytes,
        page_size,
    })
}

/// Rolls back the hot journal a crash left beside the SQLite file at `path`, as the next
/// SQLite connection would: every page record whose checksum holds is written back, the file
/// is cut to its size before the interrupted write, and the journal is deleted. Answers
/// whether there was a journal to roll back; an empty or headerless journal is only deleted.
///
/// # Errors
/// Reading, writing or syncing fails.
pub fn roll_back_hot_journal(path: &Path) -> Result<bool> {
    let journal_path = beside(path, "-journal");
    let journal = match std::fs::read(&journal_path) {
        Ok(journal) => journal,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(io(&error)),
    };
    let Some(header) = journal_header(&journal) else {
        std::fs::remove_file(&journal_path).map_err(|error| io(&error))?;
        return Ok(false);
    };
    if header.records == 0 {
        std::fs::remove_file(&journal_path).map_err(|error| io(&error))?;
        return Ok(false);
    }
    let mut file = OpenOptions::new().read(true).write(true).open(path).map_err(|error| io(&error))?;
    let record_bytes = header.page_size + 8;
    let mut at = header.sector_bytes;
    let mut applied = 0u32;
    while (header.records == UNKNOWN_RECORD_COUNT || applied < header.records)
        && let Some(record) = journal.get(at..at + record_bytes)
    {
        let page_number = u32_at(record, 0).unwrap_or(0);
        let page = &record[4..4 + header.page_size];
        let checksum = u32_at(record, 4 + header.page_size).unwrap_or(0);
        if page_number == 0 || checksum != page_checksum(header.nonce, page) {
            break;
        }
        if page_number <= header.original_pages {
            let offset = u64::from(page_number - 1) * header.page_size as u64;
            file.seek(SeekFrom::Start(offset))
                .and_then(|_| file.write_all(page))
                .map_err(|error| io(&error))?;
        }
        applied = applied.saturating_add(1);
        at += record_bytes;
    }
    file.set_len(u64::from(header.original_pages) * header.page_size as u64)
        .and_then(|()| file.sync_all())
        .map_err(|error| io(&error))?;
    drop(file);
    std::fs::remove_file(&journal_path).map_err(|error| io(&error))?;
    sync_directory_of(&journal_path);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SqliteFile, SqliteObject, SqliteRow, SqliteValue, write_database};

    fn database(rows: &[&str]) -> Vec<u8> {
        write_database(&[SqliteObject::Table {
            name: "t".to_owned(),
            sql: "CREATE TABLE t (v TEXT)".to_owned(),
            rows: rows
                .iter()
                .zip(1i64..)
                .map(|(text, rowid)| SqliteRow {
                    rowid,
                    values: vec![SqliteValue::Text((*text).to_owned())],
                })
                .collect(),
        }])
        .unwrap()
    }

    fn texts(path: &Path) -> Vec<String> {
        let file = SqliteFile::open(path).unwrap();
        let schema = file.schema().unwrap();
        let root = schema.iter().find(|entry| entry.name == "t").unwrap().root_page;
        file.rows(root)
            .unwrap()
            .into_iter()
            .map(|row| match &row.values[0] {
                SqliteValue::Text(text) => text.clone(),
                _ => String::new(),
            })
            .collect()
    }

    fn scratch(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("eggdb-sqlite-journal-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        directory.join("data.sqlite")
    }

    #[test]
    fn a_replaced_file_holds_the_new_rows_raises_its_counter_and_leaves_no_journal() {
        let path = scratch("replace");
        std::fs::write(&path, database(&["old"])).unwrap();
        let counter = change_counter(&path).unwrap();
        replace_database(&path, &database(&["new", "rows"]), counter).unwrap();
        assert_eq!(texts(&path), ["new", "rows"]);
        assert_eq!(change_counter(&path).unwrap(), counter.wrapping_add(1));
        assert!(!beside(&path, "-journal").exists());
        assert!(replace_database(&path, &database(&["late"]), counter).is_err());
        assert_eq!(texts(&path), ["new", "rows"]);
    }

    #[test]
    fn a_hot_journal_puts_the_old_pages_back() {
        let path = scratch("rollback");
        let old = database(&["kept"]);
        std::fs::write(&path, &old).unwrap();
        let page_size = page_size_of(&old).unwrap();
        std::fs::write(beside(&path, "-journal"), journal_bytes(&old, page_size)).unwrap();
        let mut torn = database(&["torn", "write", "half", "done"]);
        torn.truncate(torn.len() / 2 + 1);
        std::fs::write(&path, &torn).unwrap();
        assert!(roll_back_hot_journal(&path).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), old);
        assert!(!beside(&path, "-journal").exists());
        assert!(!roll_back_hot_journal(&path).unwrap());
    }

    #[test]
    fn checksums_add_every_two_hundredth_byte_from_the_end() {
        let mut page = vec![0u8; 512];
        page[312] = 3;
        page[112] = 4;
        page[0] = 100;
        assert_eq!(page_checksum(10, &page), 17);
    }
}
