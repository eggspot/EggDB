use crate::error::{Error, ErrorKind, Result};
use crate::io::{IoCompletion, IoOp, IoQueue, IoQueueOptions, RejectedOp, check_reap, queue_full};
use crate::os_cpus::OsCpus;
use crate::os_fs::OsFile;
use crate::threads::{SpawnedThread, ThreadOptions};
use std::collections::VecDeque;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex, PoisonError, mpsc};

/// The operating system's asynchronous file I/O, made by
/// [`OsFileSystem::io_queue`](crate::FileSystem::io_queue).
///
/// For now every operating system runs the operations on a pool of worker threads named
/// `eggdb-io-<n>`, which stays the path for files on macOS and the BSDs, whose kqueue has no
/// asynchronous file I/O. With no threads, the operations run on the caller's thread inside
/// [`IoQueue::reap`], in the order they were submitted. Where panics unwind (release builds
/// abort instead), an operation that panics on a worker completes with [`ErrorKind::Other`]
/// instead of leaving its completion missing.
#[derive(Debug)]
pub struct OsIoQueue {
    capacity: usize,
    in_flight: usize,
    backend: Backend,
}

#[derive(Debug)]
enum Backend {
    Inline(VecDeque<Job>),
    Pool(Pool),
}

#[derive(Debug)]
struct Job {
    op: IoOp<OsFile>,
    user_data: u64,
}

#[derive(Debug)]
struct Pool {
    jobs: Option<mpsc::Sender<Job>>,
    completions: mpsc::Receiver<IoCompletion<OsFile>>,
    workers: Vec<SpawnedThread<()>>,
}

impl OsIoQueue {
    const WORKER_STACK_SIZE: usize = 64 * 1024;

    pub(crate) fn new(options: IoQueueOptions) -> Result<Self> {
        let (capacity, threads) = options.validated()?;
        let backend = if threads == 0 {
            Backend::Inline(VecDeque::new())
        } else {
            Backend::Pool(Pool::start(threads)?)
        };
        Ok(Self {
            capacity,
            in_flight: 0,
            backend,
        })
    }
}

impl IoQueue for OsIoQueue {
    type File = OsFile;

    fn submit(
        &mut self,
        op: IoOp<OsFile>,
        user_data: u64,
    ) -> core::result::Result<(), RejectedOp<OsFile>> {
        if self.in_flight >= self.capacity {
            return Err(queue_full(op));
        }
        let job = Job { op, user_data };
        match &mut self.backend {
            Backend::Inline(pending) => pending.push_back(job),
            Backend::Pool(pool) => pool.send(job)?,
        }
        self.in_flight += 1;
        Ok(())
    }

    fn reap(&mut self, completions: &mut Vec<IoCompletion<OsFile>>, min: usize) -> Result<usize> {
        check_reap(min, self.in_flight)?;
        let before = completions.len();
        let outcome = match &mut self.backend {
            Backend::Inline(pending) => {
                completions.extend(pending.drain(..).map(Job::run));
                Ok(())
            }
            Backend::Pool(pool) => pool.receive(completions, min),
        };
        let reaped = completions.len() - before;
        self.in_flight -= reaped;
        outcome.map(|()| reaped)
    }

    fn in_flight(&self) -> usize {
        self.in_flight
    }

    fn capacity(&self) -> usize {
        self.capacity
    }
}

impl Drop for OsIoQueue {
    fn drop(&mut self) {
        if let Backend::Inline(pending) = &mut self.backend {
            pending.drain(..).for_each(|job| drop(job.run()));
        }
    }
}

impl Job {
    fn run(mut self) -> IoCompletion<OsFile> {
        let result = self.op.perform();
        IoCompletion {
            user_data: self.user_data,
            result,
            op: self.op,
        }
    }
}

impl Job {
    fn run_catching_panics(mut self) -> IoCompletion<OsFile> {
        let result = panic::catch_unwind(AssertUnwindSafe(|| self.op.perform()))
            .unwrap_or_else(|_| Err(Error::new(ErrorKind::Other)));
        IoCompletion {
            user_data: self.user_data,
            result,
            op: self.op,
        }
    }
}

impl Pool {
    fn start(threads: usize) -> Result<Self> {
        let (jobs, queued) = mpsc::channel::<Job>();
        let (finished, completions) = mpsc::channel();
        let queued = Arc::new(Mutex::new(queued));
        let mut pool = Self {
            jobs: Some(jobs),
            completions,
            workers: Vec::with_capacity(threads),
        };
        let cpus = OsCpus::new();
        for number in 0..threads {
            let queued = Arc::clone(&queued);
            let finished = finished.clone();
            let worker = ThreadOptions::new()
                .name(format!("eggdb-io-{number}"))
                .stack_size(OsIoQueue::WORKER_STACK_SIZE)
                .spawn(&cpus, move || work(&queued, &finished))?;
            pool.workers.push(worker);
        }
        Ok(pool)
    }

    fn send(&self, job: Job) -> core::result::Result<(), RejectedOp<OsFile>> {
        let Some(jobs) = &self.jobs else {
            return Err(stopped(job.op));
        };
        jobs.send(job).map_err(|returned| stopped(returned.0.op))
    }

    fn receive(&self, completions: &mut Vec<IoCompletion<OsFile>>, min: usize) -> Result<()> {
        for _ in 0..min {
            let completion = self
                .completions
                .recv()
                .map_err(|_| Error::new(ErrorKind::Other))?;
            completions.push(completion);
        }
        completions.extend(self.completions.try_iter());
        Ok(())
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.jobs = None;
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn work(queued: &Mutex<mpsc::Receiver<Job>>, finished: &mpsc::Sender<IoCompletion<OsFile>>) {
    loop {
        let next = queued.lock().unwrap_or_else(PoisonError::into_inner).recv();
        let Ok(job) = next else {
            return;
        };
        let _ = finished.send(job.run_catching_panics());
    }
}

fn stopped(op: IoOp<OsFile>) -> RejectedOp<OsFile> {
    RejectedOp {
        error: Error::new(ErrorKind::Other),
        op,
    }
}

#[cfg(test)]
mod tests {
    use crate::{FileSystem, IoOp, IoQueue, IoQueueOptions, OpenMode, OsFileSystem};
    use std::sync::Arc;

    #[test]
    fn a_queue_without_threads_runs_operations_in_submission_order() {
        let path = std::env::temp_dir().join(format!("eggdb-io-inline-{}", std::process::id()));
        let fs = OsFileSystem::new();
        let file = Arc::new(fs.open(&path, OpenMode::Create).unwrap());
        let mut queue = fs.io_queue(IoQueueOptions::new().threads(0)).unwrap();
        for tag in 0..8_u8 {
            let op = IoOp::Write {
                file: Arc::clone(&file),
                offset: 0,
                buffer: vec![tag].into(),
            };
            assert!(queue.submit(op, u64::from(tag)).is_ok());
        }
        let mut completions = Vec::new();
        assert_eq!(queue.reap(&mut completions, 0).unwrap(), 8);
        let tags: Vec<u64> = completions.iter().map(|done| done.user_data).collect();
        assert_eq!(tags, (0..8).collect::<Vec<u64>>());
        let mut last = [0];
        crate::File::read_exact_at(&*file, &mut last, 0).unwrap();
        assert_eq!(last, [7]);
        drop(file);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_pool_reaps_at_least_what_it_waits_for() {
        let path = std::env::temp_dir().join(format!("eggdb-io-partial-{}", std::process::id()));
        let fs = OsFileSystem::new();
        let file = Arc::new(fs.open(&path, OpenMode::Create).unwrap());
        let mut queue = fs.io_queue(IoQueueOptions::new().threads(2)).unwrap();
        for tag in 0..6 {
            let op = IoOp::SyncData {
                file: Arc::clone(&file),
            };
            assert!(queue.submit(op, tag).is_ok());
        }
        let mut completions = Vec::new();
        let reaped = queue.reap(&mut completions, 2).unwrap();
        assert!(reaped >= 2 && reaped == completions.len());
        assert_eq!(queue.in_flight(), 6 - reaped);
        let rest = queue.in_flight();
        queue.reap(&mut completions, rest).unwrap();
        assert_eq!(completions.len(), 6);
        assert!(completions.iter().all(|done| done.result.is_ok()));
        drop(file);
        std::fs::remove_file(&path).unwrap();
    }
}
