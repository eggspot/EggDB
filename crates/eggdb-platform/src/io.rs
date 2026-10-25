use crate::aligned::AlignedBuffer;
use crate::error::{Error, ErrorKind, Result};
use crate::fs::File;
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

/// A buffer owned by an operation while it is in flight and handed back when it completes.
#[derive(Debug)]
#[non_exhaustive]
pub enum IoBuffer {
    /// Ordinary heap memory, for cached handles.
    Heap(Vec<u8>),
    /// Aligned memory, as uncached handles require.
    Aligned(AlignedBuffer),
}

impl Deref for IoBuffer {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Heap(bytes) => bytes,
            Self::Aligned(buffer) => buffer,
        }
    }
}

impl DerefMut for IoBuffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        match self {
            Self::Heap(bytes) => bytes,
            Self::Aligned(buffer) => buffer,
        }
    }
}

impl From<Vec<u8>> for IoBuffer {
    fn from(bytes: Vec<u8>) -> Self {
        Self::Heap(bytes)
    }
}

impl From<AlignedBuffer> for IoBuffer {
    fn from(buffer: AlignedBuffer) -> Self {
        Self::Aligned(buffer)
    }
}

/// One asynchronous operation on a file, owning the file handle and the buffer until it
/// completes.
///
/// Reads and writes transfer at most the buffer's length and may transfer less, as
/// [`File::read_at`] and [`File::write_at`] do; the completion says how much. After a short
/// read, the bytes of the buffer past the count are unspecified.
#[derive(Debug)]
#[non_exhaustive]
pub enum IoOp<F> {
    /// Reads into `buffer` from `offset`, as [`File::read_at`] does.
    Read {
        /// The file to read.
        file: Arc<F>,
        /// Where the read starts.
        offset: u64,
        /// The bytes to fill.
        buffer: IoBuffer,
    },
    /// Writes `buffer` at `offset`, as [`File::write_at`] does.
    Write {
        /// The file to write.
        file: Arc<F>,
        /// Where the write starts.
        offset: u64,
        /// The bytes to write.
        buffer: IoBuffer,
    },
    /// Makes the file's contents and size durable, as [`File::sync_data`] does.
    SyncData {
        /// The file to sync.
        file: Arc<F>,
    },
    /// Makes the file's contents, size and metadata durable, as [`File::sync_all`] does.
    SyncAll {
        /// The file to sync.
        file: Arc<F>,
    },
}

impl<F> IoOp<F> {
    /// The file the operation works on.
    pub fn file(&self) -> &Arc<F> {
        match self {
            Self::Read { file, .. }
            | Self::Write { file, .. }
            | Self::SyncData { file }
            | Self::SyncAll { file } => file,
        }
    }

    /// The buffer of a read or a write.
    pub fn buffer(&self) -> Option<&IoBuffer> {
        match self {
            Self::Read { buffer, .. } | Self::Write { buffer, .. } => Some(buffer),
            Self::SyncData { .. } | Self::SyncAll { .. } => None,
        }
    }

    /// Gives back the buffer of a read or a write.
    pub fn into_buffer(self) -> Option<IoBuffer> {
        match self {
            Self::Read { buffer, .. } | Self::Write { buffer, .. } => Some(buffer),
            Self::SyncData { .. } | Self::SyncAll { .. } => None,
        }
    }
}

impl<F: File> IoOp<F> {
    pub(crate) fn perform(&mut self) -> Result<usize> {
        self.perform_up_to(usize::MAX)
    }

    pub(crate) fn perform_up_to(&mut self, limit: usize) -> Result<usize> {
        match self {
            Self::Read {
                file,
                offset,
                buffer,
            } => {
                let len = buffer.len().min(limit);
                file.read_at(&mut buffer[..len], *offset)
            }
            Self::Write {
                file,
                offset,
                buffer,
            } => {
                let len = buffer.len().min(limit);
                file.write_at(&buffer[..len], *offset)
            }
            Self::SyncData { file } => file.sync_data().map(|()| 0),
            Self::SyncAll { file } => file.sync_all().map(|()| 0),
        }
    }
}

/// A finished operation: the caller's tag, the outcome and the operation with its buffer.
#[derive(Debug)]
pub struct IoCompletion<F> {
    /// The tag given to [`IoQueue::submit`].
    pub user_data: u64,
    /// How many bytes a read or a write transferred, 0 for a sync, or why it failed.
    pub result: Result<usize>,
    /// The operation, handing back its file and buffer.
    pub op: IoOp<F>,
}

/// An operation [`IoQueue::submit`] did not accept, handed back with the reason.
#[derive(Debug)]
pub struct RejectedOp<F> {
    /// Why the operation was not accepted.
    pub error: Error,
    /// The operation, untouched.
    pub op: IoOp<F>,
}

impl<F> fmt::Display for RejectedOp<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "operation not submitted: {}", self.error)
    }
}

impl<F: fmt::Debug> std::error::Error for RejectedOp<F> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// A queue of asynchronous file operations, owned by one thread: operations are submitted with
/// a tag and their completions are reaped later, in any order.
///
/// Operations in flight are not ordered against each other. A sync is only guaranteed to cover
/// the writes whose successful completions were reaped before the sync was submitted, so the
/// engine waits for a write's completion before submitting the sync that must make it durable.
/// Overlapping writes, or a read and a write of the same range, are never in flight at once,
/// and a failed write leaves its range unknown. Dropping a queue waits for every operation in
/// flight, since the operating system may still be using their buffers.
pub trait IoQueue: Send + fmt::Debug {
    /// The file handle type the operations use.
    type File: File;

    /// Starts `op`, tagged with `user_data` for its completion. Fails with
    /// [`ErrorKind::QueueFull`] when [`IoQueue::in_flight`] has reached
    /// [`IoQueue::capacity`], handing the operation back.
    fn submit(
        &mut self,
        op: IoOp<Self::File>,
        user_data: u64,
    ) -> core::result::Result<(), RejectedOp<Self::File>>;

    /// Appends the completions of finished operations to `completions`, first waiting until
    /// at least `min` have finished, and returns how many it appended. Fails with
    /// [`ErrorKind::InvalidInput`] when `min` is more than [`IoQueue::in_flight`], since that
    /// wait would never end. When it fails while waiting, the completions it already appended
    /// stay in `completions` and are no longer in flight.
    fn reap(
        &mut self,
        completions: &mut Vec<IoCompletion<Self::File>>,
        min: usize,
    ) -> Result<usize>;

    /// How many operations were submitted and not yet reaped.
    fn in_flight(&self) -> usize;

    /// The most operations that may be in flight at once.
    fn capacity(&self) -> usize;
}

/// How an [`OsIoQueue`](crate::OsIoQueue) runs its operations, as asked for with
/// [`IoQueueOptions::backend`] and reported by [`OsIoQueue::backend`](crate::OsIoQueue::backend).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IoBackend {
    /// On the caller's thread inside [`IoQueue::reap`], in the order they were submitted.
    Inline,
    /// On a pool of worker threads running positional reads, writes and syncs.
    ThreadPool,
    /// Through Linux's io_uring, from kernel 5.6.
    IoUring,
    /// Through Windows' I/O completion ports, for reads and writes of uncached files; cached
    /// files and syncs go to the queue's worker threads, or run inline without threads. Only
    /// on request: the automatic choice on Windows is still the thread pool.
    Iocp,
}

/// How [`FileSystem::io_queue`](crate::FileSystem::io_queue) builds a queue: how many
/// operations it holds, how many threads it may use and, when the caller asks, which
/// [`IoBackend`] runs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoQueueOptions {
    capacity: usize,
    threads: usize,
    backend: Option<IoBackend>,
}

impl IoQueueOptions {
    /// The capacity of a queue built with [`IoQueueOptions::new`].
    pub const DEFAULT_CAPACITY: usize = 128;

    /// The largest capacity a queue may have, the size of Linux's largest io_uring.
    pub const MAX_CAPACITY: usize = 32_768;

    /// The most threads a queue may use.
    pub const MAX_THREADS: usize = 256;

    /// The thread count of a queue built with [`IoQueueOptions::new`]: 4, or 0 on targets
    /// without threads.
    pub const DEFAULT_THREADS: usize = if cfg!(target_family = "wasm") { 0 } else { 4 };

    /// Options for a queue of [`IoQueueOptions::DEFAULT_CAPACITY`] operations using
    /// [`IoQueueOptions::DEFAULT_THREADS`] threads.
    pub fn new() -> Self {
        Self {
            capacity: Self::DEFAULT_CAPACITY,
            threads: Self::DEFAULT_THREADS,
            backend: None,
        }
    }

    /// Sets the most operations in flight at once.
    pub fn capacity(mut self, operations: usize) -> Self {
        self.capacity = operations;
        self
    }

    /// Sets how many worker threads an [`IoBackend::ThreadPool`] or an [`IoBackend::Iocp`] starts.
    /// When no backend is asked for and the queue has no asynchronous file I/O from the operating
    /// system (everywhere but Linux for now, or after io_uring was refused), 0 runs the operations
    /// [`IoBackend::Inline`]. A count above the capacity is lowered to it.
    pub fn threads(mut self, threads: usize) -> Self {
        self.threads = threads;
        self
    }

    /// Asks for one backend, given or refused: the queue never falls back to another. Without
    /// it, the queue takes the operating system's asynchronous file I/O where it is offered
    /// and otherwise the thread pool, or runs inline when [`IoQueueOptions::threads`] is 0.
    /// Every queue, the simulation's too, refuses [`IoBackend::ThreadPool`] with no threads
    /// with [`ErrorKind::InvalidInput`]. A machine refuses a backend it lacks with
    /// [`ErrorKind::Unsupported`]; the simulation accepts every other request and runs every
    /// operation its own way, whatever was asked for.
    pub fn backend(mut self, backend: IoBackend) -> Self {
        self.backend = Some(backend);
        self
    }

    pub(crate) fn requested_backend(self) -> Option<IoBackend> {
        self.backend
    }

    pub(crate) fn validated(self) -> Result<(usize, usize)> {
        if self.capacity == 0
            || self.capacity > Self::MAX_CAPACITY
            || self.threads > Self::MAX_THREADS
        {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        if self.backend == Some(IoBackend::ThreadPool) && self.threads == 0 {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        Ok((self.capacity, self.threads.min(self.capacity)))
    }
}

impl Default for IoQueueOptions {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn check_reap(min: usize, in_flight: usize) -> Result<()> {
    if min > in_flight {
        return Err(Error::new(ErrorKind::InvalidInput));
    }
    Ok(())
}

pub(crate) fn queue_full<F>(op: IoOp<F>) -> RejectedOp<F> {
    RejectedOp {
        error: Error::new(ErrorKind::QueueFull),
        op,
    }
}

#[cfg(test)]
mod tests {
    use super::{IoBuffer, IoQueueOptions};
    use crate::{AlignedBuffer, ErrorKind};

    #[test]
    fn buffers_expose_their_bytes_whatever_they_hold() {
        let mut heap = IoBuffer::from(vec![1, 2, 3]);
        heap[0] = 9;
        assert_eq!(&heap[..], [9, 2, 3]);
        let aligned = IoBuffer::from(AlignedBuffer::zeroed(AlignedBuffer::ALIGNMENT).unwrap());
        assert_eq!(aligned.len(), AlignedBuffer::ALIGNMENT);
    }

    #[test]
    fn options_reject_empty_and_oversized_queues() {
        for options in [
            IoQueueOptions::new().capacity(0),
            IoQueueOptions::new().capacity(IoQueueOptions::MAX_CAPACITY + 1),
            IoQueueOptions::new().threads(IoQueueOptions::MAX_THREADS + 1),
        ] {
            assert_eq!(
                options.validated().unwrap_err().kind(),
                ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn threads_never_outnumber_the_operations_they_serve() {
        let options = IoQueueOptions::new().capacity(2).threads(8);
        assert_eq!(options.validated().unwrap(), (2, 2));
    }
}
