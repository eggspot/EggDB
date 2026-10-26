use crate::error::Result;
use crate::process::ChildExit;
use std::ffi::c_void;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::process::Child;

#[repr(C)]
#[derive(Default)]
struct ProcessMemoryCounters {
    size: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn K32GetProcessMemoryInfo(
        process: *mut c_void,
        counters: *mut ProcessMemoryCounters,
        size: u32,
    ) -> i32;
}

pub(crate) fn wait_with_peak_memory(mut child: Child) -> Result<ChildExit> {
    let status = child.wait()?;
    let size = u32::try_from(size_of::<ProcessMemoryCounters>())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut counters = ProcessMemoryCounters {
        size,
        ..ProcessMemoryCounters::default()
    };
    // SAFETY: the handle belongs to `child`, which keeps it open until it is dropped after
    // this call, and `counters` is a live, writable structure of the size passed.
    let succeeded = unsafe { K32GetProcessMemoryInfo(child.as_raw_handle(), &mut counters, size) };
    if succeeded == 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(ChildExit {
        code: status.code(),
        peak_resident_bytes: counters.peak_working_set_size as u64,
    })
}
