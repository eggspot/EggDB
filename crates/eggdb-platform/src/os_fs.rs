use crate::error::{Error, ErrorKind, Result};
use crate::fs::{File, FileSystem, OpenMode};
use crate::io::IoQueueOptions;
use crate::os_io::OsIoQueue;
use crate::uncached;
use std::fs;
use std::io;
use std::path::Path;

/// The operating system's file system.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsFileSystem;

impl OsFileSystem {
    /// Creates a handle to the operating system's file system.
    pub fn new() -> Self {
        Self
    }
}

impl FileSystem for OsFileSystem {
    type File = OsFile;
    type IoQueue = OsIoQueue;

    fn open(&self, path: &Path, mode: OpenMode) -> Result<OsFile> {
        let file = open_options(mode)
            .open(path)
            .map_err(|error| open_error(path, mode, error))?;
        refuse_directory(&file)?;
        Ok(OsFile::new(file, mode, false))
    }

    fn open_uncached(&self, path: &Path, mode: OpenMode) -> Result<OsFile> {
        let mut options = open_options(mode);
        uncached::request(&mut options)?;
        let opened = options
            .open(path)
            .map_err(|error| match directory_error(path, mode) {
                Some(kind) => Error::from(error).with_kind(kind),
                None => uncached::open_error(error),
            })
            .and_then(|file| refuse_directory(&file).map(|()| file))
            .and_then(|file| uncached::apply(&file).map(|()| file));
        match opened {
            Ok(file) => Ok(OsFile::new(file, mode, true)),
            Err(error) => {
                if mode == OpenMode::CreateNew && error.kind() == ErrorKind::Unsupported {
                    let _ = fs::remove_file(path);
                }
                Err(error)
            }
        }
    }

    fn remove_file(&self, path: &Path) -> Result<()> {
        fs::remove_file(path).map_err(|error| directory_given(path, error))
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        if fs::symlink_metadata(from).is_ok_and(|metadata| metadata.is_dir()) {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        fs::rename(from, to).map_err(|error| directory_given(to, error))
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        fs::create_dir_all(path).map_err(|error| {
            let through_a_file = error.kind() == io::ErrorKind::NotADirectory;
            let error = Error::from(error);
            if through_a_file {
                error.with_kind(ErrorKind::AlreadyExists)
            } else {
                error
            }
        })
    }

    fn sync_dir(&self, path: &Path) -> Result<()> {
        sync_directory(path)
    }

    fn exists(&self, path: &Path) -> Result<bool> {
        match path.try_exists() {
            Ok(exists) => Ok(exists),
            Err(error) if error.kind() == io::ErrorKind::NotADirectory => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn io_queue(&self, options: IoQueueOptions) -> Result<OsIoQueue> {
        OsIoQueue::new(options)
    }
}

fn open_options(mode: OpenMode) -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(mode.is_writable());
    match mode {
        OpenMode::Read | OpenMode::ReadWrite => {}
        OpenMode::Create => {
            options.create(true);
        }
        OpenMode::CreateNew => {
            options.create_new(true);
        }
    }
    options
}

fn is_directory(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

fn directory_error(path: &Path, mode: OpenMode) -> Option<ErrorKind> {
    match mode {
        _ if !is_directory(path) => None,
        OpenMode::CreateNew => Some(ErrorKind::AlreadyExists),
        OpenMode::Read | OpenMode::ReadWrite | OpenMode::Create => Some(ErrorKind::InvalidInput),
    }
}

fn open_error(path: &Path, mode: OpenMode, error: io::Error) -> Error {
    let kind = directory_error(path, mode);
    let error = Error::from(error);
    match kind {
        Some(kind) => error.with_kind(kind),
        None => error,
    }
}

fn directory_given(path: &Path, error: io::Error) -> Error {
    let error = Error::from(error);
    if is_directory(path) {
        error.with_kind(ErrorKind::InvalidInput)
    } else {
        error
    }
}

fn refuse_directory(file: &fs::File) -> Result<()> {
    if file.metadata()?.is_dir() {
        return Err(Error::new(ErrorKind::InvalidInput));
    }
    Ok(())
}

fn require_directory(directory: &fs::File) -> Result<()> {
    if directory.metadata()?.is_dir() {
        Ok(())
    } else {
        Err(Error::new(ErrorKind::InvalidInput))
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    let directory = fs::File::open(path)?;
    require_directory(&directory)?;
    Ok(directory.sync_all()?)
}

#[cfg(windows)]
fn sync_directory(path: &Path) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let directory = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    require_directory(&directory)?;
    Ok(directory.sync_all()?)
}

#[cfg(not(any(unix, windows)))]
fn sync_directory(path: &Path) -> Result<()> {
    if fs::metadata(path)?.is_dir() {
        Ok(())
    } else {
        Err(Error::new(ErrorKind::InvalidInput))
    }
}

/// A file opened through [`OsFileSystem`].
#[derive(Debug)]
pub struct OsFile {
    file: fs::File,
    writable: bool,
    uncached: bool,
    #[cfg(not(any(unix, windows)))]
    cursor: std::sync::Mutex<()>,
}

#[cfg(unix)]
impl std::os::fd::AsRawFd for OsFile {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.file.as_raw_fd()
    }
}

impl OsFile {
    pub(crate) fn is_writable(&self) -> bool {
        self.writable
    }

    fn require_writable(&self) -> Result<()> {
        if self.is_writable() {
            Ok(())
        } else {
            Err(Error::new(ErrorKind::PermissionDenied))
        }
    }

    fn new(file: fs::File, mode: OpenMode, uncached: bool) -> Self {
        Self {
            file,
            writable: mode.is_writable(),
            uncached,
            #[cfg(not(any(unix, windows)))]
            cursor: std::sync::Mutex::new(()),
        }
    }
}

impl File for OsFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        self.positioned_read(buf, offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize> {
        self.require_writable()?;
        if buf.is_empty() {
            return Ok(0);
        }
        self.positioned_write(buf, offset)
    }

    fn size(&self) -> Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    fn set_size(&self, size: u64) -> Result<()> {
        self.require_writable()?;
        Ok(self.file.set_len(size)?)
    }

    fn is_uncached(&self) -> bool {
        self.uncached
    }

    fn sync_data(&self) -> Result<()> {
        self.require_writable()?;
        Ok(self.file.sync_data()?)
    }

    fn sync_all(&self) -> Result<()> {
        self.require_writable()?;
        Ok(self.file.sync_all()?)
    }
}

#[cfg(unix)]
impl OsFile {
    fn positioned_read(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        use std::os::unix::fs::FileExt;
        Ok(self.file.read_at(buf, offset)?)
    }

    fn positioned_write(&self, buf: &[u8], offset: u64) -> Result<usize> {
        use std::os::unix::fs::FileExt;
        Ok(self.file.write_at(buf, offset)?)
    }
}

#[cfg(windows)]
impl OsFile {
    fn positioned_read(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        use std::os::windows::fs::FileExt;
        Ok(self.file.seek_read(buf, offset)?)
    }

    fn positioned_write(&self, buf: &[u8], offset: u64) -> Result<usize> {
        use std::os::windows::fs::FileExt;
        Ok(self.file.seek_write(buf, offset)?)
    }
}

#[cfg(not(any(unix, windows)))]
impl OsFile {
    fn positioned_read(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        use std::io::{Read, Seek, SeekFrom};
        let _cursor = self
            .cursor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut file = &self.file;
        file.seek(SeekFrom::Start(offset))?;
        Ok(file.read(buf)?)
    }

    fn positioned_write(&self, buf: &[u8], offset: u64) -> Result<usize> {
        use std::io::{Seek, SeekFrom, Write};
        let _cursor = self
            .cursor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut file = &self.file;
        file.seek(SeekFrom::Start(offset))?;
        Ok(file.write(buf)?)
    }
}
