use crate::error::{SqlError, SqlResult, SqlState};
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

/// The PostgreSQL release whose behaviour EggDB's PostgreSQL dialect follows, as clients read it
/// from `server_version`.
pub const POSTGRESQL_COMPATIBLE_VERSION: &str = "16.0";

const POSTGRESQL_COMPATIBLE_VERSION_NUMBER: &str = "160000";

pub(crate) const DIALECT_SETTING: &str = "eggdb.dialect";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Free,
    Boolean,
    Utf8Only,
    OnOnly,
    Fixed,
}

struct Known {
    name: &'static str,
    default: fn() -> String,
    change: Change,
    reported: bool,
    description: &'static str,
}

fn server_version() -> String {
    format!(
        "{POSTGRESQL_COMPATIBLE_VERSION} (EggDB {})",
        env!("CARGO_PKG_VERSION")
    )
}

macro_rules! fixed_text {
    ($value:expr) => {
        || $value.to_owned()
    };
}

const KNOWN: &[Known] = &[
    Known {
        name: "application_name",
        default: fixed_text!(""),
        change: Change::Free,
        reported: true,
        description: "Sets the application name to be reported in statistics and logs.",
    },
    Known {
        name: "bytea_output",
        default: fixed_text!("hex"),
        change: Change::Free,
        reported: false,
        description: "Sets the output format for bytea.",
    },
    Known {
        name: "check_function_bodies",
        default: fixed_text!("on"),
        change: Change::Free,
        reported: false,
        description: "Checks the bodies of PL/pgSQL functions and procedures at CREATE.",
    },
    Known {
        name: "client_encoding",
        default: fixed_text!("UTF8"),
        change: Change::Utf8Only,
        reported: true,
        description: "Sets the client's character set encoding; EggDB speaks UTF8 (SQL_ASCII passes its bytes through).",
    },
    Known {
        name: "client_min_messages",
        default: fixed_text!("notice"),
        change: Change::Free,
        reported: false,
        description: "Sets the message levels that are sent to the client.",
    },
    Known {
        name: "DateStyle",
        default: fixed_text!("ISO, MDY"),
        change: Change::Free,
        reported: true,
        description: "Sets the display format for date and time values.",
    },
    Known {
        name: "default_table_access_method",
        default: fixed_text!("heap"),
        change: Change::Free,
        reported: false,
        description: "Remembered; EggDB has one table storage.",
    },
    Known {
        name: "default_tablespace",
        default: fixed_text!(""),
        change: Change::Free,
        reported: false,
        description: "Remembered; EggDB keeps one data file.",
    },
    Known {
        name: "default_with_oids",
        default: fixed_text!("off"),
        change: Change::Free,
        reported: false,
        description: "Remembered, for old pg_dump scripts; EggDB tables have no OIDs.",
    },
    Known {
        name: "default_transaction_isolation",
        default: fixed_text!("serializable"),
        change: Change::Fixed,
        reported: false,
        description: "One writer at a time and snapshot readers make every transaction serializable.",
    },
    Known {
        name: "default_transaction_read_only",
        default: fixed_text!("off"),
        change: Change::Boolean,
        reported: true,
        description: "Sets the default read-only status of new transactions.",
    },
    Known {
        name: "escape_string_warning",
        default: fixed_text!("on"),
        change: Change::Free,
        reported: false,
        description: "Remembered, for old pg_dump scripts.",
    },
    Known {
        name: "extra_float_digits",
        default: fixed_text!("1"),
        change: Change::Free,
        reported: false,
        description: "Floating-point values are always printed in their shortest exact form.",
    },
    Known {
        name: "idle_in_transaction_session_timeout",
        default: fixed_text!("0"),
        change: Change::Free,
        reported: false,
        description: "Remembered, not applied yet.",
    },
    Known {
        name: "in_hot_standby",
        default: fixed_text!("off"),
        change: Change::Fixed,
        reported: true,
        description: "Shows whether hot standby is currently active.",
    },
    Known {
        name: "integer_datetimes",
        default: fixed_text!("on"),
        change: Change::Fixed,
        reported: true,
        description: "Shows whether datetimes are integer based.",
    },
    Known {
        name: "IntervalStyle",
        default: fixed_text!("postgres"),
        change: Change::Free,
        reported: true,
        description: "Sets the display format for interval values.",
    },
    Known {
        name: "is_superuser",
        default: fixed_text!("on"),
        change: Change::Fixed,
        reported: true,
        description: "Shows whether the session's user is a superuser.",
    },
    Known {
        name: "lock_timeout",
        default: fixed_text!("0"),
        change: Change::Free,
        reported: false,
        description: "Sets the longest a statement waits for the database's writer; 0 waits the database's write timeout.",
    },
    Known {
        name: "log_min_duration_statement",
        default: fixed_text!("-1"),
        change: Change::Free,
        reported: false,
        description: "Sets the minimum execution time above which statements are logged by the server.",
    },
    Known {
        name: "max_identifier_length",
        default: fixed_text!("63"),
        change: Change::Fixed,
        reported: false,
        description: "Shows the maximum identifier length.",
    },
    Known {
        name: "max_index_keys",
        default: fixed_text!("32"),
        change: Change::Fixed,
        reported: false,
        description: "Shows the maximum number of index keys.",
    },
    Known {
        name: "max_function_args",
        default: fixed_text!("100"),
        change: Change::Fixed,
        reported: false,
        description: "Shows the maximum number of function arguments.",
    },
    Known {
        name: "row_security",
        default: fixed_text!("on"),
        change: Change::Free,
        reported: false,
        description: "Remembered; row security policies are not supported yet.",
    },
    Known {
        name: "search_path",
        default: fixed_text!("\"$user\", public"),
        change: Change::Free,
        reported: false,
        description: "Sets the schema search order for names that are not schema-qualified.",
    },
    Known {
        name: "server_encoding",
        default: fixed_text!("UTF8"),
        change: Change::Fixed,
        reported: true,
        description: "Shows the server (database) character set encoding.",
    },
    Known {
        name: "server_version",
        default: server_version,
        change: Change::Fixed,
        reported: true,
        description: "Shows the server version.",
    },
    Known {
        name: "server_version_num",
        default: fixed_text!(POSTGRESQL_COMPATIBLE_VERSION_NUMBER),
        change: Change::Fixed,
        reported: false,
        description: "Shows the server version as an integer.",
    },
    Known {
        name: "session_authorization",
        default: fixed_text!("eggdb"),
        change: Change::Fixed,
        reported: true,
        description: "The user this session authenticated as.",
    },
    Known {
        name: "standard_conforming_strings",
        default: fixed_text!("on"),
        change: Change::OnOnly,
        reported: true,
        description: "Causes '...' strings to treat backslashes literally.",
    },
    Known {
        name: "statement_timeout",
        default: fixed_text!("0"),
        change: Change::Free,
        reported: false,
        description: "Sets the maximum allowed duration of any statement.",
    },
    Known {
        name: "synchronize_seqscans",
        default: fixed_text!("on"),
        change: Change::Free,
        reported: false,
        description: "Remembered, for pg_dump scripts.",
    },
    Known {
        name: "TimeZone",
        default: fixed_text!("UTC"),
        change: Change::Free,
        reported: true,
        description: "Sets the time zone for displaying and interpreting time stamps.",
    },
    Known {
        name: "transaction_timeout",
        default: fixed_text!("0"),
        change: Change::Free,
        reported: false,
        description: "Remembered, not applied yet.",
    },
    Known {
        name: "transaction_isolation",
        default: fixed_text!("serializable"),
        change: Change::Fixed,
        reported: false,
        description: "One writer at a time and snapshot readers make every transaction serializable.",
    },
    Known {
        name: "transaction_read_only",
        default: fixed_text!("off"),
        change: Change::Fixed,
        reported: false,
        description: "Shows whether the current transaction is read-only.",
    },
    Known {
        name: "xmloption",
        default: fixed_text!("content"),
        change: Change::Free,
        reported: false,
        description: "Remembered; EggDB has no xml type yet.",
    },
];

fn known(name: &str) -> Option<&'static Known> {
    KNOWN
        .iter()
        .find(|known| known.name.eq_ignore_ascii_case(name))
}

fn setting_zone(value: &str) -> Option<crate::time_zones::TimeZone> {
    match value.trim().parse::<f64>() {
        Ok(hours) if hours.is_finite() && hours.abs() < 16.0 => {
            crate::time_zones::TimeZone::fixed((hours * 3600.0).round() as i32)
        }
        Ok(_) => None,
        Err(_) => crate::time_zones::TimeZone::named(value),
    }
}

fn is_custom(name: &str) -> bool {
    name.contains('.') && !name.starts_with('.') && !name.ends_with('.')
}

fn unrecognized(name: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_OBJECT,
        format!("unrecognized configuration parameter \"{name}\""),
    )
}

pub(crate) fn check_parameter_name(name: &str) -> SqlResult<()> {
    if known(name).is_some() || is_custom(name) {
        Ok(())
    } else {
        Err(unrecognized(name))
    }
}

const BOOLEAN_TRUE: [&str; 6] = ["on", "true", "yes", "1", "t", "y"];
const BOOLEAN_FALSE: [&str; 7] = ["off", "false", "no", "0", "f", "n", "of"];

pub(crate) fn boolean_setting(value: &str) -> Option<bool> {
    match value {
        "on" => return Some(true),
        "off" => return Some(false),
        _ => {}
    }
    let value = value.trim();
    let spelled = |words: &[&str]| words.iter().any(|word| value.eq_ignore_ascii_case(word));
    if spelled(&BOOLEAN_TRUE) {
        Some(true)
    } else if spelled(&BOOLEAN_FALSE) {
        Some(false)
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingScope {
    Session,
    Transaction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SettingChange {
    pub(crate) name: String,
    pub(crate) value: Option<String>,
    pub(crate) scope: SettingScope,
}

#[derive(Debug, Clone)]
pub(crate) struct ParameterValues {
    values: BTreeMap<&'static str, String>,
    custom: BTreeMap<String, String>,
    time_zone: crate::time_zones::TimeZone,
}

#[derive(Debug, Clone)]
pub(crate) struct SavedSettings {
    values: ParameterValues,
    transaction: TransactionSettings,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TransactionSettings {
    at_begin: Option<ParameterValues>,
    at_commit: BTreeMap<String, Option<String>>,
}

impl TransactionSettings {
    pub(crate) fn is_empty(&self) -> bool {
        self.at_begin.is_none() && self.at_commit.is_empty()
    }

    pub(crate) fn before_change(&mut self, settings: &Settings, name: &str, scope: SettingScope) {
        if self.at_begin.is_none() {
            self.at_begin = Some(settings.parameter_values());
        }
        let name = Settings::name_of(name);
        match scope {
            SettingScope::Session => {
                self.at_commit.remove(&name);
            }
            SettingScope::Transaction => {
                if let Entry::Vacant(entry) = self.at_commit.entry(name) {
                    let value = settings.get(entry.key()).ok();
                    entry.insert(value);
                }
            }
        }
    }

    pub(crate) fn before_reset_all(&mut self, settings: &Settings) {
        if self.at_begin.is_none() {
            self.at_begin = Some(settings.parameter_values());
        }
        self.at_commit.clear();
    }

    pub(crate) fn commit(&mut self, settings: &mut Settings) {
        for (name, value) in std::mem::take(&mut self.at_commit) {
            let _ = settings.set(&name, value.as_deref());
        }
        self.at_begin = None;
    }

    pub(crate) fn rollback(&mut self, settings: &mut Settings) {
        if let Some(saved) = self.at_begin.take() {
            settings.restore_parameter_values(saved);
        }
        self.at_commit.clear();
    }

    pub(crate) fn save(&self, settings: &Settings) -> SavedSettings {
        SavedSettings {
            values: settings.parameter_values(),
            transaction: self.clone(),
        }
    }

    pub(crate) fn restore(&mut self, settings: &mut Settings, saved: SavedSettings) {
        settings.restore_parameter_values(saved.values);
        *self = saved.transaction;
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Settings {
    values: BTreeMap<&'static str, String>,
    custom: BTreeMap<String, String>,
    pub(crate) database: String,
    pub(crate) time_zone: crate::time_zones::TimeZone,
    pub(crate) mysql: crate::mysql_variables::MySqlVariables,
    pub(crate) sqlite: SqliteState,
    pub(crate) sqlserver: SqlServerState,
    pub(crate) oracle_user_schema: Option<String>,
    pub(crate) checked_role: Option<String>,
    pub(crate) login_role: Option<String>,
    pub(crate) database_names: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SqlServerState {
    pub(crate) connection_id: u32,
    pub(crate) transaction_count: u32,
    pub(crate) no_count: bool,
    pub(crate) implicit_transactions: bool,
    pub(crate) format_only: bool,
    pub(crate) showplan: Option<crate::ast::ShowplanKind>,
    pub(crate) statistics_profile: bool,
    pub(crate) xact_abort: bool,
    pub(crate) uncommittable: bool,
    pub(crate) fetch_status: Option<i32>,
    pub(crate) cursor_rows: i32,
    pub(crate) cursors: Vec<crate::tsql::CursorState>,
}

#[derive(Debug, Clone)]
pub(crate) struct SqliteState {
    pub(crate) last_insert_rowid: i64,
    pub(crate) changes: i64,
    pub(crate) total_changes: i64,
    pub(crate) user_version: i64,
    pub(crate) foreign_keys: bool,
    pub(crate) journal_mode: &'static str,
    pub(crate) synchronous: i64,
}

impl Default for SqliteState {
    fn default() -> Self {
        Self {
            last_insert_rowid: 0,
            changes: 0,
            total_changes: 0,
            user_version: 0,
            foreign_keys: false,
            journal_mode: "wal",
            synchronous: 2,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            values: KNOWN
                .iter()
                .map(|known| (known.name, (known.default)()))
                .collect(),
            custom: BTreeMap::new(),
            database: "eggdb".to_owned(),
            time_zone: crate::time_zones::TimeZone::UTC,
            mysql: crate::mysql_variables::MySqlVariables::default(),
            sqlite: SqliteState::default(),
            sqlserver: SqlServerState::default(),
            oracle_user_schema: None,
            checked_role: None,
            login_role: None,
            database_names: Vec::new(),
        }
    }
}

impl Settings {
    pub(crate) fn get(&self, name: &str) -> SqlResult<String> {
        if let Some(value) = self.values.get(name) {
            return Ok(value.clone());
        }
        if let Some(known) = known(name) {
            return Ok(self.values.get(known.name).cloned().unwrap_or_default());
        }
        let lowered = name.to_ascii_lowercase();
        self.custom
            .get(&lowered)
            .cloned()
            .ok_or_else(|| unrecognized(name))
    }

    pub(crate) fn set(&mut self, name: &str, value: Option<&str>) -> SqlResult<()> {
        let Some(known) = known(name) else {
            if !is_custom(name) {
                return Err(unrecognized(name));
            }
            let lowered = name.to_ascii_lowercase();
            match value {
                Some(value) => self.custom.insert(lowered, value.to_owned()),
                None => self.custom.remove(&lowered),
            };
            return Ok(());
        };
        if known.name == "IntervalStyle"
            && let Some(given) = value
            && crate::interval_style::IntervalStyle::named(given).is_none()
        {
            return Err(SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("invalid value for parameter \"IntervalStyle\": \"{given}\""),
            )
            .with_hint("Available values: postgres, postgres_verbose, sql_standard, iso_8601."));
        }
        let value = match value {
            None if known.change == Change::Fixed => return Ok(()),
            None => (known.default)(),
            Some(value) => match known.change {
                Change::Free => value.to_owned(),
                Change::Boolean => match boolean_setting(value) {
                    Some(true) => "on".to_owned(),
                    Some(false) => "off".to_owned(),
                    None => {
                        return Err(SqlError::new(
                            SqlState::INVALID_PARAMETER_VALUE,
                            format!("parameter \"{}\" requires a Boolean value", known.name),
                        ));
                    }
                },
                Change::Utf8Only => {
                    if value.eq_ignore_ascii_case("sql_ascii") {
                        "SQL_ASCII".to_owned()
                    } else if ["utf8", "utf-8", "unicode"]
                        .iter()
                        .any(|spelling| value.eq_ignore_ascii_case(spelling))
                    {
                        "UTF8".to_owned()
                    } else {
                        return Err(SqlError::not_supported(format!(
                            "client_encoding {value} is not supported; EggDB speaks UTF8 only"
                        )));
                    }
                }
                Change::OnOnly => {
                    if !["on", "true", "yes", "1"]
                        .iter()
                        .any(|spelling| value.eq_ignore_ascii_case(spelling))
                    {
                        return Err(SqlError::not_supported(format!(
                            "{} can only be on in this release",
                            known.name
                        )));
                    }
                    "on".to_owned()
                }
                Change::Fixed if value == self.get(known.name)? => value.to_owned(),
                Change::Fixed => {
                    return Err(SqlError::new(
                        SqlState::CANT_CHANGE_RUNTIME_PARAM,
                        format!("parameter \"{}\" cannot be changed", known.name),
                    ));
                }
            },
        };
        let value = if known.name == "TimeZone" {
            let zone = setting_zone(&value).ok_or_else(|| {
                SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("invalid value for parameter \"TimeZone\": \"{value}\""),
                )
            })?;
            self.time_zone = zone;
            zone.region_name().map_or(value, str::to_owned)
        } else {
            value
        };
        self.values.insert(known.name, value);
        Ok(())
    }

    pub(crate) fn parameter_values(&self) -> ParameterValues {
        ParameterValues {
            values: self.values.clone(),
            custom: self.custom.clone(),
            time_zone: self.time_zone,
        }
    }

    pub(crate) fn restore_parameter_values(&mut self, saved: ParameterValues) {
        let dialect = self.custom.get(DIALECT_SETTING).cloned();
        let user = self.values.get("session_authorization").cloned();
        self.values = saved.values;
        self.custom = saved.custom;
        self.time_zone = saved.time_zone;
        match dialect {
            Some(dialect) => self.custom.insert(DIALECT_SETTING.to_owned(), dialect),
            None => self.custom.remove(DIALECT_SETTING),
        };
        if let Some(user) = user {
            self.values.insert("session_authorization", user);
        }
    }

    pub(crate) fn show_transaction_read_only(&mut self, read_only: bool) {
        let shown = if read_only { "on" } else { "off" };
        if self
            .values
            .get("transaction_read_only")
            .is_none_or(|value| value != shown)
        {
            self.values
                .insert("transaction_read_only", shown.to_owned());
        }
    }

    pub(crate) fn is_on(&self, name: &str) -> bool {
        match self.known_value(name) {
            Some(value) => boolean_setting(value).unwrap_or(false),
            None => self
                .get(name)
                .ok()
                .and_then(|value| boolean_setting(&value))
                .unwrap_or(false),
        }
    }

    pub(crate) fn known_value(&self, name: &str) -> Option<&str> {
        match self.values.get(name) {
            Some(value) => Some(value),
            None => known(name)
                .and_then(|known| self.values.get(known.name))
                .map(String::as_str),
        }
    }

    pub(crate) fn set_superuser(&mut self, superuser: bool) {
        let shown = if superuser { "on" } else { "off" };
        self.values.insert("is_superuser", shown.to_owned());
    }

    pub(crate) fn set_session_user(&mut self, user: &str) {
        self.values.insert("session_authorization", user.to_owned());
    }

    pub(crate) fn reset_all(&mut self) {
        let user = self.get("session_authorization").unwrap_or_default();
        for known in KNOWN {
            if known.change != Change::Fixed {
                self.values.insert(known.name, (known.default)());
            }
        }
        self.custom.clear();
        self.time_zone = crate::time_zones::TimeZone::UTC;
        self.set_session_user(&user);
    }

    pub(crate) fn names_reset_all_changes(&self) -> Vec<String> {
        let mut names: Vec<String> = KNOWN
            .iter()
            .filter(|known| known.change != Change::Fixed)
            .filter(|known| {
                self.values
                    .get(known.name)
                    .is_some_and(|value| *value != (known.default)())
            })
            .map(|known| known.name.to_owned())
            .collect();
        names.extend(
            self.custom
                .keys()
                .filter(|name| name.as_str() != DIALECT_SETTING)
                .cloned(),
        );
        names
    }

    pub(crate) fn changeable(name: &str) -> bool {
        known(name).is_none_or(|known| !matches!(known.change, Change::Fixed))
    }

    pub(crate) fn name_of(name: &str) -> String {
        known(name).map_or_else(|| name.to_ascii_lowercase(), |known| known.name.to_owned())
    }

    pub(crate) fn reported(&self) -> Vec<(&'static str, String)> {
        KNOWN
            .iter()
            .filter(|known| known.reported)
            .map(|known| {
                (
                    known.name,
                    self.values.get(known.name).cloned().unwrap_or_default(),
                )
            })
            .collect()
    }

    pub(crate) fn all(&self) -> Vec<(String, String, String)> {
        let mut rows: Vec<(String, String, String)> = KNOWN
            .iter()
            .map(|known| {
                (
                    known.name.to_owned(),
                    self.values.get(known.name).cloned().unwrap_or_default(),
                    known.description.to_owned(),
                )
            })
            .collect();
        rows.extend(
            self.custom
                .iter()
                .map(|(name, value)| (name.clone(), value.clone(), String::new())),
        );
        rows.sort_by_key(|row| row.0.to_ascii_lowercase());
        rows
    }
}
