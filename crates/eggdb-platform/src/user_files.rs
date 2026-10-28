//! Files that belong to the user running EggDB rather than to a database: where a program
//! keeps per-user state, such as a prompt's history, by each system's convention, and how to
//! create such files so that only their owner can read them.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// The directory for EggDB's per-user state: `$XDG_STATE_HOME/eggdb`, else
/// `~/.local/state/eggdb` on Unix (macOS included), and `%LOCALAPPDATA%\EggDB` on Windows.
/// `None` when the environment names no home.
pub fn state_directory() -> Option<PathBuf> {
    if cfg!(windows) {
        return non_empty_path("LOCALAPPDATA").map(|base| base.join("EggDB"));
    }
    non_empty_path("XDG_STATE_HOME")
        .or_else(|| non_empty_path("HOME").map(|home| home.join(".local").join("state")))
        .map(|base| base.join("eggdb"))
}

fn non_empty_path(variable: &str) -> Option<PathBuf> {
    std::env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Creates `directory` and its missing parents; on Unix the ones it creates have mode 0700.
pub fn create_private_directory(directory: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(directory)
}

/// Opens `path` for appending, creating it when it is missing; on Unix the file has mode 0600
/// afterwards, also when it existed with a looser one.
pub fn open_private_append(path: &Path) -> io::Result<File> {
    owner_only(private_options().append(true).open(path)?)
}

/// Opens `path` for appending as [`open_private_append`] does, but only when it is a plain
/// file of its own: a symbolic link at the path is never followed (the open itself refuses it,
/// so a link swapped in while opening is refused too), and a file that is not a regular file
/// (a FIFO, a device, a directory) or that has another name (a hard link) is refused before
/// anything is written to it or its mode is changed. A FIFO never makes it wait. So nothing
/// planted at the path can send the writes to another file. The file can be locked with
/// [`File::try_lock`] on every system: on Windows, where locking needs read or write access
/// and appending alone gives neither, it is also opened for reading.
pub fn open_private_append_no_follow(path: &Path) -> io::Result<File> {
    let file = match open_append_no_follow(path) {
        Ok(file) => file,
        Err(error) => return Err(explained(path, error)),
    };
    refuse_unless_plain(&file)?;
    owner_only(file)
}

fn refused(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, reason.to_owned())
}

fn explained(path: &Path, error: io::Error) -> io::Error {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => refused("it is a symbolic link"),
        Ok(metadata) if !metadata.is_file() => refused("it is not a regular file"),
        _ => error,
    }
}

fn refuse_unless_plain(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if metadata.file_type().is_symlink() {
        return Err(refused("it is a symbolic link"));
    }
    if !metadata.is_file() {
        return Err(refused("it is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(refused("it has another name (a hard link)"));
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod open_flags {
    #[cfg(any(
        target_arch = "x86_64",
        target_arch = "x86",
        target_arch = "riscv64",
        target_arch = "s390x",
        target_arch = "loongarch64"
    ))]
    pub(super) const NO_FOLLOW: Option<i32> = Some(0o400000);
    #[cfg(any(
        target_arch = "aarch64",
        target_arch = "arm",
        target_arch = "powerpc",
        target_arch = "powerpc64"
    ))]
    pub(super) const NO_FOLLOW: Option<i32> = Some(0o100000);
    #[cfg(any(target_arch = "mips", target_arch = "mips64"))]
    pub(super) const NO_FOLLOW: Option<i32> = Some(0x20000);
    #[cfg(not(any(
        target_arch = "x86_64",
        target_arch = "x86",
        target_arch = "riscv64",
        target_arch = "s390x",
        target_arch = "loongarch64",
        target_arch = "aarch64",
        target_arch = "arm",
        target_arch = "powerpc",
        target_arch = "powerpc64",
        target_arch = "mips",
        target_arch = "mips64"
    )))]
    pub(super) const NO_FOLLOW: Option<i32> = None;
    #[cfg(any(target_arch = "mips", target_arch = "mips64"))]
    pub(super) const NON_BLOCK: i32 = 0x80;
    #[cfg(any(target_arch = "sparc", target_arch = "sparc64"))]
    pub(super) const NON_BLOCK: i32 = 0x4000;
    #[cfg(not(any(
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "sparc",
        target_arch = "sparc64"
    )))]
    pub(super) const NON_BLOCK: i32 = 0o4000;
}

#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
mod open_flags {
    pub(super) const NO_FOLLOW: Option<i32> = Some(0x100);
    pub(super) const NON_BLOCK: i32 = 0x4;
}

#[cfg(any(target_os = "illumos", target_os = "solaris"))]
mod open_flags {
    pub(super) const NO_FOLLOW: Option<i32> = Some(0x20000);
    pub(super) const NON_BLOCK: i32 = 0x80;
}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly",
        target_os = "illumos",
        target_os = "solaris"
    ))
))]
mod open_flags {
    pub(super) const NO_FOLLOW: Option<i32> = None;
    pub(super) const NON_BLOCK: i32 = 0;
}

#[cfg(unix)]
fn open_append_no_follow(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut options = private_options();
    options.append(true);
    if let Some(no_follow) = open_flags::NO_FOLLOW {
        options.custom_flags(no_follow | open_flags::NON_BLOCK);
        return options.open(path);
    }
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| !metadata.is_file()) {
        return Err(refused("it is not a regular file"));
    }
    let file = options.open(path)?;
    let opened = file.metadata()?;
    let at_path = std::fs::symlink_metadata(path)?;
    if at_path.file_type().is_symlink()
        || opened.dev() != at_path.dev()
        || opened.ino() != at_path.ino()
    {
        return Err(refused(
            "another file took its place while it was being opened",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn open_append_no_follow(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let mut options = private_options();
    options
        .read(true)
        .append(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    options.open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_append_no_follow(path: &Path) -> io::Result<File> {
    let mut options = private_options();
    options.append(true);
    options.open(path)
}

/// Opens `path` for writing from the start, emptying it, creating it when it is missing; on
/// Unix the file has mode 0600 afterwards.
pub fn open_private_rewrite(path: &Path) -> io::Result<File> {
    owner_only(private_options().write(true).truncate(true).open(path)?)
}

fn owner_only(file: File) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if file.metadata()?.permissions().mode() & 0o077 != 0 {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(file)
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.create(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
}

#[cfg(test)]
mod tests {
    use super::{
        create_private_directory, open_private_append, open_private_append_no_follow,
        open_private_rewrite,
    };
    use std::io::Write;

    #[test]
    fn private_files_append_and_rewrite() {
        let directory = std::env::temp_dir()
            .join(format!("eggdb-user-files-{}", std::process::id()))
            .join("nested");
        create_private_directory(&directory).unwrap();
        let path = directory.join("history");
        open_private_append(&path)
            .unwrap()
            .write_all(b"a\n")
            .unwrap();
        open_private_append(&path)
            .unwrap()
            .write_all(b"b\n")
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"a\nb\n");
        open_private_rewrite(&path)
            .unwrap()
            .write_all(b"c\n")
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"c\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            drop(open_private_append(&path).unwrap());
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_symbolic_link_is_never_followed_for_a_no_follow_append() {
        let directory =
            std::env::temp_dir().join(format!("eggdb-no-follow-{}", std::process::id()));
        create_private_directory(&directory).unwrap();
        let path = directory.join("audit");
        let _ = std::fs::remove_file(&path);
        open_private_append_no_follow(&path)
            .unwrap()
            .write_all(b"a\n")
            .unwrap();
        open_private_append_no_follow(&path)
            .unwrap()
            .write_all(b"b\n")
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"a\nb\n");
        #[cfg(unix)]
        {
            let target = directory.join("elsewhere");
            std::fs::write(&target, b"kept\n").unwrap();
            let refused = |planted: &std::path::Path| {
                let error = open_private_append_no_follow(planted).unwrap_err();
                assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{error}");
                assert_eq!(std::fs::read(&target).unwrap(), b"kept\n");
            };
            let link = directory.join("planted");
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(&target, &link).unwrap();
            refused(&link);
            let dangling = directory.join("dangling");
            let _ = std::fs::remove_file(&dangling);
            std::os::unix::fs::symlink(directory.join("nowhere"), &dangling).unwrap();
            refused(&dangling);
            assert!(!directory.join("nowhere").exists());
            let hard = directory.join("hard");
            let _ = std::fs::remove_file(&hard);
            std::fs::hard_link(&target, &hard).unwrap();
            refused(&hard);
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
                refused(&hard);
                let mode = std::fs::metadata(&target).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o644);
            }
            let fifo = directory.join("fifo");
            let _ = std::fs::remove_file(&fifo);
            let made = std::process::Command::new("mkfifo").arg(&fifo).status();
            if made.is_ok_and(|status| status.success()) {
                let start = std::time::Instant::now();
                assert!(open_private_append_no_follow(&fifo).is_err());
                assert!(start.elapsed() < std::time::Duration::from_secs(5));
                let _ = std::fs::remove_file(&fifo);
            }
            for path in [&link, &dangling, &hard, &target] {
                let _ = std::fs::remove_file(path);
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_no_follow_append_can_be_locked_and_reopened_while_a_reader_holds_the_file() {
        let directory =
            std::env::temp_dir().join(format!("eggdb-no-follow-lock-{}", std::process::id()));
        create_private_directory(&directory).unwrap();
        let path = directory.join("audit.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut first = open_private_append_no_follow(&path).unwrap();
        first.try_lock().unwrap();
        first.write_all(b"one\n").unwrap();
        first.sync_data().unwrap();
        drop(first);
        let reader = std::fs::File::open(&path).unwrap();
        let mut second = open_private_append_no_follow(&path).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match second.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(error) => panic!("{error:?}"),
            }
        }
        assert!(second.metadata().unwrap().len() > 0);
        second.write_all(b"two\n").unwrap();
        second.sync_data().unwrap();
        drop(second);
        drop(reader);
        assert_eq!(std::fs::read(&path).unwrap(), b"one\ntwo\n");
        let _ = std::fs::remove_file(&path);
    }
}
