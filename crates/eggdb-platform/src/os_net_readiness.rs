#[cfg(all(
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
    ),
    not(all(target_arch = "x86_64", target_pointer_width = "32"))
))]
mod epoll;

#[cfg(all(
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
    ),
    not(all(target_arch = "x86_64", target_pointer_width = "32"))
))]
use epoll::{BACKEND, Selector, connect_without_waiting};

#[cfg(target_vendor = "apple")]
mod kqueue;

#[cfg(target_vendor = "apple")]
use kqueue::{BACKEND, Selector, connect_without_waiting};

#[cfg(target_vendor = "apple")]
pub(crate) use kqueue::suppress_sigpipe;

#[cfg(not(target_vendor = "apple"))]
pub(crate) fn suppress_sigpipe(_: &std::net::TcpStream) -> std::io::Result<()> {
    Ok(())
}

#[cfg(any(
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
        ),
        not(all(target_arch = "x86_64", target_pointer_width = "32"))
    ),
    target_vendor = "apple"
))]
mod queue;

#[cfg(any(
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
        ),
        not(all(target_arch = "x86_64", target_pointer_width = "32"))
    ),
    target_vendor = "apple"
))]
pub(crate) use queue::{NATIVE, ReadinessQueue};

#[cfg(not(any(
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
        ),
        not(all(target_arch = "x86_64", target_pointer_width = "32"))
    ),
    target_vendor = "apple"
)))]
pub(crate) use unsupported::{NATIVE, ReadinessQueue};

#[cfg(not(any(
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
        ),
        not(all(target_arch = "x86_64", target_pointer_width = "32"))
    ),
    target_vendor = "apple"
)))]
mod unsupported {
    use crate::error::{Error, ErrorKind, Result};
    use crate::net::{NetBackend, NetCompletion, NetOp};
    use crate::os_net::{OsListener, OsStream};

    pub(crate) const NATIVE: Option<NetBackend> = None;

    #[derive(Debug)]
    pub(crate) enum ReadinessQueue {}

    impl ReadinessQueue {
        pub(crate) fn new(_: usize) -> Result<Self> {
            Err(Error::new(ErrorKind::Unsupported))
        }

        pub(crate) fn backend(&self) -> NetBackend {
            match *self {}
        }

        pub(crate) fn submit(&mut self, _: NetOp<OsListener, OsStream>, _: u64) {
            match *self {}
        }

        pub(crate) fn reap(
            &mut self,
            _: &mut Vec<NetCompletion<OsListener, OsStream>>,
            _: usize,
        ) -> Result<()> {
            match *self {}
        }
    }
}
