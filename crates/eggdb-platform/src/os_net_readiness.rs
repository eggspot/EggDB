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
            ),
            not(all(target_arch = "x86_64", target_pointer_width = "32"))
        ),
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "illumos",
        target_os = "solaris"
    ) => {
        mod poll;
        mod queue;
        mod selector;

        cfg_select! {
            target_vendor = "apple" => {
                mod kqueue;
                use kqueue as native;
                pub(crate) use kqueue::{accept, prepare_listener, suppress_sigpipe};
                #[cfg(test)]
                pub(crate) use kqueue::sigpipe_suppressed;
            }
            any(target_os = "linux", target_os = "android") => {
                mod epoll;
                use epoll as native;
                pub(crate) use epoll::{accept, prepare_listener, suppress_sigpipe};
                #[cfg(target_os = "linux")]
                pub(crate) use epoll::connect_without_waiting;
            }
            _ => {
                mod plain_unix;
                mod std_sockets;
                use plain_unix as native;
                pub(crate) use std_sockets::{accept, prepare_listener, suppress_sigpipe};
            }
        }

        pub(crate) use queue::{NATIVE, ReadinessQueue, supports};
        #[cfg(target_os = "linux")]
        pub(crate) use queue::{Connecting, finished_connecting};
    }
    _ => {
        mod std_sockets;
        mod unsupported;

        pub(crate) use std_sockets::{accept, prepare_listener, suppress_sigpipe};
        pub(crate) use unsupported::{NATIVE, ReadinessQueue, supports};
    }
}
