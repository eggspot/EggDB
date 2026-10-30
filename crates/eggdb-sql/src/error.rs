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
    /// `57014` query_canceled, such as a `COPY` the client failed with `CopyFail`.
    pub const QUERY_CANCELED: Self = Self(*b"57014");
    /// `42501` insufficient_privilege, such as `COPY` to or from a server file.
    pub const INSUFFICIENT_PRIVILEGE: Self = Self(*b"42501");
    /// `57P01` admin_shutdown: the server is stopping.
    pub const ADMIN_SHUTDOWN: Self = Self(*b"57P01");
    /// `0A000` feature_not_supported: valid SQL that this release does not run yet.
    pub const FEATURE_NOT_SUPPORTED: Self = Self(*b"0A000");
    /// `09000` triggered_action_exception: a trigger's body ended the statement that fired
    /// it, as SQLite's `RAISE(IGNORE)` does for the row being changed.
    pub const TRIGGERED_ACTION_EXCEPTION: Self = Self(*b"09000");
    /// `45000`: the SQLSTATE MySQL's `SIGNAL` uses for an unhandled user-defined condition.
    pub const USER_DEFINED_CONDITION: Self = Self(*b"45000");
    /// `P0001` raise_exception: an error a statement raised on purpose, such as Transact-SQL's
    /// `THROW` and `RAISERROR`.
    pub const RAISE_EXCEPTION: Self = Self(*b"P0001");
    /// `P0002` no_data_found: a PL/pgSQL `INTO STRICT` that found no row.
    pub const NO_DATA_FOUND: Self = Self(*b"P0002");
    /// `P0003` too_many_rows: a PL/pgSQL `INTO STRICT` that found more than one row.
    pub const TOO_MANY_ROWS: Self = Self(*b"P0003");
    /// `P0004` assert_failure: a PL/pgSQL `ASSERT` whose condition was not true.
    pub const ASSERT_FAILURE: Self = Self(*b"P0004");
    /// `20000` case_not_found: a PL/pgSQL `CASE` statement with no matching branch and no `ELSE`.
    pub const CASE_NOT_FOUND: Self = Self(*b"20000");
    /// `22005` error_in_assignment, such as assigning to a `CONSTANT` PL/pgSQL variable.
    pub const ERROR_IN_ASSIGNMENT: Self = Self(*b"22005");
    /// `2F005` function_executed_no_return_statement: a function's body ended without `RETURN`.
    pub const FUNCTION_EXECUTED_NO_RETURN_STATEMENT: Self = Self(*b"2F005");
    /// `0Z002` stacked_diagnostics_accessed_without_active_handler.
    pub const STACKED_DIAGNOSTICS_ACCESSED_WITHOUT_ACTIVE_HANDLER: Self = Self(*b"0Z002");
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
    /// `22000` data_exception, such as a range whose lower bound is above its upper bound.
    pub const DATA_EXCEPTION: Self = Self(*b"22000");
    /// `22023` invalid_parameter_value.
    pub const INVALID_PARAMETER_VALUE: Self = Self(*b"22023");
    /// `22011` substring_error.
    pub const SUBSTRING_ERROR: Self = Self(*b"22011");
    /// `22004` null_value_not_allowed.
    pub const NULL_VALUE_NOT_ALLOWED: Self = Self(*b"22004");
    /// `22015` interval_field_overflow.
    pub const INTERVAL_FIELD_OVERFLOW: Self = Self(*b"22015");
    /// `2202E` array_subscript_error.
    pub const ARRAY_SUBSCRIPT_ERROR: Self = Self(*b"2202E");
    /// `2201B` invalid_regular_expression.
    pub const INVALID_REGULAR_EXPRESSION: Self = Self(*b"2201B");
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
    /// `22P04` bad_copy_file_format: `COPY` data that does not follow its format.
    pub const BAD_COPY_FILE_FORMAT: Self = Self(*b"22P04");
    /// `22P05` untranslatable_character: a character with no equivalent in the other
    /// encoding of a conversion.
    pub const UNTRANSLATABLE_CHARACTER: Self = Self(*b"22P05");
    /// `23502` not_null_violation.
    pub const NOT_NULL_VIOLATION: Self = Self(*b"23502");
    /// `23505` unique_violation.
    pub const UNIQUE_VIOLATION: Self = Self(*b"23505");
    /// `23503` foreign_key_violation.
    pub const FOREIGN_KEY_VIOLATION: Self = Self(*b"23503");
    /// `23514` check_violation.
    pub const CHECK_VIOLATION: Self = Self(*b"23514");
    /// `2200H` sequence_generator_limit_exceeded.
    pub const SEQUENCE_GENERATOR_LIMIT_EXCEEDED: Self = Self(*b"2200H");
    /// `2BP01` dependent_objects_still_exist.
    pub const DEPENDENT_OBJECTS_STILL_EXIST: Self = Self(*b"2BP01");
    /// `42710` duplicate_object.
    pub const DUPLICATE_OBJECT: Self = Self(*b"42710");
    /// `42830` invalid_foreign_key.
    pub const INVALID_FOREIGN_KEY: Self = Self(*b"42830");
    /// `3F000` invalid_schema_name.
    pub const INVALID_SCHEMA_NAME: Self = Self(*b"3F000");
    /// `42602` invalid_name.
    pub const INVALID_NAME: Self = Self(*b"42602");
    /// `25001` active_sql_transaction.
    pub const ACTIVE_SQL_TRANSACTION: Self = Self(*b"25001");
    /// `25006` read_only_sql_transaction.
    pub const READ_ONLY_SQL_TRANSACTION: Self = Self(*b"25006");
    /// `25P01` no_active_sql_transaction.
    pub const NO_ACTIVE_SQL_TRANSACTION: Self = Self(*b"25P01");
    /// `25P02` in_failed_sql_transaction.
    pub const IN_FAILED_SQL_TRANSACTION: Self = Self(*b"25P02");
    /// `40000` transaction_rollback.
    pub const TRANSACTION_ROLLBACK: Self = Self(*b"40000");
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
    /// `428C9` generated_always: a value given for a `GENERATED ALWAYS` identity column.
    pub const GENERATED_ALWAYS: Self = Self(*b"428C9");
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
    /// `42725` ambiguous_function.
    pub const AMBIGUOUS_FUNCTION: Self = Self(*b"42725");
    /// `42723` duplicate_function.
    pub const DUPLICATE_FUNCTION: Self = Self(*b"42723");
    /// `42P13` invalid_function_definition.
    pub const INVALID_FUNCTION_DEFINITION: Self = Self(*b"42P13");
    /// `42P01` undefined_table.
    pub const UNDEFINED_TABLE: Self = Self(*b"42P01");
    /// `42P07` duplicate_table.
    pub const DUPLICATE_TABLE: Self = Self(*b"42P07");
    /// `42P10` invalid_column_reference.
    pub const INVALID_COLUMN_REFERENCE: Self = Self(*b"42P10");
    /// `42P19` invalid_recursion.
    pub const INVALID_RECURSION: Self = Self(*b"42P19");
    /// `42P20` windowing_error.
    pub const WINDOWING_ERROR: Self = Self(*b"42P20");
    /// `22013` invalid_preceding_or_following_size.
    pub const INVALID_PRECEDING_OR_FOLLOWING_SIZE: Self = Self(*b"22013");
    /// `22014` invalid_argument_for_ntile_function.
    pub const INVALID_ARGUMENT_FOR_NTILE: Self = Self(*b"22014");
    /// `22016` invalid_argument_for_nth_value_function.
    pub const INVALID_ARGUMENT_FOR_NTH_VALUE: Self = Self(*b"22016");
    /// `44000` with_check_option_violation.
    pub const WITH_CHECK_OPTION_VIOLATION: Self = Self(*b"44000");
    /// `42P16` invalid_table_definition.
    pub const INVALID_TABLE_DEFINITION: Self = Self(*b"42P16");
    /// `42P06` duplicate_schema.
    pub const DUPLICATE_SCHEMA: Self = Self(*b"42P06");
    /// `42939` reserved_name: a name kept for the system, such as `pg_catalog`.
    pub const RESERVED_NAME: Self = Self(*b"42939");
    /// `42P17` invalid_object_definition, such as a view that refers to itself.
    pub const INVALID_OBJECT_DEFINITION: Self = Self(*b"42P17");
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

    pub(crate) fn from_text(code: &str) -> Option<Self> {
        let bytes: [u8; 5] = code.as_bytes().try_into().ok()?;
        bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || byte.is_ascii_uppercase())
            .then_some(Self(bytes))
    }

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
    context: Option<String>,
    raised: Option<Raised>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Raised {
    number: u32,
    severity: u8,
    state: u8,
    ends_batch: bool,
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
                context: None,
                raised: None,
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

    /// The same error with a context line, such as `COPY t, line 3, column b: "x"`, which
    /// PostgreSQL reports in the `CONTEXT` field.
    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.report.context = Some(context.into());
        self
    }

    pub(crate) fn at(mut self, position: usize) -> Self {
        self.report.position = Some(position);
        self
    }

    pub(crate) fn without_position(mut self) -> Self {
        self.report.position = None;
        self
    }

    pub(crate) fn raised_in_sqlserver(
        number: u32,
        severity: u8,
        state: u8,
        message: impl Into<String>,
        ends_batch: bool,
    ) -> Self {
        let mut error = Self::new(SqlState::RAISE_EXCEPTION, message);
        error.report.vendor = Some(crate::Dialect::SqlServer);
        error.report.vendor_code = u16::try_from(number).ok();
        error.report.raised = Some(Raised {
            number,
            severity,
            state,
            ends_batch,
        });
        error
    }

    pub(crate) fn ends_sqlserver_batch(&self) -> Option<bool> {
        self.report.raised.map(|raised| raised.ends_batch)
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

    pub(crate) fn with_names_displayed(mut self) -> Self {
        let report = &mut self.report;
        for text in [&mut report.message]
            .into_iter()
            .chain(report.detail.as_mut())
            .chain(report.hint.as_mut())
            .chain(report.table.as_mut())
            .chain(report.context.as_mut())
        {
            if text.contains(crate::schemas::QUALIFIER) {
                *text = crate::schemas::display(text);
            }
        }
        self
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

    /// The same error with a SQL Server error number, such as `208` for `Invalid object
    /// name`; [`SqlError::severity`] gives its severity.
    pub fn with_sqlserver_code(mut self, code: u16) -> Self {
        self.report.vendor_code = Some(code);
        self.report.vendor = Some(crate::Dialect::SqlServer);
        self
    }

    /// The severity SQL Server reports with an error of its numbering (`Level 16` in
    /// `Msg 208, Level 16, State 1`), or `None` for an error numbered by another dialect.
    pub fn severity(&self) -> Option<u8> {
        if self.report.vendor != Some(crate::Dialect::SqlServer) {
            return None;
        }
        if let Some(raised) = self.report.raised {
            return Some(raised.severity);
        }
        self.report
            .vendor_code
            .map(crate::sqlserver_errors::severity_of)
    }

    /// The error number SQL Server reports with an error of its numbering, such as `208` or
    /// the number a `THROW` or `RAISERROR` chose (up to 2147483647), or `None` for an error
    /// numbered by another dialect.
    pub fn sqlserver_number(&self) -> Option<u32> {
        if self.report.vendor != Some(crate::Dialect::SqlServer) {
            return None;
        }
        match self.report.raised {
            Some(raised) => Some(raised.number),
            None => self.report.vendor_code.map(u32::from),
        }
    }

    /// The state SQL Server reports with an error of its numbering (`State 1` in `Msg 208,
    /// Level 16, State 1`): the one a `THROW` or `RAISERROR` chose, else 1; `None` for an error
    /// numbered by another dialect.
    pub fn sqlserver_state(&self) -> Option<u8> {
        if self.report.vendor != Some(crate::Dialect::SqlServer) {
            return None;
        }
        Some(self.report.raised.map_or(1, |raised| raised.state))
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

    /// The context line, when there is one: where in a `COPY`'s data the error was found.
    pub fn context(&self) -> Option<&str> {
        self.report.context.as_deref()
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
        if self.report.vendor == Some(crate::Dialect::SqlServer) {
            let code = self.sqlserver_number().unwrap_or(0);
            let severity = self.severity().unwrap_or(16);
            let state = self.sqlserver_state().unwrap_or(1);
            return write!(
                f,
                "Msg {code}, Level {severity}, State {state}: {}",
                self.report.message
            );
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
