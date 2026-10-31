#[cfg(all(test, target_os = "linux"))]
thread_local! {
    pub(crate) static REFUSE_SETUP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(all(
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
))]
pub(crate) use ring::*;

#[cfg(not(all(
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
)))]
pub(crate) use unsupported::*;

#[cfg(all(
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
))]
mod ring {
    use crate::error::{Error, ErrorKind, Result};
    use crate::io::{IoCompletion, IoOp, RejectedOp};
    use crate::os_fs::OsFile;
    use std::collections::VecDeque;
    use std::ffi::{c_int, c_long, c_uint, c_void};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::ptr::{self, NonNull};
    use std::sync::atomic::{AtomicU32, Ordering};

    mod net;

    pub(crate) use net::NetRing;

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) const BUILT: bool = true;

    const SYS_IO_URING_SETUP: c_long = 425;
    const SYS_IO_URING_ENTER: c_long = 426;

    const IORING_FEAT_SINGLE_MMAP: u32 = 1 << 0;
    const IORING_FEAT_RW_CUR_POS: u32 = 1 << 3;
    const IORING_ENTER_GETEVENTS: c_uint = 1 << 0;
    const IORING_OFF_SQES: i64 = 0x1000_0000;
    const IORING_OP_FSYNC: u8 = 3;
    const IORING_OP_READ: u8 = 22;
    const IORING_OP_WRITE: u8 = 23;
    const IORING_FSYNC_DATASYNC: u32 = 1 << 0;

    const PROT_READ: c_int = 1;
    const PROT_WRITE: c_int = 2;
    const MAP_SHARED: c_int = 1;
    const MAP_FAILED: *mut c_void = usize::MAX as *mut c_void;
    const EINTR: i32 = 4;

    const MAX_TRANSFER: usize = 0x7fff_f000;
    const LARGEST_OFFSET: u64 = i64::MAX as u64;

    unsafe extern "C" {
        fn syscall(number: c_long, ...) -> c_long;
        #[cfg_attr(
            not(any(target_env = "musl", target_env = "ohos")),
            link_name = "mmap64"
        )]
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            offset: i64,
        ) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> c_int;
    }

    #[repr(C)]
    #[derive(Debug, Default)]
    struct SqRingOffsets {
        head: u32,
        tail: u32,
        ring_mask: u32,
        ring_entries: u32,
        flags: u32,
        dropped: u32,
        array: u32,
        resv1: u32,
        user_addr: u64,
    }

    #[repr(C)]
    #[derive(Debug, Default)]
    struct CqRingOffsets {
        head: u32,
        tail: u32,
        ring_mask: u32,
        ring_entries: u32,
        overflow: u32,
        cqes: u32,
        flags: u32,
        resv1: u32,
        user_addr: u64,
    }

    #[repr(C)]
    #[derive(Debug, Default)]
    struct Params {
        sq_entries: u32,
        cq_entries: u32,
        flags: u32,
        sq_thread_cpu: u32,
        sq_thread_idle: u32,
        features: u32,
        wq_fd: u32,
        resv: [u32; 3],
        sq_off: SqRingOffsets,
        cq_off: CqRingOffsets,
    }

    #[repr(C)]
    #[derive(Debug, Default)]
    struct Submission {
        opcode: u8,
        flags: u8,
        ioprio: u16,
        fd: i32,
        off: u64,
        addr: u64,
        len: u32,
        op_flags: u32,
        user_data: u64,
        buf_index: u16,
        personality: u16,
        splice_fd_in: i32,
        addr3: u64,
        pad: u64,
    }

    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    struct Completion {
        user_data: u64,
        res: i32,
        flags: u32,
    }

    const _: () = assert!(size_of::<Params>() == 120);
    const _: () = assert!(size_of::<Submission>() == 64);
    const _: () = assert!(size_of::<Completion>() == 16);

    #[derive(Debug)]
    struct Mapping {
        ptr: NonNull<c_void>,
        len: usize,
    }

    impl Mapping {
        fn new(fd: &OwnedFd, len: usize, offset: i64) -> Result<Self> {
            // SAFETY: a fresh shared mapping of the ring file descriptor at an address the kernel
            // picks; nothing in this process is overwritten.
            let ptr = unsafe {
                mmap(
                    ptr::null_mut(),
                    len,
                    PROT_READ | PROT_WRITE,
                    MAP_SHARED,
                    fd.as_raw_fd(),
                    offset,
                )
            };
            if ptr == MAP_FAILED {
                return Err(io::Error::last_os_error().into());
            }
            let ptr = NonNull::new(ptr).ok_or_else(|| Error::new(ErrorKind::Other))?;
            Ok(Self { ptr, len })
        }

        fn at<T>(&self, offset: u32) -> *mut T {
            self.ptr
                .as_ptr()
                .cast::<u8>()
                .wrapping_add(offset as usize)
                .cast()
        }
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: `ptr` and `len` describe a mapping made by `mmap` and unmapped only here.
            unsafe { munmap(self.ptr.as_ptr(), self.len) };
        }
    }

    #[derive(Debug)]
    struct RawRing {
        sq_head: *const AtomicU32,
        sq_tail: *const AtomicU32,
        sq_mask: u32,
        sq_array: *mut u32,
        cq_head: *const AtomicU32,
        cq_tail: *const AtomicU32,
        cq_mask: u32,
        cqes: *const Completion,
        sqes: *mut Submission,
        submissions: Option<Mapping>,
        rings: Option<Mapping>,
        fd: OwnedFd,
    }

    // SAFETY: the raw pointers point into mappings the ring owns, and only the ring's single owner
    // uses them, so moving the ring to another thread is sound.
    unsafe impl Send for RawRing {}

    impl RawRing {
        fn new(capacity: usize, needed_features: u32) -> Result<Self> {
            let entries = u32::try_from(capacity.next_power_of_two())
                .map_err(|_| Error::new(ErrorKind::InvalidInput))?;
            #[cfg(all(test, target_os = "linux"))]
            if super::REFUSE_SETUP.with(std::cell::Cell::get) {
                return Err(Error::new(ErrorKind::Unsupported));
            }
            let mut params = Params::default();
            // SAFETY: io_uring_setup reads the entry count and fills `params`, a properly sized and
            // aligned `struct io_uring_params` that lives for the whole call.
            let fd = unsafe {
                syscall(
                    SYS_IO_URING_SETUP,
                    entries as c_long,
                    ptr::from_mut(&mut params),
                )
            };
            let fd = c_int::try_from(fd).map_err(|_| Error::new(ErrorKind::Other))?;
            if fd < 0 {
                let error = Error::from(io::Error::last_os_error());
                let kind = match error.kind() {
                    ErrorKind::OutOfMemory => ErrorKind::OutOfMemory,
                    _ => ErrorKind::Unsupported,
                };
                return Err(error.with_kind(kind));
            }
            // SAFETY: io_uring_setup returned a new file descriptor that nothing else owns.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            let needed = IORING_FEAT_SINGLE_MMAP | needed_features;
            if params.features & needed != needed {
                return Err(Error::new(ErrorKind::Unsupported));
            }
            let sq_len =
                params.sq_off.array as usize + params.sq_entries as usize * size_of::<u32>();
            let cq_len =
                params.cq_off.cqes as usize + params.cq_entries as usize * size_of::<Completion>();
            let rings = Mapping::new(&fd, sq_len.max(cq_len), 0)?;
            let submissions = Mapping::new(
                &fd,
                params.sq_entries as usize * size_of::<Submission>(),
                IORING_OFF_SQES,
            )?;
            // SAFETY: the kernel placed the ring mask and the ring's entry count at these offsets
            // of the mapping, which stays mapped while the ring exists.
            let sq_mask = unsafe { *rings.at::<u32>(params.sq_off.ring_mask) };
            // SAFETY: as above, for the completion ring's mask.
            let cq_mask = unsafe { *rings.at::<u32>(params.cq_off.ring_mask) };
            Ok(Self {
                sq_head: rings.at(params.sq_off.head),
                sq_tail: rings.at(params.sq_off.tail),
                sq_mask,
                sq_array: rings.at(params.sq_off.array),
                cq_head: rings.at(params.cq_off.head),
                cq_tail: rings.at(params.cq_off.tail),
                cq_mask,
                cqes: rings.at(params.cq_off.cqes),
                sqes: submissions.at(0),
                submissions: Some(submissions),
                rings: Some(rings),
                fd,
            })
        }

        fn tail(&self) -> u32 {
            // SAFETY: the tail lies in the ring mapping, which lives as long as `self`; we are
            // its only writer, so a relaxed load reads our own last store.
            unsafe { &*self.sq_tail }.load(Ordering::Relaxed)
        }

        fn unsubmitted(&self) -> u32 {
            // SAFETY: the head lies in the ring mapping; the kernel moves it as it consumes entries.
            let head = unsafe { &*self.sq_head }.load(Ordering::Acquire);
            self.tail().wrapping_sub(head)
        }

        fn is_full(&self) -> bool {
            self.unsubmitted() > self.sq_mask
        }

        fn push(&mut self, submission: Submission) -> u32 {
            let tail = self.tail();
            let index = tail & self.sq_mask;
            // SAFETY: `index` is masked to the ring's size, so the entry lies inside the mapping of
            // submission entries, which lives as long as `self` and which the kernel reads only
            // after the tail moves past the entry.
            unsafe { self.sqes.wrapping_add(index as usize).write(submission) };
            // SAFETY: the index array has one `u32` per entry, and `index` is masked to it.
            unsafe { self.sq_array.wrapping_add(index as usize).write(index) };
            // SAFETY: the tail lies in the ring mapping; the release store publishes the entry.
            unsafe { &*self.sq_tail }.store(tail.wrapping_add(1), Ordering::Release);
            tail
        }

        fn withdraw(&mut self, tail: u32) {
            // SAFETY: the tail lies in the ring mapping, which lives as long as `self`. The
            // kernel did not consume the entry and reads the ring only inside io_uring_enter, so
            // moving the tail back withdraws it.
            unsafe { &*self.sq_tail }.store(tail, Ordering::Release);
        }

        fn consumed_since(&self, tail: u32) -> bool {
            // SAFETY: the head lies in the ring mapping; the kernel moves it as it consumes entries.
            unsafe { &*self.sq_head }.load(Ordering::Acquire) != tail
        }

        fn take_completions(&mut self, taken: &mut Vec<Completion>) {
            // SAFETY: we are the completion ring head's only writer.
            let mut head = unsafe { &*self.cq_head }.load(Ordering::Relaxed);
            // SAFETY: the acquire load of the kernel's tail makes the entries before it visible.
            let tail = unsafe { &*self.cq_tail }.load(Ordering::Acquire);
            while head != tail {
                // SAFETY: `head` is masked to the completion ring's size and lies before the tail,
                // so the kernel has written this entry and will not touch it until the head moves.
                let completion = unsafe {
                    self.cqes
                        .wrapping_add((head & self.cq_mask) as usize)
                        .read()
                };
                taken.push(completion);
                head = head.wrapping_add(1);
            }
            // SAFETY: the release store hands the consumed entries back to the kernel.
            unsafe { &*self.cq_head }.store(head, Ordering::Release);
        }

        fn enter(&self, to_submit: c_uint, min_complete: c_uint, flags: c_uint) -> Result<c_uint> {
            loop {
                match self.enter_once(to_submit, min_complete, flags, ptr::null(), 0) {
                    Err(error) if error.raw_os_error() == Some(EINTR) => {}
                    entered => return entered.map_err(Error::from),
                }
            }
        }

        fn enter_once(
            &self,
            to_submit: c_uint,
            min_complete: c_uint,
            flags: c_uint,
            argument: *const c_void,
            argument_size: usize,
        ) -> io::Result<c_uint> {
            // SAFETY: io_uring_enter on our own ring descriptor; `argument` is null or points to
            // `argument_size` bytes that live for the whole call.
            let entered = unsafe {
                syscall(
                    SYS_IO_URING_ENTER,
                    c_long::from(self.fd.as_raw_fd()),
                    to_submit as c_long,
                    min_complete as c_long,
                    flags as c_long,
                    argument,
                    argument_size,
                )
            };
            if entered >= 0 {
                Ok(c_uint::try_from(entered).unwrap_or(c_uint::MAX))
            } else {
                Err(io::Error::last_os_error())
            }
        }

        fn leak(&mut self) {
            std::mem::forget(self.rings.take());
            std::mem::forget(self.submissions.take());
        }
    }

    #[derive(Debug)]
    struct InFlight {
        op: IoOp<OsFile>,
        user_data: u64,
    }

    /// One io_uring instance, used by one queue at a time.
    #[derive(Debug)]
    pub(crate) struct Ring {
        raw: RawRing,
        slots: Vec<Option<InFlight>>,
        free_slots: Vec<usize>,
        in_kernel: usize,
        ready: VecDeque<IoCompletion<OsFile>>,
        taken: Vec<Completion>,
    }

    impl Ring {
        pub(crate) fn new(capacity: usize) -> Result<Self> {
            Ok(Self {
                raw: RawRing::new(capacity, IORING_FEAT_RW_CUR_POS)?,
                slots: Vec::new(),
                free_slots: Vec::new(),
                in_kernel: 0,
                ready: VecDeque::new(),
                taken: Vec::new(),
            })
        }

        pub(crate) fn submit(
            &mut self,
            op: IoOp<OsFile>,
            user_data: u64,
        ) -> core::result::Result<(), RejectedOp<OsFile>> {
            if let Some(result) = answered_without_the_kernel(&op) {
                self.ready.push_back(IoCompletion {
                    user_data,
                    result,
                    op,
                });
                return Ok(());
            }
            let mut op = op;
            let slot = self.free_slots.pop().unwrap_or(self.slots.len());
            if slot == self.slots.len() {
                self.slots.push(None);
            }
            let tail = self.raw.push(submission_for(&mut op, slot as u64));
            let entered = self.raw.enter(1, 0, 0);
            if self.raw.consumed_since(tail) {
                self.slots[slot] = Some(InFlight { op, user_data });
                self.in_kernel += 1;
                return Ok(());
            }
            self.raw.withdraw(tail);
            self.free_slots.push(slot);
            let error = entered
                .err()
                .unwrap_or_else(|| Error::new(ErrorKind::Other));
            Err(RejectedOp { error, op })
        }

        pub(crate) fn reap(
            &mut self,
            completions: &mut Vec<IoCompletion<OsFile>>,
            min: usize,
        ) -> Result<()> {
            let before = completions.len();
            completions.extend(self.ready.drain(..));
            self.drain(completions);
            while completions.len() - before < min {
                let wanted = min - (completions.len() - before);
                let wanted = c_uint::try_from(wanted).unwrap_or(c_uint::MAX);
                self.raw.enter(0, wanted, IORING_ENTER_GETEVENTS)?;
                self.drain(completions);
            }
            Ok(())
        }

        fn drain(&mut self, completions: &mut Vec<IoCompletion<OsFile>>) {
            let mut taken = std::mem::take(&mut self.taken);
            self.raw.take_completions(&mut taken);
            for completion in taken.drain(..) {
                let finished = usize::try_from(completion.user_data)
                    .ok()
                    .and_then(|slot| Some((slot, self.slots.get_mut(slot)?.take()?)));
                if let Some((slot, in_flight)) = finished {
                    self.free_slots.push(slot);
                    self.in_kernel -= 1;
                    completions.push(IoCompletion {
                        user_data: in_flight.user_data,
                        result: outcome(completion.res),
                        op: in_flight.op,
                    });
                }
            }
            self.taken = taken;
        }
    }

    impl Drop for Ring {
        fn drop(&mut self) {
            let mut finished = Vec::new();
            while self.in_kernel > 0 {
                self.drain(&mut finished);
                finished.clear();
                if self.in_kernel > 0 && self.raw.enter(0, 1, IORING_ENTER_GETEVENTS).is_err() {
                    std::mem::forget(std::mem::take(&mut self.slots));
                    self.raw.leak();
                    return;
                }
            }
        }
    }

    fn answered_without_the_kernel(op: &IoOp<OsFile>) -> Option<Result<usize>> {
        let refused = |kind| Some(Err(Error::new(kind)));
        match op {
            IoOp::Read { offset, .. } if *offset > LARGEST_OFFSET => {
                refused(ErrorKind::InvalidInput)
            }
            IoOp::Read { .. } => None,
            IoOp::Write { file, .. } | IoOp::SyncData { file } | IoOp::SyncAll { file }
                if !file.is_writable() =>
            {
                refused(ErrorKind::PermissionDenied)
            }
            IoOp::Write { buffer, .. } if buffer.is_empty() => Some(Ok(0)),
            IoOp::Write { offset, .. } if *offset > LARGEST_OFFSET => {
                refused(ErrorKind::InvalidInput)
            }
            IoOp::Write { .. } | IoOp::SyncData { .. } | IoOp::SyncAll { .. } => None,
        }
    }

    fn submission_for(op: &mut IoOp<OsFile>, user_data: u64) -> Submission {
        let mut submission = Submission {
            fd: op.file().as_raw_fd(),
            user_data,
            ..Submission::default()
        };
        match op {
            IoOp::Read { offset, buffer, .. } => {
                submission.opcode = IORING_OP_READ;
                submission.off = *offset;
                submission.addr = buffer.as_mut_ptr().addr() as u64;
                submission.len = transfer_len(buffer.len());
            }
            IoOp::Write { offset, buffer, .. } => {
                submission.opcode = IORING_OP_WRITE;
                submission.off = *offset;
                submission.addr = buffer.as_ptr().addr() as u64;
                submission.len = transfer_len(buffer.len());
            }
            IoOp::SyncData { .. } => {
                submission.opcode = IORING_OP_FSYNC;
                submission.op_flags = IORING_FSYNC_DATASYNC;
            }
            IoOp::SyncAll { .. } => submission.opcode = IORING_OP_FSYNC,
        }
        submission
    }

    fn transfer_len(len: usize) -> u32 {
        u32::try_from(len.min(MAX_TRANSFER)).unwrap_or(0)
    }

    fn outcome(res: i32) -> Result<usize> {
        match usize::try_from(res) {
            Ok(transferred) => Ok(transferred),
            Err(_) => Err(io::Error::from_raw_os_error(res.saturating_neg()).into()),
        }
    }
}

#[cfg(not(all(
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
)))]
mod unsupported {
    use crate::error::{Error, ErrorKind, Result};
    use crate::io::{IoCompletion, IoOp, RejectedOp};
    use crate::net::NetCompletion;
    use crate::net::NetOp;
    use crate::os_fs::OsFile;
    use crate::os_net::{OsListener, OsStream};
    use std::convert::Infallible;
    use std::time::Instant;

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) const BUILT: bool = false;

    #[derive(Debug)]
    pub(crate) struct Ring(Infallible);

    #[derive(Debug)]
    pub(crate) struct NetRing(Infallible);

    impl NetRing {
        pub(crate) fn new(_capacity: usize) -> Result<Self> {
            Err(Error::new(ErrorKind::Unsupported))
        }

        pub(crate) fn submit(
            &mut self,
            _op: NetOp<OsListener, OsStream>,
            _user_data: u64,
        ) {
            match self.0 {}
        }

        pub(crate) fn reap(
            &mut self,
            _completions: &mut Vec<NetCompletion<OsListener, OsStream>>,
            _min: usize,
            _deadline: Option<Instant>,
        ) -> Result<()> {
            match self.0 {}
        }

        pub(crate) fn cancel(&mut self, _user_data: u64) -> bool {
            match self.0 {}
        }
    }

    impl Ring {
        pub(crate) fn new(_capacity: usize) -> Result<Self> {
            Err(Error::new(ErrorKind::Unsupported))
        }

        pub(crate) fn submit(
            &mut self,
            _op: IoOp<OsFile>,
            _user_data: u64,
        ) -> core::result::Result<(), RejectedOp<OsFile>> {
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
