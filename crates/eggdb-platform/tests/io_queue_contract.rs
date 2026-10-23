//! The behavior every `IoQueue` shares, checked against the operating system's thread pool,
//! its inline queue without threads, and the simulation.

use eggdb_platform::{
    AlignedBuffer, ErrorKind, File, FileSystem, IoBuffer, IoCompletion, IoOp, IoQueue,
    IoQueueOptions, OpenMode, OsFileSystem, SimFileSystem,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let unique = format!(
            "eggdb-io-{}-{}",
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
    ($name:ident, |$fs:ident, $path:ident, $options:ident| $body:block) => {
        #[test]
        fn $name() {
            fn run<F: FileSystem>(
                $fs: &F,
                $path: &dyn Fn(&str) -> PathBuf,
                $options: IoQueueOptions,
            ) $body
            for threads in [4, 0] {
                let dir = TestDir::new();
                let options = IoQueueOptions::new().threads(threads);
                run(&OsFileSystem::new(), &|name| dir.0.join(name), options);
            }
            for seed in 0..8 {
                let fs = SimFileSystem::new();
                fs.seed_io_queues(seed);
                run(&fs, &|name| Path::new(name).to_path_buf(), IoQueueOptions::new());
            }
        }
    };
}

fn reap_all<Q: IoQueue>(queue: &mut Q) -> Vec<IoCompletion<Q::File>> {
    let mut completions = Vec::new();
    let in_flight = queue.in_flight();
    queue.reap(&mut completions, in_flight).unwrap();
    assert_eq!(queue.in_flight(), 0);
    completions.sort_by_key(|completion| completion.user_data);
    completions
}

fn write<F>(file: &Arc<F>, offset: u64, bytes: &[u8]) -> IoOp<F> {
    IoOp::Write {
        file: Arc::clone(file),
        offset,
        buffer: bytes.to_vec().into(),
    }
}

fn read<F>(file: &Arc<F>, offset: u64, len: usize) -> IoOp<F> {
    IoOp::Read {
        file: Arc::clone(file),
        offset,
        buffer: vec![0; len].into(),
    }
}

fn submit<Q: IoQueue>(queue: &mut Q, op: IoOp<Q::File>, user_data: u64) {
    if let Err(rejected) = queue.submit(op, user_data) {
        panic!("submit failed: {rejected}");
    }
}

fn write_fully<Q: IoQueue>(queue: &mut Q, file: &Arc<Q::File>, offset: u64, bytes: &[u8]) {
    let mut done = 0;
    while done < bytes.len() {
        let at = offset + u64::try_from(done).unwrap();
        submit(queue, write(file, at, &bytes[done..]), 0);
        let completion = reap_all(queue).pop().unwrap();
        done += completion.result.unwrap();
    }
}

contract!(
    writes_and_reads_complete_with_their_tags_and_buffers,
    |fs, path, options| {
        let file = Arc::new(fs.open(&path("data"), OpenMode::CreateNew).unwrap());
        let mut queue = fs.io_queue(options).unwrap();
        for block in 0..8_u8 {
            write_fully(&mut queue, &file, u64::from(block) * 4, &[block; 4]);
        }
        for block in 0..8_u8 {
            submit(
                &mut queue,
                read(&file, u64::from(block) * 4, 4),
                u64::from(block),
            );
        }
        assert_eq!(queue.in_flight(), 8);
        for (block, completion) in (0..8_u8).zip(reap_all(&mut queue)) {
            assert_eq!(completion.user_data, u64::from(block));
            let read = completion.result.unwrap();
            let buffer = completion.op.into_buffer().unwrap();
            assert!(buffer[..read].iter().all(|&byte| byte == block));
        }
    }
);

contract!(
    syncs_complete_with_nothing_transferred,
    |fs, path, options| {
        let file = Arc::new(fs.open(&path("data"), OpenMode::CreateNew).unwrap());
        let mut queue = fs.io_queue(options).unwrap();
        write_fully(&mut queue, &file, 0, b"durable");
        submit(
            &mut queue,
            IoOp::SyncData {
                file: Arc::clone(&file),
            },
            1,
        );
        submit(
            &mut queue,
            IoOp::SyncAll {
                file: Arc::clone(&file),
            },
            2,
        );
        for completion in reap_all(&mut queue) {
            assert_eq!(completion.result.unwrap(), 0);
            assert!(completion.op.buffer().is_none());
        }
        let mut contents = [0; 7];
        file.read_exact_at(&mut contents, 0).unwrap();
        assert_eq!(&contents, b"durable");
    }
);

contract!(
    a_full_queue_hands_the_operation_back,
    |fs, path, options| {
        let file = Arc::new(fs.open(&path("data"), OpenMode::CreateNew).unwrap());
        let mut queue = fs.io_queue(options.capacity(2)).unwrap();
        assert_eq!(queue.capacity(), 2);
        submit(&mut queue, read(&file, 0, 1), 0);
        submit(&mut queue, read(&file, 0, 1), 1);
        let rejected = queue.submit(read(&file, 0, 3), 2).unwrap_err();
        assert_eq!(rejected.error.kind(), ErrorKind::QueueFull);
        assert_eq!(rejected.op.buffer().unwrap().len(), 3);
        assert_eq!(reap_all(&mut queue).len(), 2);
        submit(&mut queue, rejected.op, 2);
        assert_eq!(reap_all(&mut queue)[0].user_data, 2);
    }
);

contract!(
    waiting_for_more_than_is_in_flight_is_refused,
    |fs, path, options| {
        let file = Arc::new(fs.open(&path("data"), OpenMode::CreateNew).unwrap());
        let mut queue = fs.io_queue(options).unwrap();
        let mut completions = Vec::new();
        let error = queue.reap(&mut completions, 1).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(queue.reap(&mut completions, 0).unwrap(), 0);
        submit(&mut queue, read(&file, 0, 1), 0);
        let error = queue.reap(&mut completions, 2).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(queue.in_flight(), 1);
    }
);

contract!(failures_arrive_in_the_completion, |fs, path, options| {
    drop(fs.open(&path("data"), OpenMode::CreateNew).unwrap());
    let file = Arc::new(fs.open(&path("data"), OpenMode::Read).unwrap());
    let mut queue = fs.io_queue(options).unwrap();
    submit(&mut queue, write(&file, 0, b"no"), 0);
    submit(
        &mut queue,
        IoOp::SyncData {
            file: Arc::clone(&file),
        },
        1,
    );
    for completion in reap_all(&mut queue) {
        assert_eq!(
            completion.result.unwrap_err().kind(),
            ErrorKind::PermissionDenied
        );
    }
});

contract!(
    dropping_a_queue_finishes_what_is_in_flight,
    |fs, path, options| {
        let file = Arc::new(fs.open(&path("data"), OpenMode::CreateNew).unwrap());
        let mut queue = fs.io_queue(options).unwrap();
        for block in 0..16_u8 {
            submit(&mut queue, write(&file, u64::from(block), &[block]), 0);
        }
        drop(queue);
        let mut contents = [0; 16];
        file.read_exact_at(&mut contents, 0).unwrap();
        assert!(
            contents
                .iter()
                .enumerate()
                .all(|(at, &byte)| usize::from(byte) == at)
        );
    }
);

contract!(
    uncached_handles_take_aligned_buffers,
    |fs, path, options| {
        let file = match fs.open_uncached(&path("pages"), OpenMode::CreateNew) {
            Ok(file) => Arc::new(file),
            Err(error) if error.kind() == ErrorKind::Unsupported => return,
            Err(error) => panic!("open_uncached failed: {error}"),
        };
        let page = AlignedBuffer::ALIGNMENT;
        let mut written = AlignedBuffer::zeroed(page).unwrap();
        written.fill(7);
        let mut queue = fs.io_queue(options).unwrap();
        submit(
            &mut queue,
            IoOp::Write {
                file: Arc::clone(&file),
                offset: 0,
                buffer: written.into(),
            },
            0,
        );
        assert_eq!(reap_all(&mut queue)[0].result.as_ref().unwrap(), &page);
        let buffer = IoBuffer::from(AlignedBuffer::zeroed(page).unwrap());
        submit(
            &mut queue,
            IoOp::Read {
                file: Arc::clone(&file),
                offset: 0,
                buffer,
            },
            1,
        );
        let completion = reap_all(&mut queue).pop().unwrap();
        let read = completion.result.unwrap();
        assert!(read > 0);
        let buffer = completion.op.into_buffer().unwrap();
        assert!(buffer[..read].iter().all(|&byte| byte == 7));
    }
);

contract!(queues_without_capacity_are_refused, |fs, path, options| {
    let _ = path;
    let error = fs.io_queue(options.capacity(0)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
});
