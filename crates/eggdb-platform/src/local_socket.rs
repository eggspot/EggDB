//! Unix domain sockets for servers that local clients reach without TCP, as psql and the mysql
//! client do by default. A socket file left by a server that died is removed and reused, a live
//! one is refused, and the socket is made readable and writable by its owner only. Windows has no
//! support yet: binding answers [`std::io::ErrorKind::Unsupported`].

use std::io;
use std::path::{Path, PathBuf};

/// Why a local socket could not be opened.
#[derive(Debug)]
pub enum LocalBindError {
    /// The directory that should hold the socket does not exist.
    MissingDirectory(PathBuf),
    /// A server is already accepting connections on the socket.
    InUse,
    /// Something other than a socket is at the path.
    NotASocket,
    /// Local sockets are not available on this OS yet.
    Unsupported,
    /// Another failure of the operating system.
    Io(io::Error),
}

impl std::fmt::Display for LocalBindError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingDirectory(directory) => write!(formatter, "directory {} does not exist", directory.display()),
            Self::InUse => formatter.write_str("a server is already accepting connections there"),
            Self::NotASocket => formatter.write_str("the path is not a socket"),
            Self::Unsupported => formatter.write_str("Unix sockets are not available on this OS yet"),
            Self::Io(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for LocalBindError {}

#[cfg(unix)]
mod imp {
    use super::{LocalBindError, Path};
    use std::io;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    /// A listening Unix domain socket.
    pub type LocalListener = std::os::unix::net::UnixListener;
    /// A connection accepted on a [`LocalListener`].
    pub type LocalStream = std::os::unix::net::UnixStream;

    pub(super) fn bind(path: &Path) -> Result<LocalListener, LocalBindError> {
        let directory = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
        if !directory.is_dir() {
            return Err(LocalBindError::MissingDirectory(directory.to_path_buf()));
        }
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if !metadata.file_type().is_socket() => return Err(LocalBindError::NotASocket),
            Ok(_) => match LocalStream::connect(path) {
                Ok(_) => return Err(LocalBindError::InUse),
                Err(_) => std::fs::remove_file(path).map_err(LocalBindError::Io)?,
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(LocalBindError::Io(error)),
        }
        let listener = LocalListener::bind(path).map_err(LocalBindError::Io)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(LocalBindError::Io)?;
        Ok(listener)
    }
}

#[cfg(not(unix))]
mod imp {
    use super::{LocalBindError, Path};
    use std::io;
    use std::time::Duration;

    /// A listening Unix domain socket (not available on this OS yet).
    #[derive(Debug)]
    pub struct LocalListener(std::convert::Infallible);

    /// A connection accepted on a [`LocalListener`] (not available on this OS yet).
    #[derive(Debug)]
    pub struct LocalStream(std::convert::Infallible);

    impl LocalListener {
        /// Accepts a connection.
        pub fn accept(&self) -> io::Result<(LocalStream, ())> {
            match self.0 {}
        }

        /// Sets non-blocking accepts.
        pub fn set_nonblocking(&self, _: bool) -> io::Result<()> {
            match self.0 {}
        }
    }

    impl LocalStream {
        /// Sets the read timeout.
        pub fn set_read_timeout(&self, _: Option<Duration>) -> io::Result<()> {
            match self.0 {}
        }

        /// Sets the write timeout.
        pub fn set_write_timeout(&self, _: Option<Duration>) -> io::Result<()> {
            match self.0 {}
        }

        /// Sets blocking mode.
        pub fn set_nonblocking(&self, _: bool) -> io::Result<()> {
            match self.0 {}
        }

        /// Shuts down one or both directions.
        pub fn shutdown(&self, _: std::net::Shutdown) -> io::Result<()> {
            match self.0 {}
        }
    }

    impl io::Read for LocalStream {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            match self.0 {}
        }
    }

    impl io::Write for LocalStream {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            match self.0 {}
        }

        fn flush(&mut self) -> io::Result<()> {
            match self.0 {}
        }
    }

    impl io::Read for &LocalStream {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            match self.0 {}
        }
    }

    pub(super) fn bind(_: &Path) -> Result<LocalListener, LocalBindError> {
        Err(LocalBindError::Unsupported)
    }
}

pub use imp::{LocalListener, LocalStream};

/// Whether this OS supports local sockets.
pub const LOCAL_SOCKETS_SUPPORTED: bool = cfg!(unix);

/// Binds a local socket at `path`, replacing a stale socket file and refusing a live one.
pub fn bind_local(path: &Path) -> Result<LocalListener, LocalBindError> {
    imp::bind(path)
}

/// The socket file a PostgreSQL client looks for in `directory` for `port`.
pub fn postgresql_socket_path(directory: &Path, port: u16) -> PathBuf {
    directory.join(format!(".s.PGSQL.{port}"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn scratch(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("eggdb-local-socket-{name}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&directory);
        directory
    }

    #[test]
    fn a_client_talks_to_a_bound_socket_and_a_stale_file_is_reused() {
        let directory = scratch("talk");
        let path = postgresql_socket_path(&directory, 5432);
        let listener = bind_local(&path).unwrap();
        let mut client = LocalStream::connect(&path).unwrap();
        let (mut accepted, _) = listener.accept().unwrap();
        client.write_all(b"ping").unwrap();
        let mut received = [0u8; 4];
        accepted.read_exact(&mut received).unwrap();
        assert_eq!(&received, b"ping");
        assert!(matches!(bind_local(&path), Err(LocalBindError::InUse)));
        drop(listener);
        drop(accepted);
        assert!(bind_local(&path).is_ok(), "a stale socket file is replaced");
    }

    #[test]
    fn missing_directories_and_plain_files_are_refused() {
        let missing = scratch("missing").join("absent").join(".s.PGSQL.1");
        assert!(matches!(bind_local(&missing), Err(LocalBindError::MissingDirectory(_))));
        let directory = scratch("file");
        let path = directory.join("plain");
        std::fs::write(&path, b"x").unwrap();
        assert!(matches!(bind_local(&path), Err(LocalBindError::NotASocket)));
    }
}
