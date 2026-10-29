use super::args::CommandName;
use eggdb::ErrorKind;
use eggdb_commands::Category;

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

pub(crate) const USAGE_CODE: &str = "invalid_arguments";
const OUTPUT_FAILED_CODE: &str = "output_failed";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    Usage {
        message: String,
        command: Option<CommandName>,
        code: &'static str,
    },
    Database {
        message: String,
        try_command: Option<String>,
        exit: Exit,
        code: &'static str,
    },
    OutputClosed,
    OutputFailed {
        message: String,
    },
}

impl Failure {
    pub(crate) fn usage(message: impl Into<String>, command: Option<CommandName>) -> Self {
        Self::coded_usage(USAGE_CODE, message, command)
    }

    pub(crate) fn coded_usage(
        code: &'static str,
        message: impl Into<String>,
        command: Option<CommandName>,
    ) -> Self {
        Self::Usage {
            message: message.into(),
            command,
            code,
        }
    }

    pub(crate) fn database(message: impl Into<String>, exit: Exit) -> Self {
        Self::coded(code_for_exit(exit), message, exit)
    }

    pub(crate) fn coded(code: &'static str, message: impl Into<String>, exit: Exit) -> Self {
        Self::Database {
            message: message.into(),
            try_command: None,
            exit,
            code,
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

    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Usage { code, .. } | Self::Database { code, .. } => *code,
            Self::OutputClosed | Self::OutputFailed { .. } => OUTPUT_FAILED_CODE,
        }
    }

    pub(crate) fn category(&self) -> Category {
        eggdb_commands::category_of(self.code()).unwrap_or(Category::Failed)
    }
}

pub(crate) fn code_for_exit(exit: Exit) -> &'static str {
    match exit {
        Exit::NotFound => "not_found",
        Exit::Usage => USAGE_CODE,
        Exit::Damaged => "damaged",
        Exit::Busy => "database_busy",
        Exit::Done | Exit::DatabaseError => "database_failed",
    }
}

pub(crate) fn code_for_error(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::NotFound => "not_found",
        ErrorKind::PermissionDenied => "permission_denied",
        ErrorKind::Locked | ErrorKind::WriterActive => "database_busy",
        ErrorKind::NotADatabase => "not_a_database",
        ErrorKind::Incompatible => "incompatible",
        ErrorKind::Corrupt => "damaged",
        ErrorKind::TooLarge => "too_large",
        ErrorKind::CollectionNotFound => "collection_not_found",
        ErrorKind::AlreadyExists => "already_exists",
        _ => "database_failed",
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
