use super::{OwnerOnlyAcl, c_void};
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
const PIPE_WAIT: u32 = 0x0000_0000;
const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x0000_0008;
const PIPE_UNLIMITED_INSTANCES: u32 = 255;
const PIPE_BUFFER_BYTES: u32 = 64 * 1024;
const INFINITE: u32 = u32::MAX;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 0x0000_0102;
const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
const SECURITY_DESCRIPTOR_WORDS: usize = 8;
const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_BROKEN_PIPE: i32 = 109;
const ERROR_NO_DATA: i32 = 232;
const ERROR_PIPE_NOT_CONNECTED: i32 = 233;
const ERROR_PIPE_CONNECTED: i32 = 535;
const ERROR_OPERATION_ABORTED: i32 = 995;
const ERROR_IO_PENDING: i32 = 997;
const MAX_PIPE_NAME_UNITS: usize = 256;

#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: RawHandle,
}

impl Overlapped {
    fn signalled_by(event: &OwnedHandle) -> Self {
        Self {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            event: event.as_raw_handle(),
        }
    }
}

#[repr(C)]
struct SecurityAttributes {
    length: u32,
    descriptor: *mut c_void,
    inherit: i32,
}

mod sys {
    use super::{Overlapped, RawHandle, SecurityAttributes, c_void};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub(super) fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            max_instances: u32,
            out_buffer: u32,
            in_buffer: u32,
            default_timeout: u32,
            security: *const SecurityAttributes,
        ) -> RawHandle;
        pub(super) fn ConnectNamedPipe(pipe: RawHandle, overlapped: *mut Overlapped) -> i32;
        pub(super) fn DisconnectNamedPipe(pipe: RawHandle) -> i32;
        pub(super) fn CreateEventW(
            attributes: *const c_void,
            manual_reset: i32,
            initial_state: i32,
            name: *const u16,
        ) -> RawHandle;
        pub(super) fn ReadFile(
            file: RawHandle,
            buffer: *mut u8,
            len: u32,
            read: *mut u32,
            overlapped: *mut Overlapped,
        ) -> i32;
        pub(super) fn WriteFile(
            file: RawHandle,
            buffer: *const u8,
            len: u32,
            written: *mut u32,
            overlapped: *mut Overlapped,
        ) -> i32;
        pub(super) fn GetOverlappedResult(
            file: RawHandle,
            overlapped: *mut Overlapped,
            transferred: *mut u32,
            wait: i32,
        ) -> i32;
        pub(super) fn WaitForSingleObject(handle: RawHandle, milliseconds: u32) -> u32;
        pub(super) fn CancelIoEx(file: RawHandle, overlapped: *mut Overlapped) -> i32;
        pub(super) fn PeekNamedPipe(
            pipe: RawHandle,
            buffer: *mut c_void,
            len: u32,
            read: *mut u32,
            available: *mut u32,
            left_in_message: *mut u32,
        ) -> i32;
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        pub(super) fn InitializeSecurityDescriptor(descriptor: *mut c_void, revision: u32) -> i32;
        pub(super) fn SetSecurityDescriptorDacl(
            descriptor: *mut c_void,
            present: i32,
            dacl: *mut c_void,
            defaulted: i32,
        ) -> i32;
    }
}

pub(super) fn pipe_path(name: &str) -> io::Result<Vec<u16>> {
    let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidInput, message.to_owned());
    if name.is_empty() {
        return Err(invalid("a named pipe needs a name"));
    }
    if name.contains('\0') {
        return Err(invalid("a named pipe's name cannot hold a NUL character"));
    }
    let mut path: Vec<u16> = r"\\.\pipe\".encode_utf16().chain(name.encode_utf16()).collect();
    if path.len() >= MAX_PIPE_NAME_UNITS {
        return Err(invalid("a named pipe's name is limited to 256 characters"));
    }
    path.push(0);
    Ok(path)
}

fn new_event() -> io::Result<OwnedHandle> {
    // SAFETY: null attributes and name ask for an unnamed, manual-reset, unsignalled event with
    // default security; the result is checked before use.
    let event = unsafe { sys::CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if event.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `event` is a handle CreateEventW just returned, owned by nothing else.
    Ok(unsafe { OwnedHandle::from_raw_handle(event) })
}

fn create_instance(path: &[u16], first: bool) -> io::Result<OwnedHandle> {
    let mut acl = OwnerOnlyAcl::for_current_user()?;
    let mut descriptor = [0u64; SECURITY_DESCRIPTOR_WORDS];
    let descriptor_pointer: *mut c_void = descriptor.as_mut_ptr().cast();
    // SAFETY: `descriptor` is 8-byte aligned writable memory larger than
    // SECURITY_DESCRIPTOR_MIN_LENGTH.
    let initialised =
        unsafe { sys::InitializeSecurityDescriptor(descriptor_pointer, SECURITY_DESCRIPTOR_REVISION) };
    if initialised == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `descriptor` was initialised above and `acl` holds a valid ACL that outlives every
    // use of the descriptor in this function.
    let set = unsafe { sys::SetSecurityDescriptorDacl(descriptor_pointer, 1, acl.as_mut_ptr(), 0) };
    if set == 0 {
        return Err(io::Error::last_os_error());
    }
    let attributes = SecurityAttributes {
        length: size_of::<SecurityAttributes>() as u32,
        descriptor: descriptor_pointer,
        inherit: 0,
    };
    let open_mode = PIPE_ACCESS_DUPLEX
        | FILE_FLAG_OVERLAPPED
        | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
    // SAFETY: `path` is NUL-terminated UTF-16 and `attributes` points at the descriptor and ACL
    // above, all alive for the whole call.
    let pipe = unsafe {
        sys::CreateNamedPipeW(
            path.as_ptr(),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            PIPE_BUFFER_BYTES,
            PIPE_BUFFER_BYTES,
            0,
            &raw const attributes,
        )
    };
    if pipe as isize == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `pipe` is a handle CreateNamedPipeW just returned, owned by nothing else.
    Ok(unsafe { OwnedHandle::from_raw_handle(pipe) })
}

struct PendingConnect {
    pipe: OwnedHandle,
    _event: OwnedHandle,
    overlapped: Box<Overlapped>,
    connected: bool,
}

impl PendingConnect {
    fn start(pipe: OwnedHandle) -> io::Result<Self> {
        let event = new_event()?;
        let mut overlapped = Box::new(Overlapped::signalled_by(&event));
        // SAFETY: `pipe` is a fresh overlapped pipe instance and `overlapped` is boxed, so it
        // stays at one address until the connect completes or is cancelled in `drop`.
        let started = unsafe { sys::ConnectNamedPipe(pipe.as_raw_handle(), &raw mut *overlapped) };
        let connected = if started != 0 {
            true
        } else {
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(ERROR_PIPE_CONNECTED) => true,
                Some(ERROR_IO_PENDING) => false,
                _ => return Err(error),
            }
        };
        Ok(Self {
            pipe,
            _event: event,
            overlapped,
            connected,
        })
    }

    fn finish(&mut self, wait_milliseconds: u32) -> io::Result<()> {
        if self.connected {
            return Ok(());
        }
        // SAFETY: the event is open for as long as `self` lives.
        let waited = unsafe { sys::WaitForSingleObject(self.overlapped.event, wait_milliseconds) };
        match waited {
            WAIT_OBJECT_0 => {}
            WAIT_TIMEOUT => return Err(io::ErrorKind::WouldBlock.into()),
            _ => return Err(io::Error::last_os_error()),
        }
        let mut transferred = 0u32;
        // SAFETY: the connect on `pipe` used `overlapped`, whose event is signalled, so the
        // operation is complete and its result can be read without waiting.
        let done = unsafe {
            sys::GetOverlappedResult(
                self.pipe.as_raw_handle(),
                &raw mut *self.overlapped,
                &raw mut transferred,
                0,
            )
        };
        self.connected = true;
        if done == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for PendingConnect {
    fn drop(&mut self) {
        if self.connected {
            return;
        }
        let mut transferred = 0u32;
        // SAFETY: `pipe` and the boxed `overlapped` of its pending connect are both alive.
        unsafe { sys::CancelIoEx(self.pipe.as_raw_handle(), &raw mut *self.overlapped) };
        // SAFETY: waiting for the cancelled connect to complete keeps `overlapped` alive until
        // the system no longer writes to it.
        unsafe {
            sys::GetOverlappedResult(
                self.pipe.as_raw_handle(),
                &raw mut *self.overlapped,
                &raw mut transferred,
                1,
            )
        };
    }
}

// SAFETY: the raw event handle inside the boxed OVERLAPPED is owned by the same PendingConnect
// and is only used under the listener's mutex, so moving it to another thread is sound.
unsafe impl Send for PendingConnect {}

pub(super) struct PipeListener {
    path: Vec<u16>,
    pending: Mutex<Option<PendingConnect>>,
    nonblocking: AtomicBool,
}

impl std::fmt::Debug for PipeListener {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PipeListener")
            .field("path", &String::from_utf16_lossy(&self.path))
            .finish_non_exhaustive()
    }
}

pub(super) enum PipeBindError {
    InUse,
    Io(io::Error),
}

impl PipeListener {
    pub(super) fn bind(name: &str) -> Result<Self, PipeBindError> {
        let path = pipe_path(name).map_err(PipeBindError::Io)?;
        let first = create_instance(&path, true).map_err(|error| {
            if error.raw_os_error() == Some(ERROR_ACCESS_DENIED) {
                PipeBindError::InUse
            } else {
                PipeBindError::Io(error)
            }
        })?;
        let pending = PendingConnect::start(first).map_err(PipeBindError::Io)?;
        Ok(Self {
            path,
            pending: Mutex::new(Some(pending)),
            nonblocking: AtomicBool::new(false),
        })
    }

    pub(super) fn set_nonblocking(&self, nonblocking: bool) {
        self.nonblocking.store(nonblocking, Ordering::Release);
    }

    pub(super) fn accept(&self) -> io::Result<PipeStream> {
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| io::Error::other("the named pipe listener's lock is poisoned"))?;
        if pending.is_none() {
            *pending = Some(PendingConnect::start(create_instance(&self.path, false)?)?);
        }
        let wait = if self.nonblocking.load(Ordering::Acquire) {
            0
        } else {
            INFINITE
        };
        let Some(waiting) = pending.as_mut() else {
            return Err(io::ErrorKind::WouldBlock.into());
        };
        if let Err(error) = waiting.finish(wait) {
            if error.kind() != io::ErrorKind::WouldBlock {
                *pending = None;
            }
            return Err(error);
        }
        let Some(connected) = pending.take() else {
            return Err(io::ErrorKind::WouldBlock.into());
        };
        *pending = create_instance(&self.path, false)
            .and_then(PendingConnect::start)
            .ok();
        let pipe = connected.pipe.try_clone()?;
        drop(connected);
        Ok(PipeStream::new(pipe, self.nonblocking.load(Ordering::Acquire)))
    }
}

#[derive(Debug)]
pub(super) struct PipeStream {
    pipe: OwnedHandle,
    read_timeout: AtomicU32,
    write_timeout: AtomicU32,
    nonblocking: AtomicBool,
}

enum Direction<'a> {
    Read(&'a mut [u8]),
    Write(&'a [u8]),
}

impl PipeStream {
    fn new(pipe: OwnedHandle, nonblocking: bool) -> Self {
        Self {
            pipe,
            read_timeout: AtomicU32::new(INFINITE),
            write_timeout: AtomicU32::new(INFINITE),
            nonblocking: AtomicBool::new(nonblocking),
        }
    }

    pub(super) fn set_read_timeout(&self, milliseconds: u32) {
        self.read_timeout.store(
            if milliseconds == 0 { INFINITE } else { milliseconds },
            Ordering::Release,
        );
    }

    pub(super) fn set_write_timeout(&self, milliseconds: u32) {
        self.write_timeout.store(
            if milliseconds == 0 { INFINITE } else { milliseconds },
            Ordering::Release,
        );
    }

    pub(super) fn set_nonblocking(&self, nonblocking: bool) {
        self.nonblocking.store(nonblocking, Ordering::Release);
    }

    pub(super) fn disconnect(&self) -> io::Result<()> {
        // SAFETY: `pipe` is an open server end of a named pipe.
        let done = unsafe { sys::DisconnectNamedPipe(self.pipe.as_raw_handle()) };
        if done == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_PIPE_NOT_CONNECTED) {
                return Err(error);
            }
        }
        Ok(())
    }

    pub(super) fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            pipe: self.pipe.try_clone()?,
            read_timeout: AtomicU32::new(self.read_timeout.load(Ordering::Acquire)),
            write_timeout: AtomicU32::new(self.write_timeout.load(Ordering::Acquire)),
            nonblocking: AtomicBool::new(self.nonblocking.load(Ordering::Acquire)),
        })
    }

    pub(super) fn bytes_waiting(&self) -> io::Result<usize> {
        let mut available = 0u32;
        // SAFETY: `pipe` is an open pipe handle; a null buffer with length 0 asks only for the
        // count, written to `available`, and the other outputs may be null.
        let done = unsafe {
            sys::PeekNamedPipe(
                self.pipe.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &raw mut available,
                std::ptr::null_mut(),
            )
        };
        if done == 0 {
            let error = io::Error::last_os_error();
            return Err(match error.raw_os_error() {
                Some(ERROR_BROKEN_PIPE) | Some(ERROR_PIPE_NOT_CONNECTED) => {
                    io::ErrorKind::UnexpectedEof.into()
                }
                _ => error,
            });
        }
        Ok(usize::try_from(available).unwrap_or(usize::MAX))
    }

    fn wait_milliseconds(&self, timeout: &AtomicU32) -> u32 {
        if self.nonblocking.load(Ordering::Acquire) {
            0
        } else {
            timeout.load(Ordering::Acquire)
        }
    }

    pub(super) fn receive(&self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let wait = self.wait_milliseconds(&self.read_timeout);
        match self.transfer(Direction::Read(buffer), wait) {
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_NO_DATA)
                ) =>
            {
                Ok(0)
            }
            other => other,
        }
    }

    pub(super) fn transmit(&self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let wait = self.wait_milliseconds(&self.write_timeout);
        self.transfer(Direction::Write(buffer), wait).map_err(|error| {
            if matches!(
                error.raw_os_error(),
                Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_NO_DATA)
            ) {
                io::Error::new(io::ErrorKind::BrokenPipe, error)
            } else {
                error
            }
        })
    }

    fn transfer(&self, direction: Direction<'_>, wait_milliseconds: u32) -> io::Result<usize> {
        let event = new_event()?;
        let mut overlapped = Overlapped::signalled_by(&event);
        let handle = self.pipe.as_raw_handle();
        let mut transferred = 0u32;
        let started = match direction {
            Direction::Read(buffer) => {
                let len = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
                // SAFETY: `buffer` is writable for `len` bytes, and it and `overlapped` stay
                // alive and unmoved until this function has waited for the read to complete.
                unsafe {
                    sys::ReadFile(
                        handle,
                        buffer.as_mut_ptr(),
                        len,
                        ptr::null_mut(),
                        &raw mut overlapped,
                    )
                }
            }
            Direction::Write(buffer) => {
                let len = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
                // SAFETY: `buffer` is readable for `len` bytes, and it and `overlapped` stay
                // alive and unmoved until this function has waited for the write to complete.
                unsafe {
                    sys::WriteFile(
                        handle,
                        buffer.as_ptr(),
                        len,
                        ptr::null_mut(),
                        &raw mut overlapped,
                    )
                }
            }
        };
        if started == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_IO_PENDING) {
                return Err(error);
            }
            // SAFETY: `event` is open and is the event of the operation just started.
            let waited = unsafe { sys::WaitForSingleObject(event.as_raw_handle(), wait_milliseconds) };
            if waited == WAIT_TIMEOUT {
                // SAFETY: `handle` and `overlapped` belong to the operation still pending.
                unsafe { sys::CancelIoEx(handle, &raw mut overlapped) };
            }
        }
        // SAFETY: waiting here makes sure the operation that used `overlapped` and the buffer
        // has completed or been cancelled before either goes out of scope.
        let done = unsafe { sys::GetOverlappedResult(handle, &raw mut overlapped, &raw mut transferred, 1) };
        if done == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_OPERATION_ABORTED) {
                return Err(if wait_milliseconds == 0 {
                    io::ErrorKind::WouldBlock.into()
                } else {
                    io::ErrorKind::TimedOut.into()
                });
            }
            return Err(error);
        }
        Ok(transferred as usize)
    }
}
