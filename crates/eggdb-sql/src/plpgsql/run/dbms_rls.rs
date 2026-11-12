use super::super::frame::RoutineFrame;
use super::dbms_scheduler::split_arguments;
use super::{Flow, Run, parsed};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::swap_uniform_case;
use crate::types::Value;

const PACKAGE: &str = "dbms_rls";
const OWNER: &str = "sys";
const POLICIES_TABLE: &str = "eggdb_internal.oracle_rls_policies";
const CREATE_SCHEMA: &str = "CREATE SCHEMA IF NOT EXISTS eggdb_internal";
const CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS eggdb_internal.oracle_rls_policies (object_schema text NOT NULL, object_name text NOT NULL, policy_name text NOT NULL, statement_types text NOT NULL, update_check boolean NOT NULL, enabled boolean NOT NULL, predicate text, PRIMARY KEY (object_schema, object_name, policy_name))";
const BASE_POLICY: &str = "eggdb_oracle_base";
const STATIC_POLICY_TYPES: [&str; 2] = ["STATIC", "SHARED_STATIC"];
const ADD_POLICY: [&str; 15] = [
    "object_schema",
    "object_name",
    "policy_name",
    "function_schema",
    "policy_function",
    "statement_types",
    "update_check",
    "enable",
    "static_policy",
    "policy_type",
    "long_predicate",
    "sec_relevant_cols",
    "sec_relevant_cols_opt",
    "namespace",
    "attribute",
];
const DROP_POLICY: [&str; 3] = ["object_schema", "object_name", "policy_name"];
const ENABLE_POLICY: [&str; 4] = ["object_schema", "object_name", "policy_name", "enable"];
const POLICY_EXISTS: u16 = 28101;
const POLICY_MISSING: u16 = 28102;
const INVALID_INPUT: u16 = 28104;
const PREDICATE_ERROR: u16 = 28113;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuardedStatement {
    Select,
    Insert,
    Update,
    Delete,
}

impl GuardedStatement {
    const ALL: [Self; 4] = [Self::Select, Self::Insert, Self::Update, Self::Delete];

    fn keyword(self) -> &'static str {
        match self {
            Self::Select => "select",
            Self::Insert => "insert",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }

    fn list(text: Option<&str>) -> SqlResult<Vec<Self>> {
        let Some(text) = text.filter(|text| !text.trim().is_empty()) else {
            return Ok(Self::ALL.to_vec());
        };
        let mut out = Vec::new();
        for part in text.split(',').map(str::trim).filter(|part| !part.is_empty()) {
            if part.eq_ignore_ascii_case("index") {
                continue;
            }
            let kind = Self::ALL
                .into_iter()
                .find(|kind| kind.keyword().eq_ignore_ascii_case(part))
                .ok_or_else(|| invalid_input("statement_types", part))?;
            if !out.contains(&kind) {
                out.push(kind);
            }
        }
        Ok(out)
    }

    fn joined(kinds: &[Self]) -> String {
        kinds.iter().map(|kind| kind.keyword().to_ascii_uppercase()).collect::<Vec<_>>().join(",")
    }
}

struct RlsCall {
    member: String,
    arguments: String,
}

impl RlsCall {
    fn of(text: &str) -> Option<Self> {
        let text = text.trim();
        if !text.get(..4)?.eq_ignore_ascii_case("call") {
            return None;
        }
        let rest = text.get(4..)?;
        let open = rest.find('(')?;
        let close = rest.rfind(')')?;
        let name = rest.get(..open)?.trim().replace('"', "").to_ascii_lowercase();
        let parts: Vec<&str> = name.split('.').map(str::trim).collect();
        let member = match parts.as_slice() {
            [PACKAGE, member] | [OWNER, PACKAGE, member] => (*member).to_owned(),
            _ => return None,
        };
        Some(Self {
            member,
            arguments: rest.get(open + 1..close)?.to_owned(),
        })
    }
}

struct PolicyTarget {
    schema: String,
    object: String,
    policy: String,
}

struct StoredPolicy {
    kinds: Vec<GuardedStatement>,
    update_check: bool,
    predicate: Option<String>,
}

fn rls_error(code: u16, message: String) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, format!("ORA-{code:05}: {message}")).with_oracle_code(code)
}

fn invalid_input(parameter: &str, value: &str) -> SqlError {
    rls_error(INVALID_INPUT, format!("input value for {parameter} is not valid: {value}"))
}

fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn simple_oracle_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters.next().is_some_and(|first| first.is_ascii_alphabetic())
        && characters.all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '$' | '#'))
}

fn oracle_reference(name: &str) -> String {
    if simple_oracle_name(name) {
        name.to_owned()
    } else {
        identifier(name)
    }
}

fn row_policy_name(policy: &str, kind: GuardedStatement) -> String {
    identifier(&format!("eggdb_vpd_{}_{}", policy.to_lowercase(), kind.keyword()))
}

fn policy_type_name(raw: &str) -> String {
    let upper = raw.trim().trim_matches('\'').to_ascii_uppercase().replace([' ', '"'], "");
    let without_owner = upper.strip_prefix("SYS.").unwrap_or(&upper);
    without_owner.strip_prefix("DBMS_RLS.").unwrap_or(without_owner).to_owned()
}

fn truth(value: &Value, default: bool) -> bool {
    match value {
        Value::Null => default,
        Value::Boolean(value) => *value,
        other => other
            .to_text()
            .map(|text| matches!(text.trim().to_ascii_uppercase().as_str(), "TRUE" | "Y" | "YES" | "1"))
            .unwrap_or(default),
    }
}

fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        other => other.to_text().filter(|text| !text.is_empty()),
    }
}

impl Run<'_> {
    fn rls_sql(&mut self, text: &str) -> SqlResult<crate::session::QueryResult> {
        let statement = parsed(text, &self.settings, &RoutineFrame::default(), Dialect::PostgreSql)?;
        self.execute(statement)
    }

    fn rls_user(&self) -> String {
        self.settings
            .get("session_authorization")
            .unwrap_or_default()
            .to_uppercase()
    }

    fn rls_bound(arguments: &str, names: &[&str], member: &str) -> SqlResult<Vec<(String, String)>> {
        let mut bound = Vec::new();
        for (index, (name, expression)) in split_arguments(arguments).into_iter().enumerate() {
            let name = match name {
                Some(name) if names.contains(&name.as_str()) => name,
                Some(name) => {
                    return Err(SqlError::new(
                        SqlState::UNDEFINED_FUNCTION,
                        format!("PLS-00306: wrong number or types of arguments in call to '{}' ({name})", member.to_ascii_uppercase()),
                    ));
                }
                None => names.get(index).map(|name| (*name).to_owned()).ok_or_else(|| {
                    SqlError::new(
                        SqlState::UNDEFINED_FUNCTION,
                        format!("PLS-00306: wrong number or types of arguments in call to '{}'", member.to_ascii_uppercase()),
                    )
                })?,
            };
            bound.push((name, expression));
        }
        Ok(bound)
    }

    fn rls_value(&mut self, bound: &[(String, String)], name: &str) -> SqlResult<Value> {
        match bound.iter().find(|(bound_name, _)| bound_name == name) {
            Some((_, expression)) => self.text_value(expression),
            None => Ok(Value::Null),
        }
    }

    fn rls_target(&mut self, bound: &[(String, String)]) -> SqlResult<PolicyTarget> {
        let schema = text_of(&self.rls_value(bound, "object_schema")?).unwrap_or_else(|| self.rls_user());
        let object = text_of(&self.rls_value(bound, "object_name")?)
            .ok_or_else(|| invalid_input("object_name", "NULL"))?;
        let policy = text_of(&self.rls_value(bound, "policy_name")?)
            .ok_or_else(|| invalid_input("policy_name", "NULL"))?;
        Ok(PolicyTarget {
            schema: schema.to_uppercase(),
            object: object.to_uppercase(),
            policy: policy.to_uppercase(),
        })
    }

    fn rls_table(&self, target: &PolicyTarget) -> String {
        let object = identifier(&swap_uniform_case(&target.object));
        if target.schema.eq_ignore_ascii_case(&self.rls_user()) {
            object
        } else {
            format!("{}.{object}", identifier(&swap_uniform_case(&target.schema)))
        }
    }

    fn rls_key(target: &PolicyTarget) -> String {
        format!(
            "object_schema = {} AND object_name = {} AND policy_name = {}",
            quoted(&target.schema),
            quoted(&target.object),
            quoted(&target.policy)
        )
    }

    fn rls_stored(&mut self, target: &PolicyTarget) -> SqlResult<Option<StoredPolicy>> {
        let result = self.rls_sql(&format!(
            "SELECT statement_types, update_check, predicate FROM {POLICIES_TABLE} WHERE {}",
            Self::rls_key(target)
        ))?;
        let Some(row) = result.rows.first() else {
            return Ok(None);
        };
        let kinds = GuardedStatement::list(row.first().and_then(text_of).as_deref())?;
        Ok(Some(StoredPolicy {
            kinds,
            update_check: row.get(1).is_some_and(|value| truth(value, false)),
            predicate: row.get(2).and_then(text_of),
        }))
    }

    fn rls_install(&mut self, target: &PolicyTarget, stored: &StoredPolicy) -> SqlResult<()> {
        let table = self.rls_table(target);
        let condition = stored
            .predicate
            .as_deref()
            .map_or_else(|| "true".to_owned(), crate::row_security::oracle_condition);
        let base = identifier(BASE_POLICY);
        for text in [
            format!("ALTER TABLE {table} ENABLE ROW LEVEL SECURITY"),
            format!("ALTER TABLE {table} FORCE ROW LEVEL SECURITY"),
            format!("DROP POLICY IF EXISTS {base} ON {table}"),
            format!("CREATE POLICY {base} ON {table} AS PERMISSIVE USING (true) WITH CHECK (true)"),
        ] {
            self.rls_sql(&text)?;
        }
        let new_rows = if stored.update_check { condition.as_str() } else { "true" };
        for kind in &stored.kinds {
            let clause = match kind {
                GuardedStatement::Select | GuardedStatement::Delete => format!("USING ({condition})"),
                GuardedStatement::Update => format!("USING ({condition}) WITH CHECK ({new_rows})"),
                GuardedStatement::Insert if stored.update_check => format!("WITH CHECK ({condition})"),
                GuardedStatement::Insert => continue,
            };
            let row_policy = row_policy_name(&target.policy, *kind);
            self.rls_sql(&format!("DROP POLICY IF EXISTS {row_policy} ON {table}"))?;
            self.rls_sql(&format!(
                "CREATE POLICY {row_policy} ON {table} AS RESTRICTIVE FOR {} {clause}",
                kind.keyword().to_ascii_uppercase()
            ))?;
        }
        Ok(())
    }

    fn rls_uninstall(&mut self, target: &PolicyTarget) -> SqlResult<()> {
        let table = self.rls_table(target);
        for kind in GuardedStatement::ALL {
            let row_policy = row_policy_name(&target.policy, kind);
            self.rls_sql(&format!("DROP POLICY IF EXISTS {row_policy} ON {table}"))?;
        }
        Ok(())
    }

    fn rls_add_policy(&mut self, bound: &[(String, String)]) -> SqlResult<()> {
        let target = self.rls_target(bound)?;
        if self.rls_stored(&target)?.is_some() {
            return Err(rls_error(POLICY_EXISTS, "policy already exists".to_owned()));
        }
        let policy_type = bound
            .iter()
            .find(|(name, _)| name == "policy_type")
            .map(|(_, raw)| policy_type_name(raw))
            .filter(|name| name != "NULL");
        let static_policy = truth(&self.rls_value(bound, "static_policy")?, false);
        match policy_type.as_deref() {
            Some(name) if STATIC_POLICY_TYPES.contains(&name) => {}
            None if static_policy => {}
            other => {
                return Err(SqlError::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!(
                        "ORA-{INVALID_INPUT:05}: input value for policy_type is not valid: {}",
                        other.unwrap_or("DYNAMIC")
                    ),
                )
                .with_oracle_code(INVALID_INPUT)
                .with_hint("EggDB runs static VPD policies so far: pass static_policy => TRUE or policy_type => DBMS_RLS.STATIC."));
            }
        }
        if text_of(&self.rls_value(bound, "sec_relevant_cols")?).is_some() {
            return Err(SqlError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "column-level VPD (sec_relevant_cols) is not supported yet".to_owned(),
            ));
        }
        let function = text_of(&self.rls_value(bound, "policy_function")?)
            .ok_or_else(|| invalid_input("policy_function", "NULL"))?;
        let function = match text_of(&self.rls_value(bound, "function_schema")?) {
            Some(schema) => format!("{}.{}", oracle_reference(&schema), oracle_reference(&function)),
            None => oracle_reference(&function),
        };
        let kinds = GuardedStatement::list(text_of(&self.rls_value(bound, "statement_types")?).as_deref())?;
        let update_check = truth(&self.rls_value(bound, "update_check")?, false);
        let enabled = truth(&self.rls_value(bound, "enable")?, true);
        let predicate = text_of(&self.text_value(&format!(
            "{function}({}, {})",
            quoted(&target.schema),
            quoted(&target.object)
        ))?)
        .filter(|predicate| !predicate.trim().is_empty());
        if let Some(predicate) = &predicate {
            let tokens = crate::lexer::tokenize_in(predicate, Dialect::Oracle)?;
            let mut parser = crate::parser::Parser::new(predicate, &tokens)
                .with_context(crate::parser::Parameters::Absent, &self.settings)
                .in_dialect(Dialect::Oracle);
            parser.expression().map_err(|error| {
                rls_error(PREDICATE_ERROR, format!("policy predicate has error: {}", error.message()))
            })?;
            if !parser.at_end() {
                return Err(rls_error(PREDICATE_ERROR, "policy predicate has error".to_owned()));
            }
        }
        self.rls_sql(CREATE_SCHEMA)?;
        self.rls_sql(CREATE_TABLE)?;
        self.rls_sql(&format!(
            "INSERT INTO {POLICIES_TABLE} VALUES ({}, {}, {}, {}, {update_check}, {enabled}, {})",
            quoted(&target.schema),
            quoted(&target.object),
            quoted(&target.policy),
            quoted(&GuardedStatement::joined(&kinds)),
            predicate.as_deref().map_or_else(|| "NULL".to_owned(), quoted)
        ))?;
        if enabled {
            let stored = StoredPolicy {
                kinds,
                update_check,
                predicate,
            };
            self.rls_install(&target, &stored)?;
        }
        Ok(())
    }

    fn rls_drop_policy(&mut self, bound: &[(String, String)]) -> SqlResult<()> {
        let target = self.rls_target(bound)?;
        self.rls_sql(CREATE_SCHEMA)?;
        self.rls_sql(CREATE_TABLE)?;
        if self.rls_stored(&target)?.is_none() {
            return Err(rls_error(POLICY_MISSING, "policy does not exist".to_owned()));
        }
        self.rls_uninstall(&target)?;
        self.rls_sql(&format!("DELETE FROM {POLICIES_TABLE} WHERE {}", Self::rls_key(&target)))?;
        Ok(())
    }

    fn rls_enable_policy(&mut self, bound: &[(String, String)]) -> SqlResult<()> {
        let target = self.rls_target(bound)?;
        let enable = truth(&self.rls_value(bound, "enable")?, true);
        self.rls_sql(CREATE_SCHEMA)?;
        self.rls_sql(CREATE_TABLE)?;
        let Some(stored) = self.rls_stored(&target)? else {
            return Err(rls_error(POLICY_MISSING, "policy does not exist".to_owned()));
        };
        self.rls_sql(&format!(
            "UPDATE {POLICIES_TABLE} SET enabled = {enable} WHERE {}",
            Self::rls_key(&target)
        ))?;
        if enable {
            self.rls_install(&target, &stored)
        } else {
            self.rls_uninstall(&target)
        }
    }

    pub(super) fn dbms_rls_call(&mut self, text: &str) -> SqlResult<Option<Flow>> {
        let Some(call) = RlsCall::of(text) else {
            return Ok(None);
        };
        let names: &[&str] = match call.member.as_str() {
            "add_policy" => &ADD_POLICY,
            "drop_policy" => &DROP_POLICY,
            "enable_policy" => &ENABLE_POLICY,
            other => {
                return Err(SqlError::new(
                    SqlState::UNDEFINED_FUNCTION,
                    format!("PLS-00302: component '{}' must be declared", other.to_ascii_uppercase()),
                ));
            }
        };
        let bound = Self::rls_bound(&call.arguments, names, &call.member)?;
        match call.member.as_str() {
            "add_policy" => self.rls_add_policy(&bound)?,
            "drop_policy" => self.rls_drop_policy(&bound)?,
            _ => self.rls_enable_policy(&bound)?,
        }
        Ok(Some(Flow::Next))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statement_types_default_to_every_guarded_statement() {
        assert_eq!(GuardedStatement::list(None).unwrap(), GuardedStatement::ALL.to_vec());
        assert_eq!(
            GuardedStatement::list(Some("select, INDEX, Update, select")).unwrap(),
            vec![GuardedStatement::Select, GuardedStatement::Update]
        );
        assert!(GuardedStatement::list(Some("merge")).is_err());
    }

    #[test]
    fn policy_types_lose_their_package_prefix() {
        assert_eq!(policy_type_name("DBMS_RLS.STATIC"), "STATIC");
        assert_eq!(policy_type_name("sys.dbms_rls.shared_static"), "SHARED_STATIC");
        assert_eq!(policy_type_name("dbms_rls.context_sensitive"), "CONTEXT_SENSITIVE");
    }

    #[test]
    fn rls_calls_name_the_package() {
        let call = RlsCall::of("CALL sys.dbms_rls.add_policy(object_name => 'T')").unwrap();
        assert_eq!(call.member, "add_policy");
        assert!(RlsCall::of("CALL dbms_scheduler.run_job('x')").is_none());
    }
}
