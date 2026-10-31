use crate::btree::BTree;
use crate::error::StorageError;
use crate::format::{Access, Features, FileId, NO_PAGE, PageSize};
use crate::page_cache::{CacheSize, PageCache};
use crate::page_file::{CreateOptions, PageFile};
use crate::snapshot_copy::CopyDestination;
use crate::store::Store;
use eggdb_platform::{File, FileSystem, OpenMode, SimClock, SimFile, SimFileSystem, SimMemory};
use std::path::Path;
use std::sync::Arc;

const KEY: [u8; 32] = [0x5a; 32];
const SECRET: &[u8] = b"the launch code is 0000";
const FRAMES: usize = 256;

fn store_of(file: PageFile<SimFile>) -> Store<SimFile> {
    let cache = PageCache::new(file, &SimMemory::new(), CacheSize::Pages(FRAMES)).unwrap();
    Store::new(cache, Arc::new(SimClock::new(1)))
}

fn encrypted_database(fs: &SimFileSystem) {
    let options = CreateOptions {
        page_size: PageSize::DEFAULT,
        file_id: FileId(*b"encrypted-file!!"),
    };
    let mut file = PageFile::create(fs, Path::new("db"), options).unwrap();
    file.use_page_key(&KEY).unwrap();
    let store = store_of(file);
    let mut write = store.begin_write().unwrap();
    let tree = write.create_tree().unwrap();
    write.set_catalog_root(tree.root());
    for index in 0..200u32 {
        let mut value = SECRET.to_vec();
        value.extend_from_slice(&index.to_le_bytes());
        write.put(&tree, &index.to_be_bytes(), &value).unwrap();
    }
    write.put(&tree, b"large", &SECRET.repeat(2000)).unwrap();
    write.commit().unwrap();
    store.checkpoint().unwrap();
    store.sync().unwrap();
}

fn raw_contents(fs: &SimFileSystem, path: &str) -> Vec<u8> {
    let file = fs.open(Path::new(path), OpenMode::Read).unwrap();
    let mut bytes = vec![0u8; file.size().unwrap() as usize];
    file.read_exact_at(&mut bytes, 0).unwrap();
    bytes
}

fn holds(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

fn reopened(fs: &SimFileSystem, path: &str, key: Option<&[u8]>) -> Result<Store<SimFile>, StorageError> {
    let mut file = PageFile::open(fs, Path::new(path), Access::ReadWrite)?;
    if let Some(key) = key {
        file.use_page_key(key)?;
    }
    Ok(store_of(file))
}

fn read_secret(store: &Store<SimFile>) -> Result<Vec<u8>, StorageError> {
    let read = store.begin_read()?;
    let tree: BTree = read.open_tree(read.catalog_root())?;
    Ok(read.get(&tree, &7u32.to_be_bytes())?.unwrap_or_default())
}

#[test]
fn an_encrypted_file_holds_no_plaintext_and_reads_back_with_its_key() {
    let fs = SimFileSystem::new();
    encrypted_database(&fs);
    let raw = raw_contents(&fs, "db");
    assert!(!holds(&raw, SECRET), "a value is readable in the file");
    let store = reopened(&fs, "db", Some(&KEY)).unwrap();
    assert!(store.superblock().unwrap().features.incompatible & Features::ENCRYPTED_PAGES != 0);
    assert!(read_secret(&store).unwrap().starts_with(SECRET));
    let read = store.begin_read().unwrap();
    let tree = read.open_tree(read.catalog_root()).unwrap();
    assert_eq!(read.get(&tree, b"large").unwrap().unwrap(), SECRET.repeat(2000));
}

#[test]
fn a_missing_or_wrong_key_is_refused() {
    let fs = SimFileSystem::new();
    encrypted_database(&fs);
    let without = reopened(&fs, "db", None).unwrap();
    assert!(matches!(read_secret(&without), Err(StorageError::NeedsKey)));
    drop(without);
    assert!(matches!(reopened(&fs, "db", Some(&[1; 32])), Err(StorageError::WrongKey)));
    assert!(matches!(reopened(&fs, "db", Some(&[1; 16])), Err(StorageError::WrongKey)));
}

#[test]
fn a_file_with_plain_data_cannot_take_a_key() {
    let fs = SimFileSystem::new();
    let options = CreateOptions {
        page_size: PageSize::DEFAULT,
        file_id: FileId(*b"plain-file-here!"),
    };
    let store = store_of(PageFile::create(&fs, Path::new("plain"), options).unwrap());
    let mut write = store.begin_write().unwrap();
    let tree = write.create_tree().unwrap();
    write.set_catalog_root(tree.root());
    write.put(&tree, b"k", SECRET).unwrap();
    write.commit().unwrap();
    store.sync().unwrap();
    drop(store);
    let mut file = PageFile::open(&fs, Path::new("plain"), Access::ReadWrite).unwrap();
    assert!(file.superblock().catalog_root != NO_PAGE);
    assert!(matches!(file.use_page_key(&KEY), Err(StorageError::NotEncrypted)));
}

#[test]
fn a_copy_of_an_encrypted_file_stays_encrypted_with_the_same_key() {
    let fs = SimFileSystem::new();
    encrypted_database(&fs);
    let store = reopened(&fs, "db", Some(&KEY)).unwrap();
    let read = store.begin_read().unwrap();
    read.copy_to(
        &fs,
        &SimMemory::new(),
        Arc::new(SimClock::new(1)),
        CopyDestination {
            path: Path::new("copy"),
            staging: Path::new("copy.backing-up"),
            replaces: false,
        },
    )
    .unwrap();
    drop(read);
    assert!(!holds(&raw_contents(&fs, "copy"), SECRET));
    let copy = reopened(&fs, "copy", Some(&KEY)).unwrap();
    assert!(read_secret(&copy).unwrap().starts_with(SECRET));
    assert!(matches!(reopened(&fs, "copy", Some(&[3; 32])), Err(StorageError::WrongKey)));
}
