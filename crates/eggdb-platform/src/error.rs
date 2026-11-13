pub use eggdb_platform_core::{Error, ErrorKind, Result};

#[cfg(test)]
mod tests {
    use super::{Error, ErrorKind};
    use std::error::Error as _;
    use std::io;

    #[test]
    fn os_errors_keep_their_kind_and_source() {
        let error = Error::from(io::Error::new(io::ErrorKind::NotFound, "no such file"));
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert!(error.source().is_some());
        assert_eq!(error.to_string(), "not found: no such file");
    }

    #[test]
    fn unknown_os_errors_become_other() {
        let error = Error::from(io::Error::other("disk on fire"));
        assert_eq!(error.kind(), ErrorKind::Other);
    }

    #[test]
    fn network_errors_map_to_one_set_of_kinds() {
        for (os, kind) in [
            (
                io::ErrorKind::ConnectionRefused,
                ErrorKind::ConnectionRefused,
            ),
            (io::ErrorKind::ConnectionReset, ErrorKind::ConnectionReset),
            (io::ErrorKind::ConnectionAborted, ErrorKind::ConnectionReset),
            (io::ErrorKind::BrokenPipe, ErrorKind::ConnectionReset),
            (io::ErrorKind::TimedOut, ErrorKind::TimedOut),
            (io::ErrorKind::AddrInUse, ErrorKind::AddrInUse),
            (io::ErrorKind::AddrNotAvailable, ErrorKind::AddrNotAvailable),
            (io::ErrorKind::NotConnected, ErrorKind::NotConnected),
            (io::ErrorKind::HostUnreachable, ErrorKind::HostUnreachable),
            (
                io::ErrorKind::NetworkUnreachable,
                ErrorKind::HostUnreachable,
            ),
        ] {
            assert_eq!(Error::from(io::Error::from(os)).kind(), kind, "{os:?}");
        }
    }

    #[test]
    fn errors_without_an_os_cause_describe_their_kind() {
        let error = Error::new(ErrorKind::InjectedFault);
        assert!(error.source().is_none());
        assert_eq!(error.to_string(), "injected fault");
    }
}
