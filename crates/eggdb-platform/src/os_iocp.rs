#[cfg(windows)]
pub(crate) use port::*;

#[cfg(not(windows))]
pub(crate) use unsupported::*;

#[cfg(windows)]
mod port {
    use crate::error::{Error, ErrorKind, Result};
    use crate::fs::File;
    use crate::io::{IoCompletion, IoOp, RejectedOp};
    use crate::os_fs::OsFile;
    use crate::os_io::{Job, Pool};
    use std::collections::VecDeque;
    use std::ffi::c_void;
    use std::fs;
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use std::ptr;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Weak};

    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;
    const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
    const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
    const FILE_SKIP_COMPLETION_PORT_ON_SUCCESS: u8 = 0x1;
    const FILE_SKIP_SET_EVENT_ON_HANDLE: u8 = 0x2;
    const INFINITE: u32 = u32::MAX;
    const WAIT_TIMEOUT: i32 = 258;
    const ERROR_HANDLE_EOF: i32 = 38;
    const ERROR_NOT_ENOUGH_MEMORY: i32 = 8;
    const ERROR_INVALID_FUNCTION: i32 = 1;
    const ERROR_INVALID_PARAMETER: i32 = 87;
    const ERROR_NOT_SUPPORTED: i32 = 50;
    const ERROR_INVALID_USER_BUFFER: i32 = 1784;
    const ERROR_IO_PENDING: i32 = 997;
    const POLL_WORKERS_MS: u32 = 10;
    const ENTRIES_PER_WAIT: usize = 64;

    const MAX_TRANSFER: usize = 0x4000_0000;
    const LARGEST_OFFSET: u64 = i64::MAX as u64;

    #[repr(C)]
    #[derive(Debug)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: *mut c_void,
    }

    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    struct OverlappedEntry {
        completion_key: usize,
        overlapped: *mut Overlapped,
        internal: usize,
        transferred: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateIoCompletionPort(
            file: RawHandle,
            existing_port: RawHandle,
            key: usize,
            threads: u32,
        ) -> RawHandle;
        fn GetQueuedCompletionStatusEx(
            port: RawHandle,
            entries: *mut OverlappedEntry,
            count: u32,
            removed: *mut u32,
            milliseconds: u32,
            alertable: i32,
        ) -> i32;
        fn PostQueuedCompletionStatus(
            port: RawHandle,
            transferred: u32,
            key: usize,
            overlapped: *mut Overlapped,
        ) -> i32;
        fn ReOpenFile(original: RawHandle, access: u32, share: u32, flags: u32) -> RawHandle;
        fn SetFileCompletionNotificationModes(file: RawHandle, flags: u8) -> i32;
        fn ReadFile(
            file: RawHandle,
            buffer: *mut c_void,
            len: u32,
            read: *mut u32,
            overlapped: *mut Overlapped,
        ) -> i32;
        fn WriteFile(
            file: RawHandle,
            buffer: *const c_void,
            len: u32,
            written: *mut u32,
            overlapped: *mut Overlapped,
        ) -> i32;
        fn GetOverlappedResult(
            file: RawHandle,
            overlapped: *mut Overlapped,
            transferred: *mut u32,
            wait: i32,
        ) -> i32;
    }

    #[repr(C)]
    #[derive(Debug)]
    struct Pending {
        overlapped: Overlapped,
        handle: RawHandle,
        op: IoOp<OsFile>,
        user_data: u64,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Started {
        InFlight,
        Finished,
    }

    #[derive(Debug)]
    struct PortHandle(OwnedHandle);

    impl PortHandle {
        fn wake(&self) {
            // SAFETY: posting a packet with no overlapped structure to a port this value owns;
            // the port reads nothing from us.
            unsafe { PostQueuedCompletionStatus(self.0.as_raw_handle(), 0, 0, ptr::null_mut()) };
        }
    }

    /// One I/O completion port, used by one queue at a time: reads and writes of uncached
    /// files go through the port, everything else through the queue's workers.
    #[derive(Debug)]
    pub(crate) struct Port {
        id: u64,
        in_port: usize,
        at_workers: usize,
        ready: VecDeque<IoCompletion<OsFile>>,
        inline: VecDeque<Job>,
        bound: Vec<Weak<OsFile>>,
        workers: Option<Pool>,
        port: Arc<PortHandle>,
    }

    impl Port {
        pub(crate) fn new(threads: usize) -> Result<Self> {
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            // SAFETY: a new completion port, not associated with any file.
            let raw = unsafe {
                CreateIoCompletionPort(
                    ptr::without_provenance_mut(usize::MAX),
                    ptr::null_mut(),
                    0,
                    1,
                )
            };
            if raw.is_null() {
                let error = io::Error::last_os_error();
                let kind = match error.raw_os_error() {
                    Some(ERROR_NOT_ENOUGH_MEMORY) => ErrorKind::OutOfMemory,
                    _ => ErrorKind::Unsupported,
                };
                return Err(Error::from(error).with_kind(kind));
            }
            // SAFETY: `CreateIoCompletionPort` returned a new handle that nothing else owns.
            let port = Arc::new(PortHandle(unsafe { OwnedHandle::from_raw_handle(raw) }));
            let workers = if threads == 0 {
                None
            } else {
                let waker = Arc::clone(&port);
                Some(Pool::start(threads, Arc::new(move || waker.wake()))?)
            };
            Ok(Self {
                id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                in_port: 0,
                at_workers: 0,
                ready: VecDeque::new(),
                inline: VecDeque::new(),
                bound: Vec::new(),
                workers,
                port,
            })
        }

        pub(crate) fn submit(&mut self, job: Job) -> core::result::Result<(), RejectedOp<OsFile>> {
            if !goes_through_the_port(&job.op) {
                return self.hand_to_workers(job);
            }
            if let Some(result) = answered_without_the_kernel(&job.op) {
                self.ready.push_back(IoCompletion {
                    user_data: job.user_data,
                    result,
                    op: job.op,
                });
                return Ok(());
            }
            let handle = match self.bind(job.op.file()) {
                Ok(handle) => handle,
                Err(error) => return Err(RejectedOp { error, op: job.op }),
            };
            self.start(handle, job)
        }

        pub(crate) fn reap(
            &mut self,
            completions: &mut Vec<IoCompletion<OsFile>>,
            min: usize,
        ) -> Result<()> {
            let before = completions.len();
            completions.extend(self.ready.drain(..));
            completions.extend(self.inline.drain(..).map(Job::run));
            self.collect(completions, 0)?;
            while completions.len() - before < min {
                let wait = if self.at_workers > 0 {
                    POLL_WORKERS_MS
                } else {
                    INFINITE
                };
                self.collect(completions, wait)?;
            }
            Ok(())
        }

        fn hand_to_workers(&mut self, job: Job) -> core::result::Result<(), RejectedOp<OsFile>> {
            match &self.workers {
                Some(workers) => {
                    workers.send(job)?;
                    self.at_workers += 1;
                }
                None => self.inline.push_back(job),
            }
            Ok(())
        }

        fn bind(&mut self, file: &Arc<OsFile>) -> Result<RawHandle> {
            let (handle, new) = file.overlapped_handle(self.id, |original| {
                reopen_on(original, file.is_writable(), &self.port, self.id)
            })?;
            if new {
                self.bound.retain(|bound| bound.strong_count() > 0);
                self.bound.push(Arc::downgrade(file));
            }
            Ok(handle)
        }

        fn start(
            &mut self,
            handle: RawHandle,
            job: Job,
        ) -> core::result::Result<(), RejectedOp<OsFile>> {
            let Job { op, user_data } = job;
            let offset = match &op {
                IoOp::Read { offset, .. } | IoOp::Write { offset, .. } => *offset,
                IoOp::SyncData { .. } | IoOp::SyncAll { .. } => 0,
            };
            let pending = Box::into_raw(Box::new(Pending {
                overlapped: Overlapped {
                    internal: 0,
                    internal_high: 0,
                    offset: offset as u32,
                    offset_high: (offset >> 32) as u32,
                    event: ptr::null_mut(),
                },
                handle,
                op,
                user_data,
            }));
            // SAFETY: `pending` is a live allocation this queue owns, and its buffer and
            // overlapped structure stay in place until its completion is read back below or
            // from the port.
            let started = unsafe { begin(pending) };
            match started {
                Ok(Started::InFlight) => {
                    self.in_port += 1;
                    Ok(())
                }
                Ok(Started::Finished) => {
                    // SAFETY: the operation finished at once, and with
                    // `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS` no packet will name `pending`, so
                    // this is its only owner.
                    let pending = unsafe { Box::from_raw(pending) };
                    self.ready.push_back(finish(*pending));
                    Ok(())
                }
                Err(error) => {
                    // SAFETY: the operation failed before it started, so the system posts no
                    // packet for it and holds no pointer into it.
                    let pending = unsafe { Box::from_raw(pending) };
                    let code = error.raw_os_error();
                    if self.in_port > 0
                        && matches!(
                            code,
                            Some(ERROR_INVALID_USER_BUFFER | ERROR_NOT_ENOUGH_MEMORY)
                        )
                    {
                        return Err(RejectedOp {
                            error: Error::from(error).with_kind(ErrorKind::QueueFull),
                            op: pending.op,
                        });
                    }
                    let result = if code == Some(ERROR_HANDLE_EOF) {
                        Ok(0)
                    } else {
                        Err(error.into())
                    };
                    self.ready.push_back(IoCompletion {
                        user_data: pending.user_data,
                        result,
                        op: pending.op,
                    });
                    Ok(())
                }
            }
        }

        fn collect(
            &mut self,
            completions: &mut Vec<IoCompletion<OsFile>>,
            milliseconds: u32,
        ) -> Result<()> {
            let mut wait = milliseconds;
            while self.in_port > 0 || self.at_workers > 0 {
                if self.dequeue(completions, wait)? < ENTRIES_PER_WAIT {
                    break;
                }
                wait = 0;
            }
            if let Some(workers) = &self.workers {
                self.at_workers -= workers.receive_finished(completions);
            }
            Ok(())
        }

        fn dequeue(
            &mut self,
            completions: &mut Vec<IoCompletion<OsFile>>,
            milliseconds: u32,
        ) -> Result<usize> {
            let mut entries = [OverlappedEntry {
                completion_key: 0,
                overlapped: ptr::null_mut(),
                internal: 0,
                transferred: 0,
            }; ENTRIES_PER_WAIT];
            let mut removed = 0_u32;
            // SAFETY: the entry array lives for the whole call and holds `ENTRIES_PER_WAIT`
            // entries, the count the call is given.
            let dequeued = unsafe {
                GetQueuedCompletionStatusEx(
                    self.port.0.as_raw_handle(),
                    entries.as_mut_ptr(),
                    ENTRIES_PER_WAIT as u32,
                    &raw mut removed,
                    milliseconds,
                    0,
                )
            };
            if dequeued == 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(WAIT_TIMEOUT) {
                    return Ok(0);
                }
                return Err(error.into());
            }
            let removed = (removed as usize).min(ENTRIES_PER_WAIT);
            for entry in &entries[..removed] {
                if entry.overlapped.is_null() {
                    continue;
                }
                // SAFETY: every packet with an overlapped structure on this port comes from an
                // operation `start` handed to the system, whose `Pending` begins with that
                // structure; the packet ends the system's use of it, so this is its only owner.
                let pending = unsafe { Box::from_raw(entry.overlapped.cast::<Pending>()) };
                self.in_port -= 1;
                completions.push(finish(*pending));
            }
            Ok(removed)
        }
    }

    impl Drop for Port {
        fn drop(&mut self) {
            self.inline.drain(..).for_each(|job| drop(job.run()));
            let mut finished = Vec::new();
            while self.in_port > 0 {
                let collected = self.collect(&mut finished, INFINITE);
                finished.clear();
                if collected.is_err() {
                    std::mem::forget(std::mem::take(&mut self.bound));
                    return;
                }
            }
            drop(self.workers.take());
            for file in self.bound.drain(..).filter_map(|file| file.upgrade()) {
                file.close_overlapped_handle(self.id);
            }
        }
    }

    fn goes_through_the_port(op: &IoOp<OsFile>) -> bool {
        matches!(op, IoOp::Read { file, .. } | IoOp::Write { file, .. } if file.is_uncached())
    }

    fn answered_without_the_kernel(op: &IoOp<OsFile>) -> Option<Result<usize>> {
        let refused = |kind| Some(Err(Error::new(kind)));
        match op {
            IoOp::Read { offset, .. } if *offset > LARGEST_OFFSET => {
                refused(ErrorKind::InvalidInput)
            }
            IoOp::Read { buffer, .. } if buffer.is_empty() => Some(Ok(0)),
            IoOp::Write { file, .. } if !file.is_writable() => refused(ErrorKind::PermissionDenied),
            IoOp::Write { buffer, .. } if buffer.is_empty() => Some(Ok(0)),
            IoOp::Write { offset, .. } if *offset > LARGEST_OFFSET => {
                refused(ErrorKind::InvalidInput)
            }
            _ => None,
        }
    }

    fn reopen_on(
        original: &fs::File,
        writable: bool,
        port: &PortHandle,
        key: u64,
    ) -> Result<OwnedHandle> {
        let access = if writable {
            GENERIC_READ | GENERIC_WRITE
        } else {
            GENERIC_READ
        };
        // SAFETY: `original` is an open file handle that outlives the call; the new handle is a
        // separate file object of the same file.
        let raw = unsafe {
            ReOpenFile(
                original.as_raw_handle(),
                access,
                FILE_SHARE_ALL,
                FILE_FLAG_OVERLAPPED | FILE_FLAG_NO_BUFFERING,
            )
        };
        if raw.is_null() || raw.addr() == usize::MAX {
            return Err(refusal(io::Error::last_os_error()));
        }
        // SAFETY: `ReOpenFile` returned a new handle that nothing else owns.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        // SAFETY: associates our own new handle with our own port; the key is only a number.
        let associated = unsafe {
            CreateIoCompletionPort(
                handle.as_raw_handle(),
                port.0.as_raw_handle(),
                key as usize,
                0,
            )
        };
        if associated.is_null() {
            return Err(refusal(io::Error::last_os_error()));
        }
        // SAFETY: sets the notification modes of our own handle before any I/O uses it.
        let set = unsafe {
            SetFileCompletionNotificationModes(
                handle.as_raw_handle(),
                FILE_SKIP_COMPLETION_PORT_ON_SUCCESS | FILE_SKIP_SET_EVENT_ON_HANDLE,
            )
        };
        if set == 0 {
            return Err(refusal(io::Error::last_os_error()));
        }
        Ok(handle)
    }

    fn refusal(error: io::Error) -> Error {
        let unsupported = matches!(
            error.raw_os_error(),
            Some(ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED | ERROR_INVALID_FUNCTION)
        );
        let error = Error::from(error);
        if unsupported {
            error.with_kind(ErrorKind::Unsupported)
        } else {
            error
        }
    }

    unsafe fn begin(pending: *mut Pending) -> io::Result<Started> {
        // SAFETY: the caller guarantees `pending` is live and ours.
        let handle = unsafe { (*pending).handle };
        // SAFETY: as above; the reference ends before the system is handed any pointer into it.
        let (buffer, len, writing) = match unsafe { &mut (*pending).op } {
            IoOp::Read { buffer, .. } => (
                buffer.as_mut_ptr().cast::<c_void>(),
                transfer_len(buffer.len()),
                false,
            ),
            IoOp::Write { buffer, .. } => (
                buffer.as_mut_ptr().cast::<c_void>(),
                transfer_len(buffer.len()),
                true,
            ),
            IoOp::SyncData { .. } | IoOp::SyncAll { .. } => {
                return Err(io::Error::from(io::ErrorKind::InvalidInput));
            }
        };
        // SAFETY: `pending` is live, so the address of its first field is valid.
        let overlapped = unsafe { &raw mut (*pending).overlapped };
        let done = if writing {
            // SAFETY: the buffer holds `len` bytes and, like the overlapped structure, stays in
            // place until the operation completes, as the caller guarantees.
            unsafe { WriteFile(handle, buffer, len, ptr::null_mut(), overlapped) }
        } else {
            // SAFETY: as above, for a read into the buffer.
            unsafe { ReadFile(handle, buffer, len, ptr::null_mut(), overlapped) }
        };
        if done != 0 {
            return Ok(Started::Finished);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_IO_PENDING) {
            Ok(Started::InFlight)
        } else {
            Err(error)
        }
    }

    fn finish(mut pending: Pending) -> IoCompletion<OsFile> {
        let mut transferred = 0_u32;
        // SAFETY: the operation has completed, so its overlapped structure is ours again and
        // the call only reads the result the system stored in it.
        let got = unsafe {
            GetOverlappedResult(
                pending.handle,
                &raw mut pending.overlapped,
                &raw mut transferred,
                0,
            )
        };
        let result = if got != 0 {
            Ok(transferred as usize)
        } else {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_HANDLE_EOF) {
                Ok(0)
            } else {
                Err(error.into())
            }
        };
        IoCompletion {
            user_data: pending.user_data,
            result,
            op: pending.op,
        }
    }

    fn transfer_len(len: usize) -> u32 {
        u32::try_from(len.min(MAX_TRANSFER)).unwrap_or(0)
    }
}

#[cfg(all(test, windows))]
mod tests {
    use crate::{
        AlignedBuffer, ErrorKind, File, FileSystem, IoBackend, IoCompletion, IoOp, IoQueue,
        IoQueueOptions, OpenMode, OsFile, OsFileSystem,
    };
    use std::path::PathBuf;
    use std::sync::Arc;

    const PAGE: usize = AlignedBuffer::ALIGNMENT;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("eggdb-iocp-{name}-{}", std::process::id()));
            let _ = std::fs::remove_file(&path);
            Self(path)
        }

        fn open(&self) -> Arc<OsFile> {
            Arc::new(
                OsFileSystem::new()
                    .open_uncached(&self.0, OpenMode::Create)
                    .unwrap(),
            )
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn port(threads: usize) -> crate::OsIoQueue {
        OsFileSystem::new()
            .io_queue(
                IoQueueOptions::new()
                    .backend(IoBackend::Iocp)
                    .threads(threads),
            )
            .unwrap()
    }

    fn page_of(byte: u8) -> AlignedBuffer {
        let mut page = AlignedBuffer::zeroed(PAGE).unwrap();
        page.fill(byte);
        page
    }

    fn reap_all(queue: &mut crate::OsIoQueue) -> Vec<IoCompletion<OsFile>> {
        let mut completions = Vec::new();
        let in_flight = queue.in_flight();
        queue.reap(&mut completions, in_flight).unwrap();
        completions.sort_by_key(|completion| completion.user_data);
        completions
    }

    #[test]
    fn many_uncached_pages_in_flight_land_where_they_were_written() {
        let scratch = Scratch::new("many");
        let file = scratch.open();
        let mut queue = port(2);
        for page in 0..64_u8 {
            let op = IoOp::Write {
                file: Arc::clone(&file),
                offset: u64::from(page) * PAGE as u64,
                buffer: page_of(page).into(),
            };
            assert!(queue.submit(op, u64::from(page)).is_ok());
        }
        for completion in reap_all(&mut queue) {
            assert_eq!(completion.result.unwrap(), PAGE);
        }
        for page in 0..64_u8 {
            let op = IoOp::Read {
                file: Arc::clone(&file),
                offset: u64::from(page) * PAGE as u64,
                buffer: AlignedBuffer::zeroed(PAGE).unwrap().into(),
            };
            assert!(queue.submit(op, u64::from(page)).is_ok());
        }
        for (page, completion) in (0..64_u8).zip(reap_all(&mut queue)) {
            assert_eq!(completion.result.unwrap(), PAGE);
            let buffer = completion.op.into_buffer().unwrap();
            assert!(buffer.iter().all(|&byte| byte == page));
        }
    }

    #[test]
    fn reads_at_the_end_of_the_file_transfer_nothing() {
        let scratch = Scratch::new("eof");
        let file = scratch.open();
        let mut queue = port(0);
        let op = IoOp::Read {
            file: Arc::clone(&file),
            offset: 16 * PAGE as u64,
            buffer: AlignedBuffer::zeroed(PAGE).unwrap().into(),
        };
        assert!(queue.submit(op, 0).is_ok());
        assert_eq!(reap_all(&mut queue)[0].result.as_ref().unwrap(), &0);
        let write = IoOp::Write {
            file: Arc::clone(&file),
            offset: 0,
            buffer: page_of(6).into(),
        };
        assert!(queue.submit(write, 1).is_ok());
        assert_eq!(reap_all(&mut queue)[0].result.as_ref().unwrap(), &PAGE);
    }

    #[test]
    fn a_refused_operation_leaves_no_packet_behind() {
        let scratch = Scratch::new("refused");
        let file = scratch.open();
        let mut queue = port(0);
        for round in 0..8 {
            let misaligned = IoOp::Write {
                file: Arc::clone(&file),
                offset: 1,
                buffer: page_of(1).into(),
            };
            assert!(queue.submit(misaligned, round).is_ok());
            let failed = reap_all(&mut queue).pop().unwrap();
            assert!(failed.result.is_err());
            let aligned = IoOp::Write {
                file: Arc::clone(&file),
                offset: 0,
                buffer: page_of(2).into(),
            };
            assert!(queue.submit(aligned, round).is_ok());
            assert_eq!(reap_all(&mut queue)[0].result.as_ref().unwrap(), &PAGE);
        }
    }

    #[test]
    fn offsets_the_system_would_read_as_special_positions_are_refused() {
        let scratch = Scratch::new("offsets");
        let file = scratch.open();
        let mut queue = port(0);
        for offset in [u64::MAX, u64::MAX - 1, 1 << 63] {
            let write = IoOp::Write {
                file: Arc::clone(&file),
                offset,
                buffer: page_of(3).into(),
            };
            assert!(queue.submit(write, 0).is_ok());
            let completion = reap_all(&mut queue).pop().unwrap();
            assert_eq!(
                completion.result.unwrap_err().kind(),
                ErrorKind::InvalidInput
            );
        }
        assert_eq!(file.size().unwrap(), 0);
    }

    #[test]
    fn each_queue_binds_its_own_handle_and_closes_it_when_dropped() {
        let scratch = Scratch::new("bind");
        let file = scratch.open();
        let mut first = port(0);
        let mut second = port(0);
        for (queue, byte) in [(&mut first, 4_u8), (&mut second, 5)] {
            let op = IoOp::Write {
                file: Arc::clone(&file),
                offset: 0,
                buffer: page_of(byte).into(),
            };
            assert!(queue.submit(op, 0).is_ok());
            assert_eq!(reap_all(queue)[0].result.as_ref().unwrap(), &PAGE);
        }
        assert_eq!(file.overlapped_handle_count(), 2);
        drop(first);
        assert_eq!(file.overlapped_handle_count(), 1);
        drop(second);
        assert_eq!(file.overlapped_handle_count(), 0);
    }

    #[test]
    fn bursts_of_cached_operations_larger_than_one_batch_all_complete() {
        let path = std::env::temp_dir().join(format!("eggdb-iocp-burst-{}", std::process::id()));
        let fs = OsFileSystem::new();
        let file = Arc::new(fs.open(&path, OpenMode::Create).unwrap());
        let mut queue = fs
            .io_queue(
                IoQueueOptions::new()
                    .backend(IoBackend::Iocp)
                    .capacity(512)
                    .threads(4),
            )
            .unwrap();
        for _ in 0..4 {
            for tag in 0..300_u64 {
                let op = IoOp::Write {
                    file: Arc::clone(&file),
                    offset: tag,
                    buffer: vec![1].into(),
                };
                assert!(queue.submit(op, tag).is_ok());
            }
            let completions = reap_all(&mut queue);
            assert_eq!(completions.len(), 300);
            assert!(completions.iter().all(|done| done.result.is_ok()));
        }
        drop(queue);
        drop(file);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn dropping_a_port_waits_for_uncached_writes_in_flight() {
        let scratch = Scratch::new("drop");
        let file = scratch.open();
        let mut queue = port(1);
        for page in 0..32_u8 {
            let op = IoOp::Write {
                file: Arc::clone(&file),
                offset: u64::from(page) * PAGE as u64,
                buffer: page_of(page).into(),
            };
            assert!(queue.submit(op, 0).is_ok());
        }
        drop(queue);
        let mut page = AlignedBuffer::zeroed(PAGE).unwrap();
        file.read_exact_at(&mut page, 31 * PAGE as u64).unwrap();
        assert!(page.iter().all(|&byte| byte == 31));
    }
}

#[cfg(not(windows))]
mod unsupported {
    use crate::error::{Error, ErrorKind, Result};
    use crate::io::{IoCompletion, RejectedOp};
    use crate::os_fs::OsFile;
    use crate::os_io::Job;
    use std::convert::Infallible;

    #[derive(Debug)]
    pub(crate) struct Port(Infallible);

    impl Port {
        pub(crate) fn new(_threads: usize) -> Result<Self> {
            Err(Error::new(ErrorKind::Unsupported))
        }

        pub(crate) fn submit(&mut self, _job: Job) -> core::result::Result<(), RejectedOp<OsFile>> {
            match self.0 {}
        }

        pub(crate) fn reap(
            &mut self,
            _completions: &mut Vec<IoCompletion<OsFile>>,
            _min: usize,
        ) -> Result<()> {
            match self.0 {}
        }
    }
}
