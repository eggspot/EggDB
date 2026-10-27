use crate::types::Value;
use std::collections::BTreeMap;

/// The MySQL release whose behaviour EggDB's MySQL dialect follows, as clients read it from
/// `@@version` and the protocol's handshake.
pub const MYSQL_COMPATIBLE_VERSION: &str = "8.0.36-EggDB";

const SQL_MODE: &str = "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION";

enum Default {
    Text(&'static str),
    Number(i64),
}

const SYSTEM_VARIABLES: &[(&str, Default)] = &[
    ("auto_increment_increment", Default::Number(1)),
    ("auto_increment_offset", Default::Number(1)),
    ("autocommit", Default::Number(1)),
    ("character_set_client", Default::Text("utf8mb4")),
    ("character_set_connection", Default::Text("utf8mb4")),
    ("character_set_database", Default::Text("utf8mb4")),
    ("character_set_results", Default::Text("utf8mb4")),
    ("character_set_server", Default::Text("utf8mb4")),
    ("character_set_system", Default::Text("utf8mb4")),
    ("collation_connection", Default::Text("utf8mb4_0900_ai_ci")),
    ("collation_database", Default::Text("utf8mb4_0900_ai_ci")),
    ("collation_server", Default::Text("utf8mb4_0900_ai_ci")),
    ("default_storage_engine", Default::Text("EggDB")),
    ("div_precision_increment", Default::Number(4)),
    ("explicit_defaults_for_timestamp", Default::Number(1)),
    ("foreign_key_checks", Default::Number(1)),
    ("have_ssl", Default::Text("DISABLED")),
    ("init_connect", Default::Text("")),
    ("interactive_timeout", Default::Number(28800)),
    ("license", Default::Text("MIT OR Apache-2.0")),
    ("lower_case_table_names", Default::Number(1)),
    ("max_allowed_packet", Default::Number(67_108_864)),
    ("net_buffer_length", Default::Number(16384)),
    ("net_read_timeout", Default::Number(30)),
    ("net_write_timeout", Default::Number(60)),
    ("performance_schema", Default::Number(0)),
    ("protocol_version", Default::Number(10)),
    ("query_cache_size", Default::Number(0)),
    ("query_cache_type", Default::Text("OFF")),
    ("sql_auto_is_null", Default::Number(0)),
    ("sql_mode", Default::Text(SQL_MODE)),
    ("sql_notes", Default::Number(1)),
    ("sql_safe_updates", Default::Number(0)),
    ("sql_select_limit", Default::Number(i64::MAX)),
    ("sql_warnings", Default::Number(0)),
    ("system_time_zone", Default::Text("UTC")),
    ("time_zone", Default::Text("SYSTEM")),
    ("transaction_isolation", Default::Text("REPEATABLE-READ")),
    ("transaction_read_only", Default::Number(0)),
    ("tx_isolation", Default::Text("REPEATABLE-READ")),
    ("tx_read_only", Default::Number(0)),
    ("unique_checks", Default::Number(1)),
    ("version", Default::Text(MYSQL_COMPATIBLE_VERSION)),
    ("version_comment", Default::Text("EggDB")),
    (
        "version_compile_machine",
        Default::Text(std::env::consts::ARCH),
    ),
    ("version_compile_os", Default::Text(std::env::consts::OS)),
    ("wait_timeout", Default::Number(28800)),
];

const BOOLEAN_VARIABLES: &[&str] = &[
    "autocommit",
    "explicit_defaults_for_timestamp",
    "foreign_key_checks",
    "sql_auto_is_null",
    "sql_notes",
    "sql_safe_updates",
    "sql_warnings",
    "transaction_read_only",
    "tx_read_only",
    "unique_checks",
];

const READ_ONLY_VARIABLES: &[&str] = &[
    "character_set_system",
    "have_ssl",
    "license",
    "lower_case_table_names",
    "performance_schema",
    "protocol_version",
    "system_time_zone",
    "version",
    "version_comment",
    "version_compile_machine",
    "version_compile_os",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SetRefused {
    ReadOnly,
    WrongValue,
}

#[derive(Debug, Clone)]
pub(crate) struct MySqlVariables {
    system: BTreeMap<String, Value>,
    user: BTreeMap<String, Value>,
    pub(crate) last_insert_id: u64,
    pub(crate) connection_id: u32,
}

impl std::default::Default for MySqlVariables {
    fn default() -> Self {
        let system = SYSTEM_VARIABLES
            .iter()
            .map(|(name, default)| ((*name).to_owned(), default_value(default)))
            .collect();
        Self {
            system,
            user: BTreeMap::new(),
            last_insert_id: 0,
            connection_id: 1,
        }
    }
}

fn default_value(default: &Default) -> Value {
    match default {
        Default::Text(text) => Value::Text((*text).to_owned()),
        Default::Number(number) => Value::BigInt(*number),
    }
}

fn switch(value: &Value) -> Option<i64> {
    match value {
        Value::Boolean(flag) => Some(i64::from(*flag)),
        Value::Text(text) => match text.to_ascii_lowercase().as_str() {
            "on" | "true" | "1" => Some(1),
            "off" | "false" | "0" => Some(0),
            _ => None,
        },
        other => other.as_integer().filter(|number| matches!(number, 0 | 1)),
    }
}

impl MySqlVariables {
    pub(crate) fn system(&self, name: &str) -> Option<Value> {
        let name = name.to_ascii_lowercase();
        match name.as_str() {
            "last_insert_id" | "identity" => Some(Value::BigInt(
                self.last_insert_id.min(i64::MAX as u64) as i64,
            )),
            "pseudo_thread_id" => Some(Value::BigInt(i64::from(self.connection_id))),
            _ => self.system.get(&name).cloned(),
        }
    }

    pub(crate) fn set_system(
        &mut self,
        name: &str,
        value: Option<Value>,
    ) -> Result<(), SetRefused> {
        let name = name.to_ascii_lowercase();
        if READ_ONLY_VARIABLES.contains(&name.as_str()) {
            return Err(SetRefused::ReadOnly);
        }
        let known = SYSTEM_VARIABLES.iter().find(|(known, _)| *known == name);
        let value = match (value, known) {
            (None, Some((_, default))) => default_value(default),
            (None, None) => Value::Null,
            (Some(value), _) if BOOLEAN_VARIABLES.contains(&name.as_str()) => {
                Value::BigInt(switch(&value).ok_or(SetRefused::WrongValue)?)
            }
            (Some(value), _) => value,
        };
        if matches!(name.as_str(), "transaction_isolation" | "tx_isolation") {
            for alias in ["transaction_isolation", "tx_isolation"] {
                self.system.insert(alias.to_owned(), value.clone());
            }
            return Ok(());
        }
        self.system.insert(name, value);
        Ok(())
    }

    pub(crate) fn user(&self, name: &str) -> Value {
        self.user
            .get(&name.to_lowercase())
            .cloned()
            .unwrap_or(Value::Null)
    }

    pub(crate) fn set_user(&mut self, name: &str, value: Value) {
        self.user.insert(name.to_lowercase(), value);
    }

    pub(crate) fn autocommit(&self) -> bool {
        self.system
            .get("autocommit")
            .and_then(Value::as_integer)
            .is_none_or(|flag| flag != 0)
    }

    pub(crate) fn all(&self) -> Vec<(String, Value)> {
        self.system
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect()
    }
}
