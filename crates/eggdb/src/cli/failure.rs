use super::args::CommandName;
use eggdb::ErrorKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exit {
    Done = 0,
    NotFound = 1,
    Usage = 2,
    DatabaseError = 3,
    Damaged = 4,
    Busy = 5,
}

impl Exit {
    pub(crate) fn code(self) -> u8 {
        self as u8
    }

    pub(crate) fn for_error(kind: ErrorKind) -> Self {
        match kind {
            ErrorKind::Corrupt => Self::Damaged,
            ErrorKind::Locked | ErrorKind::WriterActive => Self::Busy,
            _ => Self::DatabaseError,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    Usage {
        message: String,
        command: Option<CommandName>,
    },
    Database {
        message: String,
        try_command: Option<String>,
        exit: Exit,
    },
    OutputClosed,
    OutputFailed {
        message: String,
    },
}

impl Failure {
    pub(crate) fn usage(message: impl Into<String>, command: Option<CommandName>) -> Self {
        Self::Usage {
            message: message.into(),
            command,
        }
    }

    pub(crate) fn database(message: impl Into<String>, exit: Exit) -> Self {
        Self::Database {
            message: message.into(),
            try_command: None,
            exit,
        }
    }

    pub(crate) fn exit(&self) -> Exit {
        match self {
            Self::Usage { .. } => Exit::Usage,
            Self::Database { exit, .. } => *exit,
            Self::OutputClosed => Exit::Done,
            Self::OutputFailed { .. } => Exit::DatabaseError,
        }
    }
}

pub(crate) fn output_failure(error: std::io::Error) -> Failure {
    if error.kind() == std::io::ErrorKind::BrokenPipe {
        Failure::OutputClosed
    } else {
        Failure::OutputFailed {
            message: format!("cannot write the output: {error}"),
        }
    }
}
