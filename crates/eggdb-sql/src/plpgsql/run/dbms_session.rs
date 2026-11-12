use super::super::frame::RoutineFrame;
use super::dbms_scheduler::split_arguments;
use super::{Flow, Run, parsed};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::swap_uniform_case;
use crate::oracle_dictionary::{
    ACTION_SETTING, CLIENT_IDENTIFIER_SETTING, CLIENT_INFO_SETTING, MODULE_SETTING,
};
use crate::session::oracle_contexts::{CONTEXTS_TABLE, CREATE_CONTEXTS_SCHEMA, CREATE_CONTEXTS_TABLE};
use crate::settings::SettingScope;
use crate::types::Value;

const OWNER: &str = "sys";
const PACKAGES: [&str; 4] = ["dbms_application_info", "dbms_session", "dbms_lock", "dbms_stats"];
const MODULE_BYTES: usize = 48;
const ACTION_BYTES: usize = 32;
const CLIENT_INFO_BYTES: usize = 64;
const CLIENT_IDENTIFIER_BYTES: usize = 64;
const GATHER_TABLE_STATS: [&str; 15] = [
    "ownname",
    "tabname",
    "partname",
    "estimate_percent",
    "block_sample",
    "method_opt",
    "degree",
    "granularity",
    "cascade",
    "stattab",
    "statid",
    "statown",
    "no_invalidate",
    "stattype",
    "force",
];
const GATHER_SCHEMA_STATS: [&str; 15] = [
    "ownname",
    "estimate_percent",
    "block_sample",
    "method_opt",
    "degree",
    "granularity",
    "cascade",
    "stattab",
    "statid",
    "options",
    "objlist",
    "statown",
    "no_invalidate",
    "force",
    "obj_filter_list",
];
const GATHER_DATABASE_STATS: [&str; 13] = [
    "estimate_percent",
    "block_sample",
    "method_opt",
    "degree",
    "granularity",
    "cascade",
    "stattab",
    "statid",
    "options",
    "objlist",
    "statown",
    "gather_sys",
    "no_invalidate",
];

struct PackageCall {
    package: &'static str,
    member: String,
    arguments: String,
}

impl PackageCall {
    fn of(text: &str) -> Option<Self> {
        let text = text.trim();
        if !text.get(..4)?.eq_ignore_ascii_case("call") {
            return None;
        }
        let rest = text.get(4..)?;
        let (name, arguments) = match (rest.find('('), rest.rfind(')')) {
            (Some(open), Some(close)) if open < close => (rest.get(..open)?, rest.get(open + 1..close)?),
            _ => (rest, ""),
        };
        let name = name.trim().replace('"', "").to_ascii_lowercase();
        let parts: Vec<&str> = name.split('.').map(str::trim).collect();
        let (package, member) = match parts.as_slice() {
            [package, member] | [OWNER, package, member] => (*package, *member),
            _ => return None,
        };
        let package = PACKAGES.into_iter().find(|known| *known == package)?;
        Some(Self {
            package,
            member: member.to_owned(),
            arguments: arguments.to_owned(),
        })
    }
}

fn undeclared(package: &str, member: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!(
            "PLS-00302: component '{}' must be declared",
            member.to_ascii_uppercase()
        ),
    )
    .with_hint(format!("{} has no member of that name in EggDB.", package.to_ascii_uppercase()))
}

fn wrong_arguments(member: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!(
            "PLS-00306: wrong number or types of arguments in call to '{}'",
            member.to_ascii_uppercase()
        ),
    )
}

fn truncated(text: String, bytes: usize) -> String {
    if text.len() <= bytes {
        return text;
    }
    let mut end = bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn insufficient_privileges() -> SqlError {
    SqlError::new(
        SqlState::INSUFFICIENT_PRIVILEGE,
        "ORA-01031: insufficient privileges",
    )
    .with_oracle_code(1031)
    .with_hint("Only the package named in the context's CREATE CONTEXT ... USING may set or clear it.")
}

fn last_part(name: &str) -> String {
    name.rsplit('.').next().unwrap_or(name).trim().trim_matches('"').to_ascii_lowercase()
}

fn quoted_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

impl Run<'_> {
    fn package_sql(&mut self, text: &str) -> SqlResult<()> {
        let statement = parsed(text, &self.settings, &RoutineFrame::default(), Dialect::PostgreSql)?;
        self.execute(statement).map(drop)
    }

    fn trusted_context(&mut self, namespace: &str) -> SqlResult<String> {
        let namespace = namespace.trim().to_uppercase();
        self.package_sql(CREATE_CONTEXTS_SCHEMA)?;
        self.package_sql(CREATE_CONTEXTS_TABLE)?;
        let statement = parsed(
            &format!(
                "SELECT package FROM {CONTEXTS_TABLE} WHERE namespace = '{}'",
                namespace.replace('\'', "''")
            ),
            &self.settings,
            &RoutineFrame::default(),
            Dialect::PostgreSql,
        )?;
        let result = self.execute(statement)?;
        let trusted = result
            .rows
            .first()
            .and_then(|row| row.first())
            .and_then(Value::to_text)
            .ok_or_else(insufficient_privileges)?;
        let running = self
            .definition
            .plsql
            .as_ref()
            .and_then(|routine| routine.package.as_deref())
            .map(last_part);
        if running.as_deref() != Some(last_part(&trusted).as_str()) {
            return Err(insufficient_privileges());
        }
        Ok(namespace)
    }

    fn package_arguments(
        &mut self,
        call: &PackageCall,
        names: &[&str],
    ) -> SqlResult<Vec<(String, Value)>> {
        let mut bound = Vec::new();
        for (index, (name, expression)) in split_arguments(&call.arguments).into_iter().enumerate() {
            let name = match name {
                Some(name) if names.contains(&name.as_str()) => name,
                Some(_) => return Err(wrong_arguments(&call.member)),
                None => names
                    .get(index)
                    .map(|name| (*name).to_owned())
                    .ok_or_else(|| wrong_arguments(&call.member))?,
            };
            let value = self.text_value(&expression)?;
            bound.push((name, value));
        }
        Ok(bound)
    }

    fn set_application_info(&mut self, setting: &str, value: Option<&Value>, bytes: usize) -> SqlResult<()> {
        let text = value
            .filter(|value| !matches!(value, Value::Null))
            .and_then(Value::to_text)
            .map(|text| truncated(text, bytes));
        self.change_setting(setting, text.as_deref(), SettingScope::Session)
    }

    fn package_sleep(&mut self, seconds: Option<&Value>, member: &str) -> SqlResult<()> {
        let Some(seconds) = seconds.and_then(Value::as_float) else {
            return Err(wrong_arguments(member));
        };
        if !seconds.is_finite() || seconds < 0.0 {
            return Err(SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("ORA-20000: the sleep time must be a number of seconds from 0, not {seconds}"),
            )
            .with_oracle_code(20000));
        }
        self.package_sql(&format!("SELECT pg_sleep({seconds})"))
    }

    fn stats_table(&self, owner: Option<String>, table: String) -> String {
        let user = self
            .settings
            .get("session_authorization")
            .unwrap_or_default()
            .to_uppercase();
        let table = quoted_identifier(&swap_uniform_case(&table.to_uppercase()));
        match owner.map(|owner| owner.to_uppercase()) {
            Some(owner) if owner != user => {
                format!("{}.{table}", quoted_identifier(&swap_uniform_case(&owner)))
            }
            _ => table,
        }
    }

    pub(super) fn dbms_session_call(&mut self, text: &str) -> SqlResult<Option<Flow>> {
        let Some(call) = PackageCall::of(text) else {
            return Ok(None);
        };
        let names: &[&str] = match (call.package, call.member.as_str()) {
            ("dbms_application_info", "set_module") => &["module_name", "action_name"],
            ("dbms_application_info", "set_action") => &["action_name"],
            ("dbms_application_info", "set_client_info") => &["client_info"],
            ("dbms_session", "set_identifier") => &["client_id"],
            ("dbms_session", "clear_identifier") => &[],
            ("dbms_session", "set_context") => &["namespace", "attribute", "value", "username", "client_id"],
            ("dbms_session", "clear_context") => &["namespace", "client_id", "attribute"],
            ("dbms_session", "clear_all_context") => &["namespace"],
            ("dbms_session" | "dbms_lock", "sleep") => &["seconds"],
            ("dbms_stats", "gather_table_stats") => &GATHER_TABLE_STATS,
            ("dbms_stats", "gather_schema_stats") => &GATHER_SCHEMA_STATS,
            ("dbms_stats", "gather_database_stats") => &GATHER_DATABASE_STATS,
            (package, member) => return Err(undeclared(package, member)),
        };
        let bound = self.package_arguments(&call, names)?;
        let argument = |name: &str| {
            bound
                .iter()
                .find(|(bound_name, _)| bound_name == name)
                .map(|(_, value)| value.clone())
        };
        match (call.package, call.member.as_str()) {
            ("dbms_application_info", "set_module") => {
                if bound.is_empty() {
                    return Err(wrong_arguments(&call.member));
                }
                self.set_application_info(MODULE_SETTING, argument("module_name").as_ref(), MODULE_BYTES)?;
                self.set_application_info(ACTION_SETTING, argument("action_name").as_ref(), ACTION_BYTES)?;
            }
            ("dbms_application_info", "set_action") => {
                self.set_application_info(ACTION_SETTING, argument("action_name").as_ref(), ACTION_BYTES)?;
            }
            ("dbms_application_info", "set_client_info") => {
                self.set_application_info(
                    CLIENT_INFO_SETTING,
                    argument("client_info").as_ref(),
                    CLIENT_INFO_BYTES,
                )?;
            }
            ("dbms_session", "set_identifier") => {
                self.set_application_info(
                    CLIENT_IDENTIFIER_SETTING,
                    argument("client_id").as_ref(),
                    CLIENT_IDENTIFIER_BYTES,
                )?;
            }
            ("dbms_session", "clear_identifier") => {
                self.change_setting(CLIENT_IDENTIFIER_SETTING, None, SettingScope::Session)?;
            }
            ("dbms_session", "set_context") => {
                let text = |name: &str| argument(name).as_ref().and_then(Value::to_text);
                let (Some(namespace), Some(attribute)) = (text("namespace"), text("attribute")) else {
                    return Err(wrong_arguments(&call.member));
                };
                let value = text("value").unwrap_or_default();
                let namespace = self.trusted_context(&namespace)?;
                crate::plsql::with_session(|session| session.set_context(&namespace, &attribute, &value))
                    .unwrap_or(Ok(()))?;
            }
            ("dbms_session", "clear_context" | "clear_all_context") => {
                let namespace = argument("namespace")
                    .as_ref()
                    .and_then(Value::to_text)
                    .ok_or_else(|| wrong_arguments(&call.member))?;
                let attribute = argument("attribute").as_ref().and_then(Value::to_text);
                let namespace = self.trusted_context(&namespace)?;
                crate::plsql::with_session(|session| session.clear_context(&namespace, attribute.as_deref()));
            }
            ("dbms_session" | "dbms_lock", _) => {
                self.package_sleep(argument("seconds").as_ref(), &call.member)?;
            }
            (_, "gather_table_stats") => {
                let owner = argument("ownname").as_ref().and_then(Value::to_text);
                let table = argument("tabname")
                    .as_ref()
                    .and_then(Value::to_text)
                    .ok_or_else(|| wrong_arguments(&call.member))?;
                let table = self.stats_table(owner, table);
                self.package_sql(&format!("ANALYZE {table}"))?;
            }
            _ => self.package_sql("ANALYZE")?,
        }
        Ok(Some(Flow::Next))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_calls_name_a_known_package() {
        let call = PackageCall::of("CALL sys.DBMS_APPLICATION_INFO.set_module('m', 'a')").unwrap();
        assert_eq!((call.package, call.member.as_str()), ("dbms_application_info", "set_module"));
        assert_eq!(call.arguments, "'m', 'a'");
        let bare = PackageCall::of("CALL dbms_session.clear_identifier").unwrap();
        assert_eq!(bare.arguments, "");
        assert!(PackageCall::of("CALL dbms_rls.add_policy('x')").is_none());
    }

    #[test]
    fn long_values_are_cut_at_a_character_boundary() {
        assert_eq!(truncated("abc".to_owned(), 2), "ab");
        assert_eq!(truncated("aé".to_owned(), 2), "a");
        assert_eq!(truncated("ok".to_owned(), 48), "ok");
    }
}
