use crate::error::Result;
use crate::net::TcpKeepAlive;
use std::net::{TcpListener, TcpStream};

/// Turns TCP keep-alives on for `listener` with `keep_alive`'s timing, so every connection it
/// accepts from then on starts with them: Linux, Android, the BSDs, illumos, Solaris, Apple's
/// systems and Windows copy a listener's socket options to the sockets it accepts.
///
/// The idle time, interval and count are set with `TCP_KEEPIDLE`, `TCP_KEEPINTVL` and
/// `TCP_KEEPCNT` (`TCP_KEEPALIVE` for the idle time on Apple's systems and Windows). Windows
/// before 10 version 1709 lacks those options, so there the idle time and interval go through
/// `SIO_KEEPALIVE_VALS` and the count stays the system's (10). OpenBSD has no per-socket
/// timing, so there only `SO_KEEPALIVE` is set and the system-wide timing applies. Fails with
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) on systems whose socket option
/// numbers EggDB does not declare, and with the operating system's error when it refuses one.
pub fn set_tcp_keep_alive(listener: &TcpListener, keep_alive: TcpKeepAlive) -> Result<()> {
    Ok(system::apply(listener, Some(keep_alive))?)
}

/// Turns TCP keep-alives on for one connection, as [`set_tcp_keep_alive`] does for the
/// connections a listener accepts, for a connection made with `connect` or accepted before
/// the listener's keep-alives were set.
pub fn set_tcp_keep_alive_stream(stream: &TcpStream, keep_alive: TcpKeepAlive) -> Result<()> {
    Ok(system::apply(stream, Some(keep_alive))?)
}

/// Accepts one connection from `listener`, as [`TcpListener::accept`] does, and gives it the
/// listener's TCP keep-alive timing on the systems that copy only part of it to accepted
/// sockets: macOS copies `SO_KEEPALIVE`, the interval and the count but not the idle time, and
/// NetBSD copies only `SO_KEEPALIVE`. Elsewhere the accepted socket already has all of it.
/// Fails with the error `accept` gives; a failure to copy the timing leaves the connection with
/// the system's timing rather than failing it.
pub fn accept_tcp(listener: &TcpListener) -> std::io::Result<(TcpStream, std::net::SocketAddr)> {
    let (stream, peer) = listener.accept()?;
    let _ = system::copy_from_listener(listener, &stream);
    Ok((stream, peer))
}

pub(crate) fn switch_tcp_keep_alive(
    stream: &TcpStream,
    keep_alive: Option<TcpKeepAlive>,
) -> Result<()> {
    Ok(system::apply(stream, keep_alive)?)
}

cfg_select! {
    any(
        all(
            any(target_os = "linux", target_os = "android"),
            any(
                target_arch = "x86_64",
                target_arch = "x86",
                target_arch = "aarch64",
                target_arch = "arm",
                target_arch = "riscv64",
                target_arch = "powerpc64",
                target_arch = "s390x",
                target_arch = "loongarch64"
            )
        ),
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "illumos",
        target_os = "solaris"
    ) => {
        mod system {
            use crate::net::TcpKeepAlive;
            use std::ffi::{c_int, c_void};
            use std::io;
            use std::os::fd::{AsRawFd, RawFd};

            cfg_select! {
                any(target_os = "linux", target_os = "android") => {
                    const SOL_SOCKET: c_int = 1;
                    const SO_KEEPALIVE: c_int = 9;
                }
                _ => {
                    const SOL_SOCKET: c_int = 0xffff;
                    const SO_KEEPALIVE: c_int = 0x8;
                }
            }

            unsafe extern "C" {
                fn setsockopt(
                    fd: c_int,
                    level: c_int,
                    name: c_int,
                    value: *const c_void,
                    length: u32,
                ) -> c_int;
                #[cfg(any(test, target_vendor = "apple", target_os = "netbsd"))]
                fn getsockopt(
                    fd: c_int,
                    level: c_int,
                    name: c_int,
                    value: *mut c_void,
                    length: *mut u32,
                ) -> c_int;
            }

            #[cfg(test)]
            pub(crate) const SUPPORTED: bool = true;

            pub(crate) fn apply(
                socket: &impl AsRawFd,
                keep_alive: Option<TcpKeepAlive>,
            ) -> io::Result<()> {
                let fd = socket.as_raw_fd();
                set(fd, SOL_SOCKET, SO_KEEPALIVE, c_int::from(keep_alive.is_some()))?;
                match keep_alive {
                    Some(keep_alive) => timing::set(fd, keep_alive),
                    None => Ok(()),
                }
            }

            #[cfg(any(target_vendor = "apple", target_os = "netbsd"))]
            pub(crate) fn copy_from_listener(
                listener: &impl AsRawFd,
                accepted: &impl AsRawFd,
            ) -> io::Result<()> {
                let from = listener.as_raw_fd();
                if get(from, SOL_SOCKET, SO_KEEPALIVE)? == 0 {
                    return Ok(());
                }
                match timing::get(from)? {
                    Some(keep_alive) => timing::set(accepted.as_raw_fd(), keep_alive),
                    None => Ok(()),
                }
            }

            #[cfg(not(any(target_vendor = "apple", target_os = "netbsd")))]
            pub(crate) fn copy_from_listener(
                _: &impl AsRawFd,
                _: &impl AsRawFd,
            ) -> io::Result<()> {
                Ok(())
            }

            #[cfg(test)]
            pub(crate) fn read_back(
                socket: &impl AsRawFd,
            ) -> io::Result<(bool, Option<TcpKeepAlive>)> {
                let fd = socket.as_raw_fd();
                let on = get(fd, SOL_SOCKET, SO_KEEPALIVE)? != 0;
                Ok((on, timing::get(fd)?))
            }

            cfg_select! {
                target_os = "openbsd" => {
                    mod timing {
                        use crate::net::TcpKeepAlive;
                        use std::io;
                        use std::os::fd::RawFd;

                        pub(super) fn set(_: RawFd, _: TcpKeepAlive) -> io::Result<()> {
                            Ok(())
                        }

                        #[cfg(test)]
                        pub(super) fn get(_: RawFd) -> io::Result<Option<TcpKeepAlive>> {
                            Ok(None)
                        }
                    }
                }
                _ => {
                    mod timing {
                        use crate::net::TcpKeepAlive;
                        use std::ffi::c_int;
                        use std::io;
                        use std::os::fd::RawFd;
                        use std::time::Duration;

                        const IPPROTO_TCP: c_int = 6;

                        cfg_select! {
                            any(target_os = "linux", target_os = "android") => {
                                const TCP_KEEPIDLE: c_int = 4;
                                const TCP_KEEPINTVL: c_int = 5;
                                const TCP_KEEPCNT: c_int = 6;
                            }
                            target_vendor = "apple" => {
                                const TCP_KEEPIDLE: c_int = 0x10;
                                const TCP_KEEPINTVL: c_int = 0x101;
                                const TCP_KEEPCNT: c_int = 0x102;
                            }
                            any(target_os = "freebsd", target_os = "dragonfly") => {
                                const TCP_KEEPIDLE: c_int = 0x100;
                                const TCP_KEEPINTVL: c_int = 0x200;
                                const TCP_KEEPCNT: c_int = 0x400;
                            }
                            target_os = "netbsd" => {
                                const TCP_KEEPIDLE: c_int = 3;
                                const TCP_KEEPINTVL: c_int = 5;
                                const TCP_KEEPCNT: c_int = 6;
                            }
                            _ => {
                                const TCP_KEEPIDLE: c_int = 0x22;
                                const TCP_KEEPINTVL: c_int = 0x24;
                                const TCP_KEEPCNT: c_int = 0x23;
                            }
                        }

                        pub(super) fn set(fd: RawFd, keep_alive: TcpKeepAlive) -> io::Result<()> {
                            let seconds = |time: Duration| {
                                c_int::try_from(time.as_secs()).unwrap_or(c_int::MAX)
                            };
                            let count = c_int::try_from(keep_alive.count()).unwrap_or(c_int::MAX);
                            super::set(fd, IPPROTO_TCP, TCP_KEEPIDLE, seconds(keep_alive.idle()))?;
                            super::set(
                                fd,
                                IPPROTO_TCP,
                                TCP_KEEPINTVL,
                                seconds(keep_alive.interval()),
                            )?;
                            super::set(fd, IPPROTO_TCP, TCP_KEEPCNT, count)
                        }

                        #[cfg(any(test, target_vendor = "apple", target_os = "netbsd"))]
                        pub(super) fn get(fd: RawFd) -> io::Result<Option<TcpKeepAlive>> {
                            let seconds = |name: c_int| -> io::Result<Duration> {
                                let value = super::get(fd, IPPROTO_TCP, name)?;
                                Ok(Duration::from_secs(u64::try_from(value).unwrap_or(0)))
                            };
                            let idle = seconds(TCP_KEEPIDLE)?;
                            let interval = seconds(TCP_KEEPINTVL)?;
                            let count = super::get(fd, IPPROTO_TCP, TCP_KEEPCNT)?;
                            let count = u32::try_from(count).unwrap_or(0);
                            Ok(TcpKeepAlive::new(idle, interval, count).ok())
                        }
                    }
                }
            }

            fn set(fd: RawFd, level: c_int, name: c_int, value: c_int) -> io::Result<()> {
                // SAFETY: value is an int that outlives the call, which only reads it.
                let result = unsafe {
                    setsockopt(
                        fd,
                        level,
                        name,
                        (&value as *const c_int).cast(),
                        size_of::<c_int>() as u32,
                    )
                };
                if result < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            }

            #[cfg(any(test, target_vendor = "apple", target_os = "netbsd"))]
            fn get(fd: RawFd, level: c_int, name: c_int) -> io::Result<c_int> {
                let mut value: c_int = 0;
                let mut length = size_of::<c_int>() as u32;
                // SAFETY: value and length are an int and its size that outlive the call, which
                // writes at most length bytes into value.
                let result = unsafe {
                    getsockopt(
                        fd,
                        level,
                        name,
                        (&mut value as *mut c_int).cast(),
                        &mut length,
                    )
                };
                if result < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(value)
            }
        }
    }
    windows => {
        mod system {
            use crate::net::TcpKeepAlive;
            use crate::os_iocp_net::control_socket;
            use std::ffi::c_void;
            use std::io;
            use std::os::windows::io::AsRawSocket;
            use std::time::Duration;

            const SOL_SOCKET: i32 = 0xffff;
            const SO_KEEPALIVE: i32 = 0x8;
            const IPPROTO_TCP: i32 = 6;
            const TCP_KEEPIDLE: i32 = 3;
            const TCP_KEEPCNT: i32 = 16;
            const TCP_KEEPINTVL: i32 = 17;
            const SIO_KEEPALIVE_VALS: u32 = 0x9800_0004;
            const SOCKET_ERROR: i32 = -1;

            #[link(name = "ws2_32")]
            unsafe extern "system" {
                fn WSAGetLastError() -> i32;
                fn setsockopt(
                    socket: usize,
                    level: i32,
                    name: i32,
                    value: *const c_void,
                    len: i32,
                ) -> i32;
                #[cfg(test)]
                fn getsockopt(
                    socket: usize,
                    level: i32,
                    name: i32,
                    value: *mut c_void,
                    len: *mut i32,
                ) -> i32;
            }

            #[cfg(test)]
            pub(crate) const SUPPORTED: bool = true;

            pub(crate) fn copy_from_listener(
                _: &impl AsRawSocket,
                _: &impl AsRawSocket,
            ) -> io::Result<()> {
                Ok(())
            }

            pub(crate) fn apply(
                socket: &impl AsRawSocket,
                keep_alive: Option<TcpKeepAlive>,
            ) -> io::Result<()> {
                let socket = raw(socket)?;
                set(socket, SOL_SOCKET, SO_KEEPALIVE, i32::from(keep_alive.is_some()))?;
                match keep_alive {
                    Some(keep_alive) => set_timing(socket, keep_alive)
                        .or_else(|_| set_keep_alive_values(socket, keep_alive)),
                    None => Ok(()),
                }
            }

            #[cfg(test)]
            pub(crate) fn read_back(
                socket: &impl AsRawSocket,
            ) -> io::Result<(bool, Option<TcpKeepAlive>)> {
                let socket = raw(socket)?;
                let mut value: i32 = 0;
                let mut length = size_of::<i32>() as i32;
                // SAFETY: value and length are an int and its size that outlive the call, which
                // writes at most length bytes into value.
                let result = unsafe {
                    getsockopt(
                        socket,
                        SOL_SOCKET,
                        SO_KEEPALIVE,
                        (&mut value as *mut i32).cast(),
                        &mut length,
                    )
                };
                if result == SOCKET_ERROR {
                    return Err(last_socket_error());
                }
                Ok((value != 0, None))
            }

            fn raw(socket: &impl AsRawSocket) -> io::Result<usize> {
                usize::try_from(socket.as_raw_socket())
                    .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
            }

            fn set_timing(socket: usize, keep_alive: TcpKeepAlive) -> io::Result<()> {
                let seconds = |time: Duration| i32::try_from(time.as_secs()).unwrap_or(i32::MAX);
                let count = i32::try_from(keep_alive.count()).unwrap_or(i32::MAX);
                set(socket, IPPROTO_TCP, TCP_KEEPIDLE, seconds(keep_alive.idle()))?;
                set(socket, IPPROTO_TCP, TCP_KEEPINTVL, seconds(keep_alive.interval()))?;
                set(socket, IPPROTO_TCP, TCP_KEEPCNT, count)
            }

            fn set_keep_alive_values(socket: usize, keep_alive: TcpKeepAlive) -> io::Result<()> {
                let milliseconds =
                    |time: Duration| u32::try_from(time.as_millis()).unwrap_or(u32::MAX);
                let fields = [
                    1,
                    milliseconds(keep_alive.idle()),
                    milliseconds(keep_alive.interval()),
                ];
                let mut values = [0_u8; 12];
                let (slots, _) = values.as_chunks_mut::<4>();
                for (slot, field) in slots.iter_mut().zip(fields) {
                    *slot = field.to_ne_bytes();
                }
                control_socket(socket, SIO_KEEPALIVE_VALS, &values)
            }

            fn set(socket: usize, level: i32, name: i32, value: i32) -> io::Result<()> {
                // SAFETY: value is an int that outlives the call, which only reads it.
                let result = unsafe {
                    setsockopt(
                        socket,
                        level,
                        name,
                        (&value as *const i32).cast(),
                        size_of::<i32>() as i32,
                    )
                };
                if result == SOCKET_ERROR {
                    return Err(last_socket_error());
                }
                Ok(())
            }

            fn last_socket_error() -> io::Error {
                // SAFETY: WSAGetLastError takes no arguments and reads this thread's last error.
                io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
            }
        }
    }
    _ => {
        mod system {
            use crate::net::TcpKeepAlive;
            use std::io;

            #[cfg(test)]
            pub(crate) const SUPPORTED: bool = false;

            pub(crate) fn apply<S>(_: &S, _: Option<TcpKeepAlive>) -> io::Result<()> {
                Err(io::Error::from(io::ErrorKind::Unsupported))
            }

            pub(crate) fn copy_from_listener<L, S>(_: &L, _: &S) -> io::Result<()> {
                Ok(())
            }

            #[cfg(test)]
            pub(crate) fn read_back<S>(_: &S) -> io::Result<(bool, Option<TcpKeepAlive>)> {
                Err(io::Error::from(io::ErrorKind::Unsupported))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        accept_tcp, set_tcp_keep_alive, set_tcp_keep_alive_stream, switch_tcp_keep_alive, system,
    };
    use crate::{ErrorKind, TcpKeepAlive};
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;

    fn timing() -> TcpKeepAlive {
        TcpKeepAlive::new(Duration::from_secs(45), Duration::from_secs(7), 4).unwrap()
    }

    fn expects_timing_read_back() -> bool {
        cfg!(any(
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "netbsd",
            target_os = "illumos",
            target_os = "solaris"
        ))
    }

    fn accepted_connections_inherit_the_timing() -> bool {
        expects_timing_read_back() && !cfg!(target_os = "netbsd")
    }

    #[test]
    fn a_connection_accepted_by_a_keep_alive_listener_reads_its_keep_alives_back() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let set = set_tcp_keep_alive(&listener, timing());
        if !system::SUPPORTED {
            assert_eq!(set.unwrap_err().kind(), ErrorKind::Unsupported);
            return;
        }
        set.unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        let (on, read_timing) = system::read_back(&accepted).unwrap();
        assert!(on, "an accepted connection inherits SO_KEEPALIVE");
        if accepted_connections_inherit_the_timing() {
            assert_eq!(read_timing, Some(timing()));
        }
        if cfg!(target_vendor = "apple") {
            let inherited = read_timing.unwrap();
            assert_eq!(
                (inherited.interval(), inherited.count()),
                (timing().interval(), timing().count())
            );
        }
    }

    #[test]
    fn a_connection_from_accept_tcp_has_the_listeners_whole_timing() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        if set_tcp_keep_alive(&listener, timing()).is_err() {
            return;
        }
        let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (accepted, _) = accept_tcp(&listener).unwrap();
        let (on, read_timing) = system::read_back(&accepted).unwrap();
        assert!(on);
        if expects_timing_read_back() || cfg!(target_vendor = "apple") {
            assert_eq!(read_timing, Some(timing()));
        }
    }

    #[test]
    fn keep_alives_turn_on_and_off_for_one_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let set = set_tcp_keep_alive_stream(&client, timing());
        if !system::SUPPORTED {
            assert_eq!(set.unwrap_err().kind(), ErrorKind::Unsupported);
            return;
        }
        set.unwrap();
        let (on, read_timing) = system::read_back(&client).unwrap();
        assert!(on);
        if expects_timing_read_back() {
            assert_eq!(read_timing, Some(timing()));
        }
        switch_tcp_keep_alive(&client, None).unwrap();
        assert!(!system::read_back(&client).unwrap().0);
    }
}
