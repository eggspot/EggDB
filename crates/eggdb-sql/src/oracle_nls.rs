use crate::error::{SqlError, SqlResult, SqlState};
use crate::oracle_datetime::{DatetimeModel, ModelUse};
use std::cell::RefCell;
use std::sync::Arc;

const AMERICA_DATE_FORMAT: &str = "DD-MON-RR";
const AMERICA_TIMESTAMP_FORMAT: &str = "DD-MON-RR HH.MI.SSXFF AM";
const AMERICA_TIMESTAMP_TZ_FORMAT: &str = "DD-MON-RR HH.MI.SSXFF AM TZR";
const AMERICA_NUMERIC_CHARACTERS: &str = ".,";

const UNSUPPORTED_PARAMETERS: [&str; 29] = [
    "nls_length_semantics",
    "edition",
    "events",
    "nls_currency",
    "nls_iso_currency",
    "nls_dual_currency",
    "nls_calendar",
    "nls_nchar_conv_excp",
    "nls_time_format",
    "nls_time_tz_format",
    "optimizer_mode",
    "optimizer_features_enable",
    "optimizer_index_cost_adj",
    "sql_trace",
    "cursor_sharing",
    "ddl_lock_timeout",
    "statistics_level",
    "query_rewrite_enabled",
    "query_rewrite_integrity",
    "skip_unusable_indexes",
    "star_transformation_enabled",
    "workarea_size_policy",
    "sort_area_size",
    "hash_area_size",
    "plsql_optimize_level",
    "plsql_code_type",
    "plsql_warnings",
    "isolation_level",
    "container",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NlsParameters {
    date_format: String,
    timestamp_format: String,
    timestamp_tz_format: String,
}

impl Default for NlsParameters {
    fn default() -> Self {
        Self {
            date_format: AMERICA_DATE_FORMAT.to_owned(),
            timestamp_format: AMERICA_TIMESTAMP_FORMAT.to_owned(),
            timestamp_tz_format: AMERICA_TIMESTAMP_TZ_FORMAT.to_owned(),
        }
    }
}

fn unsupported(name: &str, value: &str) -> SqlError {
    SqlError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!(
            "unimplemented feature: {} = {value}",
            name.to_ascii_uppercase()
        ),
    )
    .with_oracle_code(3001)
}

fn invalid_option() -> SqlError {
    SqlError::new(SqlState::SYNTAX_ERROR, "invalid option for ALTER SESSION").with_oracle_code(2248)
}

fn invalid_environment() -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        "Cannot access NLS data files or invalid environment specified",
    )
    .with_oracle_code(12705)
}

pub(crate) fn invalid_parameter_string() -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        "invalid NLS parameter string used in SQL function",
    )
    .with_oracle_code(12702)
}

pub(crate) fn require_american(value: &str) -> SqlResult<()> {
    if value.trim().eq_ignore_ascii_case("american") {
        Ok(())
    } else {
        Err(invalid_environment())
    }
}

fn unquoted(value: &str) -> &str {
    let trimmed = value.trim();
    trimmed
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .unwrap_or(trimmed)
}

pub(crate) fn parameter_assignment(text: &str) -> SqlResult<(String, String)> {
    let Some((name, value)) = text.split_once('=') else {
        return Err(invalid_parameter_string());
    };
    let name = name.trim();
    let value = unquoted(value);
    let simple = !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && !value.is_empty()
        && !value.contains(['=', '\'']);
    if !simple {
        return Err(invalid_parameter_string());
    }
    Ok((name.to_ascii_lowercase(), value.to_owned()))
}

pub(crate) fn check_numeric_parameter(text: &str) -> SqlResult<()> {
    let (name, value) = parameter_assignment(text)?;
    match name.as_str() {
        "nls_numeric_characters" if value.chars().count() != 2 => Err(invalid_parameter_string()),
        "nls_numeric_characters" if value == AMERICA_NUMERIC_CHARACTERS => Ok(()),
        "nls_numeric_characters" | "nls_currency" | "nls_iso_currency" | "nls_dual_currency" => {
            Err(unsupported(&name, &value))
        }
        _ => Err(invalid_parameter_string()),
    }
}

fn checked_format(value: &str, date: bool) -> SqlResult<String> {
    let format = unquoted(value).to_owned();
    if DatetimeModel::parse(&format, ModelUse::Output)?.has_fraction() && date {
        return Err(crate::oracle_datetime::not_recognized());
    }
    Ok(format)
}

impl NlsParameters {
    pub(crate) fn date_format(&self) -> &str {
        &self.date_format
    }

    pub(crate) fn timestamp_format(&self) -> &str {
        &self.timestamp_format
    }

    pub(crate) fn timestamp_tz_format(&self) -> &str {
        &self.timestamp_tz_format
    }

    pub(crate) fn with(&self, name: &str, value: &str) -> SqlResult<Self> {
        let mut next = self.clone();
        let plain = unquoted(value);
        match name {
            "nls_date_format" => next.date_format = checked_format(value, true)?,
            "nls_timestamp_format" => next.timestamp_format = checked_format(value, false)?,
            "nls_timestamp_tz_format" => next.timestamp_tz_format = checked_format(value, false)?,
            "nls_language" | "nls_date_language" => require_american(plain)?,
            "nls_territory" => {
                if !plain.eq_ignore_ascii_case("america") {
                    return Err(invalid_environment());
                }
                let defaults = Self::default();
                next.date_format = defaults.date_format;
                next.timestamp_format = defaults.timestamp_format;
                next.timestamp_tz_format = defaults.timestamp_tz_format;
            }
            "nls_numeric_characters" if plain == AMERICA_NUMERIC_CHARACTERS => {}
            "nls_sort" | "nls_comp" if plain.eq_ignore_ascii_case("binary") => {}
            "nls_numeric_characters" | "nls_sort" | "nls_comp" => {
                return Err(unsupported(name, plain));
            }
            _ if UNSUPPORTED_PARAMETERS.contains(&name) => return Err(unsupported(name, plain)),
            _ => return Err(invalid_option()),
        }
        Ok(next)
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<NlsParameters>>> = const { RefCell::new(None) };
}

pub(crate) struct NlsGuard(Option<Arc<NlsParameters>>);

impl Drop for NlsGuard {
    fn drop(&mut self) {
        let previous = self.0.take();
        CURRENT.with(|current| *current.borrow_mut() = previous);
    }
}

pub(crate) fn enter(parameters: Arc<NlsParameters>) -> NlsGuard {
    crate::oracle_datetime::clear_model_cache();
    NlsGuard(CURRENT.with(|current| current.borrow_mut().replace(parameters)))
}

pub(crate) fn current() -> Arc<NlsParameters> {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .map_or_else(|| Arc::new(NlsParameters::default()), Arc::clone)
    })
}

#[cfg(test)]
mod tests {
    use super::{NlsParameters, check_numeric_parameter, current, enter, parameter_assignment};
    use std::sync::Arc;

    fn code(result: Result<NlsParameters, crate::SqlError>) -> Option<u16> {
        result.err().and_then(|error| error.vendor_code())
    }

    #[test]
    fn the_territory_resets_the_formats_it_implies() {
        let changed = NlsParameters::default()
            .with("nls_date_format", "'YYYY-MM-DD'")
            .expect("format");
        assert_eq!(changed.date_format(), "YYYY-MM-DD");
        let reset = changed.with("nls_territory", "America").expect("territory");
        assert_eq!(reset.date_format(), "DD-MON-RR");
        assert_eq!(reset.timestamp_format(), "DD-MON-RR HH.MI.SSXFF AM");
        let language = changed.with("nls_language", "AMERICAN").expect("language");
        assert_eq!(language.date_format(), "YYYY-MM-DD");
    }

    #[test]
    fn unknown_unsupported_and_foreign_settings_take_their_own_errors() {
        let nls = NlsParameters::default();
        assert_eq!(code(nls.with("no_such_parameter", "1")), Some(2248));
        assert_eq!(code(nls.with("nls_length_semantics", "CHAR")), Some(3001));
        assert_eq!(
            code(nls.with("events", "'10046 trace name context forever'")),
            Some(3001)
        );
        assert_eq!(code(nls.with("edition", "ora$base")), Some(3001));
        assert_eq!(code(nls.with("nls_sort", "XGERMAN")), Some(3001));
        assert!(nls.with("nls_sort", "BINARY").is_ok());
        assert!(nls.with("nls_comp", "binary").is_ok());
        assert_eq!(code(nls.with("nls_language", "GERMAN")), Some(12705));
        assert_eq!(code(nls.with("nls_territory", "KOREA")), Some(12705));
        assert_eq!(
            code(nls.with("nls_date_format", "'HH24:MI:SS.FF'")),
            Some(1821)
        );
        assert!(nls.with("nls_timestamp_format", "'HH24:MI:SS.FF'").is_ok());
        assert_eq!(code(nls.with("nls_date_format", "'QQQ'")), Some(1821));
    }

    #[test]
    fn function_parameter_strings_are_checked() {
        assert_eq!(
            parameter_assignment("NLS_DATE_LANGUAGE = American").ok(),
            Some(("nls_date_language".to_owned(), "American".to_owned()))
        );
        assert!(check_numeric_parameter("NLS_NUMERIC_CHARACTERS = '.,'").is_ok());
        let code = |text: &str| {
            check_numeric_parameter(text)
                .err()
                .and_then(|e| e.vendor_code())
        };
        assert_eq!(code("NLS_NUMERIC_CHARACTERS = ',.'"), Some(3001));
        assert_eq!(code("NLS_NUMERIC_CHARACTERS = '.'"), Some(12702));
        assert_eq!(code("nonsense"), Some(12702));
    }

    #[test]
    fn the_entered_parameters_are_current_until_the_guard_drops() {
        let changed = NlsParameters::default()
            .with("nls_date_format", "'YYYY'")
            .expect("format");
        {
            let _guard = enter(Arc::new(changed));
            assert_eq!(current().date_format(), "YYYY");
        }
        assert_eq!(current().date_format(), "DD-MON-RR");
    }
}
