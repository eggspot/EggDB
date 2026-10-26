use crate::error::Result;
use crate::os_wait_status::WaitStatus;
use crate::process::ChildExit;
use std::ffi::{c_int, c_long};
use std::io;
use std::process::Command;

#[cfg(target_vendor = "apple")]
const PEAK_UNIT_BYTES: u64 = 1;
#[cfg(not(target_vendor = "apple"))]
const PEAK_UNIT_BYTES: u64 = 1024;

const RESOURCE_COUNTERS_AFTER_PEAK: usize = 13;

#[repr(C)]
#[derive(Default)]
struct TimeValue {
    seconds: c_long,
    microseconds: c_long,
}

#[repr(C)]
#[derive(Default)]
struct ResourceUsage {
    user_time: TimeValue,
    system_time: TimeValue,
    peak_resident: c_long,
    other_counters: [c_long; RESOURCE_COUNTERS_AFTER_PEAK],
}

unsafe extern "C" {
    #[cfg_attr(target_os = "netbsd", link_name = "__wait450")]
    fn wait4(pid: c_int, status: *mut c_int, options: c_int, usage: *mut ResourceUsage) -> c_int;
}

pub(crate) fn run_with_peak_memory(mut command: Command) -> Result<ChildExit> {
    let child = command.spawn()?;
    let pid =
        c_int::try_from(child.id()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut status: c_int = 0;
    let mut usage = ResourceUsage::default();
    loop {
        // SAFETY: `status` and `usage` are live, writable and laid out as `wait4` expects on
        // this 64-bit target, and `pid` names the child just spawned, which `Child` never
        // waits for, so no other wait can reap it first.
        let reaped = unsafe { wait4(pid, &mut status, 0, &mut usage) };
        if reaped == pid {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
    drop(child);
    Ok(ChildExit {
        code: WaitStatus::decode(status).exit_code(),
        peak_resident_bytes: u64::try_from(usage.peak_resident)
            .unwrap_or(0)
            .saturating_mul(PEAK_UNIT_BYTES),
    })
}
