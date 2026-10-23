use crate::error::{Error, ErrorKind, Result};
use crate::fs::{File, FileSystem, OpenMode};
use crate::uncached;
use std::fs;
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

    fn open(&self, path: &Path, mode: OpenMode) -> Result<OsFile> {
        Ok(OsFile::new(open_options(mode).open(path)?, mode, false))
    }

    fn open_uncached(&self, path: &Path, mode: OpenMode) -> Result<OsFile> {
        let mut options = open_options(mode);
        uncached::request(&mut options)?;
        let opened = options
            .open(path)
            .map_err(uncached::open_error)
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
        Ok(fs::remove_file(path)?)
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        Ok(fs::rename(from, to)?)
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        Ok(fs::create_dir_all(path)?)
    }

    fn sync_dir(&self, path: &Path) -> Result<()> {
        sync_directory(path)
    }

    fn exists(&self, path: &Path) -> Result<bool> {
        Ok(path.try_exists()?)
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

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    Ok(fs::File::open(path)?.sync_all()?)
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
    Ok(directory.sync_all()?)
}

#[cfg(not(any(unix, windows)))]
fn sync_directory(path: &Path) -> Result<()> {
    fs::metadata(path)?;
    Ok(())
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

impl OsFile {
    fn require_writable(&self) -> Result<()> {
        if self.writable {
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
