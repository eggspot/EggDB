pub(crate) const NO_DATA_FOUND_CODE: u16 = 1403;
pub(crate) const USER_EXCEPTION_CODE: u16 = 6510;
pub(crate) const APPLICATION_ERROR_LOWEST: i64 = -20999;
pub(crate) const APPLICATION_ERROR_HIGHEST: i64 = -20000;
const NO_DATA_FOUND_SQLCODE: i32 = 100;
const USER_EXCEPTION_SQLCODE: i32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PredefinedException {
    pub(crate) name: &'static str,
    pub(crate) code: u16,
    pub(crate) message: &'static str,
}

const fn predefined(name: &'static str, code: u16, message: &'static str) -> PredefinedException {
    PredefinedException {
        name,
        code,
        message,
    }
}

const PREDEFINED: [PredefinedException; 22] = [
    predefined(
        "access_into_null",
        6530,
        "Reference to uninitialized composite",
    ),
    predefined(
        "case_not_found",
        6592,
        "CASE not found while executing CASE statement",
    ),
    predefined(
        "collection_is_null",
        6531,
        "Reference to uninitialized collection",
    ),
    predefined("cursor_already_open", 6511, "PL/SQL: cursor already open"),
    predefined("dup_val_on_index", 1, "unique constraint violated"),
    predefined("invalid_cursor", 1001, "invalid cursor"),
    predefined("invalid_number", 1722, "invalid number"),
    predefined(
        "login_denied",
        1017,
        "invalid username/password; logon denied",
    ),
    predefined("no_data_found", NO_DATA_FOUND_CODE, "no data found"),
    predefined("no_data_needed", 6548, "no more rows needed"),
    predefined("not_logged_on", 1012, "not logged on"),
    predefined("program_error", 6501, "PL/SQL: program error"),
    predefined(
        "rowtype_mismatch",
        6504,
        "PL/SQL: Return types of Result Set variables or query do not match",
    ),
    predefined(
        "self_is_null",
        30625,
        "method dispatch on NULL SELF argument is disallowed",
    ),
    predefined("storage_error", 6500, "PL/SQL: storage error"),
    predefined("subscript_beyond_count", 6533, "Subscript beyond count"),
    predefined("subscript_outside_limit", 6532, "Subscript outside of limit"),
    predefined("sys_invalid_rowid", 1410, "invalid ROWID"),
    predefined(
        "timeout_on_resource",
        51,
        "timeout occurred while waiting for a resource",
    ),
    predefined(
        "too_many_rows",
        1422,
        "exact fetch returns more than requested number of rows",
    ),
    predefined("value_error", 6502, "PL/SQL: numeric or value error"),
    predefined("zero_divide", 1476, "divisor is equal to zero"),
];

pub(crate) fn predefined_by_name(name: &str) -> Option<PredefinedException> {
    PREDEFINED
        .iter()
        .find(|exception| exception.name == name)
        .copied()
}

pub(crate) fn predefined_by_code(code: u16) -> Option<PredefinedException> {
    PREDEFINED
        .iter()
        .find(|exception| exception.code == code)
        .copied()
}

pub(crate) fn sqlcode_of(code: u16) -> i32 {
    match code {
        NO_DATA_FOUND_CODE => NO_DATA_FOUND_SQLCODE,
        USER_EXCEPTION_CODE => USER_EXCEPTION_SQLCODE,
        other => -i32::from(other),
    }
}

pub(crate) fn application_error_code(number: i64) -> Option<u16> {
    if !(APPLICATION_ERROR_LOWEST..=APPLICATION_ERROR_HIGHEST).contains(&number) {
        return None;
    }
    number.checked_neg().and_then(|code| u16::try_from(code).ok())
}
