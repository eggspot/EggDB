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
const PTRACE_O_TRACEEXEC: usize = 0x10;
const PTRACE_O_TRACEEXIT: usize = 0x40;
const PTRACE_O_EXITKILL: usize = 0x10_0000;
const PTRACE_EVENT_EXEC: c_int = 4;
const PTRACE_EVENT_EXIT: c_int = 6;
const SIGTRAP: c_int = 5;
const SIGKILL: c_int = 9;
const PEAK_FIELD: &str = "VmHWM:";

unsafe extern "C" {
    fn ptrace(request: c_int, ...) -> c_long;
    fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
    fn kill(pid: c_int, signal: c_int) -> c_int;
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
    let end = trace_to_exit(pid);
    if end.leaves_the_child_unreaped() {
        kill_and_reap(pid);
    }
    drop(child);
    end.into_exit()
}

#[derive(Debug)]
enum TraceEnd {
    Reaped {
        code: Option<i32>,
        peak: Option<Result<u64>>,
    },
    RequestFailed(Error),
    WaitFailed(Error),
}

impl TraceEnd {
    fn leaves_the_child_unreaped(&self) -> bool {
        matches!(self, Self::RequestFailed(_))
    }

    fn into_exit(self) -> Result<ChildExit> {
        match self {
            Self::Reaped { code, peak } => Ok(ChildExit {
                code,
                peak_resident_bytes: peak.ok_or_else(|| Error::new(ErrorKind::Other))??,
            }),
            Self::RequestFailed(error) | Self::WaitFailed(error) => Err(error),
        }
    }
}

fn trace_to_exit(pid: c_int) -> TraceEnd {
    let mut options_set = false;
    let mut peak = None;
    loop {
        let status = match wait_for(pid) {
            Ok(status) => status,
            Err(error) => return TraceEnd::WaitFailed(error),
        };
        let continued = match status {
            WaitStatus::Stopped { event, .. } if event == PTRACE_EVENT_EXIT => {
                peak = Some(peak_resident_bytes(pid));
                request(PTRACE_CONT, pid, 0)
            }
            WaitStatus::Stopped { event, .. } if event == PTRACE_EVENT_EXEC => {
                request(PTRACE_CONT, pid, 0)
            }
            WaitStatus::Stopped { signal, .. } if signal == SIGTRAP && !options_set => {
                options_set = true;
                request(
                    PTRACE_SETOPTIONS,
                    pid,
                    PTRACE_O_TRACEEXEC | PTRACE_O_TRACEEXIT | PTRACE_O_EXITKILL,
                )
                .and_then(|()| request(PTRACE_CONT, pid, 0))
            }
            WaitStatus::Stopped { signal, .. } => {
                request(PTRACE_CONT, pid, usize::try_from(signal).unwrap_or(0))
            }
            exit => {
                return TraceEnd::Reaped {
                    code: exit.exit_code(),
                    peak,
                };
            }
        };
        if let Err(error) = continued {
            return TraceEnd::RequestFailed(error);
        }
    }
}

fn kill_and_reap(pid: c_int) {
    // SAFETY: `kill` takes no pointers, and it is only called after a ptrace request failed on
    // `pid`, a child this process traces and has not reaped (no wait returned its exit), so the
    // number still names that child and cannot name another process.
    let _ = unsafe { kill(pid, SIGKILL) };
    loop {
        match wait_for(pid) {
            Ok(WaitStatus::Stopped { .. }) => {
                let _ = request(PTRACE_CONT, pid, 0);
            }
            Ok(WaitStatus::Exited(_) | WaitStatus::Signaled) | Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TraceEnd, parse_peak};
    use crate::error::{Error, ErrorKind};

    #[test]
    fn only_a_failed_request_leaves_the_child_to_kill_and_a_reaped_child_is_never_killed() {
        let reaped_without_a_peak = TraceEnd::Reaped {
            code: None,
            peak: None,
        };
        assert!(!reaped_without_a_peak.leaves_the_child_unreaped());
        assert_eq!(
            reaped_without_a_peak.into_exit().unwrap_err().kind(),
            ErrorKind::Other
        );
        let reaped_with_an_unreadable_peak = TraceEnd::Reaped {
            code: Some(0),
            peak: Some(Err(Error::new(ErrorKind::NotFound))),
        };
        assert!(!reaped_with_an_unreadable_peak.leaves_the_child_unreaped());
        assert_eq!(
            reaped_with_an_unreadable_peak
                .into_exit()
                .unwrap_err()
                .kind(),
            ErrorKind::NotFound
        );
        let wait_failed = TraceEnd::WaitFailed(Error::new(ErrorKind::Other));
        assert!(!wait_failed.leaves_the_child_unreaped());
        let request_failed = TraceEnd::RequestFailed(Error::new(ErrorKind::PermissionDenied));
        assert!(request_failed.leaves_the_child_unreaped());
        let exit = TraceEnd::Reaped {
            code: Some(3),
            peak: Some(Ok(4096)),
        }
        .into_exit()
        .unwrap();
        assert_eq!((exit.code, exit.peak_resident_bytes), (Some(3), 4096));
    }

    #[test]
    fn the_peak_is_read_from_the_vm_hwm_line_in_kibibytes() {
        let status =
            "Name:\teggdb\nVmPeak:\t   10000 kB\nVmHWM:\t    1616 kB\nVmRSS:\t    1600 kB\n";
        assert_eq!(parse_peak(status), Some(1616 * 1024));
        assert_eq!(parse_peak("Name:\tzombie\n"), None);
        assert_eq!(parse_peak("VmHWM:\t12 MB\n"), None);
    }
}
