use std::fmt;

/// A five-character SQLSTATE code, as PostgreSQL's documentation lists them in its appendix
/// "PostgreSQL Error Codes".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SqlState([u8; 5]);

impl SqlState {
    /// `08006` connection_failure.
    pub const CONNECTION_FAILURE: Self = Self(*b"08006");
    /// `08P01` protocol_violation.
    pub const PROTOCOL_VIOLATION: Self = Self(*b"08P01");
    /// `26000` invalid_sql_statement_name: no prepared statement by that name.
    pub const INVALID_SQL_STATEMENT_NAME: Self = Self(*b"26000");
    /// `28000` invalid_authorization_specification.
    pub const INVALID_AUTHORIZATION_SPECIFICATION: Self = Self(*b"28000");
    /// `28P01` invalid_password.
    pub const INVALID_PASSWORD: Self = Self(*b"28P01");
    /// `34000` invalid_cursor_name: no portal by that name.
    pub const INVALID_CURSOR_NAME: Self = Self(*b"34000");
    /// `3D000` invalid_catalog_name: no database by that name.
    pub const INVALID_CATALOG_NAME: Self = Self(*b"3D000");
    /// `42P03` duplicate_cursor: a portal by that name exists.
    pub const DUPLICATE_CURSOR: Self = Self(*b"42P03");
    /// `42P05` duplicate_prepared_statement.
    pub const DUPLICATE_PREPARED_STATEMENT: Self = Self(*b"42P05");
    /// `53300` too_many_connections.
    pub const TOO_MANY_CONNECTIONS: Self = Self(*b"53300");
    /// `57P01` admin_shutdown: the server is stopping.
    pub const ADMIN_SHUTDOWN: Self = Self(*b"57P01");
    /// `0A000` feature_not_supported: valid SQL that this release does not run yet.
    pub const FEATURE_NOT_SUPPORTED: Self = Self(*b"0A000");
    /// `21000` cardinality_violation.
    pub const CARDINALITY_VIOLATION: Self = Self(*b"21000");
    /// `22001` string_data_right_truncation.
    pub const STRING_DATA_RIGHT_TRUNCATION: Self = Self(*b"22001");
    /// `22021` character_not_in_repertoire: bytes that are not valid UTF-8.
    pub const CHARACTER_NOT_IN_REPERTOIRE: Self = Self(*b"22021");
    /// `22003` numeric_value_out_of_range.
    pub const NUMERIC_VALUE_OUT_OF_RANGE: Self = Self(*b"22003");
    /// `22007` invalid_datetime_format.
    pub const INVALID_DATETIME_FORMAT: Self = Self(*b"22007");
    /// `22008` datetime_field_overflow: a date or time stamp outside its type's range.
    pub const DATETIME_FIELD_OVERFLOW: Self = Self(*b"22008");
    /// `22023` invalid_parameter_value.
    pub const INVALID_PARAMETER_VALUE: Self = Self(*b"22023");
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
    /// `22P03` invalid_binary_representation.
    pub const INVALID_BINARY_REPRESENTATION: Self = Self(*b"22P03");
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
    /// `42P19` invalid_recursion.
    pub const INVALID_RECURSION: Self = Self(*b"42P19");
    /// `42P16` invalid_table_definition.
    pub const INVALID_TABLE_DEFINITION: Self = Self(*b"42P16");
    /// `42P02` undefined_parameter: a `$n` without a value.
    pub const UNDEFINED_PARAMETER: Self = Self(*b"42P02");
    /// `42P18` indeterminate_datatype.
    pub const INDETERMINATE_DATATYPE: Self = Self(*b"42P18");
    /// `53200` out_of_memory.
    pub const OUT_OF_MEMORY: Self = Self(*b"53200");
    /// `54000` program_limit_exceeded.
    pub const PROGRAM_LIMIT_EXCEEDED: Self = Self(*b"54000");
    /// `54011` too_many_columns.
    pub const TOO_MANY_COLUMNS: Self = Self(*b"54011");
    /// `54001` statement_too_complex.
    pub const STATEMENT_TOO_COMPLEX: Self = Self(*b"54001");
    /// `55000` object_not_in_prerequisite_state.
    pub const OBJECT_NOT_IN_PREREQUISITE_STATE: Self = Self(*b"55000");
    /// `55P03` lock_not_available: another writer holds the database.
    pub const LOCK_NOT_AVAILABLE: Self = Self(*b"55P03");
    /// `55P02` cant_change_runtime_param.
    pub const CANT_CHANGE_RUNTIME_PARAM: Self = Self(*b"55P02");
    /// `58030` io_error.
    pub const IO_ERROR: Self = Self(*b"58030");
    /// `XX000` internal_error.
    pub const INTERNAL_ERROR: Self = Self(*b"XX000");
    /// `XX001` data_corrupted.
    pub const DATA_CORRUPTED: Self = Self(*b"XX001");
    /// `42S01`: MySQL's base table or view already exists.
    pub const MYSQL_TABLE_EXISTS: Self = Self(*b"42S01");
    /// `42S02`: MySQL's base table or view not found.
    pub const MYSQL_TABLE_NOT_FOUND: Self = Self(*b"42S02");
    /// `42S21`: MySQL's column already exists.
    pub const MYSQL_COLUMN_EXISTS: Self = Self(*b"42S21");
    /// `42S22`: MySQL's column not found.
    pub const MYSQL_COLUMN_NOT_FOUND: Self = Self(*b"42S22");
    /// `42000`: MySQL's syntax error or access rule violation.
    pub const MYSQL_SYNTAX_OR_ACCESS: Self = Self(*b"42000");
    /// `23000`: MySQL's integrity constraint violation.
    pub const MYSQL_INTEGRITY: Self = Self(*b"23000");
    /// `21S01`: MySQL's insert value list does not match the column list.
    pub const MYSQL_VALUE_COUNT: Self = Self(*b"21S01");
    /// `HY000`: MySQL's general error.
    pub const MYSQL_GENERAL: Self = Self(*b"HY000");
    /// `08S01`: MySQL's communication link failure.
    pub const MYSQL_COMMUNICATION: Self = Self(*b"08S01");

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
    vendor_code: Option<u16>,
    vendor: Option<crate::Dialect>,
    table: Option<String>,
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
                vendor_code: None,
                vendor: None,
                table: None,
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

    pub(crate) fn with_table(mut self, table: &str) -> Self {
        self.report.table = Some(table.to_owned());
        self
    }

    pub(crate) fn table(&self) -> Option<&str> {
        self.report.table.as_deref()
    }

    /// The same error with a MySQL error number, such as `1146`.
    pub fn with_vendor_code(mut self, code: u16) -> Self {
        self.report.vendor_code = Some(code);
        self.report.vendor = Some(crate::Dialect::MySql);
        self
    }

    /// The same error with a SQLite extended result code, such as `1555`
    /// (`SQLITE_CONSTRAINT_PRIMARYKEY`); [`crate::sqlite_result_code_name`] names it.
    pub fn with_sqlite_code(mut self, code: u16) -> Self {
        self.report.vendor_code = Some(code);
        self.report.vendor = Some(crate::Dialect::Sqlite);
        self
    }

    /// The same error with an Oracle error number, such as `942` for `ORA-00942`.
    pub fn with_oracle_code(mut self, code: u16) -> Self {
        self.report.vendor_code = Some(code);
        self.report.vendor = Some(crate::Dialect::Oracle);
        self
    }

    /// The database's own error number, when the dialect has them: MySQL's `1146` for a
    /// missing table, SQLite's extended result code `1` (`SQLITE_ERROR`) for a syntax error,
    /// and so on.
    pub fn vendor_code(&self) -> Option<u16> {
        self.report.vendor_code
    }

    /// The dialect whose numbering [`SqlError::vendor_code`] follows: MySQL's error numbers
    /// SQLite's result codes or Oracle's `ORA-` numbers.
    pub fn vendor(&self) -> Option<crate::Dialect> {
        self.report.vendor
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
        if self.report.vendor == Some(crate::Dialect::Sqlite) {
            return write!(f, "Error: {}", self.report.message);
        }
        if self.report.vendor == Some(crate::Dialect::Oracle) {
            let code = self.report.vendor_code.unwrap_or(0);
            return write!(f, "ORA-{code:05}: {}", self.report.message);
        }
        if let Some(code) = self.report.vendor_code {
            return write!(
                f,
                "ERROR {code} ({}): {}",
                self.report.state, self.report.message
            );
        }
        write!(
            f,
            "{} (SQLSTATE {})",
            self.report.message, self.report.state
        )
    }
}

impl std::error::Error for SqlError {}
