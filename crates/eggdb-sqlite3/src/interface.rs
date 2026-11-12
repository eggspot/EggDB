use crate::functions::error_text;
use crate::{SQLITE_MISUSE, SQLITE_OK, Sqlite3, Sqlite3Stmt, statement_mut, text_of};
use eggdb::sql::TransactionStatus;
use std::ffi::{CStr, CString, c_char, c_int};
use std::sync::OnceLock;

const SOURCE_ID: &CStr = c"eggdb sqlite3 interface";
const SQLITE_TXN_NONE: c_int = 0;
const SQLITE_TXN_WRITE: c_int = 2;

/// The English text SQLite gives result code `code`, valid for the life of the program.
#[unsafe(no_mangle)]
pub extern "C" fn sqlite3_errstr(code: c_int) -> *const c_char {
    static TEXTS: OnceLock<Vec<(c_int, CString)>> = OnceLock::new();
    let texts = TEXTS.get_or_init(|| {
        (0..=255)
            .chain([100, 101])
            .map(|code| (code, error_text(code)))
            .collect()
    });
    let wanted = if matches!(code, 100 | 101) {
        code
    } else {
        code & 0xff
    };
    texts
        .iter()
        .find(|(known, _)| *known == wanted)
        .map_or(std::ptr::null(), |(_, text)| text.as_ptr())
}

/// The last error's extended result code; EggDB reports the primary code.
///
/// # Safety
/// `db` is NULL or a live connection.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_extended_errcode(db: *mut Sqlite3) -> c_int {
    // SAFETY: the same contract as sqlite3_errcode's.
    unsafe { crate::sqlite3_errcode(db) }
}

/// Accepted and ignored: results carry the primary codes either way.
///
/// # Safety
/// `db` is NULL or a live connection.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_extended_result_codes(db: *mut Sqlite3, _on: c_int) -> c_int {
    if db.is_null() {
        SQLITE_MISUSE
    } else {
        SQLITE_OK
    }
}

/// 1 outside an explicit transaction, 0 inside one.
///
/// # Safety
/// `db` is a live connection.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_get_autocommit(db: *mut Sqlite3) -> c_int {
    // SAFETY: the caller passes a live connection.
    unsafe { db.as_ref() }.map_or(1, |connection| {
        c_int::from(connection.session.transaction_status() == TransactionStatus::Idle)
    })
}

/// `SQLITE_TXN_WRITE` inside an explicit transaction, `SQLITE_TXN_NONE` outside; EggDB's
/// transactions take the writer when they begin, so there is no read-only state to report.
///
/// # Safety
/// `db` is a live connection; `schema` is NULL or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_txn_state(db: *mut Sqlite3, _schema: *const c_char) -> c_int {
    // SAFETY: the same contract as sqlite3_get_autocommit's.
    if unsafe { sqlite3_get_autocommit(db) } == 1 {
        SQLITE_TXN_NONE
    } else {
        SQLITE_TXN_WRITE
    }
}

/// Accepted: a writer waits for the one before it on its own, so there is nothing to time out.
///
/// # Safety
/// `db` is NULL or a live connection.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_busy_timeout(db: *mut Sqlite3, _milliseconds: c_int) -> c_int {
    if db.is_null() {
        SQLITE_MISUSE
    } else {
        SQLITE_OK
    }
}

/// Stops the statement running on the connection at its next check, which then fails.
///
/// # Safety
/// `db` is NULL or a live connection; it may be called from another thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_interrupt(db: *mut Sqlite3) {
    // SAFETY: the caller passes NULL or a live connection; the cancel signal is shared state
    // made for being set from another thread.
    if let Some(connection) = unsafe { db.as_ref() } {
        connection.session.cancel_signal().cancel();
    }
}

/// 2, SQLite's multi-thread mode: a connection may move between threads, but only one thread
/// may use it at a time.
#[unsafe(no_mangle)]
pub extern "C" fn sqlite3_threadsafe() -> c_int {
    2
}

/// The library's source identifier.
#[unsafe(no_mangle)]
pub extern "C" fn sqlite3_sourceid() -> *const c_char {
    SOURCE_ID.as_ptr()
}

/// 1 when `sql` ends with a complete statement (a `;` outside quotes and comments, and outside
/// a trigger's `BEGIN ... END`), 0 otherwise.
///
/// # Safety
/// `sql` is a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_complete(sql: *const c_char) -> c_int {
    // SAFETY: the caller passes a NUL-terminated string.
    let Some(text) = (unsafe { text_of(sql) }) else {
        return 0;
    };
    c_int::from(is_complete(text))
}

fn is_complete(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut at = 0;
    let mut complete = false;
    let mut depth = 0usize;
    let mut word = String::new();
    let mut in_trigger = false;
    let mut words_in_statement = Vec::new();
    while at < bytes.len() {
        let byte = bytes[at];
        if byte.is_ascii_alphanumeric() || byte == b'_' {
            word.push(char::from(byte.to_ascii_lowercase()));
            at += 1;
            continue;
        }
        if !word.is_empty() {
            words_in_statement.push(std::mem::take(&mut word));
            complete = false;
            if words_in_statement.len() <= 3 && words_in_statement.iter().any(|seen| seen == "trigger")
            {
                in_trigger = true;
            }
            match words_in_statement.last().map(String::as_str) {
                Some("begin" | "case") if in_trigger => depth += 1,
                Some("end") if in_trigger => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        match byte {
            b'\'' | b'"' | b'`' | b'[' => {
                let close = if byte == b'[' { b']' } else { byte };
                at += 1;
                while at < bytes.len() && bytes[at] != close {
                    at += 1;
                }
                if at >= bytes.len() {
                    return false;
                }
                complete = false;
            }
            b'-' if bytes.get(at + 1) == Some(&b'-') => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
                continue;
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                at += 2;
                while at + 1 < bytes.len() && !(bytes[at] == b'*' && bytes[at + 1] == b'/') {
                    at += 1;
                }
                if at + 1 >= bytes.len() {
                    return false;
                }
                at += 1;
            }
            b';' if depth == 0 => {
                complete = true;
                in_trigger = false;
                words_in_statement.clear();
            }
            byte if byte.is_ascii_whitespace() => {}
            _ => complete = false,
        }
        at += 1;
    }
    if !word.is_empty() {
        return false;
    }
    complete
}

/// The SQL text the statement was prepared from, valid until it is finalized.
///
/// # Safety
/// `statement` is a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_sql(statement: *mut Sqlite3Stmt) -> *const c_char {
    // SAFETY: the caller passes a live statement.
    unsafe { statement_mut(statement) }.map_or(std::ptr::null(), |statement| {
        if statement.text_c.is_none() {
            statement.text_c = CString::new(statement.text.replace('\0', " ")).ok();
        }
        statement
            .text_c
            .as_ref()
            .map_or(std::ptr::null(), |text| text.as_ptr())
    })
}

/// The number of values in the current row: the column count after `SQLITE_ROW`, else 0.
///
/// # Safety
/// `statement` is a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_data_count(statement: *mut Sqlite3Stmt) -> c_int {
    // SAFETY: the caller passes a live statement.
    unsafe { statement_mut(statement) }.map_or(0, |statement| {
        if statement.next_row == 0 || statement.next_row > statement.rows.len() {
            0
        } else {
            c_int::try_from(statement.columns.len()).unwrap_or(c_int::MAX)
        }
    })
}

/// 1 when the statement does not change the database by itself: a `SELECT`, `VALUES`, an
/// `EXPLAIN` or a `PRAGMA` that only reads.
///
/// # Safety
/// `statement` is a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_stmt_readonly(statement: *mut Sqlite3Stmt) -> c_int {
    // SAFETY: the caller passes a live statement.
    unsafe { statement_mut(statement) }.map_or(1, |statement| {
        let first = statement
            .text
            .split(|character: char| !character.is_ascii_alphabetic())
            .find(|word| !word.is_empty())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let reads = match first.as_str() {
            "select" | "values" | "explain" => true,
            "with" => !statement.text.to_ascii_lowercase().split_whitespace().any(|word| {
                matches!(word, "insert" | "update" | "delete" | "replace")
            }),
            "pragma" => !statement.text.contains('='),
            _ => false,
        };
        c_int::from(reads)
    })
}

/// 1 when the statement has run and still has rows to step through.
///
/// # Safety
/// `statement` is NULL or a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_stmt_busy(statement: *mut Sqlite3Stmt) -> c_int {
    // SAFETY: the caller passes NULL or a live statement.
    unsafe { statement_mut(statement) }.map_or(0, |statement| {
        c_int::from(statement.ran && statement.next_row > 0 && statement.next_row < statement.rows.len())
    })
}

/// The connection the statement belongs to.
///
/// # Safety
/// `statement` is a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_db_handle(statement: *mut Sqlite3Stmt) -> *mut Sqlite3 {
    // SAFETY: the caller passes a live statement.
    unsafe { statement_mut(statement) }.map_or(std::ptr::null_mut(), |statement| statement.db)
}

/// The declared type of result column `column`, valid until the statement is finalized; NULL
/// when the column is an expression with no declared type.
///
/// # Safety
/// `statement` is a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_column_decltype(
    statement: *mut Sqlite3Stmt,
    column: c_int,
) -> *const c_char {
    // SAFETY: the caller passes a live statement.
    unsafe { statement_mut(statement) }
        .and_then(|statement| {
            statement
                .declared_types
                .get(usize::try_from(column).ok()?)?
                .as_ref()
        })
        .map_or(std::ptr::null(), |name| name.as_ptr())
}

/// The name of parameter `index` (from 1): `?NNN` for a numbered one, NULL for a plain `?`.
///
/// # Safety
/// `statement` is a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_bind_parameter_name(
    statement: *mut Sqlite3Stmt,
    index: c_int,
) -> *const c_char {
    // SAFETY: the caller passes a live statement.
    let Some(statement) = (unsafe { statement_mut(statement) }) else {
        return std::ptr::null();
    };
    if statement.parameter_names.is_empty() {
        statement.parameter_names = numbered_parameters(&statement.text, statement.bindings.len());
    }
    usize::try_from(index)
        .ok()
        .and_then(|index| index.checked_sub(1))
        .and_then(|index| statement.parameter_names.get(index)?.as_ref())
        .map_or(std::ptr::null(), |name| name.as_ptr())
}

/// The index (from 1) of the parameter named `name` (`?NNN`), or 0 when there is none.
///
/// # Safety
/// `statement` is a live statement and `name` a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_bind_parameter_index(
    statement: *mut Sqlite3Stmt,
    name: *const c_char,
) -> c_int {
    // SAFETY: the caller passes a NUL-terminated name.
    let Some(wanted) = (unsafe { text_of(name) }) else {
        return 0;
    };
    // SAFETY: the caller passes a live statement.
    let Some(statement) = (unsafe { statement_mut(statement) }) else {
        return 0;
    };
    if statement.parameter_names.is_empty() {
        statement.parameter_names = numbered_parameters(&statement.text, statement.bindings.len());
    }
    statement
        .parameter_names
        .iter()
        .position(|found| found.as_ref().is_some_and(|found| found.to_bytes() == wanted.as_bytes()))
        .and_then(|at| c_int::try_from(at + 1).ok())
        .unwrap_or(0)
}

fn numbered_parameters(text: &str, count: usize) -> Vec<Option<CString>> {
    let mut names = vec![None; count];
    let bytes = text.as_bytes();
    let mut quote: Option<u8> = None;
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        match quote {
            Some(open) if byte == open => quote = None,
            Some(_) => {}
            None if matches!(byte, b'\'' | b'"' | b'`') => quote = Some(byte),
            None if byte == b'?' => {
                let digits: String = bytes[at + 1..]
                    .iter()
                    .take_while(|digit| digit.is_ascii_digit())
                    .map(|&digit| char::from(digit))
                    .collect();
                if let Ok(number) = digits.parse::<usize>()
                    && let Some(slot) = number.checked_sub(1).and_then(|slot| names.get_mut(slot))
                {
                    *slot = CString::new(format!("?{number}")).ok();
                }
                at += digits.len();
            }
            None => {}
        }
        at += 1;
    }
    names
}

#[cfg(test)]
mod tests {
    use super::is_complete;

    #[test]
    fn statements_are_complete_at_a_semicolon_outside_quotes_comments_and_triggers() {
        assert!(is_complete("SELECT 1;"));
        assert!(is_complete("SELECT 1; -- done\n"));
        assert!(!is_complete("SELECT 1"));
        assert!(!is_complete("SELECT ';'"));
        assert!(!is_complete("SELECT 1 /* ; */"));
        assert!(!is_complete(
            "CREATE TRIGGER t AFTER INSERT ON x BEGIN SELECT 1;"
        ));
        assert!(is_complete(
            "CREATE TRIGGER t AFTER INSERT ON x BEGIN SELECT 1; END;"
        ));
    }
}
