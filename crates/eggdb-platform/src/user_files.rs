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
    use super::{create_private_directory, open_private_append, open_private_rewrite};
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
}
