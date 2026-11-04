//! The behavior every `IoQueue` shares, checked against each backend of the operating system's
//! queue (the automatic choice, inline, the thread pool, io_uring on Linux and I/O completion
//! ports on Windows) and the simulation.

use eggdb_platform::{
    AlignedBuffer, ErrorKind, File, FileSystem, IoBackend, IoBuffer, IoCompletion, IoOp, IoQueue,
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
            for options in os_backends() {
                let dir = TestDir::new();
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

fn io_uring_is_built() -> bool {
    cfg!(all(
        target_os = "linux",
        any(
            target_arch = "x86_64",
            target_arch = "x86",
            target_arch = "aarch64",
            target_arch = "arm",
            target_arch = "riscv64",
            target_arch = "powerpc64",
            target_arch = "s390x",
            target_arch = "loongarch64"
        ),
        not(all(target_arch = "x86_64", target_pointer_width = "32"))
    ))
}

fn os_backends() -> Vec<IoQueueOptions> {
    let mut backends = vec![
        IoQueueOptions::new(),
        IoQueueOptions::new().threads(0),
        IoQueueOptions::new().backend(IoBackend::Inline),
        IoQueueOptions::new().backend(IoBackend::ThreadPool),
    ];
    if io_uring_is_built() {
        backends.push(IoQueueOptions::new().backend(IoBackend::IoUring));
    }
    if cfg!(windows) {
        backends.push(IoQueueOptions::new().backend(IoBackend::Iocp));
        backends.push(IoQueueOptions::new().backend(IoBackend::Iocp).threads(0));
    }
    backends
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

#[test]
fn an_asked_for_backend_is_given_or_refused() {
    let fs = OsFileSystem::new();
    let pool = fs
        .io_queue(IoQueueOptions::new().backend(IoBackend::ThreadPool))
        .unwrap();
    assert_eq!(pool.backend(), IoBackend::ThreadPool);
    let inline = fs
        .io_queue(IoQueueOptions::new().backend(IoBackend::Inline))
        .unwrap();
    assert_eq!(inline.backend(), IoBackend::Inline);
    let error = fs
        .io_queue(
            IoQueueOptions::new()
                .backend(IoBackend::ThreadPool)
                .threads(0),
        )
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    let iocp = fs.io_queue(IoQueueOptions::new().backend(IoBackend::Iocp));
    if cfg!(windows) {
        assert_eq!(iocp.unwrap().backend(), IoBackend::Iocp);
    } else {
        assert_eq!(iocp.unwrap_err().kind(), ErrorKind::Unsupported);
    }
    let uring = fs.io_queue(IoQueueOptions::new().backend(IoBackend::IoUring));
    if io_uring_is_built() {
        assert_eq!(uring.unwrap().backend(), IoBackend::IoUring);
    } else {
        assert_eq!(uring.unwrap_err().kind(), ErrorKind::Unsupported);
    }
}

#[test]
fn the_automatic_choice_is_io_uring_where_it_is_built_and_the_pool_elsewhere() {
    let fs = OsFileSystem::new();
    let queue = fs.io_queue(IoQueueOptions::new()).unwrap();
    let without_threads = fs.io_queue(IoQueueOptions::new().threads(0)).unwrap();
    if io_uring_is_built() {
        assert_eq!(queue.backend(), IoBackend::IoUring);
        assert_eq!(without_threads.backend(), IoBackend::IoUring);
    } else {
        assert_eq!(queue.backend(), IoBackend::ThreadPool);
        assert_eq!(without_threads.backend(), IoBackend::Inline);
    }
}

#[test]
fn the_simulation_refuses_what_every_machine_refuses_and_ignores_the_rest() {
    let fs = SimFileSystem::new();
    let no_pool = fs.io_queue(
        IoQueueOptions::new()
            .backend(IoBackend::ThreadPool)
            .threads(0),
    );
    assert_eq!(no_pool.unwrap_err().kind(), ErrorKind::InvalidInput);
    for backend in [
        IoBackend::Inline,
        IoBackend::ThreadPool,
        IoBackend::IoUring,
        IoBackend::Iocp,
    ] {
        assert!(fs.io_queue(IoQueueOptions::new().backend(backend)).is_ok());
    }
}
