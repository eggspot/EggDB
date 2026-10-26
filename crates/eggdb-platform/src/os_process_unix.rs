use crate::error::Result;
use crate::process::ChildExit;
use std::ffi::{c_int, c_long};
use std::io;
use std::process::Child;

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

pub(crate) fn wait_with_peak_memory(child: Child) -> Result<ChildExit> {
    let pid =
        c_int::try_from(child.id()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut status: c_int = 0;
    let mut usage = ResourceUsage::default();
    loop {
        // SAFETY: `status` and `usage` are live, writable and laid out as `wait4` expects on
        // this 64-bit target, and `pid` names a child of this process that `Child` has not
        // waited for, so no other wait can reap it first.
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
        code: exit_code(status),
        peak_resident_bytes: u64::try_from(usage.peak_resident).unwrap_or(0) * PEAK_UNIT_BYTES,
    })
}

fn exit_code(status: c_int) -> Option<i32> {
    let ended_by_signal = status & 0x7f != 0;
    (!ended_by_signal).then_some((status >> 8) & 0xff)
}

#[cfg(test)]
mod tests {
    use super::exit_code;

    #[test]
    fn a_wait_status_gives_the_exit_code_unless_a_signal_ended_the_child() {
        assert_eq!(exit_code(0), Some(0));
        assert_eq!(exit_code(3 << 8), Some(3));
        assert_eq!(exit_code(9), None);
    }
}
