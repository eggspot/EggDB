#[cfg(windows)]
pub(crate) use port::*;

#[cfg(not(windows))]
pub(crate) use unsupported::*;

#[cfg(windows)]
mod port {
    use crate::error::{Error, ErrorKind, Result};
    use crate::net::{Address, NetCompletion, NetOp, NetOutput, answered_at_once};
    use crate::os_net::{OsListener, OsStream};
    use std::collections::VecDeque;
    use std::ffi::c_void;
    use std::io;
    use std::net::{SocketAddr, TcpStream};
    use std::os::windows::io::{
        AsRawHandle, AsRawSocket, FromRawHandle, FromRawSocket, OwnedHandle, OwnedSocket,
        RawHandle,
    };
    use std::ptr;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    const AF_INET: i32 = 2;
    const AF_INET6: i32 = 23;
    const SOCK_STREAM: i32 = 1;
    const IPPROTO_TCP: i32 = 6;
    const WSA_FLAG_OVERLAPPED: u32 = 0x01;
    const WSA_FLAG_NO_HANDLE_INHERIT: u32 = 0x80;
    const SOL_SOCKET: i32 = 0xffff;
    const SO_UPDATE_ACCEPT_CONTEXT: i32 = 0x700b;
    const SO_UPDATE_CONNECT_CONTEXT: i32 = 0x7010;
    const SIO_GET_EXTENSION_FUNCTION_POINTER: u32 = 0xc800_0006;
    const INVALID_SOCKET: usize = usize::MAX;
    const SOCKET_ERROR: i32 = -1;
    const WSA_IO_PENDING: i32 = 997;
    const ERROR_OPERATION_ABORTED: i32 = 995;
    const WAIT_TIMEOUT: i32 = 258;
    const INFINITE: u32 = u32::MAX;
    const ENTRIES_PER_WAIT: usize = 64;
    const MAX_TRANSFER: usize = 0x4000_0000;
    const ADDRESS_SPACE: usize = 28 + 16;
    const WINSOCK_2_2: u16 = 0x0202;

    const WSAID_ACCEPTEX: Guid = Guid {
        data1: 0xb536_7df1,
        data2: 0xcbac,
        data3: 0x11cf,
        data4: [0x95, 0xca, 0x00, 0x80, 0x5f, 0x48, 0xa1, 0x92],
    };
    const WSAID_CONNECTEX: Guid = Guid {
        data1: 0x25a2_07b9,
        data2: 0xddf3,
        data3: 0x4660,
        data4: [0x8e, 0xe9, 0x76, 0xe5, 0x8c, 0x74, 0x06, 0x3e],
    };

    static NEXT_PORT_ID: AtomicU64 = AtomicU64::new(1);

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

    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    struct Guid {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }

    #[repr(C)]
    #[derive(Debug)]
    struct WsaBuf {
        len: u32,
        buf: *mut u8,
    }

    #[repr(C, align(8))]
    struct WsaData([u8; 512]);

    type AcceptEx = unsafe extern "system" fn(
        listener: usize,
        accepted: usize,
        output: *mut c_void,
        received_len: u32,
        local_len: u32,
        remote_len: u32,
        received: *mut u32,
        overlapped: *mut Overlapped,
    ) -> i32;

    type ConnectEx = unsafe extern "system" fn(
        socket: usize,
        name: *const c_void,
        name_len: i32,
        send: *const c_void,
        send_len: u32,
        sent: *mut u32,
        overlapped: *mut Overlapped,
    ) -> i32;

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
        fn CancelIoEx(file: RawHandle, overlapped: *mut Overlapped) -> i32;
    }

    #[link(name = "ws2_32")]
    unsafe extern "system" {
        fn WSAStartup(version: u16, data: *mut WsaData) -> i32;
        fn WSACleanup() -> i32;
        fn WSAGetLastError() -> i32;
        fn WSASocketW(
            family: i32,
            kind: i32,
            protocol: i32,
            info: *mut c_void,
            group: u32,
            flags: u32,
        ) -> usize;
        fn WSAIoctl(
            socket: usize,
            code: u32,
            input: *const c_void,
            input_len: u32,
            output: *mut c_void,
            output_len: u32,
            returned: *mut u32,
            overlapped: *mut Overlapped,
            routine: *const c_void,
        ) -> i32;
        fn WSARecv(
            socket: usize,
            buffers: *mut WsaBuf,
            count: u32,
            received: *mut u32,
            flags: *mut u32,
            overlapped: *mut Overlapped,
            routine: *const c_void,
        ) -> i32;
        fn WSASend(
            socket: usize,
            buffers: *mut WsaBuf,
            count: u32,
            sent: *mut u32,
            flags: u32,
            overlapped: *mut Overlapped,
            routine: *const c_void,
        ) -> i32;
        fn WSAGetOverlappedResult(
            socket: usize,
            overlapped: *mut Overlapped,
            transferred: *mut u32,
            wait: i32,
            flags: *mut u32,
        ) -> i32;
        fn setsockopt(
            socket: usize,
            level: i32,
            name: i32,
            value: *const c_void,
            len: i32,
        ) -> i32;
        fn bind(socket: usize, name: *const c_void, len: i32) -> i32;
    }

    type Op = NetOp<OsListener, OsStream>;
    type NetDone = NetCompletion<OsListener, OsStream>;

    /// Which completion port a socket was tied to, as Windows lets a socket deliver its packets
    /// to one port only, for as long as the socket lives.
    #[derive(Debug, Default)]
    pub(crate) struct PortBinding(AtomicU64);

    #[derive(Debug)]
    struct Winsock;

    impl Winsock {
        fn start() -> Result<Self> {
            let mut data = WsaData([0; 512]);
            // SAFETY: `data` is larger than a WSADATA and lives for the whole call.
            let started = unsafe { WSAStartup(WINSOCK_2_2, &raw mut data) };
            if started == 0 {
                Ok(Self)
            } else {
                Err(Error::from(io::Error::from_raw_os_error(started)).with_kind(ErrorKind::Unsupported))
            }
        }
    }

    impl Drop for Winsock {
        fn drop(&mut self) {
            // SAFETY: balances the successful WSAStartup this value stands for.
            unsafe { WSACleanup() };
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct Extensions {
        accept: AcceptEx,
        connect: ConnectEx,
    }

    #[repr(C)]
    struct Pending {
        overlapped: Overlapped,
        socket: usize,
        op: Op,
        user_data: u64,
        new_socket: Option<OwnedSocket>,
        addresses: [u8; 2 * ADDRESS_SPACE],
        target: [u8; 28],
        buffer: WsaBuf,
        flags: u32,
    }

    #[derive(Debug)]
    struct InPort {
        pending: *mut Pending,
        user_data: u64,
        socket: usize,
        canceled: bool,
    }

    /// One I/O completion port running a network queue's operations as overlapped Winsock
    /// calls: `AcceptEx`, `ConnectEx`, `WSARecv` and `WSASend`.
    pub(crate) struct NetPort {
        id: u64,
        extensions: Extensions,
        in_port: Vec<InPort>,
        ready: VecDeque<NetDone>,
        port: OwnedHandle,
        _winsock: Winsock,
    }

    impl std::fmt::Debug for NetPort {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("NetPort")
                .field("id", &self.id)
                .field("in_port", &self.in_port.len())
                .field("ready", &self.ready.len())
                .finish_non_exhaustive()
        }
    }

    // SAFETY: the pending operations the raw pointers name are owned by this port alone until
    // their packets arrive, and only the port's single owner touches them.
    unsafe impl Send for NetPort {}

    impl NetPort {
        pub(crate) fn new() -> Result<Self> {
            let winsock = Winsock::start()?;
            // SAFETY: a new completion port that is not tied to any handle yet.
            let port = unsafe {
                CreateIoCompletionPort(
                    ptr::without_provenance_mut(usize::MAX),
                    ptr::null_mut(),
                    0,
                    1,
                )
            };
            if port.is_null() {
                return Err(io::Error::last_os_error().into());
            }
            // SAFETY: CreateIoCompletionPort returned a new handle that nothing else owns.
            let port = unsafe { OwnedHandle::from_raw_handle(port) };
            let probe = new_socket(AF_INET)?;
            let extensions = Extensions {
                // SAFETY: the pointer Winsock hands out for WSAID_ACCEPTEX has AcceptEx's
                // signature.
                accept: unsafe { std::mem::transmute::<usize, AcceptEx>(extension(&probe, WSAID_ACCEPTEX)?) },
                // SAFETY: the pointer Winsock hands out for WSAID_CONNECTEX has ConnectEx's
                // signature.
                connect: unsafe { std::mem::transmute::<usize, ConnectEx>(extension(&probe, WSAID_CONNECTEX)?) },
            };
            Ok(Self {
                id: NEXT_PORT_ID.fetch_add(1, Ordering::Relaxed),
                extensions,
                in_port: Vec::new(),
                ready: VecDeque::new(),
                port,
                _winsock: winsock,
            })
        }

        pub(crate) fn submit(&mut self, op: Op, user_data: u64) {
            if let Some(result) = answered_at_once(&op) {
                self.ready.push_back(NetCompletion {
                    user_data,
                    result,
                    op,
                });
                return;
            }
            let pending = Box::new(Pending {
                overlapped: Overlapped {
                    internal: 0,
                    internal_high: 0,
                    offset: 0,
                    offset_high: 0,
                    event: ptr::null_mut(),
                },
                socket: INVALID_SOCKET,
                op,
                user_data,
                new_socket: None,
                addresses: [0; 2 * ADDRESS_SPACE],
                target: [0; 28],
                buffer: WsaBuf {
                    len: 0,
                    buf: ptr::null_mut(),
                },
                flags: 0,
            });
            if let Err((pending, error)) = self.start(pending) {
                self.ready.push_back(NetCompletion {
                    user_data,
                    result: Err(error),
                    op: pending.op,
                });
            }
        }

        pub(crate) fn reap(
            &mut self,
            completions: &mut Vec<NetDone>,
            min: usize,
            deadline: Option<Instant>,
        ) -> Result<()> {
            let before = completions.len();
            completions.extend(self.ready.drain(..));
            let mut waited = false;
            while completions.len() - before < min && !self.in_port.is_empty() {
                let milliseconds = match deadline {
                    None => INFINITE,
                    Some(deadline) => {
                        let left = deadline.saturating_duration_since(Instant::now());
                        if waited && left.is_zero() {
                            return Ok(());
                        }
                        let rounded_up = left.as_nanos().div_ceil(1_000_000);
                        u32::try_from(rounded_up).unwrap_or(INFINITE - 1).min(INFINITE - 1)
                    }
                };
                self.dequeue(completions, milliseconds)?;
                waited = true;
            }
            Ok(())
        }

        pub(crate) fn cancel(&mut self, user_data: u64) -> bool {
            let mut found = self
                .ready
                .iter()
                .any(|completion| completion.user_data == user_data);
            for in_port in self
                .in_port
                .iter_mut()
                .filter(|in_port| in_port.user_data == user_data)
            {
                found = true;
                if !in_port.canceled {
                    in_port.canceled = true;
                    cancel_one(in_port);
                }
            }
            found
        }

        fn start(&mut self, pending: Box<Pending>) -> core::result::Result<(), (Box<Pending>, Error)> {
            let mut pending = pending;
            if let Err(error) = self.prepare(&mut pending) {
                return Err((pending, error));
            }
            let user_data = pending.user_data;
            let socket = pending.socket;
            let raw = Box::into_raw(pending);
            // SAFETY: `raw` comes from a live box this port now owns until its packet arrives;
            // nothing else touches it while the system may.
            match unsafe { begin(raw, self.extensions) } {
                Ok(()) => {
                    self.in_port.push(InPort {
                        pending: raw,
                        user_data,
                        socket,
                        canceled: false,
                    });
                    Ok(())
                }
                Err(error) => {
                    // SAFETY: the call failed without starting the operation, so no packet will
                    // name `raw` and the box is ours again.
                    let pending = unsafe { Box::from_raw(raw) };
                    Err((pending, error.into()))
                }
            }
        }

        fn prepare(&self, pending: &mut Pending) -> Result<()> {
            match &pending.op {
                NetOp::Accept { listener } => {
                    let socket = raw_socket(listener.listener.as_raw_socket());
                    self.bind_once(socket, &listener.port_binding)?;
                    let family = match listener.listener.local_addr()? {
                        SocketAddr::V4(_) => AF_INET,
                        SocketAddr::V6(_) => AF_INET6,
                    };
                    pending.new_socket = Some(new_socket(family)?);
                    pending.socket = socket;
                }
                NetOp::Connect { address } => {
                    let Address::Tcp(address) = *address;
                    let (family, unspecified) = match address {
                        SocketAddr::V4(_) => (AF_INET, SocketAddr::from(([0, 0, 0, 0], 0))),
                        SocketAddr::V6(_) => (AF_INET6, SocketAddr::from(([0_u16; 8], 0))),
                    };
                    let connecting = new_socket(family)?;
                    let socket = raw_socket(connecting.as_raw_socket());
                    let mut local = [0_u8; 28];
                    let local_len = encode_address(unspecified, &mut local);
                    // SAFETY: `local` holds an encoded socket address of `local_len` bytes,
                    // read only during the call.
                    if unsafe { bind(socket, local.as_ptr().cast(), local_len) } == SOCKET_ERROR {
                        return Err(last_socket_error().into());
                    }
                    self.associate(socket)?;
                    encode_address(address, &mut pending.target);
                    pending.new_socket = Some(connecting);
                    pending.socket = socket;
                }
                NetOp::Receive { stream, .. } | NetOp::Send { stream, .. } => {
                    let socket = raw_socket(stream.stream.as_raw_socket());
                    self.bind_once(socket, &stream.port_binding)?;
                    pending.socket = socket;
                }
            }
            Ok(())
        }

        fn bind_once(&self, socket: usize, binding: &PortBinding) -> Result<()> {
            match binding
                .0
                .compare_exchange(0, self.id, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => self.associate(socket).inspect_err(|_| binding.0.store(0, Ordering::Release)),
                Err(bound) if bound == self.id => Ok(()),
                Err(_) => Err(Error::new(ErrorKind::Unsupported)),
            }
        }

        fn associate(&self, socket: usize) -> Result<()> {
            // SAFETY: ties a socket this queue's operation holds alive to the port this value
            // owns; the call keeps neither pointer.
            let tied = unsafe {
                CreateIoCompletionPort(
                    ptr::without_provenance_mut(socket),
                    self.port.as_raw_handle(),
                    0,
                    0,
                )
            };
            if tied.is_null() {
                Err(io::Error::last_os_error().into())
            } else {
                Ok(())
            }
        }

        fn dequeue(&mut self, completions: &mut Vec<NetDone>, milliseconds: u32) -> Result<()> {
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
                    self.port.as_raw_handle(),
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
                    return Ok(());
                }
                return Err(error.into());
            }
            let removed = (removed as usize).min(ENTRIES_PER_WAIT);
            for entry in &entries[..removed] {
                let raw = entry.overlapped.cast::<Pending>();
                let Some(index) = self.in_port.iter().position(|in_port| in_port.pending == raw)
                else {
                    continue;
                };
                self.in_port.swap_remove(index);
                // SAFETY: the packet names an operation this port started, whose `Pending`
                // begins with that overlapped structure; the packet ends the system's use of
                // it, so this is its only owner.
                let pending = unsafe { Box::from_raw(raw) };
                completions.push(finish(*pending, self.id));
            }
            Ok(())
        }
    }

    impl Drop for NetPort {
        fn drop(&mut self) {
            for in_port in self.in_port.iter_mut().filter(|in_port| !in_port.canceled) {
                in_port.canceled = true;
                cancel_one(in_port);
            }
            let mut finished = Vec::new();
            while !self.in_port.is_empty() {
                if self.dequeue(&mut finished, INFINITE).is_err() {
                    self.in_port.clear();
                    return;
                }
                finished.clear();
            }
        }
    }

    fn cancel_one(in_port: &InPort) {
        // SAFETY: asks the system to cancel one operation that is still in flight on the
        // socket; the overlapped pointer only identifies it.
        unsafe { CancelIoEx(ptr::without_provenance_mut(in_port.socket), in_port.pending.cast()) };
    }

    unsafe fn begin(raw: *mut Pending, extensions: Extensions) -> io::Result<()> {
        // SAFETY: the caller guarantees `raw` is live and ours; the reference ends before the
        // system is handed any pointer into it.
        let pending = unsafe { &mut *raw };
        let socket = pending.socket;
        let overlapped = &raw mut pending.overlapped;
        match &mut pending.op {
            NetOp::Accept { .. } => {
                let accepted = pending
                    .new_socket
                    .as_ref()
                    .map_or(INVALID_SOCKET, |socket| raw_socket(socket.as_raw_socket()));
                // SAFETY: the address buffer and the overlapped structure stay in place inside
                // the boxed `Pending` until the packet arrives, as the caller guarantees.
                let accepted = unsafe {
                    (extensions.accept)(
                        socket,
                        accepted,
                        pending.addresses.as_mut_ptr().cast(),
                        0,
                        ADDRESS_SPACE as u32,
                        ADDRESS_SPACE as u32,
                        ptr::null_mut(),
                        overlapped,
                    )
                };
                success_or_pending(accepted != 0, io::Error::last_os_error)
            }
            NetOp::Connect { address } => {
                let Address::Tcp(address) = *address;
                let len = if address.is_ipv4() { 16 } else { 28 };
                // SAFETY: the encoded target address and the overlapped structure stay in place
                // inside the boxed `Pending` until the packet arrives.
                let connected = unsafe {
                    (extensions.connect)(
                        socket,
                        pending.target.as_ptr().cast(),
                        len,
                        ptr::null(),
                        0,
                        ptr::null_mut(),
                        overlapped,
                    )
                };
                success_or_pending(connected != 0, io::Error::last_os_error)
            }
            NetOp::Receive { buffer, .. } => {
                pending.buffer = WsaBuf {
                    len: transfer_len(buffer.len()),
                    buf: buffer.as_mut_ptr(),
                };
                // SAFETY: the buffer description, the buffer, the flags and the overlapped
                // structure all stay in place until the packet arrives.
                let received = unsafe {
                    WSARecv(
                        socket,
                        &raw mut pending.buffer,
                        1,
                        ptr::null_mut(),
                        &raw mut pending.flags,
                        overlapped,
                        ptr::null(),
                    )
                };
                success_or_pending(received == 0, last_socket_error)
            }
            NetOp::Send { buffer, .. } => {
                pending.buffer = WsaBuf {
                    len: transfer_len(buffer.len()),
                    buf: buffer.as_mut_ptr(),
                };
                // SAFETY: as for a receive, for the bytes sent from the buffer.
                let sent = unsafe {
                    WSASend(
                        socket,
                        &raw mut pending.buffer,
                        1,
                        ptr::null_mut(),
                        0,
                        overlapped,
                        ptr::null(),
                    )
                };
                success_or_pending(sent == 0, last_socket_error)
            }
        }
    }

    fn success_or_pending(succeeded: bool, error: fn() -> io::Error) -> io::Result<()> {
        if succeeded {
            return Ok(());
        }
        let error = error();
        if error.raw_os_error() == Some(WSA_IO_PENDING) {
            Ok(())
        } else {
            Err(error)
        }
    }

    fn finish(mut pending: Pending, port_id: u64) -> NetDone {
        let mut transferred = 0_u32;
        let mut flags = 0_u32;
        // SAFETY: the operation has completed, so its overlapped structure is ours again and the
        // call only reads the result the system stored in it.
        let got = unsafe {
            WSAGetOverlappedResult(
                pending.socket,
                &raw mut pending.overlapped,
                &raw mut transferred,
                0,
                &raw mut flags,
            )
        };
        let result = if got == 0 {
            let error = last_socket_error();
            if error.raw_os_error() == Some(ERROR_OPERATION_ABORTED) {
                Err(Error::new(ErrorKind::Canceled))
            } else {
                Err(error.into())
            }
        } else {
            completed(&mut pending, transferred, port_id)
        };
        NetCompletion {
            user_data: pending.user_data,
            result,
            op: pending.op,
        }
    }

    fn completed(pending: &mut Pending, transferred: u32, port_id: u64) -> Result<NetOutput<OsStream>> {
        let listening = pending.socket;
        match (&pending.op, pending.new_socket.take()) {
            (NetOp::Accept { .. }, Some(accepted)) => {
                let socket = raw_socket(accepted.as_raw_socket());
                let listening = listening.to_ne_bytes();
                update_context(socket, SO_UPDATE_ACCEPT_CONTEXT, &listening)?;
                let stream = OsStream::set_up(TcpStream::from(accepted))?;
                Ok(NetOutput::Connected(stream))
            }
            (NetOp::Connect { .. }, Some(connected)) => {
                let socket = raw_socket(connected.as_raw_socket());
                update_context(socket, SO_UPDATE_CONNECT_CONTEXT, &[])?;
                let stream = OsStream::set_up(TcpStream::from(connected))?;
                stream.port_binding.0.store(port_id, Ordering::Release);
                Ok(NetOutput::Connected(stream))
            }
            (NetOp::Receive { .. } | NetOp::Send { .. }, _) => {
                Ok(NetOutput::Transferred(transferred as usize))
            }
            (NetOp::Accept { .. } | NetOp::Connect { .. }, None) => Err(Error::new(ErrorKind::Other)),
        }
    }

    fn update_context(socket: usize, option: i32, value: &[u8]) -> io::Result<()> {
        let value_ptr = if value.is_empty() { ptr::null() } else { value.as_ptr().cast() };
        // SAFETY: `value` holds `value.len()` bytes read only during the call.
        let set = unsafe { setsockopt(socket, SOL_SOCKET, option, value_ptr, value.len() as i32) };
        if set == SOCKET_ERROR {
            Err(last_socket_error())
        } else {
            Ok(())
        }
    }

    fn new_socket(family: i32) -> Result<OwnedSocket> {
        // SAFETY: WSASocketW takes no pointers here; an invalid result is reported as an error.
        let socket = unsafe {
            WSASocketW(
                family,
                SOCK_STREAM,
                IPPROTO_TCP,
                ptr::null_mut(),
                0,
                WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
            )
        };
        if socket == INVALID_SOCKET {
            return Err(last_socket_error().into());
        }
        // SAFETY: WSASocketW returned a new socket that nothing else owns.
        Ok(unsafe { OwnedSocket::from_raw_socket(socket as u64) })
    }

    fn extension(probe: &OwnedSocket, id: Guid) -> Result<usize> {
        let mut pointer = 0_usize;
        let mut returned = 0_u32;
        // SAFETY: the GUID and the pointer-sized output live for the whole call, whose lengths
        // are given exactly.
        let asked = unsafe {
            WSAIoctl(
                raw_socket(probe.as_raw_socket()),
                SIO_GET_EXTENSION_FUNCTION_POINTER,
                ptr::from_ref(&id).cast(),
                size_of::<Guid>() as u32,
                ptr::from_mut(&mut pointer).cast(),
                size_of::<usize>() as u32,
                &raw mut returned,
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if asked == SOCKET_ERROR || pointer == 0 {
            return Err(Error::from(last_socket_error()).with_kind(ErrorKind::Unsupported));
        }
        Ok(pointer)
    }

    fn encode_address(address: SocketAddr, into: &mut [u8; 28]) -> i32 {
        *into = [0; 28];
        match address {
            SocketAddr::V4(address) => {
                into[0..2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
                into[2..4].copy_from_slice(&address.port().to_be_bytes());
                into[4..8].copy_from_slice(&address.ip().octets());
                16
            }
            SocketAddr::V6(address) => {
                into[0..2].copy_from_slice(&(AF_INET6 as u16).to_ne_bytes());
                into[2..4].copy_from_slice(&address.port().to_be_bytes());
                into[4..8].copy_from_slice(&address.flowinfo().to_ne_bytes());
                into[8..24].copy_from_slice(&address.ip().octets());
                into[24..28].copy_from_slice(&address.scope_id().to_ne_bytes());
                28
            }
        }
    }

    fn raw_socket(socket: u64) -> usize {
        usize::try_from(socket).unwrap_or(INVALID_SOCKET)
    }

    fn last_socket_error() -> io::Error {
        // SAFETY: WSAGetLastError takes no arguments and reads this thread's last error.
        io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
    }

    fn transfer_len(len: usize) -> u32 {
        u32::try_from(len.min(MAX_TRANSFER)).unwrap_or(0)
    }
}

#[cfg(not(windows))]
mod unsupported {
    use crate::error::{Error, ErrorKind, Result};
    use crate::net::{NetCompletion, NetOp};
    use crate::os_net::{OsListener, OsStream};
    use std::convert::Infallible;
    use std::time::Instant;

    #[derive(Debug)]
    pub(crate) struct NetPort(Infallible);

    impl NetPort {
        pub(crate) fn new() -> Result<Self> {
            Err(Error::new(ErrorKind::Unsupported))
        }

        pub(crate) fn submit(&mut self, _op: NetOp<OsListener, OsStream>, _user_data: u64) {
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
}
