//! Which file a handle or a path names, so a program that locked a file can tell whether the
//! path still leads to it or was meanwhile renamed over (D974).

use crate::error::Result;
use std::path::Path;

/// The identity of a file on its volume: equal identities are the same file, whatever path or
/// handle reached it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    device: u64,
    index: u64,
}

#[cfg(unix)]
mod imp {
    use super::FileIdentity;
    use crate::error::Result;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    pub(super) fn of_file(file: &std::fs::File) -> Result<FileIdentity> {
        let metadata = file.metadata()?;
        Ok(FileIdentity {
            device: metadata.dev(),
            index: metadata.ino(),
        })
    }

    pub(super) fn at(path: &Path) -> Result<FileIdentity> {
        let metadata = std::fs::metadata(path)?;
        Ok(FileIdentity {
            device: metadata.dev(),
            index: metadata.ino(),
        })
    }
}

#[cfg(windows)]
mod imp {
    use super::FileIdentity;
    use crate::error::Result;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, RawHandle};
    use std::path::Path;

    const FILE_READ_ATTRIBUTES: u32 = 0x0080;
    const FILE_SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

    #[repr(C)]
    #[derive(Default)]
    struct FileInformation {
        attributes: u32,
        creation: [u32; 2],
        last_access: [u32; 2],
        last_write: [u32; 2],
        volume_serial: u32,
        size_high: u32,
        size_low: u32,
        links: u32,
        index_high: u32,
        index_low: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(file: RawHandle, information: *mut FileInformation) -> i32;
    }

    fn of_handle(handle: RawHandle) -> Result<FileIdentity> {
        let mut information = FileInformation::default();
        // SAFETY: `handle` is an open file handle borrowed for the call, and `information` a
        // live BY_HANDLE_FILE_INFORMATION it fills.
        let read = unsafe { GetFileInformationByHandle(handle, &raw mut information) };
        if read == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(FileIdentity {
            device: u64::from(information.volume_serial),
            index: (u64::from(information.index_high) << 32) | u64::from(information.index_low),
        })
    }

    pub(super) fn of_file(file: &std::fs::File) -> Result<FileIdentity> {
        of_handle(file.as_raw_handle())
    }

    pub(super) fn at(path: &Path) -> Result<FileIdentity> {
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_ALL)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        of_handle(file.as_raw_handle())
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use super::FileIdentity;
    use crate::error::{Error, ErrorKind, Result};
    use std::path::Path;

    pub(super) fn of_file(_file: &std::fs::File) -> Result<FileIdentity> {
        Err(Error::new(ErrorKind::Unsupported))
    }

    pub(super) fn at(_path: &Path) -> Result<FileIdentity> {
        Err(Error::new(ErrorKind::Unsupported))
    }
}

pub(crate) fn of_file(file: &std::fs::File) -> Result<FileIdentity> {
    imp::of_file(file)
}

/// The identity of the file `path` leads to now. Fails as opening the path would, with
/// [`ErrorKind::NotFound`](crate::ErrorKind::NotFound) when nothing is there, and with
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) where the platform cannot tell.
pub fn file_identity_at(path: &Path) -> Result<FileIdentity> {
    imp::at(path)
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::{file_identity_at, of_file};

    #[test]
    fn a_path_renamed_over_names_another_file() {
        let directory = std::env::temp_dir().join(format!("eggdb-identity-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&directory);
        let path = directory.join("data");
        let replacement = directory.join("data.new");
        std::fs::write(&path, b"old").unwrap();
        std::fs::write(&replacement, b"new").unwrap();
        let held = std::fs::File::open(&path).unwrap();
        let before = of_file(&held).unwrap();
        assert_eq!(file_identity_at(&path).unwrap(), before);
        std::fs::rename(&replacement, &path).unwrap();
        assert_ne!(file_identity_at(&path).unwrap(), before);
        drop(held);
        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_dir(&directory);
    }
}
