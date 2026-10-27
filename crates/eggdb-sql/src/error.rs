use std::fmt;

/// A five-character SQLSTATE code, as PostgreSQL's documentation lists them in its appendix
/// "PostgreSQL Error Codes".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SqlState([u8; 5]);

impl SqlState {
    /// `0A000` feature_not_supported: valid SQL that this release does not run yet.
    pub const FEATURE_NOT_SUPPORTED: Self = Self(*b"0A000");
    /// `21000` cardinality_violation.
    pub const CARDINALITY_VIOLATION: Self = Self(*b"21000");
    /// `22001` string_data_right_truncation.
    pub const STRING_DATA_RIGHT_TRUNCATION: Self = Self(*b"22001");
    /// `22003` numeric_value_out_of_range.
    pub const NUMERIC_VALUE_OUT_OF_RANGE: Self = Self(*b"22003");
    /// `22012` division_by_zero.
    pub const DIVISION_BY_ZERO: Self = Self(*b"22012");
    /// `22019` invalid_escape_character.
    pub const INVALID_ESCAPE_CHARACTER: Self = Self(*b"22019");
    /// `2201F` invalid_argument_for_power_function.
    pub const INVALID_ARGUMENT_FOR_POWER: Self = Self(*b"2201F");
    /// `2201W` invalid_row_count_in_limit_clause.
    pub const INVALID_ROW_COUNT_IN_LIMIT_CLAUSE: Self = Self(*b"2201W");
    /// `2201X` invalid_row_count_in_result_offset_clause.
    pub const INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE: Self = Self(*b"2201X");
    /// `22025` invalid_escape_sequence.
    pub const INVALID_ESCAPE_SEQUENCE: Self = Self(*b"22025");
    /// `22P02` invalid_text_representation.
    pub const INVALID_TEXT_REPRESENTATION: Self = Self(*b"22P02");
    /// `23502` not_null_violation.
    pub const NOT_NULL_VIOLATION: Self = Self(*b"23502");
    /// `23505` unique_violation.
    pub const UNIQUE_VIOLATION: Self = Self(*b"23505");
    /// `25001` active_sql_transaction.
    pub const ACTIVE_SQL_TRANSACTION: Self = Self(*b"25001");
    /// `25006` read_only_sql_transaction.
    pub const READ_ONLY_SQL_TRANSACTION: Self = Self(*b"25006");
    /// `25P01` no_active_sql_transaction.
    pub const NO_ACTIVE_SQL_TRANSACTION: Self = Self(*b"25P01");
    /// `25P02` in_failed_sql_transaction.
    pub const IN_FAILED_SQL_TRANSACTION: Self = Self(*b"25P02");
    /// `42601` syntax_error.
    pub const SYNTAX_ERROR: Self = Self(*b"42601");
    /// `42622` name_too_long.
    pub const NAME_TOO_LONG: Self = Self(*b"42622");
    /// `42701` duplicate_column.
    pub const DUPLICATE_COLUMN: Self = Self(*b"42701");
    /// `42702` ambiguous_column.
    pub const AMBIGUOUS_COLUMN: Self = Self(*b"42702");
    /// `42703` undefined_column.
    pub const UNDEFINED_COLUMN: Self = Self(*b"42703");
    /// `42704` undefined_object.
    pub const UNDEFINED_OBJECT: Self = Self(*b"42704");
    /// `42712` duplicate_alias.
    pub const DUPLICATE_ALIAS: Self = Self(*b"42712");
    /// `42803` grouping_error.
    pub const GROUPING_ERROR: Self = Self(*b"42803");
    /// `42804` datatype_mismatch.
    pub const DATATYPE_MISMATCH: Self = Self(*b"42804");
    /// `42809` wrong_object_type.
    pub const WRONG_OBJECT_TYPE: Self = Self(*b"42809");
    /// `42846` cannot_coerce.
    pub const CANNOT_COERCE: Self = Self(*b"42846");
    /// `42883` undefined_function.
    pub const UNDEFINED_FUNCTION: Self = Self(*b"42883");
    /// `42P01` undefined_table.
    pub const UNDEFINED_TABLE: Self = Self(*b"42P01");
    /// `42P07` duplicate_table.
    pub const DUPLICATE_TABLE: Self = Self(*b"42P07");
    /// `42P10` invalid_column_reference.
    pub const INVALID_COLUMN_REFERENCE: Self = Self(*b"42P10");
    /// `42P16` invalid_table_definition.
    pub const INVALID_TABLE_DEFINITION: Self = Self(*b"42P16");
    /// `53200` out_of_memory.
    pub const OUT_OF_MEMORY: Self = Self(*b"53200");
    /// `54000` program_limit_exceeded.
    pub const PROGRAM_LIMIT_EXCEEDED: Self = Self(*b"54000");
    /// `54011` too_many_columns.
    pub const TOO_MANY_COLUMNS: Self = Self(*b"54011");
    /// `54001` statement_too_complex.
    pub const STATEMENT_TOO_COMPLEX: Self = Self(*b"54001");
    /// `55P03` lock_not_available: another writer holds the database.
    pub const LOCK_NOT_AVAILABLE: Self = Self(*b"55P03");
    /// `58030` io_error.
    pub const IO_ERROR: Self = Self(*b"58030");
    /// `XX000` internal_error.
    pub const INTERNAL_ERROR: Self = Self(*b"XX000");
    /// `XX001` data_corrupted.
    pub const DATA_CORRUPTED: Self = Self(*b"XX001");

    /// The code as text, such as `"42601"`.
    pub fn code(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or("XX000")
    }
}

impl fmt::Display for SqlState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// Why a SQL statement failed, in PostgreSQL's terms: a SQLSTATE code, the primary message,
/// and optionally a detail, a hint and the position in the statement text it refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlError {
    report: Box<Report>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Report {
    state: SqlState,
    message: String,
    detail: Option<String>,
    hint: Option<String>,
    position: Option<usize>,
}

/// The result of SQL operations.
pub type SqlResult<T> = Result<T, SqlError>;

impl SqlError {
    /// An error with this SQLSTATE and primary message.
    pub fn new(state: SqlState, message: impl Into<String>) -> Self {
        Self {
            report: Box::new(Report {
                state,
                message: message.into(),
                detail: None,
                hint: None,
                position: None,
            }),
        }
    }

    pub(crate) fn not_supported(what: impl Into<String>) -> Self {
        Self::new(SqlState::FEATURE_NOT_SUPPORTED, what).with_hint(
            "EggDB runs a first part of the PostgreSQL dialect; docs/sql.md lists what runs today and docs/roadmap.md what comes next.",
        )
    }

    pub(crate) fn syntax(message: impl Into<String>, position: usize) -> Self {
        Self::new(SqlState::SYNTAX_ERROR, message).at(position)
    }

    /// The same error with a detail line.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.report.detail = Some(detail.into());
        self
    }

    /// The same error with a hint line.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.report.hint = Some(hint.into());
        self
    }

    pub(crate) fn at(mut self, position: usize) -> Self {
        self.report.position = Some(position);
        self
    }

    pub(crate) fn at_if_unplaced(mut self, position: usize) -> Self {
        if self.report.position.is_none() {
            self.report.position = Some(position);
        }
        self
    }

    /// The SQLSTATE code.
    pub fn state(&self) -> SqlState {
        self.report.state
    }

    /// The primary message, such as `relation "users" does not exist`.
    pub fn message(&self) -> &str {
        &self.report.message
    }

    /// The detail line, when there is one.
    pub fn detail(&self) -> Option<&str> {
        self.report.detail.as_deref()
    }

    /// The hint line, when there is one.
    pub fn hint(&self) -> Option<&str> {
        self.report.hint.as_deref()
    }

    /// The byte offset in the text given to [`Session::execute`](crate::Session::execute) that
    /// the error refers to, when it refers to one.
    pub fn position(&self) -> Option<usize> {
        self.report.position
    }
}

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (SQLSTATE {})",
            self.report.message, self.report.state
        )
    }
}

impl std::error::Error for SqlError {}
