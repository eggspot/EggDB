//! The behavior every `FileSystem` shares, checked against the operating system and the simulation.

use eggdb_platform::{
    AlignedBuffer, ErrorKind, File, FileSystem, OpenMode, OsFileSystem, SimFileSystem,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let unique = format!(
            "eggdb-platform-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

macro_rules! contract {
    ($name:ident, |$fs:ident, $path:ident| $body:block) => {
        #[test]
        fn $name() {
            fn run<F: FileSystem>($fs: &F, $path: &dyn Fn(&str) -> PathBuf) $body
            let dir = TestDir::new();
            run(&OsFileSystem::new(), &|name| dir.0.join(name));
            run(&SimFileSystem::new(), &|name| Path::new(name).to_path_buf());
        }
    };
}

fn read_all(file: &impl File) -> Vec<u8> {
    let mut contents = vec![0; usize::try_from(file.size().unwrap()).unwrap()];
    file.read_exact_at(&mut contents, 0).unwrap();
    contents
}

contract!(written_bytes_read_back_at_their_offsets, |fs, path| {
    let file = fs.open(&path("data"), OpenMode::CreateNew).unwrap();
    file.write_all_at(b"hello", 0).unwrap();
    file.write_all_at(b"world", 5).unwrap();
    let mut middle = [0; 4];
    file.read_exact_at(&mut middle, 3).unwrap();
    assert_eq!(&middle, b"lowo");
    assert_eq!(file.size().unwrap(), 10);
});

contract!(writing_past_the_end_fills_the_gap_with_zeros, |fs, path| {
    let file = fs.open(&path("data"), OpenMode::CreateNew).unwrap();
    file.write_all_at(b"x", 3).unwrap();
    assert_eq!(read_all(&file), [0, 0, 0, b'x']);
});

contract!(reading_at_or_past_the_end_returns_nothing, |fs, path| {
    let file = fs.open(&path("data"), OpenMode::CreateNew).unwrap();
    file.write_all_at(b"abc", 0).unwrap();
    let mut buf = [0; 8];
    assert_eq!(file.read_at(&mut buf, 3).unwrap(), 0);
    assert_eq!(file.read_at(&mut buf, 1_000).unwrap(), 0);
    let error = file.read_exact_at(&mut buf, 0).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UnexpectedEof);
});

contract!(set_size_truncates_and_extends_with_zeros, |fs, path| {
    let file = fs.open(&path("data"), OpenMode::CreateNew).unwrap();
    file.write_all_at(b"abcdef", 0).unwrap();
    file.set_size(2).unwrap();
    assert_eq!(read_all(&file), b"ab");
    file.set_size(4).unwrap();
    assert_eq!(read_all(&file), [b'a', b'b', 0, 0]);
});

contract!(contents_persist_across_handles, |fs, path| {
    let file = fs.open(&path("data"), OpenMode::Create).unwrap();
    file.write_all_at(b"kept", 0).unwrap();
    file.sync_data().unwrap();
    file.sync_all().unwrap();
    drop(file);
    let again = fs.open(&path("data"), OpenMode::Read).unwrap();
    assert_eq!(read_all(&again), b"kept");
    let reopened = fs.open(&path("data"), OpenMode::Create).unwrap();
    assert_eq!(read_all(&reopened), b"kept");
});

contract!(open_modes_require_or_forbid_an_existing_file, |fs, path| {
    let missing = path("missing");
    for mode in [OpenMode::Read, OpenMode::ReadWrite] {
        let error = fs.open(&missing, mode).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NotFound);
    }
    fs.open(&path("data"), OpenMode::CreateNew).unwrap();
    let error = fs.open(&path("data"), OpenMode::CreateNew).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::AlreadyExists);
});

contract!(read_only_handles_refuse_writes, |fs, path| {
    fs.open(&path("data"), OpenMode::CreateNew).unwrap();
    let file = fs.open(&path("data"), OpenMode::Read).unwrap();
    assert!(file.write_at(b"x", 0).is_err());
    assert!(file.set_size(10).is_err());
    assert_eq!(file.size().unwrap(), 0);
});

contract!(rename_replaces_the_target_and_remove_deletes, |fs, path| {
    let old = fs.open(&path("old"), OpenMode::CreateNew).unwrap();
    old.write_all_at(b"old", 0).unwrap();
    let new = fs.open(&path("new"), OpenMode::CreateNew).unwrap();
    new.write_all_at(b"new", 0).unwrap();
    drop((old, new));
    fs.rename(&path("new"), &path("old")).unwrap();
    assert!(!fs.exists(&path("new")).unwrap());
    let current = fs.open(&path("old"), OpenMode::Read).unwrap();
    assert_eq!(read_all(&current), b"new");
    drop(current);
    fs.remove_file(&path("old")).unwrap();
    assert!(!fs.exists(&path("old")).unwrap());
    let error = fs.remove_file(&path("old")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound);
});

contract!(
    directories_are_created_with_their_parents_and_synced,
    |fs, path| {
        let nested = path("a/b/c");
        fs.create_dir_all(&nested).unwrap();
        fs.create_dir_all(&nested).unwrap();
        assert!(fs.exists(&nested).unwrap());
        fs.sync_dir(&nested).unwrap();
        let file = fs.open(&nested.join("pages"), OpenMode::CreateNew).unwrap();
        file.write_all_at(b"p", 0).unwrap();
        fs.sync_dir(&nested).unwrap();
        let error = fs.sync_dir(&path("no-such-dir")).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NotFound);
    }
);

contract!(creating_a_file_in_a_missing_directory_fails, |fs, path| {
    let error = fs
        .open(&path("no-such-dir/data"), OpenMode::Create)
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound);
});

contract!(one_handle_is_shared_between_threads, |fs, path| {
    let file = fs.open(&path("data"), OpenMode::CreateNew).unwrap();
    std::thread::scope(|scope| {
        for writer in 0..4u8 {
            let file = &file;
            scope.spawn(move || {
                let offset = u64::from(writer) * 100;
                file.write_all_at(&[writer; 100], offset).unwrap();
            });
        }
    });
    let contents = read_all(&file);
    for (writer, chunk) in contents.chunks(100).enumerate() {
        assert!(chunk.iter().all(|&byte| usize::from(byte) == writer));
    }
});

contract!(an_empty_write_past_the_end_changes_nothing, |fs, path| {
    let file = fs.open(&path("data"), OpenMode::CreateNew).unwrap();
    assert_eq!(file.write_at(&[], 100).unwrap(), 0);
    assert_eq!(file.size().unwrap(), 0);
});

contract!(
    a_removed_file_stays_readable_through_an_open_handle,
    |fs, path| {
        let file = fs.open(&path("segment"), OpenMode::CreateNew).unwrap();
        file.write_all_at(b"old log", 0).unwrap();
        fs.remove_file(&path("segment")).unwrap();
        assert!(!fs.exists(&path("segment")).unwrap());
        assert_eq!(read_all(&file), b"old log");
    }
);

contract!(
    renaming_over_an_open_file_keeps_its_handle_readable,
    |fs, path| {
        let current = fs.open(&path("current"), OpenMode::CreateNew).unwrap();
        current.write_all_at(b"v1", 0).unwrap();
        let next = fs.open(&path("next"), OpenMode::CreateNew).unwrap();
        next.write_all_at(b"v2", 0).unwrap();
        drop(next);
        fs.rename(&path("next"), &path("current")).unwrap();
        assert_eq!(read_all(&current), b"v1");
        let reopened = fs.open(&path("current"), OpenMode::Read).unwrap();
        assert_eq!(read_all(&reopened), b"v2");
    }
);

contract!(
    uncached_handles_read_back_whole_aligned_pages,
    |fs, path| {
        let file = match fs.open_uncached(&path("pages"), OpenMode::CreateNew) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::Unsupported => return,
            Err(error) => panic!("open_uncached failed: {error}"),
        };
        assert!(file.is_uncached());
        let page = AlignedBuffer::ALIGNMENT;
        let mut written = AlignedBuffer::zeroed(2 * page).unwrap();
        for (index, byte) in written.iter_mut().enumerate() {
            *byte = u8::try_from(index % 251).unwrap();
        }
        file.write_all_at(&written, 0).unwrap();
        file.sync_data().unwrap();
        let mut read = AlignedBuffer::zeroed(page).unwrap();
        file.read_exact_at(&mut read, 4096).unwrap();
        assert_eq!(&read[..], &written[page..]);
        assert_eq!(file.size().unwrap(), 8192);
    }
);

contract!(syncing_needs_a_writable_handle, |fs, path| {
    let file = fs.open(&path("data"), OpenMode::CreateNew).unwrap();
    file.write_all_at(b"x", 0).unwrap();
    file.sync_all().unwrap();
    let reader = fs.open(&path("data"), OpenMode::Read).unwrap();
    for result in [reader.sync_data(), reader.sync_all()] {
        assert_eq!(result.unwrap_err().kind(), ErrorKind::PermissionDenied);
    }
});

contract!(an_empty_write_changes_nothing_at_any_offset, |fs, path| {
    let file = fs.open(&path("data"), OpenMode::CreateNew).unwrap();
    assert_eq!(file.write_at(&[], 1 << 40).unwrap(), 0);
    assert_eq!(file.size().unwrap(), 0);
});

contract!(renaming_a_file_onto_itself_changes_nothing, |fs, path| {
    let file = fs.open(&path("data"), OpenMode::CreateNew).unwrap();
    file.write_all_at(b"kept", 0).unwrap();
    drop(file);
    fs.rename(&path("data"), &path("data")).unwrap();
    let file = fs.open(&path("data"), OpenMode::Read).unwrap();
    assert_eq!(read_all(&file), b"kept");
});

contract!(dot_components_name_the_same_file, |fs, path| {
    fs.create_dir_all(&path("./dir")).unwrap();
    let file = fs.open(&path("dir/./data"), OpenMode::CreateNew).unwrap();
    file.write_all_at(b"here", 0).unwrap();
    drop(file);
    assert!(fs.exists(&path("dir/data")).unwrap());
    fs.sync_dir(&path("./dir")).unwrap();
    let file = fs.open(&path("./dir/data"), OpenMode::Read).unwrap();
    assert_eq!(read_all(&file), b"here");
});
