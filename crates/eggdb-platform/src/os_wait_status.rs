use std::ffi::c_int;

const TERMINATING_SIGNAL_MASK: c_int = 0x7f;
const STOPPED: c_int = 0x7f;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitStatus {
    Exited(i32),
    Signaled,
    Stopped { signal: c_int, event: c_int },
}

impl WaitStatus {
    pub(crate) fn decode(status: c_int) -> Self {
        let low = status & 0xff;
        if low == STOPPED {
            Self::Stopped {
                signal: (status >> 8) & 0xff,
                event: (status >> 16) & 0xff,
            }
        } else if status & TERMINATING_SIGNAL_MASK == 0 {
            Self::Exited((status >> 8) & 0xff)
        } else {
            Self::Signaled
        }
    }

    pub(crate) fn exit_code(self) -> Option<i32> {
        match self {
            Self::Exited(code) => Some(code),
            Self::Signaled | Self::Stopped { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::WaitStatus;

    #[test]
    fn wait_statuses_decode_into_exits_signals_and_stops() {
        assert_eq!(WaitStatus::decode(0), WaitStatus::Exited(0));
        assert_eq!(WaitStatus::decode(3 << 8), WaitStatus::Exited(3));
        assert_eq!(WaitStatus::decode(9), WaitStatus::Signaled);
        assert_eq!(WaitStatus::decode(0x80 | 6), WaitStatus::Signaled);
        assert_eq!(
            WaitStatus::decode(0x7f | (5 << 8)),
            WaitStatus::Stopped {
                signal: 5,
                event: 0
            }
        );
        assert_eq!(
            WaitStatus::decode(0x7f | (5 << 8) | (6 << 16)),
            WaitStatus::Stopped {
                signal: 5,
                event: 6
            }
        );
        assert_eq!(WaitStatus::decode(3 << 8).exit_code(), Some(3));
        assert_eq!(WaitStatus::decode(9).exit_code(), None);
    }
}
