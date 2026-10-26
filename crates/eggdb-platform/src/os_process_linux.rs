use crate::error::{Error, ErrorKind, Result};
use crate::os_wait_status::WaitStatus;
use crate::process::ChildExit;
use std::ffi::{c_int, c_long};
use std::io;
use std::os::unix::process::CommandExt;
use std::process::Command;

const PTRACE_TRACEME: c_int = 0;
const PTRACE_CONT: c_int = 7;
const PTRACE_SETOPTIONS: c_int = 0x4200;
const PTRACE_O_TRACEEXIT: usize = 0x40;
const PTRACE_O_EXITKILL: usize = 0x10_0000;
const PTRACE_EVENT_EXIT: c_int = 6;
const SIGTRAP: c_int = 5;
const PEAK_FIELD: &str = "VmHWM:";

unsafe extern "C" {
    fn ptrace(request: c_int, ...) -> c_long;
    fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
}

fn trace_me() -> io::Result<()> {
    // SAFETY: PTRACE_TRACEME takes no pointers and only marks this process as traced by its
    // parent; it is a single system call, safe between fork and exec.
    let traced = unsafe { ptrace(PTRACE_TRACEME, 0 as c_int, 0usize, 0usize) };
    if traced == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn request(request: c_int, pid: c_int, data: usize) -> Result<()> {
    // SAFETY: PTRACE_SETOPTIONS and PTRACE_CONT read `data` as a number, never as an address,
    // and `pid` is a child this process traces that is stopped, as both requests require.
    let answered = unsafe { ptrace(request, pid, 0usize, data) };
    if answered == -1 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

fn wait_for(pid: c_int) -> Result<WaitStatus> {
    let mut status: c_int = 0;
    loop {
        // SAFETY: `status` is a live, writable integer, and `pid` is a child of this process
        // that `Child` never waits for.
        let reaped = unsafe { waitpid(pid, &mut status, 0) };
        if reaped == pid {
            return Ok(WaitStatus::decode(status));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
}

fn peak_resident_bytes(pid: c_int) -> Result<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    parse_peak(&status).ok_or_else(|| Error::new(ErrorKind::Unsupported))
}

fn parse_peak(status: &str) -> Option<u64> {
    let line = status.lines().find(|line| line.starts_with(PEAK_FIELD))?;
    let kibibytes = line[PEAK_FIELD.len()..].trim().strip_suffix("kB")?.trim();
    kibibytes.parse::<u64>().ok()?.checked_mul(1024)
}

pub(crate) fn run_with_peak_memory(mut command: Command) -> Result<ChildExit> {
    // SAFETY: the closure runs in the child between fork and exec and makes one system call,
    // which is async-signal-safe and touches no memory shared with the parent.
    let command = unsafe { command.pre_exec(trace_me) };
    let child = command.spawn()?;
    let pid =
        c_int::try_from(child.id()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut traced_exit = false;
    let mut peak = None;
    loop {
        match wait_for(pid)? {
            WaitStatus::Stopped { event, .. } if event == PTRACE_EVENT_EXIT => {
                peak = Some(peak_resident_bytes(pid));
                request(PTRACE_CONT, pid, 0)?;
            }
            WaitStatus::Stopped { signal, .. } if signal == SIGTRAP && !traced_exit => {
                request(
                    PTRACE_SETOPTIONS,
                    pid,
                    PTRACE_O_TRACEEXIT | PTRACE_O_EXITKILL,
                )?;
                traced_exit = true;
                request(PTRACE_CONT, pid, 0)?;
            }
            WaitStatus::Stopped { signal, .. } => {
                request(PTRACE_CONT, pid, usize::try_from(signal).unwrap_or(0))?;
            }
            exit => {
                drop(child);
                return Ok(ChildExit {
                    code: exit.exit_code(),
                    peak_resident_bytes: peak.ok_or_else(|| Error::new(ErrorKind::Other))??,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_peak;

    #[test]
    fn the_peak_is_read_from_the_vm_hwm_line_in_kibibytes() {
        let status =
            "Name:\teggdb\nVmPeak:\t   10000 kB\nVmHWM:\t    1616 kB\nVmRSS:\t    1600 kB\n";
        assert_eq!(parse_peak(status), Some(1616 * 1024));
        assert_eq!(parse_peak("Name:\tzombie\n"), None);
        assert_eq!(parse_peak("VmHWM:\t12 MB\n"), None);
    }
}
