//! Copy-on-write clones of whole files, where the file system makes them (D1021): APFS's
//! `clonefile` on Apple systems, the `FICLONE` ioctl on Linux file systems with shared
//! extents (Btrfs, XFS, bcachefs), and block cloning on Windows' ReFS. A clone takes no time
//! and no space until either file changes.

use std::path::Path;

/// Clones the file at `source` to the new file `target`, sharing its blocks until either file
/// changes. `Ok(false)` when the file system or the platform cannot clone, so the caller copies
/// instead; no file is left at `target` then.
pub fn clone_file(source: &Path, target: &Path) -> std::io::Result<bool> {
    imp::clone_file(source, target)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod imp {
    use std::ffi::{CString, c_char, c_int};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    const CLONE_NOFOLLOW: u32 = 0x0001;
    const EXDEV: i32 = 18;
    const ENOTSUP: i32 = 45;

    unsafe extern "C" {
        fn clonefile(source: *const c_char, target: *const c_char, flags: u32) -> c_int;
    }

    fn c_path(path: &Path) -> std::io::Result<CString> {
        CString::new(path.as_os_str().as_bytes()).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "a path holds a NUL byte"))
    }

    pub(super) fn clone_file(source: &Path, target: &Path) -> std::io::Result<bool> {
        let source = c_path(source)?;
        let target = c_path(target)?;
        // SAFETY: both pointers are NUL-terminated paths owned by the CStrings above, alive for
        // the call; clonefile reads them and keeps neither.
        let result = unsafe { clonefile(source.as_ptr(), target.as_ptr(), CLONE_NOFOLLOW) };
        if result == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error().is_some_and(|code| [EXDEV, ENOTSUP].contains(&code)) {
            return Ok(false);
        }
        Err(error)
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp {
    use std::ffi::{c_int, c_ulong};
    use std::os::fd::AsRawFd;
    use std::path::Path;

    const FICLONE: c_ulong = 0x4004_9409;
    const EXDEV: i32 = 18;
    const EINVAL: i32 = 22;
    const ENOTTY: i32 = 25;
    const EOPNOTSUPP: i32 = 95;

    unsafe extern "C" {
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    }

    pub(super) fn clone_file(source: &Path, target: &Path) -> std::io::Result<bool> {
        let from = std::fs::File::open(source)?;
        let to = std::fs::OpenOptions::new().write(true).create_new(true).open(target)?;
        // SAFETY: both descriptors belong to files opened above and stay open for the call;
        // FICLONE takes the source descriptor as its one argument and writes no memory of ours.
        let result = unsafe { ioctl(to.as_raw_fd(), FICLONE, from.as_raw_fd()) };
        if result == 0 {
            to.sync_all()?;
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        drop(to);
        std::fs::remove_file(target)?;
        if error.raw_os_error().is_some_and(|code| [EXDEV, EINVAL, ENOTTY, EOPNOTSUPP].contains(&code)) {
            return Ok(false);
        }
        Err(error)
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    const FSCTL_DUPLICATE_EXTENTS_TO_FILE: u32 = 0x0009_8344;
    const FSCTL_GET_INTEGRITY_INFORMATION: u32 = 0x0009_027C;
    const FILE_SUPPORTS_BLOCK_REFCOUNTING: u32 = 0x0800_0000;
    const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x0200;
    const MOST_BYTES_PER_CLONE: u64 = 1 << 31;

    #[repr(C)]
    struct DuplicateExtentsData {
        file_handle: *mut c_void,
        source_file_offset: i64,
        target_file_offset: i64,
        byte_count: i64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct IntegrityInformation {
        _checksum_algorithm: u16,
        _reserved: u16,
        flags: u32,
        _checksum_chunk_size_in_bytes: u32,
        cluster_size_in_bytes: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct FileInformation {
        attributes: u32,
        _creation: [u32; 2],
        _last_access: [u32; 2],
        _last_write: [u32; 2],
        _volume_serial: u32,
        _size_high: u32,
        _size_low: u32,
        _links: u32,
        _index_high: u32,
        _index_low: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn DeviceIoControl(
            device: *mut c_void,
            code: u32,
            input: *const c_void,
            input_size: u32,
            output: *mut c_void,
            output_size: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        fn GetVolumeInformationByHandleW(
            file: *mut c_void,
            volume_name: *mut u16,
            volume_name_size: u32,
            serial: *mut u32,
            component_length: *mut u32,
            flags: *mut u32,
            file_system_name: *mut u16,
            file_system_name_size: u32,
        ) -> i32;
        fn GetFileInformationByHandle(file: *mut c_void, information: *mut FileInformation) -> i32;
    }

    fn supports_cloning(file: &std::fs::File) -> bool {
        let mut flags = 0u32;
        // SAFETY: the handle is an open file borrowed for the call; only `flags` is asked for, a
        // live u32, and every other out pointer is null with a size of zero, as the API allows.
        let read = unsafe {
            GetVolumeInformationByHandleW(
                file.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut flags,
                std::ptr::null_mut(),
                0,
            )
        };
        read != 0 && flags & FILE_SUPPORTS_BLOCK_REFCOUNTING != 0
    }

    fn is_sparse(file: &std::fs::File) -> bool {
        let mut information = FileInformation::default();
        // SAFETY: the handle is an open file borrowed for the call, and `information` a live
        // BY_HANDLE_FILE_INFORMATION it fills.
        let read = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &raw mut information) };
        read == 0 || information.attributes & FILE_ATTRIBUTE_SPARSE_FILE != 0
    }

    fn cluster_size(file: &std::fs::File) -> Option<u64> {
        let mut information = IntegrityInformation::default();
        let mut returned = 0u32;
        // SAFETY: the handle is an open file borrowed for the call; the output buffer is a live
        // FSCTL_GET_INTEGRITY_INFORMATION_BUFFER of the size given, and `returned` a live u32.
        let read = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                FSCTL_GET_INTEGRITY_INFORMATION,
                std::ptr::null(),
                0,
                (&raw mut information).cast(),
                std::mem::size_of::<IntegrityInformation>() as u32,
                &raw mut returned,
                std::ptr::null_mut(),
            )
        };
        (read != 0 && information.cluster_size_in_bytes > 0 && information.flags == 0)
            .then_some(u64::from(information.cluster_size_in_bytes))
    }

    pub(super) fn clone_file(source: &Path, target: &Path) -> std::io::Result<bool> {
        let from = std::fs::File::open(source)?;
        if !supports_cloning(&from) || is_sparse(&from) {
            return Ok(false);
        }
        let Some(cluster) = cluster_size(&from) else {
            return Ok(false);
        };
        let length = from.metadata()?.len();
        let to = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(target)?;
        let cloned = (|| -> std::io::Result<bool> {
            to.set_len(length)?;
            let rounded = length.div_ceil(cluster) * cluster;
            let mut offset = 0u64;
            while offset < rounded {
                let count = (rounded - offset).min(MOST_BYTES_PER_CLONE);
                let request = DuplicateExtentsData {
                    file_handle: from.as_raw_handle(),
                    source_file_offset: offset as i64,
                    target_file_offset: offset as i64,
                    byte_count: count as i64,
                };
                let mut returned = 0u32;
                // SAFETY: both handles are open files borrowed for the call; the input is a live
                // DUPLICATE_EXTENTS_DATA of the size given, there is no output, and `returned` is
                // a live u32.
                let done = unsafe {
                    DeviceIoControl(
                        to.as_raw_handle(),
                        FSCTL_DUPLICATE_EXTENTS_TO_FILE,
                        (&raw const request).cast(),
                        std::mem::size_of::<DuplicateExtentsData>() as u32,
                        std::ptr::null_mut(),
                        0,
                        &raw mut returned,
                        std::ptr::null_mut(),
                    )
                };
                if done == 0 {
                    return Ok(false);
                }
                offset += count;
            }
            to.sync_all()?;
            Ok(true)
        })();
        match cloned {
            Ok(true) => Ok(true),
            outcome => {
                drop(to);
                std::fs::remove_file(target)?;
                outcome
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "linux", target_os = "android", windows)))]
mod imp {
    use std::path::Path;

    pub(super) fn clone_file(_source: &Path, _target: &Path) -> std::io::Result<bool> {
        Ok(false)
    }
}
