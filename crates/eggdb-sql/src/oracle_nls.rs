use crate::Value;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::oracle_date_language::DateLanguage;
use crate::oracle_datetime::{DatetimeModel, ModelUse};
use std::cell::RefCell;
use std::sync::Arc;

const AMERICA_DATE_FORMAT: &str = "DD-MON-RR";
const AMERICA_TIMESTAMP_FORMAT: &str = "DD-MON-RR HH.MI.SSXFF AM";
const AMERICA_TIMESTAMP_TZ_FORMAT: &str = "DD-MON-RR HH.MI.SSXFF AM TZR";
const AMERICA_CURRENCY: &str = "$";
const AMERICA_DECIMAL_CHARACTER: char = '.';
const AMERICA_GROUP_SEPARATOR: char = ',';
const MAX_CURRENCY_BYTES: usize = 10;
const DATE_FUNCTION_PARAMETERS: [&str; 1] = ["nls_date_language"];
const NUMBER_FUNCTION_PARAMETERS: [&str; 4] = [
    "nls_numeric_characters",
    "nls_currency",
    "nls_iso_currency",
    "nls_dual_currency",
];

const ISO_CURRENCIES: [(&str, &str); 22] = [
    ("AMERICA", "USD"),
    ("AUSTRALIA", "AUD"),
    ("AUSTRIA", "EUR"),
    ("BELGIUM", "EUR"),
    ("BRAZIL", "BRL"),
    ("CANADA", "CAD"),
    ("CHINA", "CNY"),
    ("DENMARK", "DKK"),
    ("FINLAND", "EUR"),
    ("FRANCE", "EUR"),
    ("GERMANY", "EUR"),
    ("INDIA", "INR"),
    ("IRELAND", "EUR"),
    ("ITALY", "EUR"),
    ("JAPAN", "JPY"),
    ("MEXICO", "MXN"),
    ("NORWAY", "NOK"),
    ("PORTUGAL", "EUR"),
    ("SPAIN", "EUR"),
    ("SWITZERLAND", "CHF"),
    ("THE NETHERLANDS", "EUR"),
    ("UNITED KINGDOM", "GBP"),
];

const UNSUPPORTED_PARAMETERS: [&str; 27] = [
    "nls_length_semantics",
    "edition",
    "events",
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

struct Territory {
    name: &'static str,
    date_format: &'static str,
    currency: &'static str,
    decimal: char,
    group: char,
}

const TERRITORIES: [Territory; 14] = [
    Territory { name: "AMERICA", date_format: AMERICA_DATE_FORMAT, currency: "$", decimal: '.', group: ',' },
    Territory { name: "UNITED KINGDOM", date_format: "DD-MON-RR", currency: "£", decimal: '.', group: ',' },
    Territory { name: "GERMANY", date_format: "DD.MM.RR", currency: "€", decimal: ',', group: '.' },
    Territory { name: "AUSTRIA", date_format: "DD.MM.RR", currency: "€", decimal: ',', group: '.' },
    Territory { name: "FRANCE", date_format: "DD/MM/RR", currency: "€", decimal: ',', group: ' ' },
    Territory { name: "ITALY", date_format: "DD-MON-RR", currency: "€", decimal: ',', group: '.' },
    Territory { name: "SPAIN", date_format: "DD/MM/RR", currency: "€", decimal: ',', group: '.' },
    Territory { name: "THE NETHERLANDS", date_format: "DD-MM-RR", currency: "€", decimal: ',', group: '.' },
    Territory { name: "SWEDEN", date_format: "RRRR-MM-DD", currency: "Kr", decimal: ',', group: ' ' },
    Territory { name: "DENMARK", date_format: "RR-MM-DD", currency: "Kr", decimal: ',', group: '.' },
    Territory { name: "NORWAY", date_format: "DD.MM.RRRR", currency: "Kr", decimal: ',', group: ' ' },
    Territory { name: "FINLAND", date_format: "DD.MM.RRRR", currency: "€", decimal: ',', group: ' ' },
    Territory { name: "POLAND", date_format: "RR/MM/DD", currency: "zł", decimal: ',', group: ' ' },
    Territory { name: "BRAZIL", date_format: "DD/MM/RR", currency: "R$", decimal: ',', group: '.' },
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NlsParameters {
    date_format: String,
    timestamp_format: String,
    timestamp_tz_format: String,
    language: DateLanguage,
    date_language: DateLanguage,
    currency: String,
    iso_currency_territory: &'static str,
    iso_currency: &'static str,
    decimal_character: char,
    group_separator: char,
}

impl Default for NlsParameters {
    fn default() -> Self {
        Self {
            date_format: AMERICA_DATE_FORMAT.to_owned(),
            timestamp_format: AMERICA_TIMESTAMP_FORMAT.to_owned(),
            timestamp_tz_format: AMERICA_TIMESTAMP_TZ_FORMAT.to_owned(),
            language: DateLanguage::American,
            date_language: DateLanguage::American,
            currency: AMERICA_CURRENCY.to_owned(),
            iso_currency_territory: ISO_CURRENCIES[0].0,
            iso_currency: ISO_CURRENCIES[0].1,
            decimal_character: AMERICA_DECIMAL_CHARACTER,
            group_separator: AMERICA_GROUP_SEPARATOR,
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

fn unquoted(value: &str) -> &str {
    let trimmed = value.trim();
    trimmed
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .unwrap_or(trimmed)
}

fn language_named(value: &str) -> SqlResult<DateLanguage> {
    DateLanguage::named(value).ok_or_else(invalid_environment)
}

fn checked_currency(value: &str) -> SqlResult<String> {
    if value.is_empty() || value.len() > MAX_CURRENCY_BYTES {
        return Err(invalid_environment());
    }
    Ok(value.to_owned())
}

fn iso_currency_of(territory: &str) -> SqlResult<(&'static str, &'static str)> {
    let wanted = territory.trim();
    ISO_CURRENCIES
        .into_iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
        .ok_or_else(invalid_environment)
}

fn checked_numeric_characters(value: &str) -> SqlResult<(char, char)> {
    let mut characters = value.chars();
    let (Some(decimal), Some(group), None) =
        (characters.next(), characters.next(), characters.next())
    else {
        return Err(invalid_environment());
    };
    let refused = |character: char| character.is_numeric() || "+-<>".contains(character);
    if decimal == group || refused(decimal) || refused(group) {
        return Err(invalid_environment());
    }
    Ok((decimal, group))
}

pub(crate) fn parameter_assignments(text: &str) -> SqlResult<Vec<(String, String)>> {
    let mut assignments = Vec::new();
    let mut rest = text.trim_start();
    while !rest.is_empty() {
        let (name, after) = rest.split_once('=').ok_or_else(invalid_parameter_string)?;
        let name = name.trim();
        let simple_name = !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
        if !simple_name {
            return Err(invalid_parameter_string());
        }
        let after = after.trim_start();
        let (value, remaining) = match after.strip_prefix('\'') {
            Some(quoted) => quoted
                .split_once('\'')
                .ok_or_else(invalid_parameter_string)?,
            None => after.split_once(char::is_whitespace).unwrap_or((after, "")),
        };
        if value.is_empty() || value.contains(['=', '\'']) {
            return Err(invalid_parameter_string());
        }
        assignments.push((name.to_ascii_lowercase(), value.to_owned()));
        rest = remaining.trim_start();
    }
    if assignments.is_empty() {
        return Err(invalid_parameter_string());
    }
    Ok(assignments)
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

    pub(crate) fn language(&self) -> DateLanguage {
        self.language
    }

    pub(crate) fn date_language(&self) -> DateLanguage {
        self.date_language
    }

    pub(crate) fn currency(&self) -> &str {
        &self.currency
    }

    pub(crate) fn iso_currency_territory(&self) -> &'static str {
        self.iso_currency_territory
    }

    pub(crate) fn iso_currency(&self) -> &'static str {
        self.iso_currency
    }

    pub(crate) fn decimal_character(&self) -> char {
        self.decimal_character
    }

    pub(crate) fn group_separator(&self) -> char {
        self.group_separator
    }

    pub(crate) fn numeric_characters(&self) -> String {
        [self.decimal_character, self.group_separator]
            .into_iter()
            .collect()
    }

    fn reset_territory(&mut self) {
        let defaults = Self::default();
        self.date_format = defaults.date_format;
        self.timestamp_format = defaults.timestamp_format;
        self.timestamp_tz_format = defaults.timestamp_tz_format;
        self.currency = defaults.currency;
        self.iso_currency_territory = defaults.iso_currency_territory;
        self.iso_currency = defaults.iso_currency;
        self.decimal_character = defaults.decimal_character;
        self.group_separator = defaults.group_separator;
    }

    pub(crate) fn with(&self, name: &str, value: &str) -> SqlResult<Self> {
        let mut next = self.clone();
        let plain = unquoted(value);
        match name {
            "nls_date_format" => next.date_format = checked_format(value, true)?,
            "nls_timestamp_format" => next.timestamp_format = checked_format(value, false)?,
            "nls_timestamp_tz_format" => next.timestamp_tz_format = checked_format(value, false)?,
            "nls_language" => {
                next.language = language_named(plain)?;
                next.date_language = next.language;
            }
            "nls_date_language" => next.date_language = language_named(plain)?,
            "nls_territory" => {
                let territory = TERRITORIES
                    .iter()
                    .find(|territory| territory.name.eq_ignore_ascii_case(plain.trim()))
                    .ok_or_else(invalid_environment)?;
                next.reset_territory();
                if territory.name != "AMERICA" {
                    next.date_format = territory.date_format.to_owned();
                    next.timestamp_format = format!("{} HH24:MI:SSXFF", territory.date_format);
                    next.timestamp_tz_format = format!("{} HH24:MI:SSXFF TZR", territory.date_format);
                    next.currency = territory.currency.to_owned();
                    next.decimal_character = territory.decimal;
                    next.group_separator = territory.group;
                    if let Ok((name, code)) = iso_currency_of(territory.name) {
                        next.iso_currency_territory = name;
                        next.iso_currency = code;
                    }
                }
            }
            "nls_currency" => next.currency = checked_currency(plain)?,
            "nls_iso_currency" => {
                (next.iso_currency_territory, next.iso_currency) = iso_currency_of(plain)?;
            }
            "nls_numeric_characters" => {
                (next.decimal_character, next.group_separator) = checked_numeric_characters(plain)?;
            }
            "nls_sort" | "nls_comp" if plain.eq_ignore_ascii_case("binary") => {}
            "nls_sort" | "nls_comp" => return Err(unsupported(name, plain)),
            _ if UNSUPPORTED_PARAMETERS.contains(&name) => return Err(unsupported(name, plain)),
            _ => return Err(invalid_option()),
        }
        Ok(next)
    }

    fn with_function_parameters(&self, text: &str, allowed: &[&str]) -> SqlResult<Self> {
        let mut next = self.clone();
        for (name, value) in parameter_assignments(text)? {
            if !allowed.contains(&name.as_str()) {
                return Err(invalid_parameter_string());
            }
            next = next.with(&name, &format!("'{value}'")).map_err(|error| {
                if error.vendor_code() == Some(3001) {
                    error
                } else {
                    invalid_parameter_string()
                }
            })?;
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

fn enter_function_parameters(
    nlsparam: Option<&Value>,
    allowed: &[&str],
) -> SqlResult<Option<NlsGuard>> {
    let Some(Value::Text(text)) = nlsparam else {
        return Ok(None);
    };
    let next = current().with_function_parameters(text, allowed)?;
    Ok(Some(enter(Arc::new(next))))
}

pub(crate) fn enter_date_function_parameters(
    nlsparam: Option<&Value>,
) -> SqlResult<Option<NlsGuard>> {
    enter_function_parameters(nlsparam, &DATE_FUNCTION_PARAMETERS)
}

pub(crate) fn enter_number_function_parameters(
    nlsparam: Option<&Value>,
) -> SqlResult<Option<NlsGuard>> {
    enter_function_parameters(nlsparam, &NUMBER_FUNCTION_PARAMETERS)
}

#[cfg(test)]
mod tests {
    use super::{
        DATE_FUNCTION_PARAMETERS, NUMBER_FUNCTION_PARAMETERS, NlsParameters, current, enter,
        parameter_assignments,
    };
    use crate::oracle_date_language::DateLanguage;
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
        assert_eq!(code(nls.with("nls_language", "KLINGON")), Some(12705));
        assert_eq!(code(nls.with("nls_territory", "KOREA")), Some(12705));
        assert_eq!(
            code(nls.with("nls_date_format", "'HH24:MI:SS.FF'")),
            Some(1821)
        );
        assert!(nls.with("nls_timestamp_format", "'HH24:MI:SS.FF'").is_ok());
        assert_eq!(code(nls.with("nls_date_format", "'KK'")), Some(1821));
    }

    #[test]
    fn function_parameter_strings_are_checked() {
        assert_eq!(
            parameter_assignments("NLS_DATE_LANGUAGE = American").ok(),
            Some(vec![(
                "nls_date_language".to_owned(),
                "American".to_owned()
            )])
        );
        assert_eq!(
            parameter_assignments("NLS_NUMERIC_CHARACTERS = ', ' NLS_CURRENCY='EUR '").ok(),
            Some(vec![
                ("nls_numeric_characters".to_owned(), ", ".to_owned()),
                ("nls_currency".to_owned(), "EUR ".to_owned()),
            ])
        );
        let numbers = |text: &str| {
            NlsParameters::default().with_function_parameters(text, &NUMBER_FUNCTION_PARAMETERS)
        };
        let swapped = numbers("NLS_NUMERIC_CHARACTERS = ',.'").expect("swapped");
        assert_eq!(
            (swapped.decimal_character(), swapped.group_separator()),
            (',', '.')
        );
        assert_eq!(code(numbers("NLS_NUMERIC_CHARACTERS = '.'")), Some(12702));
        assert_eq!(code(numbers("NLS_NUMERIC_CHARACTERS = '..'")), Some(12702));
        assert_eq!(code(numbers("NLS_NUMERIC_CHARACTERS = '1,'")), Some(12702));
        assert_eq!(code(numbers("NLS_DUAL_CURRENCY = 'X'")), Some(3001));
        assert_eq!(code(numbers("NLS_DATE_LANGUAGE = German")), Some(12702));
        assert_eq!(code(numbers("nonsense")), Some(12702));
        assert_eq!(code(numbers("")), Some(12702));
        let dates = |text: &str| {
            NlsParameters::default().with_function_parameters(text, &DATE_FUNCTION_PARAMETERS)
        };
        assert_eq!(
            dates("NLS_DATE_LANGUAGE = 'French'")
                .map(|nls| nls.date_language())
                .ok(),
            Some(DateLanguage::French)
        );
        assert_eq!(code(dates("NLS_DATE_LANGUAGE = Klingon")), Some(12702));
    }

    #[test]
    fn language_currency_and_numeric_characters_are_session_settings() {
        let nls = NlsParameters::default();
        let german = nls.with("nls_language", "german").expect("language");
        assert_eq!(german.language(), DateLanguage::German);
        assert_eq!(german.date_language(), DateLanguage::German);
        let dutch = german
            .with("nls_date_language", "'DUTCH'")
            .expect("date language");
        assert_eq!(dutch.language(), DateLanguage::German);
        assert_eq!(dutch.date_language(), DateLanguage::Dutch);
        let priced = nls
            .with("nls_currency", "'€'")
            .and_then(|nls| nls.with("nls_iso_currency", "'united kingdom'"))
            .and_then(|nls| nls.with("nls_numeric_characters", "',.'"))
            .expect("currency");
        assert_eq!(priced.currency(), "€");
        assert_eq!(priced.iso_currency(), "GBP");
        assert_eq!(priced.iso_currency_territory(), "UNITED KINGDOM");
        assert_eq!(priced.numeric_characters(), ",.");
        let reset = priced.with("nls_territory", "AMERICA").expect("territory");
        assert_eq!(reset.currency(), "$");
        assert_eq!(reset.iso_currency(), "USD");
        assert_eq!(reset.numeric_characters(), ".,");
        assert_eq!(code(nls.with("nls_iso_currency", "ATLANTIS")), Some(12705));
        assert_eq!(code(nls.with("nls_currency", "'ELEVENBYTES'")), Some(12705));
        assert_eq!(
            code(nls.with("nls_numeric_characters", "'+,'")),
            Some(12705)
        );
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
