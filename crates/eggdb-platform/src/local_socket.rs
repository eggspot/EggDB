//! Unix domain sockets for servers that local clients reach without TCP, as psql and the mysql
//! client do by default. A socket file left by a server that died is removed and reused, a live
//! one is refused, and the socket is made readable and writable by its owner only.
//!
//! On Windows 10 version 1803 and later the same API runs on Windows' own `AF_UNIX` sockets: the
//! socket file's access list is replaced by one that lets only the current user in, which stands
//! in for mode 0600, and older Windows answers [`LocalBindError::Unsupported`]. Windows reports
//! neither the peer's address nor its credentials on these sockets, so [`LocalListener::accept`]
//! returns `()` in place of an address there.

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
    /// Local sockets are not available on this OS, or on this Windows version (they need
    /// Windows 10 version 1803 or later).
    Unsupported,
    /// Another failure of the operating system.
    Io(io::Error),
}

impl std::fmt::Display for LocalBindError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingDirectory(directory) => write!(
                formatter,
                "directory {} does not exist",
                directory.display()
            ),
            Self::InUse => formatter.write_str("a server is already accepting connections there"),
            Self::NotASocket => formatter.write_str("the path is not a socket"),
            Self::Unsupported => formatter.write_str(
                "Unix sockets are not available on this system (Windows needs Windows 10 version 1803 or later)",
            ),
            Self::Io(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for LocalBindError {}

#[cfg(any(unix, windows))]
fn require_directory(path: &Path) -> Result<(), LocalBindError> {
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if directory.is_dir() {
        Ok(())
    } else {
        Err(LocalBindError::MissingDirectory(directory.to_path_buf()))
    }
}

#[cfg(any(windows, test))]
const SOCKET_PATH_CAPACITY: usize = 108;

#[cfg(any(windows, test))]
const ADDRESS_FAMILY_UNIX: u16 = 1;

#[cfg(any(windows, test))]
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct SocketAddressUnix {
    family: u16,
    path: [u8; SOCKET_PATH_CAPACITY],
}

#[cfg(any(windows, test))]
fn socket_address(path: &Path) -> io::Result<SocketAddressUnix> {
    let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidInput, message);
    let text = path
        .to_str()
        .ok_or_else(|| invalid("the socket path is not valid Unicode".to_owned()))?;
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return Err(invalid("the socket path is empty".to_owned()));
    }
    if bytes.contains(&0) {
        return Err(invalid(
            "the socket path contains a NUL character".to_owned(),
        ));
    }
    if bytes.len() >= SOCKET_PATH_CAPACITY {
        return Err(invalid(format!(
            "the socket path is {} bytes long; at most {} fit",
            bytes.len(),
            SOCKET_PATH_CAPACITY - 1
        )));
    }
    let mut address = SocketAddressUnix {
        family: ADDRESS_FAMILY_UNIX,
        path: [0; SOCKET_PATH_CAPACITY],
    };
    for (slot, byte) in address.path.iter_mut().zip(bytes) {
        *slot = *byte;
    }
    Ok(address)
}

#[cfg(any(windows, test))]
fn timeout_milliseconds(timeout: Option<std::time::Duration>) -> io::Result<u32> {
    let Some(timeout) = timeout else {
        return Ok(0);
    };
    if timeout.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot set a 0 duration timeout",
        ));
    }
    Ok(u32::try_from(timeout.as_nanos().div_ceil(1_000_000)).unwrap_or(u32::MAX))
}

#[cfg(unix)]
mod imp {
    use super::{LocalBindError, Path, PathBuf, require_directory};
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};

    /// A listening Unix domain socket.
    pub type LocalListener = std::os::unix::net::UnixListener;
    /// A connection accepted on a [`LocalListener`].
    pub type LocalStream = std::os::unix::net::UnixStream;

    pub(super) fn bind(path: &Path) -> Result<LocalListener, LocalBindError> {
        require_directory(path)?;
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if !metadata.file_type().is_socket() => {
                return Err(LocalBindError::NotASocket);
            }
            Ok(_) => match LocalStream::connect(path) {
                Ok(_) => return Err(LocalBindError::InUse),
                Err(_) => std::fs::remove_file(path).map_err(LocalBindError::Io)?,
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(LocalBindError::Io(error)),
        }
        let staging = staging_directory(path).map_err(LocalBindError::Io)?;
        let bound = bind_staged(&staging, path);
        let _ = std::fs::remove_dir(&staging);
        bound
    }

    const STAGING_ATTEMPTS: u32 = 1000;
    const STAGED_SOCKET: &str = "s";

    pub(super) fn staging_directory(path: &Path) -> io::Result<PathBuf> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        for attempt in 0..STAGING_ATTEMPTS {
            let candidate = parent.join(format!(".e{attempt}"));
            match builder.create(&candidate) {
                Ok(()) => return Ok(candidate),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no free staging directory next to the socket",
        ))
    }

    fn bind_staged(staging: &Path, path: &Path) -> Result<LocalListener, LocalBindError> {
        let staged = staging.join(STAGED_SOCKET);
        let listener = LocalListener::bind(&staged).map_err(LocalBindError::Io)?;
        let published = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600))
            .and_then(|()| std::fs::hard_link(&staged, path));
        let _ = std::fs::remove_file(&staged);
        match published {
            Ok(()) => Ok(listener),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                Err(LocalBindError::InUse)
            }
            Err(error) => Err(LocalBindError::Io(error)),
        }
    }
}

#[cfg(windows)]
mod windows;

#[cfg(windows)]
use windows as imp;

#[cfg(not(any(unix, windows)))]
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

        /// Opens a second handle to the same socket.
        pub fn try_clone(&self) -> io::Result<Self> {
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

        /// Opens a second handle to the same connection.
        pub fn try_clone(&self) -> io::Result<Self> {
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

/// Whether this OS has local sockets; on Windows binding still answers
/// [`LocalBindError::Unsupported`] before Windows 10 version 1803.
pub const LOCAL_SOCKETS_SUPPORTED: bool = cfg!(any(unix, windows));

/// Binds a local socket at `path`, replacing a stale socket file and refusing a live one.
pub fn bind_local(path: &Path) -> Result<LocalListener, LocalBindError> {
    imp::bind(path)
}

/// Whether this OS has named pipes for local clients ([`bind_named_pipe`]): Windows only.
pub const NAMED_PIPES_SUPPORTED: bool = cfg!(windows);

/// The pipe the mysql client opens on Windows when given `--protocol=PIPE` without a name.
pub const MYSQL_DEFAULT_PIPE: &str = "MySQL";

/// Listens on the Windows named pipe `\\.\pipe\<name>`, the way the mysql client and SQL
/// Server's clients reach a local server on Windows. Only the current user may connect and
/// remote clients are refused; a pipe of that name that already exists answers
/// [`LocalBindError::InUse`]. Other OSes answer [`LocalBindError::Unsupported`].
pub fn bind_named_pipe(name: &str) -> Result<LocalListener, LocalBindError> {
    #[cfg(windows)]
    {
        windows::bind_named_pipe(name)
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        Err(LocalBindError::Unsupported)
    }
}

/// The socket file a PostgreSQL client looks for in `directory` for `port`.
pub fn postgresql_socket_path(directory: &Path, port: u16) -> PathBuf {
    directory.join(format!(".s.PGSQL.{port}"))
}

#[cfg(test)]
mod address_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_socket_path_is_encoded_as_a_nul_terminated_sockaddr_un() {
        let address = socket_address(Path::new("C:\\eggdb\\.s.PGSQL.5432")).unwrap();
        assert_eq!(address.family, 1);
        let text = b"C:\\eggdb\\.s.PGSQL.5432";
        assert_eq!(&address.path[..text.len()], text);
        assert!(address.path[text.len()..].iter().all(|byte| *byte == 0));
        assert_eq!(std::mem::size_of::<SocketAddressUnix>(), 110);
    }

    #[test]
    fn a_socket_path_must_fit_with_its_terminator_and_hold_no_nul() {
        let longest = "a".repeat(SOCKET_PATH_CAPACITY - 1);
        let encoded = socket_address(Path::new(&longest)).unwrap();
        assert_eq!(encoded.path[SOCKET_PATH_CAPACITY - 1], 0);
        let too_long = "a".repeat(SOCKET_PATH_CAPACITY);
        let error = socket_address(Path::new(&too_long)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("at most 107"), "{error}");
        let nul = socket_address(Path::new("a\0b")).unwrap_err();
        assert_eq!(nul.kind(), io::ErrorKind::InvalidInput);
        assert!(socket_address(Path::new("")).is_err());
    }

    #[test]
    fn timeouts_become_whole_milliseconds_rounded_up() {
        assert_eq!(timeout_milliseconds(None).unwrap(), 0);
        assert_eq!(
            timeout_milliseconds(Some(Duration::from_micros(1))).unwrap(),
            1
        );
        assert_eq!(
            timeout_milliseconds(Some(Duration::from_millis(1500))).unwrap(),
            1500
        );
        assert_eq!(
            timeout_milliseconds(Some(Duration::from_secs(u64::MAX))).unwrap(),
            u32::MAX
        );
        assert_eq!(
            timeout_milliseconds(Some(Duration::ZERO))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn scratch(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("eggdb-local-socket-{name}-{}", std::process::id()));
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

    #[cfg(unix)]
    #[test]
    fn a_socket_appears_only_owner_only_and_leaves_no_staging_behind() {
        use std::os::unix::fs::PermissionsExt;
        let directory = scratch("owner");
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::create_dir(directory.join(".e0")).unwrap();
        let path = directory.join("eggdb.sock");
        let listener = bind_local(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let mut names: Vec<String> = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, [".e0", "eggdb.sock"]);
        let _client = LocalStream::connect(&path).unwrap();
        listener.accept().unwrap();
        let staging = imp::staging_directory(&path).unwrap();
        let staging_mode = std::fs::metadata(&staging).unwrap().permissions().mode();
        assert_eq!(staging_mode & 0o777, 0o700);
        std::fs::remove_dir(&staging).unwrap();
        drop(listener);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn missing_directories_and_plain_files_are_refused() {
        let missing = scratch("missing").join("absent").join(".s.PGSQL.1");
        assert!(matches!(
            bind_local(&missing),
            Err(LocalBindError::MissingDirectory(_))
        ));
        let directory = scratch("file");
        let path = directory.join("plain");
        std::fs::write(&path, b"x").unwrap();
        assert!(matches!(bind_local(&path), Err(LocalBindError::NotASocket)));
    }
}
