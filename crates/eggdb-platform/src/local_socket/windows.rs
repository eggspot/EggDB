use super::{
    LocalBindError, Path, SocketAddressUnix, require_directory, socket_address,
    timeout_milliseconds,
};
use std::ffi::c_void;
use std::io;
use std::net::Shutdown;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{
    AsRawHandle, AsRawSocket, AsSocket, BorrowedSocket, FromRawHandle, FromRawSocket, OwnedHandle,
    OwnedSocket, RawHandle, RawSocket,
};
use std::ptr;
use std::sync::OnceLock;
use std::time::Duration;

const AF_UNIX: i32 = 1;
const SOCK_STREAM: i32 = 1;
const WSA_FLAG_OVERLAPPED: u32 = 0x01;
const WSA_FLAG_NO_HANDLE_INHERIT: u32 = 0x80;
const INVALID_SOCKET: usize = usize::MAX;
const SOCKET_ERROR: i32 = -1;
const SOL_SOCKET: i32 = 0xffff;
const SO_SNDTIMEO: i32 = 0x1005;
const SO_RCVTIMEO: i32 = 0x1006;
const FIONBIO: i32 = 0x8004_667e_u32 as i32;
const SD_RECEIVE: i32 = 0;
const SD_SEND: i32 = 1;
const SD_BOTH: i32 = 2;
const WSAEAFNOSUPPORT: i32 = 10047;
const WSAESHUTDOWN: i32 = 10058;
const LISTEN_BACKLOG: i32 = 128;
const WINSOCK_2_2: u16 = 0x0202;
const ADDRESS_LEN: i32 = size_of::<SocketAddressUnix>() as i32;

const FILE_READ_ATTRIBUTES: u32 = 0x0080;
const READ_CONTROL: u32 = 0x0002_0000;
const WRITE_DAC: u32 = 0x0004_0000;
const FILE_ALL_ACCESS: u32 = 0x001f_01ff;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
const IO_REPARSE_TAG_AF_UNIX: u32 = 0x8000_0023;
const FILE_ATTRIBUTE_TAG_INFO_CLASS: i32 = 9;

const TOKEN_QUERY: u32 = 0x0008;
const TOKEN_USER_CLASS: i32 = 1;
const ACL_REVISION: u32 = 2;
const ACL_HEADER_LEN: u32 = 8;
const ACCESS_ALLOWED_ACE_LEN_WITHOUT_SID: u32 = 8;
const SE_FILE_OBJECT: i32 = 1;
const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;
const PROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x8000_0000;
const TOKEN_USER_WORDS: usize = 32;
const ACL_BUFFER_WORDS: usize = 64;
const WSA_DATA_WORDS: usize = 64;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct FileAttributeTagInfo {
    attributes: u32,
    reparse_tag: u32,
}

mod sys {
    use super::{FileAttributeTagInfo, RawHandle, c_void};

    #[link(name = "ws2_32")]
    unsafe extern "system" {
        pub(super) fn WSAStartup(version: u16, data: *mut c_void) -> i32;
        pub(super) fn WSAGetLastError() -> i32;
        pub(super) fn WSASocketW(
            family: i32,
            kind: i32,
            protocol: i32,
            info: *mut c_void,
            group: u32,
            flags: u32,
        ) -> usize;
        pub(super) fn bind(socket: usize, name: *const c_void, len: i32) -> i32;
        pub(super) fn listen(socket: usize, backlog: i32) -> i32;
        pub(super) fn accept(socket: usize, address: *mut c_void, len: *mut i32) -> usize;
        pub(super) fn connect(socket: usize, name: *const c_void, len: i32) -> i32;
        pub(super) fn recv(socket: usize, buffer: *mut u8, len: i32, flags: i32) -> i32;
        pub(super) fn send(socket: usize, buffer: *const u8, len: i32, flags: i32) -> i32;
        pub(super) fn setsockopt(
            socket: usize,
            level: i32,
            name: i32,
            value: *const c_void,
            len: i32,
        ) -> i32;
        pub(super) fn ioctlsocket(socket: usize, command: i32, argument: *mut u32) -> i32;
        pub(super) fn shutdown(socket: usize, how: i32) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub(super) fn GetCurrentProcess() -> RawHandle;
        pub(super) fn GetFileInformationByHandleEx(
            file: RawHandle,
            class: i32,
            info: *mut FileAttributeTagInfo,
            len: u32,
        ) -> i32;
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        pub(super) fn OpenProcessToken(
            process: RawHandle,
            access: u32,
            token: *mut RawHandle,
        ) -> i32;
        pub(super) fn GetTokenInformation(
            token: RawHandle,
            class: i32,
            info: *mut c_void,
            len: u32,
            returned: *mut u32,
        ) -> i32;
        pub(super) fn GetLengthSid(sid: *mut c_void) -> u32;
        pub(super) fn InitializeAcl(acl: *mut c_void, len: u32, revision: u32) -> i32;
        pub(super) fn AddAccessAllowedAce(
            acl: *mut c_void,
            revision: u32,
            mask: u32,
            sid: *mut c_void,
        ) -> i32;
        pub(super) fn SetSecurityInfo(
            handle: RawHandle,
            object_type: i32,
            information: u32,
            owner: *mut c_void,
            group: *mut c_void,
            dacl: *const c_void,
            sacl: *const c_void,
        ) -> u32;
    }
}

/// A listening Unix domain socket: Windows' `AF_UNIX` socket, Windows 10 version 1803 and later.
#[derive(Debug)]
pub struct LocalListener(OwnedSocket);

/// A connection accepted on a [`LocalListener`], or made with [`LocalStream::connect`].
#[derive(Debug)]
pub struct LocalStream(OwnedSocket);

impl LocalListener {
    /// Binds a socket at `path` and listens on it, failing when anything is already there.
    pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        Self::listen(bound_socket(path.as_ref())?)
    }

    fn listen(socket: OwnedSocket) -> io::Result<Self> {
        // SAFETY: `socket` is an open, bound stream socket.
        let listening = unsafe { sys::listen(raw(&socket), LISTEN_BACKLOG) };
        check(listening)?;
        Ok(Self(socket))
    }

    /// Accepts a connection. Windows does not report the peer's address on these sockets, so
    /// the second value is `()`.
    pub fn accept(&self) -> io::Result<(LocalStream, ())> {
        // SAFETY: a null address and length ask Winsock for no peer address.
        let accepted = unsafe { sys::accept(raw(&self.0), ptr::null_mut(), ptr::null_mut()) };
        Ok((LocalStream(owned(accepted)?), ()))
    }

    /// Sets non-blocking accepts; accepted connections start in the same mode.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        set_nonblocking(&self.0, nonblocking)
    }

    /// Opens a second handle to the same socket.
    pub fn try_clone(&self) -> io::Result<Self> {
        self.0.try_clone().map(Self)
    }
}

impl LocalStream {
    /// Connects to the socket at `path`.
    pub fn connect<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let address = socket_address(path.as_ref())?;
        let socket = new_socket()?;
        // SAFETY: `address` is a sockaddr_un that outlives the call and `ADDRESS_LEN` is its size.
        let connected =
            unsafe { sys::connect(raw(&socket), (&raw const address).cast(), ADDRESS_LEN) };
        check(connected)?;
        Ok(Self(socket))
    }

    /// Sets how long a read waits; `None` waits forever.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        set_timeout(&self.0, SO_RCVTIMEO, timeout)
    }

    /// Sets how long a write waits; `None` waits forever.
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        set_timeout(&self.0, SO_SNDTIMEO, timeout)
    }

    /// Sets non-blocking reads and writes.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        set_nonblocking(&self.0, nonblocking)
    }

    /// Shuts down one or both directions.
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        let how = match how {
            Shutdown::Read => SD_RECEIVE,
            Shutdown::Write => SD_SEND,
            Shutdown::Both => SD_BOTH,
        };
        // SAFETY: `self.0` is an open socket and `how` one of the SD_ values.
        let done = unsafe { sys::shutdown(raw(&self.0), how) };
        check(done)
    }

    /// Opens a second handle to the same connection.
    pub fn try_clone(&self) -> io::Result<Self> {
        self.0.try_clone().map(Self)
    }

    fn receive(&self, buffer: &mut [u8]) -> io::Result<usize> {
        let len = i32::try_from(buffer.len()).unwrap_or(i32::MAX);
        // SAFETY: `buffer` is writable for at least `len` bytes for the whole call.
        let received = unsafe { sys::recv(raw(&self.0), buffer.as_mut_ptr(), len, 0) };
        if received == SOCKET_ERROR {
            let error = last_error();
            return if error.raw_os_error() == Some(WSAESHUTDOWN) {
                Ok(0)
            } else {
                Err(error)
            };
        }
        Ok(usize::try_from(received).unwrap_or(0))
    }

    fn transmit(&self, buffer: &[u8]) -> io::Result<usize> {
        let len = i32::try_from(buffer.len()).unwrap_or(i32::MAX);
        // SAFETY: `buffer` is readable for at least `len` bytes for the whole call.
        let sent = unsafe { sys::send(raw(&self.0), buffer.as_ptr(), len, 0) };
        if sent == SOCKET_ERROR {
            return Err(last_error());
        }
        Ok(usize::try_from(sent).unwrap_or(0))
    }
}

impl AsSocket for LocalListener {
    fn as_socket(&self) -> BorrowedSocket<'_> {
        self.0.as_socket()
    }
}

impl AsSocket for LocalStream {
    fn as_socket(&self) -> BorrowedSocket<'_> {
        self.0.as_socket()
    }
}

impl io::Read for LocalStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.receive(buffer)
    }
}

impl io::Read for &LocalStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.receive(buffer)
    }
}

impl io::Write for LocalStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.transmit(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl io::Write for &LocalStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.transmit(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn bind(path: &Path) -> Result<LocalListener, LocalBindError> {
    require_directory(path)?;
    match existing_entry(path) {
        Ok(Entry::Missing) => {}
        Ok(Entry::Other) => return Err(LocalBindError::NotASocket),
        Ok(Entry::Socket) => match LocalStream::connect(path) {
            Ok(_) => return Err(LocalBindError::InUse),
            Err(_) => std::fs::remove_file(path).map_err(LocalBindError::Io)?,
        },
        Err(error) => return Err(LocalBindError::Io(error)),
    }
    let socket = bound_socket(path).map_err(|error| {
        if error.raw_os_error() == Some(WSAEAFNOSUPPORT) {
            LocalBindError::Unsupported
        } else {
            LocalBindError::Io(error)
        }
    })?;
    let listening = restrict_to_owner(path).and_then(|()| LocalListener::listen(socket));
    listening.map_err(|error| {
        let _ = std::fs::remove_file(path);
        LocalBindError::Io(error)
    })
}

fn bound_socket(path: &Path) -> io::Result<OwnedSocket> {
    let address = socket_address(path)?;
    let socket = new_socket()?;
    // SAFETY: `address` is a sockaddr_un that outlives the call and `ADDRESS_LEN` is its size.
    let bound = unsafe { sys::bind(raw(&socket), (&raw const address).cast(), ADDRESS_LEN) };
    check(bound)?;
    Ok(socket)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    Missing,
    Socket,
    Other,
}

fn existing_entry(path: &Path) -> io::Result<Entry> {
    let opened = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path);
    let file = match opened {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Entry::Missing),
        Err(error) => return Err(error),
    };
    let mut info = FileAttributeTagInfo {
        attributes: 0,
        reparse_tag: 0,
    };
    // SAFETY: `file` is open with FILE_READ_ATTRIBUTES, and `info` is a live
    // FILE_ATTRIBUTE_TAG_INFO of exactly the size passed.
    let read = unsafe {
        sys::GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FILE_ATTRIBUTE_TAG_INFO_CLASS,
            &raw mut info,
            size_of::<FileAttributeTagInfo>() as u32,
        )
    };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }
    let is_socket = info.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        && info.reparse_tag == IO_REPARSE_TAG_AF_UNIX;
    Ok(if is_socket {
        Entry::Socket
    } else {
        Entry::Other
    })
}

fn restrict_to_owner(path: &Path) -> io::Result<()> {
    let mut user = [0u64; TOKEN_USER_WORDS];
    let sid = current_user_sid(&mut user)?;
    // SAFETY: `sid` points at the valid SID GetTokenInformation wrote into `user`, which is not
    // moved while `sid` is used.
    let sid_len = unsafe { sys::GetLengthSid(sid) };
    let acl_len = ACL_HEADER_LEN + ACCESS_ALLOWED_ACE_LEN_WITHOUT_SID + sid_len;
    let mut acl = [0u32; ACL_BUFFER_WORDS];
    if !usize::try_from(acl_len).is_ok_and(|len| len <= size_of_val(&acl)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the current user's security identifier is too long",
        ));
    }
    let acl_pointer: *mut c_void = acl.as_mut_ptr().cast();
    // SAFETY: `acl` is DWORD-aligned writable memory of at least `acl_len` bytes.
    let initialised = unsafe { sys::InitializeAcl(acl_pointer, acl_len, ACL_REVISION) };
    if initialised == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `acl` was initialised above with room for exactly this one ACE, and `sid` is the
    // valid SID inside `user`.
    let added =
        unsafe { sys::AddAccessAllowedAce(acl_pointer, ACL_REVISION, FILE_ALL_ACCESS, sid) };
    if added == 0 {
        return Err(io::Error::last_os_error());
    }
    let file = std::fs::OpenOptions::new()
        .access_mode(READ_CONTROL | WRITE_DAC)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    // SAFETY: `file` is open with WRITE_DAC, `acl` holds the valid ACL built above and outlives
    // the call, and the null owner, group and SACL leave those unchanged.
    let set = unsafe {
        sys::SetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            acl_pointer,
            ptr::null(),
        )
    };
    if set != 0 {
        return Err(io::Error::from_raw_os_error(set as i32));
    }
    Ok(())
}

fn current_user_sid(user: &mut [u64; TOKEN_USER_WORDS]) -> io::Result<*mut c_void> {
    // SAFETY: `GetCurrentProcess` takes no arguments and returns a pseudo-handle that needs no
    // closing.
    let process = unsafe { sys::GetCurrentProcess() };
    let mut token: RawHandle = ptr::null_mut();
    // SAFETY: `process` is the always-valid current-process pseudo-handle and `token` a live
    // out-pointer.
    let opened = unsafe { sys::OpenProcessToken(process, TOKEN_QUERY, &raw mut token) };
    if opened == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `token` was just opened by OpenProcessToken and nothing else owns it.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut returned = 0u32;
    // SAFETY: `token` is open with TOKEN_QUERY, `user` is writable for the length passed, and
    // `returned` is a live out-pointer.
    let read = unsafe {
        sys::GetTokenInformation(
            token.as_raw_handle(),
            TOKEN_USER_CLASS,
            user.as_mut_ptr().cast(),
            size_of_val(user) as u32,
            &raw mut returned,
        )
    };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: GetTokenInformation wrote a TOKEN_USER at the start of `user`, which is 8-byte
    // aligned; its first member, a SID_AND_ATTRIBUTES, starts with the SID pointer.
    let sid = unsafe { ptr::read(user.as_ptr().cast::<*mut c_void>()) };
    if sid.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the process token names no user",
        ));
    }
    Ok(sid)
}

fn start_winsock() -> io::Result<()> {
    static STARTED: OnceLock<i32> = OnceLock::new();
    let code = *STARTED.get_or_init(|| {
        let mut data = [0u64; WSA_DATA_WORDS];
        // SAFETY: `data` is larger than a WSADATA, suitably aligned, and lives for the whole
        // call; the start is never balanced by WSACleanup, so Winsock stays up for the life of
        // the process.
        unsafe { sys::WSAStartup(WINSOCK_2_2, data.as_mut_ptr().cast()) }
    });
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code))
    }
}

fn new_socket() -> io::Result<OwnedSocket> {
    start_winsock()?;
    // SAFETY: plain integer arguments and no protocol info; the result is checked by `owned`.
    let socket = unsafe {
        sys::WSASocketW(
            AF_UNIX,
            SOCK_STREAM,
            0,
            ptr::null_mut(),
            0,
            WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
        )
    };
    owned(socket)
}

fn owned(socket: usize) -> io::Result<OwnedSocket> {
    if socket == INVALID_SOCKET {
        return Err(last_error());
    }
    // SAFETY: `socket` is a valid socket Winsock just returned to us, owned by nothing else.
    Ok(unsafe { OwnedSocket::from_raw_socket(socket as RawSocket) })
}

fn raw(socket: &OwnedSocket) -> usize {
    socket.as_raw_socket() as usize
}

fn last_error() -> io::Error {
    // SAFETY: `WSAGetLastError` only reads the calling thread's last Winsock error.
    io::Error::from_raw_os_error(unsafe { sys::WSAGetLastError() })
}

fn check(result: i32) -> io::Result<()> {
    if result == SOCKET_ERROR {
        Err(last_error())
    } else {
        Ok(())
    }
}

fn set_nonblocking(socket: &OwnedSocket, nonblocking: bool) -> io::Result<()> {
    let mut value = u32::from(nonblocking);
    // SAFETY: `socket` is open and `value` is a live u_long for the whole call.
    let done = unsafe { sys::ioctlsocket(raw(socket), FIONBIO, &raw mut value) };
    check(done)
}

fn set_timeout(socket: &OwnedSocket, option: i32, timeout: Option<Duration>) -> io::Result<()> {
    let milliseconds = timeout_milliseconds(timeout)?;
    // SAFETY: `socket` is open and `milliseconds` is a DWORD that outlives the call, its size
    // passed as the length.
    let done = unsafe {
        sys::setsockopt(
            raw(socket),
            SOL_SOCKET,
            option,
            (&raw const milliseconds).cast(),
            size_of::<u32>() as i32,
        )
    };
    check(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("eggdb-afunix-{name}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&directory);
        directory
    }

    #[test]
    fn an_echo_round_trips_through_a_bound_socket_and_its_clones() {
        let path = scratch("echo").join("echo.sock");
        let _ = std::fs::remove_file(&path);
        let listener = bind(&path).unwrap();
        assert_eq!(existing_entry(&path).unwrap(), Entry::Socket);
        let echo = std::thread::spawn(move || {
            let (accepted, ()) = listener.accept().unwrap();
            let mut reader = accepted.try_clone().unwrap();
            let mut received = [0u8; 5];
            reader.read_exact(&mut received).unwrap();
            let mut writer: &LocalStream = &accepted;
            writer.write_all(&received).unwrap();
            accepted.shutdown(Shutdown::Write).unwrap();
        });
        let mut client = LocalStream::connect(&path).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        client.write_all(b"hello").unwrap();
        let mut answer = Vec::new();
        client.read_to_end(&mut answer).unwrap();
        assert_eq!(answer, b"hello");
        echo.join().unwrap();
    }

    #[test]
    fn a_stale_socket_file_is_replaced_and_a_live_one_refused() {
        let path = scratch("stale").join(".s.PGSQL.5432");
        let _ = std::fs::remove_file(&path);
        let listener = bind(&path).unwrap();
        assert!(matches!(bind(&path), Err(LocalBindError::InUse)));
        drop(listener);
        assert_eq!(existing_entry(&path).unwrap(), Entry::Socket);
        let listener = bind(&path).unwrap();
        let _client = LocalStream::connect(&path).unwrap();
        let (_accepted, ()) = listener.accept().unwrap();
    }

    #[test]
    fn a_nonblocking_listener_reports_would_block_and_reads_time_out() {
        let path = scratch("nonblocking").join("idle.sock");
        let _ = std::fs::remove_file(&path);
        let listener = bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let error = listener.accept().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        let client = LocalStream::connect(&path).unwrap();
        let (accepted, ()) = listener.accept().unwrap();
        accepted.set_nonblocking(false).unwrap();
        accepted
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let mut buffer = [0u8; 1];
        let mut reader: &LocalStream = &accepted;
        let timed_out = reader.read(&mut buffer).unwrap_err();
        assert!(
            matches!(
                timed_out.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ),
            "{timed_out}"
        );
        drop(client);
    }

    #[test]
    fn a_plain_file_is_not_taken_for_a_socket() {
        let path = scratch("plain").join("plain");
        std::fs::write(&path, b"x").unwrap();
        assert_eq!(existing_entry(&path).unwrap(), Entry::Other);
        assert_eq!(
            existing_entry(&scratch("plain").join("absent")).unwrap(),
            Entry::Missing
        );
    }
}
